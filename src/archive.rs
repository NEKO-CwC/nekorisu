//! The upload engine: copies sessions from this machine to the storage in the remote
//! layout of the archive format. Everything it writes is new and under `nekorisu/v1/`; nothing is
//! overwritten or deleted.
//!
//! Per session: read this device's newest manifest, compare each local file with what
//! that manifest recorded, and upload only what is new, as segments cut at line
//! boundaries (a line still being written is left for next time). Segments and line
//! maps go first, the manifest last, then one operations-log file for the whole run.
//! Only structure is read from the files (sizes, line breaks, record types, a few
//! metadata fields); nothing here looks at message content.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agents::{AgentKind, Discovery, FileRole, Session};
use crate::daemon::device::Device;
use crate::linemap::LineMap;
use crate::storage::{Put, StorageError, Webdav};
use crate::timefmt::now_ms;

/// Everything this client writes lives under here.
pub const ROOT: &str = "nekorisu/v1";
/// A new stretch of a file is cut into segments of about this size (at a line boundary), so a
/// big first upload neither sits in memory whole nor needs one huge request. Tests use a small
/// one so that a few KiB of synthetic lines make several segments.
const SEGMENT_LIMIT: u64 = if cfg!(test) { 8 << 10 } else { 16 << 20 };

const MANIFEST_FORMAT: &str = "nekorisu.manifest/1";
const LINEMAP_FORMAT: &str = "nekorisu.linemap/1";

// ---- what goes into the storage ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub id: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub session: SessionInfo,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    pub files: Vec<FileEntry>,
    #[serde(default)]
    pub links: Links,
    #[serde(default)]
    pub rewrite: bool,
    #[serde(default)]
    pub encryption: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionInfo {
    pub agent: String,
    pub id: String,
    pub agent_version: String,
    pub project: String,
    pub cwd: String,
    pub title: Option<String>,
    pub first_ts_ms: i64,
    pub last_ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub role: String,
    /// Bytes uploaded so far.
    pub size: u64,
    /// SHA-256 of all those bytes.
    pub sha256: String,
    #[serde(default)]
    pub mtime_ms: i64,
    /// "as-is", or "decompressed" for a Codex `.jsonl.zst`.
    pub form: String,
    pub segments: Vec<SegmentInfo>,
    #[serde(default)]
    pub lines: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentInfo {
    pub start: u64,
    pub len: u64,
    pub sha256: String,
    pub lines: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Links {
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub fork_from: Option<String>,
}

#[derive(Serialize)]
struct LineMapFile {
    format: &'static str,
    raw_types: Vec<String>,
    /// `[offset in the segment, length without line break, kind, index into raw_types, ts_ms]`
    lines: Vec<(u64, u64, &'static str, u16, i64)>,
}

#[derive(Serialize)]
struct LogLine {
    seq: u64,
    device: String,
    at_ms: i64,
    op: &'static str,
    sessions: Vec<String>,
    tags: Vec<String>,
    note: Option<String>,
    detail: LogDetail,
}

#[derive(Serialize)]
struct LogDetail {
    manifest: String,
    bytes: u64,
}

// ---- input and output of a run ----

/// One session to upload, with the relations the manifest records.
#[derive(Debug, Clone)]
pub struct Job {
    pub session: Session,
    /// `<agent>:<id>` of the parent thread.
    pub parent: Option<String>,
    pub children: Vec<String>,
}

/// Progress, as it happens. The callback answers whether to go on; `false` (the task was
/// cancelled) stops the run after the current segment.
pub type Progress<'a> = &'a (dyn Fn(Event) -> bool + Send + Sync);

#[derive(Debug, Clone, Copy)]
pub enum Event {
    /// All sessions are compared with the storage; this many bytes will be uploaded.
    Planned {
        total_bytes: u64,
    },
    Uploaded {
        bytes: u64,
    },
    /// Everything is uploaded; the operations log is next.
    Committing,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionReport {
    pub session: String,
    pub manifest: Option<String>,
    pub files: usize,
    pub new_bytes: u64,
    pub new_lines: u64,
    pub up_to_date: bool,
    pub rewrite: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub sessions: Vec<SessionReport>,
    /// The manifests are written but the operations log was not.
    pub log_error: Option<String>,
}

impl Outcome {
    pub fn failed(&self) -> usize {
        self.sessions.iter().filter(|s| s.error.is_some()).count()
    }
}

pub fn session_key(agent: AgentKind, id: &str) -> String {
    format!("{}:{id}", agent.name())
}

/// The sessions to upload for `wanted`: each one's whole tree, from its root down.
/// For Codex that is the thread plus its sub-agent threads; Claude Code's sub-agent
/// transcripts are files of the parent session already.
pub fn jobs_for(found: &[Discovery], wanted: &[(AgentKind, String)]) -> Vec<Job> {
    let all: Vec<&Session> = found.iter().flat_map(|d| &d.sessions).collect();
    let index: HashMap<(AgentKind, &str), &Session> = all.iter().map(|s| ((s.agent, s.id.as_str()), *s)).collect();
    let mut kids: HashMap<(AgentKind, &str), Vec<&str>> = HashMap::new();
    for s in &all {
        if let Some(parent) = &s.parent {
            kids.entry((s.agent, parent.as_str())).or_default().push(&s.id);
        }
    }
    for list in kids.values_mut() {
        list.sort_unstable();
    }

    let mut queue: VecDeque<&Session> = VecDeque::new();
    for (agent, id) in wanted {
        let Some(mut session) = index.get(&(*agent, id.as_str())).copied() else {
            continue;
        };
        for _ in 0..64 {
            match session.parent.as_deref().and_then(|p| index.get(&(session.agent, p))) {
                Some(parent) => session = parent,
                None => break,
            }
        }
        queue.push_back(session);
    }
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    while let Some(session) = queue.pop_front() {
        if !seen.insert((session.agent, session.id.clone())) {
            continue;
        }
        let children = kids
            .get(&(session.agent, session.id.as_str()))
            .map(|ids| ids.as_slice())
            .unwrap_or_default();
        for id in children {
            if let Some(child) = index.get(&(session.agent, *id)) {
                queue.push_back(child);
            }
        }
        jobs.push(Job {
            session: session.clone(),
            parent: session.parent.as_deref().map(|p| session_key(session.agent, p)),
            children: children.iter().map(|id| session_key(session.agent, id)).collect(),
        });
    }
    jobs
}

// ---- running ----

/// Uploads `jobs`. A problem with one session's local files fails that session only; a problem
/// with the storage stops the run (the rest is reported as not uploaded).
pub async fn run(store: &Webdav, device: &Device, jobs: Vec<Job>, on: Progress<'_>) -> Outcome {
    enum Slot {
        Done(SessionReport),
        Plan(Box<SessionPlan>),
    }
    let mut slots = Vec::new();
    let mut stopped: Option<String> = None;
    for job in jobs {
        let key = session_key(job.session.agent, &job.session.id);
        if let Some(why) = &stopped {
            slots.push(Slot::Done(failure(&key, format!("没有上传：{why}"))));
            continue;
        }
        match plan_session(store, device, job).await {
            Ok(plan) => slots.push(Slot::Plan(Box::new(plan))),
            Err(err) => {
                if err.downcast_ref::<StorageError>().is_some() {
                    stopped = Some(format!("{err:#}"));
                }
                slots.push(Slot::Done(failure(&key, format!("{err:#}"))));
            }
        }
    }

    let total: u64 = slots
        .iter()
        .map(|s| match s {
            Slot::Plan(plan) => plan.new_bytes,
            Slot::Done(_) => 0,
        })
        .sum();
    if !on(Event::Planned { total_bytes: total }) {
        stopped = Some(Cancelled.to_string());
    }

    let mut outcome = Outcome::default();
    let mut uploaded: Vec<(String, String, u64)> = Vec::new(); // session, manifest id, bytes
    let mut device_written = false;
    for slot in slots {
        let plan = match slot {
            Slot::Done(report) => {
                outcome.sessions.push(report);
                continue;
            }
            Slot::Plan(plan) => *plan,
        };
        let key = session_key(plan.agent, &plan.id);
        if !plan.changed {
            outcome.sessions.push(SessionReport {
                session: key,
                manifest: plan.prior_id.clone(),
                files: plan.items.len(),
                new_bytes: 0,
                new_lines: 0,
                up_to_date: true,
                rewrite: false,
                error: None,
            });
            continue;
        }
        if let Some(why) = &stopped {
            outcome.sessions.push(failure(&key, format!("没有上传：{why}")));
            continue;
        }
        let result = async {
            if !device_written {
                write_device(store, device).await?;
            }
            upload_session(store, device, plan, on).await
        }
        .await;
        device_written = true;
        match result {
            Ok(report) => {
                uploaded.push((
                    report.session.clone(),
                    report.manifest.clone().unwrap_or_default(),
                    report.new_bytes,
                ));
                outcome.sessions.push(report);
            }
            Err(err) => {
                if err.downcast_ref::<StorageError>().is_some() || err.downcast_ref::<Cancelled>().is_some() {
                    stopped = Some(format!("{err:#}"));
                }
                outcome.sessions.push(failure(&key, format!("{err:#}")));
            }
        }
    }

    if !uploaded.is_empty() {
        // What was written is real even if the task was cancelled meanwhile, so it is logged.
        on(Event::Committing);
        if let Err(err) = write_log(store, device, &uploaded).await {
            outcome.log_error = Some(format!("{err:#}"));
        }
    }
    outcome
}

/// The task was cancelled while uploading.
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "任务已取消")
    }
}

impl std::error::Error for Cancelled {}

fn failure(session: &str, error: String) -> SessionReport {
    SessionReport {
        session: session.to_string(),
        manifest: None,
        files: 0,
        new_bytes: 0,
        new_lines: 0,
        up_to_date: false,
        rewrite: false,
        error: Some(error),
    }
}

// ---- planning: compare local files with this device's newest manifest ----

struct SessionPlan {
    agent: AgentKind,
    id: String,
    seq: u64,
    prior_id: Option<String>,
    items: Vec<Item>,
    info: SessionInfo,
    links: Links,
    changed: bool,
    rewrite: bool,
    new_bytes: u64,
}

enum Item {
    /// Nothing new: the entry of the previous manifest, carried over (also for files the
    /// agent has deleted locally since; the archive keeps them).
    Keep(FileEntry),
    Send {
        local: LocalFile,
        send: SendPlan,
    },
}

struct SendPlan {
    start: u64,
    end: u64,
    /// The local file no longer starts with what was uploaded: send it again from byte 0.
    rewrite: bool,
    map: Option<LineMap>,
    /// Hash state after the first `start` bytes.
    hasher: Sha256,
    base_segments: Vec<SegmentInfo>,
    base_lines: u64,
}

#[derive(Clone)]
struct LocalFile {
    path: PathBuf,
    /// Path inside the session on the storage: the agent-relative path, `.zst` dropped.
    uploaded: String,
    compressed: bool,
    role: FileRole,
    mtime_ms: i64,
    /// JSONL transcripts follow the line rules and get a line map; the rest are opaque bytes.
    line_based: bool,
}

impl LocalFile {
    fn form(&self) -> &'static str {
        if self.compressed { "decompressed" } else { "as-is" }
    }
}

fn local_files(session: &Session) -> Vec<LocalFile> {
    let mut files: Vec<LocalFile> = session
        .files
        .iter()
        .map(|f| {
            let compressed = f.rel.ends_with(".jsonl.zst");
            LocalFile {
                path: f.path.clone(),
                uploaded: f
                    .rel
                    .strip_suffix(".zst")
                    .filter(|_| compressed)
                    .unwrap_or(&f.rel)
                    .to_string(),
                compressed,
                role: f.role,
                mtime_ms: f.mtime_ms,
                line_based: matches!(f.role, FileRole::Transcript | FileRole::Subagent),
            }
        })
        .collect();
    // If a Codex rollout exists both ways while it is being compressed, the plain one wins.
    files.sort_by(|a, b| (&a.uploaded, a.compressed).cmp(&(&b.uploaded, b.compressed)));
    files.dedup_by(|later, first| later.uploaded == first.uploaded);
    files
}

fn role_name(role: FileRole) -> &'static str {
    match role {
        FileRole::Transcript => "transcript",
        FileRole::Subagent => "subagent",
        FileRole::ToolResult => "tool_result",
        FileRole::FileHistory => "file_history",
        FileRole::Asset => "asset",
        FileRole::Sidecar => "other",
    }
}

fn session_dir(agent: &str, id: &str) -> String {
    format!("{ROOT}/sessions/{agent}/{id}")
}

async fn plan_session(store: &Webdav, device: &Device, job: Job) -> anyhow::Result<SessionPlan> {
    let session = &job.session;
    let dir = session_dir(session.agent.name(), &session.id);
    let (seq, prior) = match latest_manifest(store, &dir, &device.id).await? {
        Some((seq, manifest)) => (seq + 1, Some(manifest)),
        None => (1, None),
    };
    let mut recorded: HashMap<String, FileEntry> = prior
        .as_ref()
        .map(|m| m.files.iter().map(|f| (f.path.clone(), f.clone())).collect())
        .unwrap_or_default();

    let mut items = Vec::new();
    let mut info = SessionInfo {
        agent: session.agent.name().to_string(),
        id: session.id.clone(),
        project: session.project.clone(),
        ..Default::default()
    };
    let mut changed = prior.is_none();
    let mut rewrite = false;
    let mut new_bytes = 0;
    for local in local_files(session) {
        let record = recorded.remove(&local.uploaded);
        let (agent, is_main) = (session.agent, local.path == session.main);
        let (job_local, job_record) = (local.clone(), record.clone());
        let planned = tokio::task::spawn_blocking(move || plan_file(agent, &job_local, job_record.as_ref(), is_main))
            .await
            .context("本地读文件的任务出错")?;
        let planned = match planned {
            Ok(planned) => planned,
            // A side file the agent deleted since discovery: skip it (an archived copy stays).
            Err(err) if err.kind() == io::ErrorKind::NotFound && !local.line_based => {
                if let Some(record) = record {
                    items.push(Item::Keep(record));
                }
                continue;
            }
            Err(err) => return Err(anyhow!("读不了 {}：{err}", local.uploaded)),
        };
        if local.role == FileRole::Transcript
            && let Some((first, last)) = planned.ts
        {
            if info.first_ts_ms == 0 || first < info.first_ts_ms {
                info.first_ts_ms = first;
            }
            info.last_ts_ms = info.last_ts_ms.max(last);
        }
        if is_main {
            info.agent_version = planned.head.version.unwrap_or_default();
            info.cwd = planned.head.cwd.unwrap_or_default();
            info.title = planned.title;
        }
        match planned.action {
            Action::Keep(entry) => items.push(Item::Keep(entry)),
            Action::Send(send) => {
                changed = true;
                rewrite |= send.rewrite;
                new_bytes += send.end - send.start;
                items.push(Item::Send { local, send });
            }
        }
    }
    // Files of the previous manifest that are not on disk any more.
    let mut gone: Vec<FileEntry> = recorded.into_values().collect();
    gone.sort_by(|a, b| a.path.cmp(&b.path));
    items.extend(gone.into_iter().map(Item::Keep));
    if info.cwd.is_empty() && session.agent == AgentKind::Codex {
        info.cwd = session.project.clone();
    }

    Ok(SessionPlan {
        agent: session.agent,
        id: session.id.clone(),
        seq,
        prior_id: prior.map(|m| m.id),
        items,
        info,
        links: Links {
            parent: job.parent,
            children: job.children,
            fork_from: None,
        },
        changed,
        rewrite,
        new_bytes,
    })
}

/// This device's newest manifest of a session: `(its sequence number, its content)`.
async fn latest_manifest(store: &Webdav, dir: &str, device: &str) -> anyhow::Result<Option<(u64, Manifest)>> {
    let prefix = format!("{device}-");
    let newest = store
        .list(&format!("{dir}/manifests"))
        .await?
        .into_iter()
        .filter(|e| !e.is_dir)
        .filter_map(|e| {
            let seq = e
                .name
                .strip_prefix(&prefix)?
                .strip_suffix(".json")?
                .parse::<u64>()
                .ok()?;
            Some(seq)
        })
        .max();
    let Some(seq) = newest else { return Ok(None) };
    let name = format!("{device}-{seq}");
    let bytes = store.get(&format!("{dir}/manifests/{name}.json"), None).await?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).with_context(|| format!("上一份 manifest（{name}）读不懂，没有继续上传"))?;
    if manifest.id != name {
        bail!("manifest {name} 里记的 id 是 {}，对不上，没有继续上传", manifest.id);
    }
    Ok(Some((seq, manifest)))
}

struct Planned {
    action: Action,
    /// First and last timestamp of the file, if it has any.
    ts: Option<(i64, i64)>,
    head: Head,
    title: Option<String>,
}

enum Action {
    Keep(FileEntry),
    Send(SendPlan),
}

/// Reads one local file once and decides what of it is new. Blocking.
fn plan_file(agent: AgentKind, local: &LocalFile, record: Option<&FileEntry>, is_main: bool) -> io::Result<Planned> {
    let mark = record.map_or(0, |r| r.size);
    let analysis = analyze(agent, local, mark)?;
    let map = analysis.map.as_ref();
    let ts = map.and_then(|m| {
        let mut times = m.lines.iter().map(|l| l.ts_ms).filter(|t| *t > 0);
        let first = times.next()?;
        Some((first, times.fold(first, |_, t| t)))
    });
    let (head, title) = if is_main {
        let head = read_head(agent, local).unwrap_or_default();
        let title = match map {
            Some(m) if agent == AgentKind::ClaudeCode => read_title(&local.path, m),
            _ => None,
        };
        (head, title)
    } else {
        (Head::default(), None)
    };

    // The recorded part is still the start of the file: same bytes, and it ended at a line break.
    let prefix_unchanged = record.is_some_and(|r| {
        let boundary = match &analysis.map {
            Some(m) => {
                r.size == 0 || r.size == m.indexed || m.lines.binary_search_by_key(&r.size, |l| l.offset).is_ok()
            }
            None => true,
        };
        boundary
            && analysis
                .prefix
                .as_ref()
                .is_some_and(|h| hex::encode(h.clone().finalize()) == r.sha256)
    });
    let send = |start: u64, rewrite: bool, hasher: Sha256, base_segments: Vec<SegmentInfo>, base_lines: u64| {
        Action::Send(SendPlan {
            start,
            end: analysis.len,
            rewrite,
            map: None,
            hasher,
            base_segments,
            base_lines,
        })
    };
    let mut action = match record {
        Some(r) if prefix_unchanged && analysis.len == r.size => Action::Keep(r.clone()),
        Some(r) if prefix_unchanged && analysis.len > r.size => {
            let base_lines = map.map_or(0, |m| m.lines.partition_point(|l| l.offset < r.size) as u64);
            let hasher = analysis.prefix.clone().expect("prefix_unchanged implies a prefix");
            send(r.size, false, hasher, r.segments.clone(), base_lines)
        }
        Some(_) => send(0, true, Sha256::new(), Vec::new(), 0),
        None => send(0, false, Sha256::new(), Vec::new(), 0),
    };
    if let Action::Send(plan) = &mut action {
        plan.map = analysis.map;
    }
    Ok(Planned {
        action,
        ts,
        head,
        title,
    })
}

struct Analysis {
    /// Bytes of complete lines (opaque files: all bytes).
    len: u64,
    /// Hash state after the first `mark` bytes; `None` if the file is shorter.
    prefix: Option<Sha256>,
    /// The line map of the whole file, for line-based files.
    map: Option<LineMap>,
}

/// One pass over the (decompressed) bytes: hashes the first `mark` bytes and, for
/// line-based files, builds the line map of the whole file. Building it from the start is what
/// lets Codex's classifier carry state (`session_meta`) into later segments.
fn analyze(agent: AgentKind, local: &LocalFile, mark: u64) -> io::Result<Analysis> {
    let mut tee = Tee::new(open_logical(local)?, mark);
    let (len, map) = if local.line_based {
        let mut map = LineMap::new(agent);
        map.extend_from(&mut tee)?;
        (map.indexed, Some(map))
    } else {
        io::copy(&mut tee, &mut io::sink())?;
        (tee.pos, None)
    };
    Ok(Analysis {
        len,
        prefix: tee.at_mark,
        map,
    })
}

/// Passes bytes through and keeps the hash state at one position.
struct Tee<R> {
    inner: R,
    pos: u64,
    mark: u64,
    running: Sha256,
    at_mark: Option<Sha256>,
}

impl<R> Tee<R> {
    fn new(inner: R, mark: u64) -> Self {
        Self {
            inner,
            pos: 0,
            mark,
            running: Sha256::new(),
            at_mark: (mark == 0).then(Sha256::new),
        }
    }
}

impl<R: Read> Read for Tee<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if self.at_mark.is_none() {
            let take = (self.mark - self.pos).min(n as u64) as usize;
            self.running.update(&buf[..take]);
            if self.pos + take as u64 == self.mark {
                self.at_mark = Some(self.running.clone());
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

fn open_logical(local: &LocalFile) -> io::Result<Box<dyn Read + Send>> {
    let file = File::open(&local.path)?;
    if local.compressed {
        Ok(Box::new(zstd::stream::read::Decoder::new(file)?))
    } else {
        Ok(Box::new(file))
    }
}

// ---- manifest fields that come from structure, not content ----

#[derive(Default)]
struct Head {
    version: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeHead {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct CodexHead {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    payload: Option<CodexHeadPayload>,
}

#[derive(Deserialize)]
struct CodexHeadPayload {
    #[serde(default)]
    cli_version: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

/// The agent's version and the working directory, from the first records of the main transcript.
fn read_head(agent: AgentKind, local: &LocalFile) -> io::Result<Head> {
    const LIMIT: u64 = 64 * 1024;
    let mut buf = Vec::new();
    open_logical(local)?.take(LIMIT).read_to_end(&mut buf)?;
    let complete = if buf.len() as u64 == LIMIT {
        buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1)
    } else {
        buf.len()
    };
    let mut head = Head::default();
    for line in buf[..complete].split(|&b| b == b'\n').take(200) {
        match agent {
            AgentKind::ClaudeCode => {
                if let Ok(record) = serde_json::from_slice::<ClaudeHead>(line) {
                    head.version = head.version.or(record.version);
                    head.cwd = head.cwd.or(record.cwd);
                }
            }
            AgentKind::Codex => {
                if let Ok(record) = serde_json::from_slice::<CodexHead>(line)
                    && record.kind == "session_meta"
                    && let Some(payload) = record.payload
                {
                    head.version = head.version.or(payload.cli_version);
                    head.cwd = head.cwd.or(payload.cwd);
                }
            }
        }
        if head.version.is_some() && head.cwd.is_some() {
            break;
        }
    }
    Ok(head)
}

/// Claude Code records the session's title as its own record type (`custom-title` set by the
/// user, `ai-title` made by the agent). Found through the line map, so only that line is read.
fn read_title(path: &Path, map: &LineMap) -> Option<String> {
    let last_of = |raw: &str| {
        let index = map.raw_types.iter().position(|t| t == raw)?;
        map.lines.iter().rev().find(|l| l.raw_type as usize == index)
    };
    let (line, key) = match last_of("custom-title") {
        Some(line) => (line, "customTitle"),
        None => (last_of("ai-title")?, "aiTitle"),
    };
    if line.len > 64 * 1024 {
        return None;
    }
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(line.offset)).ok()?;
    let mut bytes = vec![0; line.len as usize];
    file.read_exact(&mut bytes).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let title = value.get(key)?.as_str()?.trim();
    (!title.is_empty()).then(|| title.chars().take(200).collect())
}

// ---- uploading ----

async fn write_device(store: &Webdav, device: &Device) -> anyhow::Result<()> {
    let body = serde_json::to_vec_pretty(&serde_json::json!({
        "id": device.id,
        "name": device.name,
        "created_at_ms": device.created_at_ms,
    }))?;
    // Written once: if an earlier run made it, that stays.
    let path = format!("{ROOT}/devices/{}.json", device.id);
    if store.stat(&path).await?.is_none() {
        put(store, path, body).await?;
    }
    Ok(())
}

/// The only way anything is written: refuses a path outside `nekorisu/v1/`.
async fn put(store: &Webdav, path: String, body: Vec<u8>) -> Result<Put, StorageError> {
    if !path.starts_with(&format!("{ROOT}/")) {
        return Err(StorageError::Invalid(format!("不会写到 {ROOT}/ 以外的地方：{path}")));
    }
    store.put_new(&path, body).await
}

/// Like `put`, for segments, whose names are made from their content: if the name is taken, the
/// content is already there, provided it has the same size (a server that reports no size says
/// 0, which proves nothing). The rule that nothing is overwritten stays: it is left alone.
async fn put_addressed(store: &Webdav, path: String, body: Vec<u8>) -> Result<(), StorageError> {
    let len = body.len() as u64;
    if put(store, path.clone(), body).await? == Put::Exists
        && let Some(size) = store.stat(&path).await?
        && size != 0
        && size != len
    {
        return Err(StorageError::Invalid(format!(
            "{path} 已经在存储里，但大小不对（{size}，应为 {len}）"
        )));
    }
    Ok(())
}

async fn upload_session(
    store: &Webdav,
    device: &Device,
    plan: SessionPlan,
    on: Progress<'_>,
) -> anyhow::Result<SessionReport> {
    let agent = plan.agent.name();
    let dir = session_dir(agent, &plan.id);
    let mut entries = Vec::new();
    let (mut new_bytes, mut new_lines) = (0, 0);
    for item in plan.items {
        match item {
            Item::Keep(entry) => entries.push(entry),
            Item::Send { local, send } => {
                let (start, end) = (send.start, send.end);
                let (entry, lines) = upload_file(store, &dir, &local, send, on).await?;
                new_bytes += end - start;
                new_lines += lines;
                entries.push(entry);
            }
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let id = format!("{}-{}", device.id, plan.seq);
    let manifest = Manifest {
        format: MANIFEST_FORMAT.to_string(),
        id: id.clone(),
        parent: plan.prior_id,
        session: plan.info,
        device: device.id.clone(),
        created_at_ms: now_ms(),
        files: entries,
        links: plan.links,
        rewrite: plan.rewrite,
        encryption: "none".to_string(),
    };
    let files = manifest.files.len();
    // The manifest is the commit point: everything it names is already in the storage.
    let put = put(
        store,
        format!("{dir}/manifests/{id}.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )
    .await?;
    if put == Put::Exists {
        bail!("manifest {id} 已经在存储里（同一台设备同时在上传？），没有覆盖它");
    }
    Ok(SessionReport {
        session: session_key(plan.agent, &plan.id),
        manifest: Some(id),
        files,
        new_bytes,
        new_lines,
        up_to_date: false,
        rewrite: plan.rewrite,
        error: None,
    })
}

/// A stretch of a file to send: where it starts and ends in the file, and which lines it holds.
struct Cut {
    start: u64,
    end: u64,
    first_line: usize,
    last_line: usize,
}

/// Cuts `[start, end)` at line boundaries into pieces of about `SEGMENT_LIMIT`, or, for opaque
/// files, at plain byte positions.
fn cuts(map: Option<&LineMap>, start: u64, end: u64) -> Vec<Cut> {
    let mut out = Vec::new();
    let Some(map) = map else {
        let mut at = start;
        while at < end {
            let next = (at + SEGMENT_LIMIT).min(end);
            out.push(Cut {
                start: at,
                end: next,
                first_line: 0,
                last_line: 0,
            });
            at = next;
        }
        return out;
    };
    let first = map.lines.partition_point(|l| l.offset < start);
    let (mut seg_start, mut seg_first) = (start, first);
    for i in first..map.lines.len() {
        let line_end = map.lines.get(i + 1).map_or(map.indexed, |next| next.offset);
        if line_end - seg_start >= SEGMENT_LIMIT {
            out.push(Cut {
                start: seg_start,
                end: line_end,
                first_line: seg_first,
                last_line: i + 1,
            });
            (seg_start, seg_first) = (line_end, i + 1);
        }
    }
    if seg_start < end {
        out.push(Cut {
            start: seg_start,
            end,
            first_line: seg_first,
            last_line: map.lines.len(),
        });
    }
    out
}

/// What one blocking read of a segment hands back.
struct Chunk {
    reader: Box<dyn Read + Send>,
    hasher: Sha256,
    bytes: Vec<u8>,
    sha256: String,
}

/// Uploads the new stretch of one file and returns its manifest entry and the new line count.
async fn upload_file(
    store: &Webdav,
    dir: &str,
    local: &LocalFile,
    send: SendPlan,
    on: Progress<'_>,
) -> anyhow::Result<(FileEntry, u64)> {
    let pieces = cuts(send.map.as_ref(), send.start, send.end);
    let mut segments = send.base_segments;
    let mut total_lines = send.base_lines;
    let mut new_lines = 0;
    let mut hasher = send.hasher;
    if !pieces.is_empty() {
        let (path, start) = (local.clone(), send.start);
        let mut reader = tokio::task::spawn_blocking(move || -> io::Result<Box<dyn Read + Send>> {
            let mut reader = open_logical(&path)?;
            let skipped = io::copy(&mut reader.by_ref().take(start), &mut io::sink())?;
            if skipped != start {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            Ok(reader)
        })
        .await??;
        for cut in &pieces {
            let len = cut.end - cut.start;
            let task_hasher = hasher;
            let chunk = tokio::task::spawn_blocking(move || -> io::Result<Chunk> {
                let mut bytes = vec![0; len as usize];
                reader.read_exact(&mut bytes)?;
                let mut hasher = task_hasher;
                hasher.update(&bytes);
                let sha256 = hex::encode(Sha256::digest(&bytes));
                Ok(Chunk {
                    reader,
                    hasher,
                    bytes,
                    sha256,
                })
            })
            .await?
            .map_err(|err| anyhow!("读 {} 出错：{err}", local.uploaded))?;
            reader = chunk.reader;
            hasher = chunk.hasher;

            let lines = cut.last_line - cut.first_line;
            let map_file = match &send.map {
                Some(map) => linemap_of(map, cut, &chunk.bytes)
                    .ok_or_else(|| anyhow!("{} 在上传的时候被改动了，稍后再试", local.uploaded))?,
                None => LineMapFile {
                    format: LINEMAP_FORMAT,
                    raw_types: Vec::new(),
                    lines: Vec::new(),
                },
            };
            let seg_path = format!("{dir}/files/{}/{}-{}.seg", local.uploaded, cut.start, chunk.sha256);
            let sha = chunk.sha256.clone();
            put_addressed(store, seg_path, chunk.bytes).await?;
            // Derived from the segment (and, for Codex, from what came before it), so one that is
            // there already is left as it is: the first writer wins.
            put(
                store,
                format!("{dir}/linemaps/{sha}.json"),
                serde_json::to_vec(&map_file)?,
            )
            .await?;
            if !on(Event::Uploaded { bytes: len }) {
                return Err(Cancelled.into());
            }
            segments.push(SegmentInfo {
                start: cut.start,
                len,
                sha256: sha,
                lines: lines as u64,
            });
            total_lines += lines as u64;
            new_lines += lines as u64;
        }
    }
    let entry = FileEntry {
        path: local.uploaded.clone(),
        role: role_name(local.role).to_string(),
        size: send.end,
        sha256: hex::encode(hasher.finalize()),
        mtime_ms: local.mtime_ms,
        form: local.form().to_string(),
        segments,
        lines: if local.line_based { total_lines } else { 0 },
    };
    Ok((entry, new_lines))
}

/// The line map of one segment, cut from the map of the whole file: offsets are relative to
/// the segment, and the record types it uses are renumbered. `None` if the bytes do not match
/// the map (the file changed between reading and uploading).
fn linemap_of(map: &LineMap, cut: &Cut, bytes: &[u8]) -> Option<LineMapFile> {
    let mut raw_types: Vec<String> = Vec::new();
    let mut renumber: HashMap<u16, u16> = HashMap::new();
    let mut lines = Vec::with_capacity(cut.last_line - cut.first_line);
    for line in &map.lines[cut.first_line..cut.last_line] {
        let offset = line.offset.checked_sub(cut.start)?;
        let end = (offset + line.len) as usize;
        let breaks = match bytes.get(end..end + 2) {
            Some(b"\r\n") => true,
            _ => bytes.get(end) == Some(&b'\n'),
        };
        if !breaks {
            return None;
        }
        let index = *renumber.entry(line.raw_type).or_insert_with(|| {
            raw_types.push(map.raw_types[line.raw_type as usize].clone());
            (raw_types.len() - 1) as u16
        });
        lines.push((offset, line.len, line.kind.as_str(), index, line.ts_ms));
    }
    if bytes.last() != Some(&b'\n') {
        return None;
    }
    Some(LineMapFile {
        format: LINEMAP_FORMAT,
        raw_types,
        lines,
    })
}

// ---- the operations log ----

/// One new file per run: `logs/<device>/<first seq>.jsonl`, one line per uploaded session.
async fn write_log(store: &Webdav, device: &Device, uploaded: &[(String, String, u64)]) -> anyhow::Result<()> {
    let dir = format!("{ROOT}/logs/{}", device.id);
    let first = next_log_seq(store, &dir).await?;
    let at_ms = now_ms();
    let mut body = Vec::new();
    for (i, (session, manifest, bytes)) in uploaded.iter().enumerate() {
        let (agent, id) = session.split_once(':').unwrap_or_default();
        let line = LogLine {
            seq: first + i as u64,
            device: device.id.clone(),
            at_ms,
            op: "archive",
            sessions: vec![session.clone()],
            tags: Vec::new(),
            note: None,
            detail: LogDetail {
                manifest: format!("{}/manifests/{manifest}.json", session_dir(agent, id)),
                bytes: *bytes,
            },
        };
        body.extend(serde_json::to_vec(&line)?);
        body.push(b'\n');
    }
    if put(store, format!("{dir}/{first}.jsonl"), body).await? == Put::Exists {
        bail!("操作日志 {first}.jsonl 已经在存储里，没有覆盖它");
    }
    Ok(())
}

/// The sequence number after the highest one of this device: look at the newest log file.
async fn next_log_seq(store: &Webdav, dir: &str) -> anyhow::Result<u64> {
    let newest = store
        .list(dir)
        .await?
        .into_iter()
        .filter(|e| !e.is_dir)
        .filter_map(|e| e.name.strip_suffix(".jsonl")?.parse::<u64>().ok())
        .max();
    let Some(start) = newest else { return Ok(1) };
    let text = store.get(&format!("{dir}/{start}.jsonl"), None).await?;
    let lines: Vec<&[u8]> = text.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect();
    let last_seq = lines
        .last()
        .and_then(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .and_then(|v| v.get("seq")?.as_u64());
    Ok(last_seq.unwrap_or(start + lines.len().saturating_sub(1) as u64) + 1)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use super::*;
    use crate::agents::{claude_code, codex};
    use crate::testdav::Server;
    use crate::timefmt::parse_rfc3339_ms;

    const CLAUDE_ID: &str = "11111111-2222-4333-8444-555555555555";
    const PARENT: &str = "019a0000-0000-7000-8000-000000000001";
    const CHILD: &str = "019a0000-0000-7000-8000-000000000002";
    const DEVICE: &str = "dev-0001";
    const CLAUDE_DIR: &str = "nekorisu/v1/sessions/claude-code/11111111-2222-4333-8444-555555555555";

    fn device() -> Device {
        Device {
            id: DEVICE.to_string(),
            name: "synthetic-box".to_string(),
            created_at_ms: 1_700_000_000_000,
        }
    }

    // ---- synthetic sessions ----

    fn ts(n: u32) -> String {
        format!("2026-11-01T10:{:02}:{:02}.000Z", n / 60, n % 60)
    }

    /// One synthetic Claude Code record; its kind follows `n % 4`: user, assistant, tool, tool.
    fn claude_line(n: u32) -> String {
        let t = ts(n);
        let body = match n % 4 {
            0 => format!(
                r#"{{"type":"user","timestamp":"{t}","cwd":"/synthetic/cwd","version":"9.9.9","message":{{"role":"user","content":"synthetic question {n}"}}}}"#
            ),
            1 => format!(
                r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"text","text":"synthetic answer {n}"}}]}}}}"#
            ),
            2 => format!(
                r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"tool_use","name":"Synthetic"}}]}}}}"#
            ),
            _ => format!(
                r#"{{"type":"user","timestamp":"{t}","message":{{"content":[{{"type":"tool_result","content":"synthetic result {n}"}}]}}}}"#
            ),
        };
        body + "\n"
    }

    fn claude_lines(range: std::ops::Range<u32>) -> String {
        range.map(claude_line).collect()
    }

    const TITLE_LINE: &str = "{\"type\":\"custom-title\",\"customTitle\":\"Synthetic title\"}\n";

    struct Claude {
        home: tempfile::TempDir,
    }

    impl Claude {
        /// A session with a main transcript (9 lines, a title record), one sub-agent transcript, a
        /// sub-agent side file and a tool result and a file-history file that end without a newline.
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let c = Claude { home };
            let main = c.main();
            fs::create_dir_all(main.parent().unwrap()).unwrap();
            fs::write(&main, claude_lines(0..4) + TITLE_LINE + &claude_lines(4..8)).unwrap();
            let session_dir = main.parent().unwrap().join(CLAUDE_ID);
            fs::create_dir_all(session_dir.join("subagents")).unwrap();
            fs::create_dir_all(session_dir.join("tool-results")).unwrap();
            let sub = (0..3)
                .map(|n| claude_line(n).replacen("\"type\"", "\"isSidechain\":true,\"type\"", 1))
                .collect::<String>();
            fs::write(session_dir.join("subagents/agent-a1b2.jsonl"), sub).unwrap();
            fs::write(
                session_dir.join("subagents/agent-a1b2.meta.json"),
                "{\"synthetic\":true}",
            )
            .unwrap();
            fs::write(
                session_dir.join("tool-results/out-1.txt"),
                "synthetic tool output, no newline",
            )
            .unwrap();
            let history = c.home.path().join("file-history").join(CLAUDE_ID);
            fs::create_dir_all(&history).unwrap();
            fs::write(history.join("abcd@v1"), "synthetic file v1\nsecond line").unwrap();
            c
        }

        fn main(&self) -> PathBuf {
            self.home
                .path()
                .join("projects/-synthetic-project")
                .join(format!("{CLAUDE_ID}.jsonl"))
        }

        fn append(&self, text: &str) {
            fs::OpenOptions::new()
                .append(true)
                .open(self.main())
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }

        fn discover(&self) -> Vec<Discovery> {
            vec![claude_code::discover_in(self.home.path().to_path_buf())]
        }

        fn wanted() -> Vec<(AgentKind, String)> {
            vec![(AgentKind::ClaudeCode, CLAUDE_ID.to_string())]
        }
    }

    fn codex_meta(id: &str, parent: Option<&str>) -> String {
        let extra = match parent {
            Some(p) => format!(
                r#","parent_thread_id":"{p}","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{p}"}}}}}}"#
            ),
            None => r#","source":"vscode""#.to_string(),
        };
        format!(
            r#"{{"timestamp":"{}","type":"session_meta","payload":{{"id":"{id}","cwd":"/synthetic/cwd","cli_version":"0.99.0"{extra}}}}}"#,
            ts(0)
        ) + "\n"
    }

    /// `item_completed` events: a user message, then an agent message, per pair of `n`.
    fn codex_events(range: std::ops::Range<u32>) -> String {
        range
            .map(|n| {
                let item = if n % 2 == 0 { "UserMessage" } else { "AgentMessage" };
                format!(
                    r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"{item}"}}}}}}"#,
                    ts(n + 1)
                ) + "\n"
            })
            .collect()
    }

    struct Codex {
        home: tempfile::TempDir,
    }

    impl Codex {
        fn new() -> Self {
            Codex {
                home: tempfile::tempdir().unwrap(),
            }
        }

        fn rollout(&self, id: &str, ext: &str) -> PathBuf {
            let dir = self.home.path().join("sessions/2026/11/01");
            fs::create_dir_all(&dir).unwrap();
            dir.join(format!("rollout-2026-11-01T10-00-00-{id}.{ext}"))
        }

        fn discover(&self) -> Vec<Discovery> {
            vec![codex::discover_in(self.home.path().to_path_buf())]
        }
    }

    // ---- running and reading back ----

    fn both(a: Vec<Discovery>, b: Vec<Discovery>) -> Vec<Discovery> {
        a.into_iter().chain(b).collect()
    }

    async fn archive(server: &Server, found: &[Discovery], wanted: &[(AgentKind, String)]) -> Outcome {
        let store = Webdav::new(server.spec()).unwrap();
        run(&store, &device(), jobs_for(found, wanted), &|_| true).await
    }

    fn manifest(server: &Server, dir: &str, seq: u64) -> Manifest {
        let bytes = fs::read(server.root().join(format!("{dir}/manifests/{DEVICE}-{seq}.json"))).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn manifest_names(server: &Server, dir: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(server.root().join(format!("{dir}/manifests")))
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// The bytes of a file as its segments in the storage give them, after checking that the
    /// segments follow one another, match their names and sizes, and match the entry's total.
    fn assemble(server: &Server, dir: &str, entry: &FileEntry) -> Vec<u8> {
        let mut whole = Vec::new();
        for segment in &entry.segments {
            let path = server.root().join(format!(
                "{dir}/files/{}/{}-{}.seg",
                entry.path, segment.start, segment.sha256
            ));
            let bytes = fs::read(&path).unwrap_or_else(|_| panic!("segment {path:?} is in the storage"));
            assert_eq!(segment.start, whole.len() as u64, "segments follow one another");
            assert_eq!(bytes.len() as u64, segment.len);
            assert_eq!(sha(&bytes), segment.sha256, "the name of a segment is its hash");
            check_linemap(server, dir, entry, segment, &bytes);
            whole.extend(bytes);
        }
        assert_eq!(whole.len() as u64, entry.size);
        assert_eq!(
            sha(&whole),
            entry.sha256,
            "the entry hashes all the bytes uploaded so far"
        );
        whole
    }

    /// Every mapped line is exactly a line of the segment, and no line is missing.
    fn check_linemap(server: &Server, dir: &str, entry: &FileEntry, segment: &SegmentInfo, bytes: &[u8]) {
        let map: serde_json::Value = serde_json::from_slice(
            &fs::read(server.root().join(format!("{dir}/linemaps/{}.json", segment.sha256))).unwrap(),
        )
        .unwrap();
        assert_eq!(map["format"], LINEMAP_FORMAT);
        let raw_types = map["raw_types"].as_array().unwrap();
        let lines = map["lines"].as_array().unwrap();
        if !entry.role.contains("transcript") && entry.role != "subagent" {
            assert!(
                lines.is_empty() && raw_types.is_empty() && segment.lines == 0,
                "opaque files have no lines"
            );
            return;
        }
        assert_eq!(lines.len() as u64, segment.lines);
        assert_eq!(
            lines.len(),
            bytes.iter().filter(|&&b| b == b'\n').count(),
            "every line is mapped"
        );
        let mut at = 0;
        for line in lines {
            let (offset, len) = (line[0].as_u64().unwrap() as usize, line[1].as_u64().unwrap() as usize);
            assert_eq!(offset, at, "lines follow one another");
            assert!(["user", "assistant", "reasoning", "tool", "meta"].contains(&line[2].as_str().unwrap()));
            assert!((line[3].as_u64().unwrap() as usize) < raw_types.len());
            assert!(line[4].as_i64().unwrap() >= 0);
            let body = &bytes[offset..offset + len];
            assert!(!body.contains(&b'\n'), "a line holds no line break");
            at = offset + len + if bytes[offset + len] == b'\r' { 2 } else { 1 };
            assert_eq!(bytes[at - 1], b'\n');
        }
        assert_eq!(at, bytes.len());
    }

    fn kinds(server: &Server, dir: &str, segment: &SegmentInfo) -> Vec<String> {
        let map: serde_json::Value = serde_json::from_slice(
            &fs::read(server.root().join(format!("{dir}/linemaps/{}.json", segment.sha256))).unwrap(),
        )
        .unwrap();
        map["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l[2].as_str().unwrap().to_string())
            .collect()
    }

    fn entry<'a>(manifest: &'a Manifest, suffix: &str) -> &'a FileEntry {
        manifest
            .files
            .iter()
            .find(|f| f.path.ends_with(suffix))
            .unwrap_or_else(|| panic!("a file ending in {suffix}"))
    }

    /// Everything the run wrote is inside `nekorisu/v1/`, and nothing else is in the storage.
    fn assert_only_under_root(server: &Server) {
        let top: Vec<String> = fs::read_dir(server.root())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(top, ["nekorisu"]);
        assert_eq!(
            fs::read_dir(server.root().join("nekorisu")).unwrap().count(),
            1,
            "only nekorisu/v1"
        );
        let puts = server.puts();
        assert!(!puts.is_empty());
        assert!(puts.iter().all(|p| p.starts_with("nekorisu/v1/")), "{puts:?}");
    }

    // ---- tests ----

    #[tokio::test]
    async fn first_upload_of_a_claude_code_session() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        let outcome = archive(&server, &claude.discover(), &Claude::wanted()).await;
        assert!(outcome.log_error.is_none());
        assert_eq!(outcome.sessions.len(), 1);
        let report = &outcome.sessions[0];
        assert_eq!(report.error, None);
        assert_eq!(report.manifest.as_deref(), Some("dev-0001-1"));
        assert!(!report.up_to_date && !report.rewrite);
        assert_eq!(report.files, 5);

        // The device file, written once, says who we are.
        let device_json: serde_json::Value =
            serde_json::from_slice(&fs::read(server.root().join("nekorisu/v1/devices/dev-0001.json")).unwrap())
                .unwrap();
        assert_eq!(device_json["id"], DEVICE);
        assert_eq!(device_json["name"], "synthetic-box");
        assert_eq!(device_json["created_at_ms"], 1_700_000_000_000_i64);

        let m = manifest(&server, CLAUDE_DIR, 1);
        assert_eq!(m.format, "nekorisu.manifest/1");
        assert_eq!(
            (m.id.as_str(), m.parent.as_deref(), m.device.as_str()),
            ("dev-0001-1", None, DEVICE)
        );
        assert!(!m.rewrite && m.encryption == "none" && m.created_at_ms > 0);
        assert_eq!(m.links.parent, None);
        assert!(m.links.children.is_empty() && m.links.fork_from.is_none());
        let s = &m.session;
        assert_eq!((s.agent.as_str(), s.id.as_str()), ("claude-code", CLAUDE_ID));
        assert_eq!((s.agent_version.as_str(), s.cwd.as_str()), ("9.9.9", "/synthetic/cwd"));
        assert_eq!(s.project, "-synthetic-project");
        assert_eq!(s.title.as_deref(), Some("Synthetic title"));
        assert_eq!(s.first_ts_ms, parse_rfc3339_ms(&ts(0)).unwrap());
        assert_eq!(s.last_ts_ms, parse_rfc3339_ms(&ts(7)).unwrap());

        // Roles, forms, and byte-exact content of every file.
        let roles: Vec<(&str, &str)> = m.files.iter().map(|f| (f.path.as_str(), f.role.as_str())).collect();
        assert_eq!(
            roles,
            [
                (format!("file-history/{CLAUDE_ID}/abcd@v1").as_str(), "file_history"),
                (
                    "projects/-synthetic-project/11111111-2222-4333-8444-555555555555.jsonl",
                    "transcript"
                ),
                (
                    "projects/-synthetic-project/11111111-2222-4333-8444-555555555555/subagents/agent-a1b2.jsonl",
                    "subagent"
                ),
                (
                    "projects/-synthetic-project/11111111-2222-4333-8444-555555555555/subagents/agent-a1b2.meta.json",
                    "other"
                ),
                (
                    "projects/-synthetic-project/11111111-2222-4333-8444-555555555555/tool-results/out-1.txt",
                    "tool_result"
                ),
            ]
        );
        let local_main = fs::read(claude.main()).unwrap();
        let main = entry(&m, &format!("{CLAUDE_ID}.jsonl"));
        assert_eq!(assemble(&server, CLAUDE_DIR, main), local_main);
        assert_eq!((main.form.as_str(), main.lines, main.segments.len()), ("as-is", 9, 1));
        assert_eq!(
            kinds(&server, CLAUDE_DIR, &main.segments[0]),
            [
                "user",
                "assistant",
                "tool",
                "tool",
                "meta",
                "user",
                "assistant",
                "tool",
                "tool"
            ]
        );
        for f in &m.files {
            let local = claude.home.path().join(&f.path);
            assert_eq!(assemble(&server, CLAUDE_DIR, f), fs::read(local).unwrap(), "{}", f.path);
            assert!(f.mtime_ms > 0);
        }
        // Side files are opaque bytes, whole, even without a trailing newline.
        assert_eq!(
            entry(&m, "out-1.txt").size,
            "synthetic tool output, no newline".len() as u64
        );

        // The operations log.
        let log = fs::read_to_string(server.root().join(format!("nekorisu/v1/logs/{DEVICE}/1.jsonl"))).unwrap();
        let lines: Vec<serde_json::Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["seq"], 1);
        assert_eq!(
            (lines[0]["device"].as_str(), lines[0]["op"].as_str()),
            (Some(DEVICE), Some("archive"))
        );
        assert!(lines[0]["at_ms"].as_i64().unwrap() > 0);
        assert_eq!(
            lines[0]["sessions"],
            serde_json::json!([format!("claude-code:{CLAUDE_ID}")])
        );
        assert_eq!(
            (&lines[0]["tags"], &lines[0]["note"]),
            (&serde_json::json!([]), &serde_json::Value::Null)
        );
        assert_eq!(lines[0]["detail"]["bytes"], report.new_bytes);
        let manifest_path = lines[0]["detail"]["manifest"].as_str().unwrap();
        assert_eq!(manifest_path, format!("{CLAUDE_DIR}/manifests/dev-0001-1.json"));
        assert!(server.root().join(manifest_path).is_file());

        assert_only_under_root(&server);
    }

    #[tokio::test]
    async fn second_upload_sends_only_what_is_new() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        archive(&server, &claude.discover(), &Claude::wanted()).await;
        let first = manifest(&server, CLAUDE_DIR, 1);
        let first_main = entry(&first, &format!("{CLAUDE_ID}.jsonl")).clone();
        let old_len = fs::metadata(claude.main()).unwrap().len();

        // New lines at the end of the main transcript; a side file the agent cleans up meanwhile.
        claude.append(&claude_lines(8..12));
        fs::remove_file(
            claude
                .main()
                .parent()
                .unwrap()
                .join(CLAUDE_ID)
                .join("tool-results/out-1.txt"),
        )
        .unwrap();
        server.forget_requests();
        let outcome = archive(&server, &claude.discover(), &Claude::wanted()).await;
        let report = &outcome.sessions[0];
        assert_eq!(report.error, None);
        assert_eq!(report.manifest.as_deref(), Some("dev-0001-2"));
        assert_eq!((report.new_lines, report.rewrite), (4, false));

        let m = manifest(&server, CLAUDE_DIR, 2);
        assert_eq!(m.parent.as_deref(), Some("dev-0001-1"));
        assert!(!m.rewrite);
        let main = entry(&m, &format!("{CLAUDE_ID}.jsonl"));
        let local = fs::read(claude.main()).unwrap();
        assert_eq!(
            assemble(&server, CLAUDE_DIR, main),
            local,
            "the whole file, from its segments"
        );
        assert_eq!(main.segments.len(), 2);
        assert_eq!(
            main.segments[0].sha256, first_main.segments[0].sha256,
            "the old segment is reused"
        );
        assert_eq!(
            (main.segments[1].start, main.segments[1].len),
            (old_len, local.len() as u64 - old_len)
        );
        assert_eq!((main.lines, main.segments[1].lines), (13, 4));
        assert_eq!(
            kinds(&server, CLAUDE_DIR, &main.segments[1]),
            ["user", "assistant", "tool", "tool"]
        );
        // The file the agent deleted is still part of the archived version.
        let kept = entry(&m, "out-1.txt");
        assert_eq!(kept.segments[0].sha256, entry(&first, "out-1.txt").segments[0].sha256);
        assert_eq!(
            assemble(&server, CLAUDE_DIR, kept),
            b"synthetic tool output, no newline"
        );

        // Only the new segment and its line map, the manifest and the log went up.
        let puts = server.puts();
        let new_segment = format!(
            "{CLAUDE_DIR}/files/projects/-synthetic-project/{CLAUDE_ID}.jsonl/{old_len}-{}.seg",
            main.segments[1].sha256
        );
        assert_eq!(
            puts,
            [
                new_segment,
                format!("{CLAUDE_DIR}/linemaps/{}.json", main.segments[1].sha256),
                format!("{CLAUDE_DIR}/manifests/dev-0001-2.json"),
                format!("nekorisu/v1/logs/{DEVICE}/2.jsonl"),
            ]
        );
        // Nothing was removed or changed in the first version.
        assert_eq!(manifest(&server, CLAUDE_DIR, 1).files.len(), 5);
        assert_only_under_root(&server);
    }

    #[tokio::test]
    async fn nothing_changed_writes_nothing() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        archive(&server, &claude.discover(), &Claude::wanted()).await;
        server.forget_requests();

        let outcome = archive(&server, &claude.discover(), &Claude::wanted()).await;
        assert_eq!(outcome.failed(), 0);
        let report = &outcome.sessions[0];
        assert!(report.up_to_date);
        assert_eq!((report.new_bytes, report.new_lines), (0, 0));
        assert_eq!(report.manifest.as_deref(), Some("dev-0001-1"));
        assert!(server.puts().is_empty(), "no PUT at all: {:?}", server.puts());
        assert_eq!(manifest_names(&server, CLAUDE_DIR), ["dev-0001-1.json"]);
        assert!(
            !server
                .root()
                .join(format!("nekorisu/v1/logs/{DEVICE}/2.jsonl"))
                .exists()
        );
    }

    #[tokio::test]
    async fn a_rewritten_file_is_sent_again_whole() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        archive(&server, &claude.discover(), &Claude::wanted()).await;

        // The agent rewrote the file: the first bytes are not what was uploaded any more.
        fs::write(claude.main(), claude_lines(100..106)).unwrap();
        let outcome = archive(&server, &claude.discover(), &Claude::wanted()).await;
        let report = &outcome.sessions[0];
        assert_eq!(report.error, None);
        assert!(report.rewrite);
        let m = manifest(&server, CLAUDE_DIR, 2);
        assert!(m.rewrite);
        assert_eq!(m.parent.as_deref(), Some("dev-0001-1"));
        let main = entry(&m, &format!("{CLAUDE_ID}.jsonl"));
        assert_eq!(main.segments.len(), 1);
        assert_eq!(main.segments[0].start, 0);
        assert_eq!((main.lines, main.size), (6, fs::metadata(claude.main()).unwrap().len()));
        assert_eq!(assemble(&server, CLAUDE_DIR, main), fs::read(claude.main()).unwrap());
        // The files that did not change are carried over as they were.
        assert_eq!(
            entry(&m, "abcd@v1").segments,
            entry(&manifest(&server, CLAUDE_DIR, 1), "abcd@v1").segments
        );
        // Write-once: the old version is all still there.
        let first = manifest(&server, CLAUDE_DIR, 1);
        assemble(&server, CLAUDE_DIR, entry(&first, &format!("{CLAUDE_ID}.jsonl")));
    }

    #[tokio::test]
    async fn a_line_still_being_written_is_left_for_next_time() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        let complete = fs::read(claude.main()).unwrap();
        let half = r#"{"type":"user","timestamp":"2026-11-01T10:09:00.000Z","message":{"content":"synthetic half"#;
        claude.append(half);
        archive(&server, &claude.discover(), &Claude::wanted()).await;
        let m = manifest(&server, CLAUDE_DIR, 1);
        let main = entry(&m, &format!("{CLAUDE_ID}.jsonl"));
        assert_eq!(main.size, complete.len() as u64, "the unfinished line is not in it");
        assert_eq!(assemble(&server, CLAUDE_DIR, main), complete);
        assert_eq!(main.lines, 9);

        // The line is finished, and one more follows.
        claude.append("\"}}\n");
        claude.append(&claude_line(10));
        archive(&server, &claude.discover(), &Claude::wanted()).await;
        let m = manifest(&server, CLAUDE_DIR, 2);
        let main = entry(&m, &format!("{CLAUDE_ID}.jsonl"));
        assert_eq!(main.segments.len(), 2);
        assert_eq!(main.segments[1].start, complete.len() as u64);
        assert_eq!(main.lines, 11);
        assert_eq!(assemble(&server, CLAUDE_DIR, main), fs::read(claude.main()).unwrap());
    }

    #[tokio::test]
    async fn big_files_are_cut_at_line_boundaries() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        // ~30 KiB of lines and a 20 KiB side file, with an 8 KiB segment limit in tests.
        claude.append(&claude_lines(100..300));
        let side = claude
            .main()
            .parent()
            .unwrap()
            .join(CLAUDE_ID)
            .join("tool-results/out-1.txt");
        let blob: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&side, &blob).unwrap();

        archive(&server, &claude.discover(), &Claude::wanted()).await;
        let m = manifest(&server, CLAUDE_DIR, 1);
        let main = entry(&m, &format!("{CLAUDE_ID}.jsonl"));
        assert!(main.segments.len() >= 3, "{} segments", main.segments.len());
        assert_eq!(assemble(&server, CLAUDE_DIR, main), fs::read(claude.main()).unwrap());
        for segment in &main.segments[..main.segments.len() - 1] {
            assert!(
                segment.len >= SEGMENT_LIMIT,
                "a segment is cut only once it reaches the limit"
            );
        }
        assert_eq!(main.lines, 209);
        let out = entry(&m, "out-1.txt");
        assert_eq!(
            out.segments.iter().map(|s| s.len).collect::<Vec<_>>(),
            [8192, 8192, 3616]
        );
        assert_eq!(assemble(&server, CLAUDE_DIR, out), blob);

        // An append that is big itself is cut the same way, starting where the old end was.
        let old = fs::metadata(claude.main()).unwrap().len();
        claude.append(&claude_lines(300..420));
        archive(&server, &claude.discover(), &Claude::wanted()).await;
        let m = manifest(&server, CLAUDE_DIR, 2);
        let main = entry(&m, &format!("{CLAUDE_ID}.jsonl"));
        assert!(main.segments.iter().any(|s| s.start == old));
        assert_eq!(assemble(&server, CLAUDE_DIR, main), fs::read(claude.main()).unwrap());
    }

    #[test]
    fn cuts_never_split_a_line() {
        let mut text = String::new();
        for n in 0..200 {
            text.push_str(&claude_line(n));
            text.push_str(&"{\"type\":\"x\"}\n".repeat((n % 7) as usize));
        }
        let mut map = LineMap::new(AgentKind::ClaudeCode);
        map.extend_from(text.as_bytes()).unwrap();
        let pieces = cuts(Some(&map), 0, map.indexed);
        assert!(pieces.len() > 3);
        let mut at = 0;
        let mut line = 0;
        for piece in &pieces {
            assert_eq!((piece.start, piece.first_line), (at, line));
            assert!(piece.end > piece.start);
            assert!(
                piece.end == map.indexed || map.lines.iter().any(|l| l.offset == piece.end),
                "ends at a line start"
            );
            assert_eq!(text.as_bytes()[piece.end as usize - 1], b'\n');
            (at, line) = (piece.end, piece.last_line);
        }
        assert_eq!((at, line), (map.indexed, map.lines.len()));
        // Starting in the middle of the map works the same.
        let from = map.lines[20].offset;
        let rest = cuts(Some(&map), from, map.indexed);
        assert_eq!((rest[0].start, rest[0].first_line), (from, 20));
        assert!(cuts(Some(&map), map.indexed, map.indexed).is_empty());
        assert!(cuts(None, 5, 5).is_empty());
    }

    #[tokio::test]
    async fn a_codex_tree_is_uploaded_with_links_and_decompressed_files() {
        let server = Server::start(None).await;
        let codex = Codex::new();
        let parent_text = codex_meta(PARENT, None) + &codex_events(0..4);
        fs::write(codex.rollout(PARENT, "jsonl"), &parent_text).unwrap();
        // The child thread is stored the way Codex compresses old rollouts.
        let child_text = codex_meta(CHILD, Some(PARENT)) + &codex_events(0..3);
        fs::write(
            codex.rollout(CHILD, "jsonl.zst"),
            zstd::encode_all(child_text.as_bytes(), 3).unwrap(),
        )
        .unwrap();

        // Asking for the child brings in the whole tree, root first.
        let outcome = archive(&server, &codex.discover(), &[(AgentKind::Codex, CHILD.to_string())]).await;
        assert_eq!(outcome.failed(), 0, "{outcome:?}");
        let keys: Vec<&str> = outcome.sessions.iter().map(|s| s.session.as_str()).collect();
        assert_eq!(keys, [format!("codex:{PARENT}"), format!("codex:{CHILD}")]);

        let parent_dir = format!("nekorisu/v1/sessions/codex/{PARENT}");
        let child_dir = format!("nekorisu/v1/sessions/codex/{CHILD}");
        let pm = manifest(&server, &parent_dir, 1);
        let cm = manifest(&server, &child_dir, 1);
        assert_eq!(pm.links.parent, None);
        assert_eq!(pm.links.children, [format!("codex:{CHILD}")]);
        assert_eq!(cm.links.parent.as_deref(), Some(format!("codex:{PARENT}").as_str()));
        assert!(cm.links.children.is_empty());
        assert_eq!(
            (pm.session.agent_version.as_str(), pm.session.cwd.as_str()),
            ("0.99.0", "/synthetic/cwd")
        );
        assert_eq!(pm.session.title, None);
        assert_eq!(pm.session.first_ts_ms, parse_rfc3339_ms(&ts(0)).unwrap());
        assert_eq!(pm.session.last_ts_ms, parse_rfc3339_ms(&ts(4)).unwrap());

        let parent_file = &pm.files[0];
        assert!(parent_file.path.ends_with(&format!("{PARENT}.jsonl")) && parent_file.form == "as-is");
        assert_eq!(assemble(&server, &parent_dir, parent_file), parent_text.as_bytes());
        // The compressed rollout went up as the decompressed bytes, without `.zst`.
        let child_file = &cm.files[0];
        assert!(
            child_file.path.ends_with(&format!("{CHILD}.jsonl")),
            "{}",
            child_file.path
        );
        assert_eq!(child_file.form, "decompressed");
        assert_eq!(assemble(&server, &child_dir, child_file), child_text.as_bytes());
        assert_eq!(
            kinds(&server, &parent_dir, &parent_file.segments[0]),
            ["meta", "user", "assistant", "user", "assistant"]
        );
        // A sub-agent thread's "user" turns are not the person.
        assert_eq!(
            kinds(&server, &child_dir, &child_file.segments[0]),
            ["meta", "meta", "assistant", "meta"]
        );

        // Compression does not make a new version: the same bytes are already up.
        let again = archive(&server, &codex.discover(), &[(AgentKind::Codex, PARENT.to_string())]).await;
        assert!(again.sessions.iter().all(|s| s.up_to_date), "{again:?}");

        // The log has one line per session, in one file, numbered from 1.
        let log = fs::read_to_string(server.root().join(format!("nekorisu/v1/logs/{DEVICE}/1.jsonl"))).unwrap();
        let seqs: Vec<u64> = log
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(seqs, [1, 2]);
        assert_only_under_root(&server);
    }

    #[tokio::test]
    async fn later_segments_are_classified_with_what_the_first_one_said() {
        let server = Server::start(None).await;
        let codex = Codex::new();
        let path = codex.rollout(CHILD, "jsonl");
        fs::write(&path, codex_meta(CHILD, Some(PARENT)) + &codex_events(0..2)).unwrap();
        let wanted = [(AgentKind::Codex, CHILD.to_string())];
        archive(&server, &codex.discover(), &wanted).await;

        // Appended later: `session_meta` is not in this segment, but it made this a sub-agent thread.
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(codex_events(2..5).as_bytes())
            .unwrap();
        archive(&server, &codex.discover(), &wanted).await;
        let dir = format!("nekorisu/v1/sessions/codex/{CHILD}");
        let m = manifest(&server, &dir, 2);
        let file = &m.files[0];
        assert_eq!(file.segments.len(), 2);
        // Event 2 and 4 are UserMessage: `meta` in a sub-agent thread, `user` if the segment were read alone.
        assert_eq!(kinds(&server, &dir, &file.segments[1]), ["meta", "assistant", "meta"]);
        assert_eq!(assemble(&server, &dir, file), fs::read(&path).unwrap());
    }

    #[tokio::test]
    async fn log_files_are_numbered_across_runs() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        let codex = Codex::new();
        fs::write(
            codex.rollout(PARENT, "jsonl"),
            codex_meta(PARENT, None) + &codex_events(0..2),
        )
        .unwrap();
        let found = both(claude.discover(), codex.discover());
        let wanted = [
            (AgentKind::ClaudeCode, CLAUDE_ID.to_string()),
            (AgentKind::Codex, PARENT.to_string()),
        ];
        let log = |name: &str| -> Vec<u64> {
            fs::read_to_string(server.root().join(format!("nekorisu/v1/logs/{DEVICE}/{name}")))
                .unwrap()
                .lines()
                .map(|l| {
                    serde_json::from_str::<serde_json::Value>(l).unwrap()["seq"]
                        .as_u64()
                        .unwrap()
                })
                .collect()
        };
        archive(&server, &found, &wanted).await;
        assert_eq!(log("1.jsonl"), [1, 2]);

        claude.append(&claude_line(8));
        archive(&server, &both(claude.discover(), codex.discover()), &wanted).await;
        assert_eq!(
            log("3.jsonl"),
            [3],
            "only the session that changed is logged, numbering goes on"
        );

        // A run that changes nothing writes no log.
        archive(&server, &both(claude.discover(), codex.discover()), &wanted).await;
        let mut names: Vec<String> = fs::read_dir(server.root().join(format!("nekorisu/v1/logs/{DEVICE}")))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["1.jsonl", "3.jsonl"]);

        // Another device has its own manifests and its own log numbering.
        let store = Webdav::new(server.spec()).unwrap();
        let other = Device {
            id: "dev-0002".into(),
            name: "other".into(),
            created_at_ms: 2,
        };
        let outcome = run(&store, &other, jobs_for(&found, &wanted[..1]), &|_| true).await;
        assert_eq!(outcome.sessions[0].manifest.as_deref(), Some("dev-0002-1"));
        assert_eq!(
            manifest_names(&server, CLAUDE_DIR),
            ["dev-0001-1.json", "dev-0001-2.json", "dev-0002-1.json"]
        );
        assert!(server.root().join("nekorisu/v1/logs/dev-0002/1.jsonl").is_file());
    }

    #[tokio::test]
    async fn progress_adds_up_and_cancelling_stops() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        let seen = std::sync::Mutex::new((0u64, 0u64, false));
        let store = Webdav::new(server.spec()).unwrap();
        let on = |event: Event| {
            let mut seen = seen.lock().unwrap();
            match event {
                Event::Planned { total_bytes } => seen.0 = total_bytes,
                Event::Uploaded { bytes } => seen.1 += bytes,
                Event::Committing => seen.2 = true,
            }
            true
        };
        let outcome = run(&store, &device(), jobs_for(&claude.discover(), &Claude::wanted()), &on).await;
        let seen = *seen.lock().unwrap();
        assert_eq!(seen.0, outcome.sessions[0].new_bytes);
        assert_eq!(seen.1, seen.0);
        assert!(seen.2);

        // A second claude home, never uploaded: the callback refuses after the first segment.
        let server = Server::start(None).await;
        let store = Webdav::new(server.spec()).unwrap();
        let cancel = |event: Event| !matches!(event, Event::Uploaded { .. });
        let outcome = run(
            &store,
            &device(),
            jobs_for(&claude.discover(), &Claude::wanted()),
            &cancel,
        )
        .await;
        assert!(
            outcome.sessions[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("已取消")),
            "{outcome:?}"
        );
        assert!(
            manifest_names(&server, CLAUDE_DIR).is_empty(),
            "no manifest: the upload did not finish"
        );
    }

    #[tokio::test]
    async fn an_interrupted_upload_can_be_resumed() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        let store = Webdav::new(server.spec()).unwrap();
        // The first run is cancelled after its first segment: pieces are up, no manifest.
        let cancel = |event: Event| !matches!(event, Event::Uploaded { .. });
        let jobs = jobs_for(&claude.discover(), &Claude::wanted());
        let outcome = run(&store, &device(), jobs.clone(), &cancel).await;
        assert!(outcome.sessions[0].error.is_some());
        assert!(manifest_names(&server, CLAUDE_DIR).is_empty());
        let before = server.puts();
        let segment_before = before.iter().find(|p| p.ends_with(".seg")).cloned().unwrap();
        server.forget_requests();

        // The next run goes through: what is there already is refused as existing and kept.
        let outcome = run(&store, &device(), jobs, &|_| true).await;
        assert_eq!(outcome.failed(), 0, "{outcome:?}");
        assert!(
            server.puts().contains(&segment_before),
            "it was offered again, and refused"
        );
        let m = manifest(&server, CLAUDE_DIR, 1);
        for f in &m.files {
            assert_eq!(
                assemble(&server, CLAUDE_DIR, f),
                fs::read(claude.home.path().join(&f.path)).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn a_name_taken_by_different_bytes_is_not_trusted() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        // Something of the wrong size already sits where the main transcript's segment must go.
        let main = fs::read(claude.main()).unwrap();
        let taken = server.root().join(format!(
            "{CLAUDE_DIR}/files/projects/-synthetic-project/{CLAUDE_ID}.jsonl/0-{}.seg",
            sha(&main)
        ));
        fs::create_dir_all(taken.parent().unwrap()).unwrap();
        fs::write(&taken, b"not the segment").unwrap();

        let outcome = archive(&server, &claude.discover(), &Claude::wanted()).await;
        let error = outcome.sessions[0].error.as_deref().unwrap();
        assert!(error.contains("大小不对"), "{error}");
        assert!(
            manifest_names(&server, CLAUDE_DIR).is_empty(),
            "no manifest names a doubtful segment"
        );
        assert_eq!(
            fs::read(&taken).unwrap(),
            b"not the segment",
            "and nothing was overwritten"
        );
    }

    #[tokio::test]
    async fn a_storage_failure_stops_the_run_and_reports_it() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        let codex = Codex::new();
        fs::write(
            codex.rollout(PARENT, "jsonl"),
            codex_meta(PARENT, None) + &codex_events(0..2),
        )
        .unwrap();
        let found = both(claude.discover(), codex.discover());
        let wanted = [
            (AgentKind::ClaudeCode, CLAUDE_ID.to_string()),
            (AgentKind::Codex, PARENT.to_string()),
        ];
        server.fail_next(1000, 500);
        let outcome = archive(&server, &found, &wanted).await;
        assert_eq!(outcome.failed(), 2);
        assert!(outcome.sessions[0].error.as_deref().unwrap().contains("HTTP 500"));
        assert!(outcome.sessions[1].error.as_deref().unwrap().starts_with("没有上传："));
        assert!(server.puts().is_empty());
    }

    #[tokio::test]
    async fn a_missing_local_transcript_fails_only_that_session() {
        let server = Server::start(None).await;
        let claude = Claude::new();
        let codex = Codex::new();
        fs::write(
            codex.rollout(PARENT, "jsonl"),
            codex_meta(PARENT, None) + &codex_events(0..2),
        )
        .unwrap();
        let found = both(claude.discover(), codex.discover());
        // The agent removed the Claude transcript after discovery.
        fs::remove_file(claude.main()).unwrap();
        let wanted = [
            (AgentKind::ClaudeCode, CLAUDE_ID.to_string()),
            (AgentKind::Codex, PARENT.to_string()),
        ];
        let outcome = archive(&server, &found, &wanted).await;
        assert!(outcome.sessions[0].error.is_some());
        assert_eq!(outcome.sessions[1].error, None, "the other session still went up");
        assert!(
            server
                .root()
                .join(format!("nekorisu/v1/sessions/codex/{PARENT}/manifests/dev-0001-1.json"))
                .is_file()
        );
        assert!(manifest_names(&server, CLAUDE_DIR).is_empty());
    }
}
