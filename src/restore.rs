//! Puts a session from the storage back where its agent keeps it, byte for byte, so the agent's
//! own resume picks it up. The steps: choose the version (several newest ones: the caller
//! chooses) → download every segment into a staging folder, checking each one and each whole
//! file against their SHA-256 → compare with what is on this machine → move the files into the
//! agent's folders → log it in the storage → let Codex register its threads.
//!
//! What is on this machine wins unless the caller says otherwise: a local file that is the start
//! of the stored one is completed; a local file with anything the stored one does not have stops
//! the restore before anything is written, unless `replace` is set, and then the local file is
//! moved to `~/.nekorisu/replaced/` rather than deleted. A restored file's mtime is the time of
//! the restore.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::agents::{AgentKind, Discovery, Session};
use crate::archive::{self, FileEntry, Manifest, session_key};
use crate::config::PathMap;
use crate::daemon::device::Device;
use crate::pathmap::Mapper;
use crate::paths::Paths;
use crate::remote::{self, DeviceInfo, Remote, Wanted};
use crate::sync::{self, LocalCopy, Rename};
use crate::timefmt::now_ms;

/// Where this machine keeps things.
pub struct Env<'a> {
    pub paths: &'a Paths,
    /// The user's home folder: working directories of other devices are mapped under it.
    pub home: PathBuf,
    pub claude_home: PathBuf,
    pub codex_home: PathBuf,
    /// Sessions on this machine now.
    pub found: &'a [Discovery],
    pub rules: &'a [PathMap],
    pub records: &'a sync::Store,
    pub storage: &'a str,
    /// The codex program; `None` looks for it (`codex::find`).
    pub codex_exe: Option<PathBuf>,
}

/// Progress; the callback answers whether to go on.
pub type Progress<'a> = &'a (dyn Fn(Event) -> bool + Send + Sync);

#[derive(Debug, Clone, Copy)]
pub enum Event {
    Planned {
        total_bytes: u64,
    },
    Downloaded {
        bytes: u64,
    },
    /// Everything is downloaded and checked; files are being put in place.
    Placing,
}

/// Local files that have something the stored version lacks.
#[derive(Debug)]
pub struct Conflict(pub String);

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Conflict {}

#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    pub session: String,
    pub manifest: String,
    pub files: usize,
    /// Bytes of the version.
    pub bytes: u64,
    /// Files that were not here and were written.
    pub written: usize,
    /// Local files that were the start of the stored ones and were completed.
    pub completed: usize,
    pub unchanged: usize,
    /// Where replaced local files went.
    pub replaced: Vec<String>,
    /// The working directory the session recorded, and where that is here (Claude Code).
    pub cwd: String,
    pub local_cwd: Option<String>,
    /// The Claude Code project folder the session is in.
    pub folder: Option<String>,
    /// How to go on with it, and the folder to run that in when there is one here.
    pub resume: String,
    pub resume_in: Option<String>,
    /// Codex: whether Codex registered the thread.
    pub registered: Option<bool>,
    pub note: Option<String>,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub sessions: Vec<SessionRow>,
    /// Restored, but the operations log in the storage was not written.
    pub log_error: Option<String>,
}

/// What a stored file is to the local one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fit {
    New,
    Same,
    /// The local file is the start of the stored one.
    Shorter,
    /// The local file has something the stored one does not.
    Differs,
}

struct FilePlan {
    entry: FileEntry,
    /// Where it goes.
    target: PathBuf,
    /// The local file that is there now (Codex may have compressed it: `.jsonl.zst`).
    existing: Option<PathBuf>,
    staged: PathBuf,
    fit: Fit,
}

struct SessionPlan {
    agent: AgentKind,
    id: String,
    manifest: Manifest,
    renames: Vec<Rename>,
    files: Vec<FilePlan>,
    /// Local files of the session the stored version does not have.
    extra: Vec<PathBuf>,
    folder: Option<String>,
    local_cwd: Option<PathBuf>,
}

/// The sessions to restore: the chosen version of the named one, and the newest version of
/// each sub-agent thread it names (Codex), down the tree.
pub async fn tree(
    remote: &Remote,
    agent: AgentKind,
    root: Manifest,
    devices: &HashMap<String, DeviceInfo>,
) -> anyhow::Result<Vec<(AgentKind, Manifest)>> {
    let mut out = vec![(agent, root)];
    let mut seen: HashSet<String> = HashSet::new();
    let mut next = 0;
    while next < out.len() {
        let parent = out[next].1.clone();
        next += 1;
        for child in &parent.links.children {
            let Some((name, id)) = child.split_once(':') else {
                continue;
            };
            let Some(child_agent) = AgentKind::parse(name) else {
                continue;
            };
            if !seen.insert(child.clone()) || out.len() > 256 {
                continue;
            }
            let (child_agent, ids) = match remote.find(id, Some(child_agent)).await {
                Ok(found) => found,
                Err(err) if err.downcast_ref::<remote::Missing>().is_some() => continue, // never uploaded
                Err(err) => return Err(err),
            };
            let all = remote.manifests(child_agent, id, &ids).await?;
            let heads = remote::heads(&all);
            // A forked sub-agent thread: the version from the device the parent version came from.
            let pick = match heads.as_slice() {
                [one] => (*one).clone(),
                many => match many.iter().find(|m| m.device == parent.device) {
                    Some(m) => (*m).clone(),
                    None => remote::choose(&all, Wanted::Latest, devices)
                        .map_err(|e| anyhow!("子线程 {child}：{e}"))?
                        .clone(),
                },
            };
            out.push((child_agent, pick));
        }
    }
    Ok(out)
}

/// Restores `sessions` (from `tree`). Nothing is written to the agents' folders unless every
/// file downloaded and checked, and no local file stands in the way (or `replace`).
pub async fn run(
    remote: &Remote,
    device: &Device,
    env: &Env<'_>,
    sessions: Vec<(AgentKind, Manifest)>,
    devices: &HashMap<String, DeviceInfo>,
    replace: bool,
    on: Progress<'_>,
) -> anyhow::Result<Outcome> {
    let staging = env.paths.staging_dir().join(uuid::Uuid::new_v4().simple().to_string());
    let result = restore_staged(remote, device, env, sessions, devices, replace, on, &staging).await;
    let _ = fs::remove_dir_all(&staging);
    result
}

#[allow(clippy::too_many_arguments)]
async fn restore_staged(
    remote: &Remote,
    device: &Device,
    env: &Env<'_>,
    sessions: Vec<(AgentKind, Manifest)>,
    devices: &HashMap<String, DeviceInfo>,
    replace: bool,
    on: Progress<'_>,
    staging: &Path,
) -> anyhow::Result<Outcome> {
    let homes: Vec<String> = devices.values().filter_map(|d| d.home.clone()).collect();
    let mapper = Mapper::new(env.rules, &homes, &env.home);
    let mut plans = Vec::new();
    for (n, (agent, manifest)) in sessions.into_iter().enumerate() {
        plans.push(plan_session(
            env,
            &mapper,
            agent,
            manifest,
            &staging.join(n.to_string()),
        )?);
    }
    let total: u64 = plans.iter().flat_map(|p| &p.files).map(|f| f.entry.size).sum();
    if !on(Event::Planned { total_bytes: total }) {
        bail!("任务已取消");
    }

    // Download and check everything first.
    for plan in &plans {
        let dir = archive::session_dir(plan.agent.name(), &plan.id);
        for file in &plan.files {
            stage(remote, &dir, &file.entry, &file.staged, on).await?;
        }
    }
    let mut problems = Vec::new();
    for plan in &mut plans {
        for file in &mut plan.files {
            if let Some(existing) = &file.existing {
                let (existing, staged) = (existing.clone(), file.staged.clone());
                file.fit = tokio::task::spawn_blocking(move || fit(&existing, &staged)).await??;
            }
            if file.fit == Fit::Differs {
                problems.push(format!("{}：{}", session_key(plan.agent, &plan.id), file.entry.path));
            }
        }
        for extra in &plan.extra {
            problems.push(format!(
                "{}：{}（存储里这个版本没有）",
                session_key(plan.agent, &plan.id),
                extra.display()
            ));
        }
    }
    if !problems.is_empty() && !replace {
        let shown: Vec<String> = problems.iter().take(5).map(|p| format!("  {p}")).collect();
        let more = if problems.len() > 5 {
            format!("\n  ……共 {} 个", problems.len())
        } else {
            String::new()
        };
        let first = plans.first().map(|p| session_key(p.agent, &p.id)).unwrap_or_default();
        return Err(Conflict(format!(
            "本机这份有存储里这个版本没有的内容，没有恢复，什么也没改：\n{}{more}\n可以先 nekorisu archive {first} 把本机这份也传上去（两份都会留在存储里）；确定要用存储里的替换本机这份，再加 --replace（MCP 里是 replace=true），本机这份会移到 {}，不会删掉。",
            shown.join("\n"),
            env.paths.replaced_dir().display()
        ))
        .into());
    }
    if !on(Event::Placing) {
        bail!("任务已取消");
    }

    // Put everything in place. From here on the task is not stopped half way.
    let stamp = now_ms().to_string();
    let mut outcome = Outcome::default();
    let mut logged = Vec::new();
    let mut codex_threads = Vec::new();
    for plan in plans {
        let mut row = SessionRow {
            session: session_key(plan.agent, &plan.id),
            manifest: plan.manifest.id.clone(),
            files: plan.files.len(),
            bytes: plan.files.iter().map(|f| f.entry.size).sum(),
            written: 0,
            completed: 0,
            unchanged: 0,
            replaced: Vec::new(),
            cwd: plan.manifest.session.cwd.clone(),
            local_cwd: plan.local_cwd.as_ref().map(|p| p.display().to_string()),
            folder: plan.folder.clone(),
            resume: resume_command(plan.agent, &plan.id),
            resume_in: plan
                .local_cwd
                .as_ref()
                .filter(|p| plan.agent == AgentKind::ClaudeCode && p.is_dir())
                .map(|p| p.display().to_string()),
            registered: None,
            note: None,
        };
        let replaced_root = env
            .paths
            .replaced_dir()
            .join(plan.agent.name())
            .join(&plan.id)
            .join(&stamp);
        let home = agent_home(env, plan.agent);
        let mut locals: HashMap<String, LocalCopy> = HashMap::new();
        for file in &plan.files {
            let compressed_existing = file.existing.as_ref().filter(|e| e != &&file.target);
            match file.fit {
                Fit::Same => {
                    row.unchanged += 1;
                    let path = file.existing.clone().unwrap_or_else(|| file.target.clone());
                    let compressed = path.extension().is_some_and(|e| e == "zst");
                    locals.insert(file.entry.path.clone(), copy(path, compressed, &file.entry));
                    continue;
                }
                Fit::New => row.written += 1,
                Fit::Shorter => row.completed += 1,
                Fit::Differs => {}
            }
            // A local copy that stays behind would be a second file for the same thing: move it
            // aside (a compressed one always, since the restored file is plain; a differing one
            // only because `replace` was given).
            if let Some(existing) = &file.existing
                && (file.fit == Fit::Differs || compressed_existing.is_some())
            {
                let aside = move_aside(existing, &home, &replaced_root)?;
                row.replaced.push(aside.display().to_string());
            }
            place(&file.staged, &file.target)?;
            locals.insert(file.entry.path.clone(), copy(file.target.clone(), false, &file.entry));
        }
        for extra in &plan.extra {
            let aside = move_aside(extra, &home, &replaced_root)?;
            row.replaced.push(aside.display().to_string());
        }
        let record = {
            let (storage, manifest, renames) = (env.storage.to_string(), plan.manifest.clone(), plan.renames.clone());
            tokio::task::spawn_blocking(move || sync::record(&storage, "restore", &manifest, renames, &locals)).await?
        };
        if let Err(err) = env.records.save(plan.agent, &plan.id, &record) {
            row.note = Some(format!("恢复好了，但本机记不下它的上传状态：{err}"));
        }
        if plan.agent == AgentKind::Codex {
            codex_threads.push(plan.id.clone());
        }
        logged.push((row.session.clone(), row.manifest.clone(), row.bytes));
        outcome.sessions.push(row);
    }

    let log = async {
        archive::write_device(&remote.store, device).await?;
        archive::write_log(&remote.store, device, "restore", &logged).await
    };
    if let Err(err) = log.await {
        outcome.log_error = Some(format!("{err:#}"));
    }

    if !codex_threads.is_empty() {
        let exe = env.codex_exe.clone().or_else(codex::find);
        let registered = match exe {
            Some(exe) => codex::register(&exe, &codex_threads, &env.codex_home).await,
            None => Err("找不到 codex 程序（可以用环境变量 CODEX_CLI_PATH 指定）".to_string()),
        };
        for row in outcome.sessions.iter_mut().filter(|r| r.session.starts_with("codex:")) {
            let id = &row.session["codex:".len()..];
            match &registered {
                Ok(done) => {
                    let ok = done.contains(id);
                    row.registered = Some(ok);
                    if !ok {
                        row.note =
                            Some("文件已放回，但 Codex 没登记上；它可能要等下次重建索引才会出现在列表里".to_string());
                    }
                }
                Err(err) => {
                    row.registered = Some(false);
                    row.note = Some(format!(
                        "文件已放回，但没能让 Codex 登记（{err}）；它可能要等下次重建索引才会出现在列表里"
                    ));
                }
            }
        }
    }
    Ok(outcome)
}

fn copy(path: PathBuf, compressed: bool, entry: &FileEntry) -> LocalCopy {
    LocalCopy {
        path,
        compressed,
        line_based: matches!(entry.role.as_str(), "transcript" | "subagent"),
    }
}

fn agent_home(env: &Env<'_>, agent: AgentKind) -> PathBuf {
    match agent {
        AgentKind::ClaudeCode => env.claude_home.clone(),
        AgentKind::Codex => env.codex_home.clone(),
    }
}

fn resume_command(agent: AgentKind, id: &str) -> String {
    match agent {
        AgentKind::ClaudeCode => format!("claude --resume {id}"),
        AgentKind::Codex => format!("codex resume {id}"),
    }
}

/// Where each file of a stored version goes here, and what is in the way.
fn plan_session(
    env: &Env<'_>,
    mapper: &Mapper,
    agent: AgentKind,
    manifest: Manifest,
    staging: &Path,
) -> anyhow::Result<SessionPlan> {
    let id = manifest.session.id.clone();
    if id.is_empty() || !crate::agents::is_uuid(&id) {
        bail!("manifest {} 里的会话 ID 不对", manifest.id);
    }
    let local: Option<&Session> = env
        .found
        .iter()
        .filter(|d| d.agent == agent)
        .flat_map(|d| &d.sessions)
        .find(|s| s.id == id);
    let home = agent_home(env, agent);
    let main = remote::main_entry(&manifest)
        .map(|e| e.path.clone())
        .unwrap_or_default();

    let mut renames = Vec::new();
    let (mut folder, mut local_cwd) = (None, None);
    match agent {
        AgentKind::ClaudeCode => {
            let stored = project_folder(&main).ok_or_else(|| anyhow!("manifest {} 里没有主记录", manifest.id))?;
            let here = local
                .and_then(|s| s.files.iter().find(|f| f.path == s.main))
                .and_then(|f| project_folder(&f.rel));
            local_cwd = mapper.map(&manifest.session.cwd);
            let chosen = match here {
                Some(here) => here,
                None => mapper
                    .claude_folder(&manifest.session.cwd)
                    .unwrap_or_else(|| stored.clone()),
            };
            if chosen != stored {
                renames.push(Rename {
                    local: format!("projects/{chosen}/"),
                    stored: format!("projects/{stored}/"),
                });
            }
            folder = Some(chosen);
        }
        AgentKind::Codex => {
            // A thread this machine keeps elsewhere (archived here but not there, or the other way
            // round) stays where it is.
            if let Some(session) = local {
                let local_main = session
                    .files
                    .iter()
                    .find(|f| f.path == session.main)
                    .map(|f| sync::stored_path(&f.rel, &[]));
                if let Some(local_main) = local_main
                    && local_main != main
                    && !main.is_empty()
                {
                    renames.push(Rename {
                        local: local_main,
                        stored: main.clone(),
                    });
                }
            }
        }
    }

    let mut files = Vec::new();
    let mut stored_here: HashSet<String> = HashSet::new();
    for (n, entry) in manifest.files.iter().enumerate() {
        if entry.path.ends_with(".desktop-released.json") {
            continue; // a deletion mark: brought back, it would delete the session again
        }
        let rel = sync::local_path(&entry.path, &renames);
        let target =
            join_rel(&home, &rel).with_context(|| format!("manifest {} 里的路径不对：{}", manifest.id, entry.path))?;
        let zst = PathBuf::from(format!("{}.zst", target.display()));
        let existing = if target.is_file() {
            Some(target.clone())
        } else if agent == AgentKind::Codex && entry.path.ends_with(".jsonl") && zst.is_file() {
            Some(zst)
        } else {
            None
        };
        stored_here.insert(entry.path.clone());
        files.push(FilePlan {
            entry: entry.clone(),
            fit: if existing.is_some() { Fit::Differs } else { Fit::New },
            target,
            existing,
            staged: staging.join(n.to_string()),
        });
    }
    let extra = local
        .map(|s| {
            s.files
                .iter()
                .filter(|f| !stored_here.contains(&sync::stored_path(&f.rel, &renames)))
                .filter(|f| !f.rel.ends_with(".desktop-released.json"))
                .map(|f| f.path.clone())
                .collect()
        })
        .unwrap_or_default();
    Ok(SessionPlan {
        agent,
        id,
        manifest,
        renames,
        files,
        extra,
        folder,
        local_cwd,
    })
}

/// `projects/<folder>/...` → `<folder>`.
fn project_folder(rel: &str) -> Option<String> {
    let mut parts = rel.split('/');
    (parts.next()? == "projects").then_some(())?;
    parts.next().filter(|p| !p.is_empty()).map(str::to_owned)
}

/// `home` joined with a stored relative path, refusing anything that could leave `home`.
fn join_rel(home: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let mut path = home.to_path_buf();
    for part in rel.split('/') {
        let bad = part.is_empty()
            || part == "."
            || part == ".."
            || part.contains('\\')
            || part.contains(':')
            || part.contains('\0');
        if bad {
            bail!("{rel}");
        }
        path.push(part);
    }
    Ok(path)
}

/// Downloads a stored file into `staged`, checking every segment and the whole.
async fn stage(remote: &Remote, dir: &str, entry: &FileEntry, staged: &Path, on: Progress<'_>) -> anyhow::Result<()> {
    if let Some(parent) = staged.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = File::create(staged)?;
    let mut hasher = Sha256::new();
    let mut size = 0;
    for seg in &entry.segments {
        if seg.start != size {
            bail!("{} 的分段不连续（{} 处）", entry.path, seg.start);
        }
        let bytes = remote.segment(dir, &entry.path, seg, false).await?;
        hasher.update(&bytes);
        out.write_all(&bytes)?;
        size += bytes.len() as u64;
        if !on(Event::Downloaded {
            bytes: bytes.len() as u64,
        }) {
            bail!("任务已取消");
        }
    }
    out.sync_all()?;
    if size != entry.size || hex::encode(hasher.finalize()) != entry.sha256 {
        bail!("{} 拼起来和 manifest 记的对不上（大小或 SHA-256 不同）", entry.path);
    }
    Ok(())
}

/// Compares a local file (decompressed if Codex compressed it) with the staged stored one.
fn fit(local: &Path, staged: &Path) -> io::Result<Fit> {
    let raw = File::open(local)?;
    let mut local: Box<dyn Read> = if local.extension().is_some_and(|e| e == "zst") {
        Box::new(zstd::stream::read::Decoder::new(raw)?)
    } else {
        Box::new(raw)
    };
    let mut stored = BufReader::new(File::open(staged)?);
    let mut a = vec![0; 1 << 16];
    let mut b = vec![0; 1 << 16];
    loop {
        let n = read_full(&mut local, &mut a)?;
        let m = read_full(&mut stored, &mut b[..n.max(1)])?;
        if n == 0 {
            return Ok(if m == 0 { Fit::Same } else { Fit::Shorter });
        }
        if m < n || a[..n] != b[..n] {
            return Ok(Fit::Differs);
        }
    }
}

fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Moves a local file under `aside_root`, keeping its path relative to the agent's home.
fn move_aside(path: &Path, home: &Path, aside_root: &Path) -> anyhow::Result<PathBuf> {
    let inner = path
        .strip_prefix(home)
        .unwrap_or(path.file_name().map(Path::new).unwrap_or(path));
    let target = aside_root.join(inner);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    rename_or_copy(path, &target).with_context(|| format!("移不走 {}", path.display()))?;
    Ok(target)
}

/// Moves a staged file to its place: a rename, or a copy next to it and a rename when the two
/// are on different volumes.
fn place(staged: &Path, target: &Path) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    rename_or_copy(staged, target).with_context(|| format!("放不进 {}", target.display()))
}

fn rename_or_copy(from: &Path, to: &Path) -> io::Result<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let tmp = PathBuf::from(format!("{}.nekorisu-tmp", to.display()));
    fs::copy(from, &tmp)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, to)?;
    fs::remove_file(from)
}

/// Lets Codex register restored threads in its own database through its public app-server
/// interface: `initialize`, then `thread/read` per thread, which makes Codex scan for the
/// rollout and add it. `thread/resume` is not used: it appends to the rollout.
pub mod codex {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const TIMEOUT: Duration = Duration::from_secs(60);

    /// The threads Codex answered for.
    pub async fn register(exe: &Path, threads: &[String], codex_home: &Path) -> Result<HashSet<String>, String> {
        let mut command = tokio::process::Command::new(exe);
        command
            .args(["app-server", "--disable", "plugins"])
            .env("CODEX_HOME", codex_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: core has no console to share
        let mut child = command
            .spawn()
            .map_err(|e| format!("启动不了 {}：{e}", exe.display()))?;
        let mut stdin = child.stdin.take().ok_or("没有标准输入")?;
        let mut lines = BufReader::new(child.stdout.take().ok_or("没有标准输出")?).lines();
        let talk = async {
            let mut send = async |message: Value| -> Result<(), String> {
                let mut text = message.to_string();
                text.push('\n');
                stdin.write_all(text.as_bytes()).await.map_err(|e| e.to_string())?;
                stdin.flush().await.map_err(|e| e.to_string())
            };
            send(json!({ "id": 0, "method": "initialize", "params": {
                "clientInfo": { "name": "nekorisu", "version": env!("CARGO_PKG_VERSION") }
            }}))
            .await?;
            await_reply(&mut lines, 0)
                .await?
                .map_err(|e| format!("initialize 失败：{e}"))?;
            let mut done = HashSet::new();
            for (n, thread) in threads.iter().enumerate() {
                let id = n as u64 + 1;
                send(json!({ "id": id, "method": "thread/read", "params": { "threadId": thread } })).await?;
                if await_reply(&mut lines, id).await?.is_ok() {
                    done.insert(thread.clone());
                }
            }
            Ok::<_, String>(done)
        };
        let result = tokio::time::timeout(TIMEOUT, talk)
            .await
            .unwrap_or_else(|_| Err("codex 没有及时回应".to_string()));
        drop(stdin);
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
        result
    }

    /// Reads until the reply to `id`; other messages (notifications) are skipped.
    async fn await_reply<R: tokio::io::AsyncBufRead + Unpin>(
        lines: &mut tokio::io::Lines<R>,
        id: u64,
    ) -> Result<Result<Value, String>, String> {
        while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error.get("message").and_then(Value::as_str).unwrap_or("出错");
                return Ok(Err(text.to_string()));
            }
            return Ok(Ok(message.get("result").cloned().unwrap_or(Value::Null)));
        }
        Err("codex 提前退出了".to_string())
    }

    /// The codex program: `CODEX_CLI_PATH`, then `PATH`, then where installers and the desktop
    /// app put it (core runs with the system's short `PATH`, so those are looked at by name).
    pub fn find() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("CODEX_CLI_PATH").map(PathBuf::from)
            && path.is_file()
        {
            return Some(path);
        }
        let names: &[&str] = if cfg!(windows) {
            &["codex.exe", "codex.cmd"]
        } else {
            &["codex"]
        };
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                for name in names {
                    let candidate = dir.join(name);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        let home = crate::paths::home_dir();
        let mut known: Vec<PathBuf> = Vec::new();
        if cfg!(windows) {
            if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
                known.push(local.join(r"Programs\OpenAI\Codex\bin\codex.exe"));
            }
            if let Some(roaming) = std::env::var_os("APPDATA").map(PathBuf::from) {
                known.push(roaming.join(r"npm\codex.cmd"));
            }
        } else {
            known.extend([
                home.join(".local/bin/codex"),
                PathBuf::from("/opt/homebrew/bin/codex"),
                PathBuf::from("/usr/local/bin/codex"),
                home.join(".npm-global/bin/codex"),
                home.join(".bun/bin/codex"),
                PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex"),
                home.join("Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex"),
            ]);
        }
        known.into_iter().find(|p| p.is_file())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agents::{claude_code, codex as codex_agent};
    use crate::archive::SyncCtx;
    use crate::storage::Webdav;
    use crate::testdav::Server;

    const ID: &str = "11111111-2222-4333-8444-555555555555";
    /// The folder a Windows device gives a session made in `C:\Users\neko\work\a`.
    const WIN_FOLDER: &str = "C--Users-neko-work-a";
    const PARENT: &str = "019a0000-0000-7000-8000-00000000000a";
    const CHILD: &str = "019a0000-0000-7000-8000-00000000000b";

    /// One device: its own nekorisu home, agent homes and user home.
    struct Side {
        device: Device,
        paths: Paths,
        records: sync::Store,
        claude: tempfile::TempDir,
        codex: tempfile::TempDir,
        home: tempfile::TempDir,
        codex_exe: Option<PathBuf>,
        _nk: tempfile::TempDir,
    }

    impl Side {
        fn new(id: &str) -> Side {
            let nk = tempfile::tempdir().unwrap();
            let paths = Paths {
                home: nk.path().to_path_buf(),
            };
            Side {
                device: Device {
                    id: id.to_string(),
                    name: format!("{id}-name"),
                    created_at_ms: 1,
                },
                records: sync::Store::new(paths.sync_dir()),
                paths,
                claude: tempfile::tempdir().unwrap(),
                codex: tempfile::tempdir().unwrap(),
                home: tempfile::tempdir().unwrap(),
                codex_exe: Some(PathBuf::from("/nonexistent/codex")),
                _nk: nk,
            }
        }

        fn found(&self) -> Vec<Discovery> {
            vec![
                claude_code::discover_in(self.claude.path().to_path_buf()),
                codex_agent::discover_in(self.codex.path().to_path_buf()),
            ]
        }

        fn claude_main(&self, folder: &str) -> PathBuf {
            self.claude
                .path()
                .join("projects")
                .join(folder)
                .join(format!("{ID}.jsonl"))
        }

        async fn archive(&self, server: &Server, agent: AgentKind, id: &str) -> archive::Outcome {
            let store = Webdav::new(server.spec()).unwrap();
            let ctx = SyncCtx {
                records: &self.records,
                storage: "test",
            };
            let found = self.found();
            let jobs = archive::jobs_for(&found, &[(agent, id.to_string())]);
            archive::run(&store, &self.device, Some(&ctx), jobs, &|_| true).await
        }

        async fn restore(
            &self,
            server: &Server,
            id: &str,
            version: Option<&str>,
            replace: bool,
        ) -> anyhow::Result<Outcome> {
            let remote = Remote::new(Arc::new(Webdav::new(server.spec()).unwrap()), self.paths.clone());
            let (agent, ids) = remote.find(id, None).await?;
            let all = remote.manifests(agent, id, &ids).await?;
            let devices = remote.devices().await;
            let wanted = version.map_or(Wanted::Latest, Wanted::Id);
            let chosen = remote::choose(&all, wanted, &devices)?.clone();
            let sessions = tree(&remote, agent, chosen, &devices).await?;
            let found = self.found();
            let env = Env {
                paths: &self.paths,
                home: self.home.path().to_path_buf(),
                claude_home: self.claude.path().to_path_buf(),
                codex_home: self.codex.path().to_path_buf(),
                found: &found,
                rules: &[],
                records: &self.records,
                storage: "test",
                codex_exe: self.codex_exe.clone(),
            };
            run(&remote, &self.device, &env, sessions, &devices, replace, &|_| true).await
        }
    }

    fn claude_line(n: u32) -> String {
        let t = format!("2026-11-01T10:{:02}:{:02}.000Z", n / 60, n % 60);
        let line = match n % 3 {
            0 => format!(
                r#"{{"type":"user","timestamp":"{t}","cwd":"C:\\Users\\neko\\work\\a","version":"9.9.9","message":{{"content":"synthetic {n}"}}}}"#
            ),
            1 => format!(
                r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"text","text":"reply {n}"}}]}}}}"#
            ),
            _ => format!(
                r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"tool_use","name":"S"}}]}}}}"#
            ),
        };
        line + "\n"
    }

    fn lines(range: std::ops::Range<u32>) -> String {
        range.map(claude_line).collect()
    }

    /// A Claude Code session made on a Windows device: transcript, a sub-agent, a tool result
    /// and a file-history entry.
    fn windows_session(side: &Side) {
        let main = side.claude_main(WIN_FOLDER);
        let dir = main.parent().unwrap().join(ID);
        fs::create_dir_all(dir.join("subagents")).unwrap();
        fs::create_dir_all(dir.join("tool-results")).unwrap();
        fs::write(&main, lines(0..9)).unwrap();
        fs::write(dir.join("subagents/agent-a1.jsonl"), lines(0..2)).unwrap();
        fs::write(dir.join("tool-results/t1.txt"), "synthetic output").unwrap();
        let history = side.claude.path().join("file-history").join(ID);
        fs::create_dir_all(&history).unwrap();
        fs::write(history.join("f@v1"), "v1").unwrap();
    }

    fn append(path: &Path, text: &str) {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn manifest_on(server: &Server, agent: &str, id: &str, mid: &str) -> Manifest {
        let path = server
            .root()
            .join(format!("nekorisu/v1/sessions/{agent}/{id}/manifests/{mid}.json"));
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_claude_code_session_goes_to_another_device_and_comes_back() {
        let server = Server::start(None).await;
        let (a, b) = (Side::new("dev-a"), Side::new("dev-b"));
        windows_session(&a);
        let out = a.archive(&server, AgentKind::ClaudeCode, ID).await;
        assert_eq!(
            out.sessions[0].manifest.as_deref(),
            Some("dev-a-1"),
            "{:?}",
            out.sessions[0].error
        );
        assert_eq!(a.records.load(AgentKind::ClaudeCode, ID).unwrap().manifest, "dev-a-1");
        // The device file says where things are on the device.
        let registered: serde_json::Value =
            serde_json::from_slice(&fs::read(server.root().join("nekorisu/v1/devices/dev-a.json")).unwrap()).unwrap();
        assert_eq!(registered["os"], std::env::consts::OS);
        assert!(registered["home"].as_str().is_some_and(|h| !h.is_empty()));

        // B has never seen it: everything is written, into the folder its own home gives.
        let outcome = b.restore(&server, ID, None, false).await.unwrap();
        let row = &outcome.sessions[0];
        let mapped = b.home.path().join("work").join("a");
        let folder = crate::pathmap::claude_project_dir(&mapped.to_string_lossy());
        assert_eq!(
            (row.written, row.completed, row.unchanged, row.folder.as_deref()),
            (4, 0, 0, Some(folder.as_str()))
        );
        assert_eq!(row.local_cwd.as_deref(), Some(mapped.to_string_lossy().as_ref()));
        assert_eq!(row.resume, format!("claude --resume {ID}"));
        assert_eq!(
            fs::read(b.claude_main(&folder)).unwrap(),
            fs::read(a.claude_main(WIN_FOLDER)).unwrap()
        );
        let sub = |side: &Side, folder: &str| {
            fs::read(
                side.claude
                    .path()
                    .join(format!("projects/{folder}/{ID}/subagents/agent-a1.jsonl")),
            )
            .unwrap()
        };
        assert_eq!(sub(&b, &folder), sub(&a, WIN_FOLDER));
        assert_eq!(
            fs::read(b.claude.path().join(format!("file-history/{ID}/f@v1"))).unwrap(),
            b"v1"
        );
        assert!(!b.claude.path().join("projects").join(WIN_FOLDER).exists());
        assert!(outcome.log_error.is_none());
        let log = fs::read_to_string(server.root().join("nekorisu/v1/logs/dev-b/1.jsonl")).unwrap();
        assert!(log.contains("\"op\":\"restore\"") && log.contains("dev-a-1"), "{log}");
        let record = b.records.load(AgentKind::ClaudeCode, ID).unwrap();
        assert_eq!((record.op.as_str(), record.manifest.as_str()), ("restore", "dev-a-1"));
        assert_eq!(record.renames[0].stored, format!("projects/{WIN_FOLDER}/"));
        // Nothing new on B as far as status is concerned.
        let found = b.found();
        let session = found[0].sessions.iter().find(|s| s.id == ID).unwrap();
        assert_eq!(
            sync::session_delta(session, &record, &mut sync::DeltaCache::new()),
            sync::Delta::default()
        );

        // B goes on; its upload continues A's version and keeps the stored names.
        let more = lines(9..12);
        append(&b.claude_main(&folder), &more);
        let out = b.archive(&server, AgentKind::ClaudeCode, ID).await;
        let report = &out.sessions[0];
        assert_eq!(report.manifest.as_deref(), Some("dev-b-1"), "{:?}", report.error);
        assert_eq!(
            (report.new_bytes, report.new_lines, report.rewrite),
            (more.len() as u64, 3, false)
        );
        let m = manifest_on(&server, "claude-code", ID, "dev-b-1");
        assert_eq!(m.parent.as_deref(), Some("dev-a-1"));
        assert!(m.files.iter().all(|f| !f.path.contains(&folder)), "{:?}", m.files);
        let main = m.files.iter().find(|f| f.role == "transcript").unwrap();
        assert_eq!(main.path, format!("projects/{WIN_FOLDER}/{ID}.jsonl"));
        assert_eq!(main.segments.len(), 2);

        // Back on A: its copy is the start of B's, so it is completed in place.
        let outcome = a.restore(&server, ID, None, false).await.unwrap();
        let row = &outcome.sessions[0];
        assert_eq!((row.completed, row.unchanged, row.written), (1, 3, 0));
        assert_eq!(row.folder.as_deref(), Some(WIN_FOLDER));
        assert!(row.replaced.is_empty());
        assert_eq!(
            fs::read(a.claude_main(WIN_FOLDER)).unwrap(),
            fs::read(b.claude_main(&folder)).unwrap()
        );
        // And A's next upload continues B's version.
        append(&a.claude_main(WIN_FOLDER), &lines(12..13));
        let out = a.archive(&server, AgentKind::ClaudeCode, ID).await;
        assert_eq!(out.sessions[0].manifest.as_deref(), Some("dev-a-2"));
        assert_eq!(
            manifest_on(&server, "claude-code", ID, "dev-a-2").parent.as_deref(),
            Some("dev-b-1")
        );
    }

    #[tokio::test]
    async fn forks_are_listed_and_local_content_wins_unless_replaced() {
        let server = Server::start(None).await;
        let (a, b) = (Side::new("dev-a"), Side::new("dev-b"));
        windows_session(&a);
        a.archive(&server, AgentKind::ClaudeCode, ID).await;
        b.restore(&server, ID, None, false).await.unwrap();
        let folder = b.records.load(AgentKind::ClaudeCode, ID).unwrap().renames[0]
            .local
            .clone();
        let b_main = b.claude.path().join(&folder).join(format!("{ID}.jsonl"));
        append(&b_main, &lines(20..22));
        b.archive(&server, AgentKind::ClaudeCode, ID).await;
        append(&a.claude_main(WIN_FOLDER), &lines(30..31));
        a.archive(&server, AgentKind::ClaudeCode, ID).await;

        // Two newest versions: no default.
        let err = a.restore(&server, ID, None, false).await.unwrap_err();
        let text = err.to_string();
        assert!(err.downcast_ref::<remote::Choose>().is_some(), "{text}");
        assert!(
            text.contains("dev-a-2") && text.contains("dev-b-1") && text.contains("dev-b-name"),
            "{text}"
        );

        // A's copy has a turn B's version lacks: stop, change nothing.
        let before = fs::read(a.claude_main(WIN_FOLDER)).unwrap();
        let err = a.restore(&server, ID, Some("dev-b-1"), false).await.unwrap_err();
        assert!(err.downcast_ref::<Conflict>().is_some(), "{err}");
        assert!(err.to_string().contains("nekorisu archive"), "{err}");
        assert_eq!(fs::read(a.claude_main(WIN_FOLDER)).unwrap(), before);
        assert!(!a.paths.replaced_dir().exists());
        assert!(fs::read_dir(a.paths.staging_dir()).map(|d| d.count()).unwrap_or(0) == 0);

        // Insisting replaces it, and A's copy is kept aside.
        let outcome = a.restore(&server, ID, Some("dev-b-1"), true).await.unwrap();
        let row = &outcome.sessions[0];
        assert_eq!(row.replaced.len(), 1);
        assert_eq!(fs::read(&row.replaced[0]).unwrap(), before);
        assert_eq!(fs::read(a.claude_main(WIN_FOLDER)).unwrap(), fs::read(&b_main).unwrap());
    }

    fn codex_rollout(side: &Side, id: &str, parent: Option<&str>, turns: u32) -> PathBuf {
        let dir = side.codex.path().join("sessions/2026/11/01");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-11-01T10-00-00-{id}.jsonl"));
        let extra = parent.map_or(String::new(), |p| format!(r#","parent_thread_id":"{p}""#));
        let mut text = format!(
            r#"{{"timestamp":"2026-11-01T10:00:00.000Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/synthetic","cli_version":"0.170.0"{extra}}}}}"#
        ) + "\n";
        for n in 0..turns {
            text += &format!(
                r#"{{"timestamp":"2026-11-01T10:00:{n:02}.000Z","type":"event_msg","payload":{{"type":"item_completed","item":{{"type":"UserMessage"}}}}}}"#
            );
            text += "\n";
        }
        fs::write(&path, text).unwrap();
        path
    }

    #[tokio::test]
    async fn a_codex_thread_comes_with_its_sub_agent_threads() {
        let server = Server::start(None).await;
        let a = Side::new("dev-a");
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut b = Side::new("dev-b");
        let parent = codex_rollout(&a, PARENT, None, 3);
        let child = codex_rollout(&a, CHILD, Some(PARENT), 2);
        let out = a.archive(&server, AgentKind::Codex, PARENT).await;
        assert_eq!(out.sessions.len(), 2);

        // B already has the child, compressed by Codex: it stays as it is.
        let rel = child.strip_prefix(a.codex.path()).unwrap();
        let b_child = b.codex.path().join(rel);
        fs::create_dir_all(b_child.parent().unwrap()).unwrap();
        let zst = PathBuf::from(format!("{}.zst", b_child.display()));
        fs::write(&zst, zstd::encode_all(&fs::read(&child).unwrap()[..], 3).unwrap()).unwrap();
        #[cfg(unix)]
        let asked = {
            let script = b.home.path().join("fake-codex");
            fs::write(
                &script,
                "#!/bin/sh\nwhile IFS= read -r line; do\n  echo \"$line\" >> \"$CODEX_HOME/asked.log\"\n  id=$(printf '%s' \"$line\" | sed -n 's/^{\"id\":\\([0-9]*\\).*/\\1/p')\n  case \"$line\" in\n    *'\"method\":\"initialize\"'*) echo '{\"method\":\"note\"}'; echo \"{\\\"id\\\":$id,\\\"result\\\":{}}\";;\n    *'\"method\":\"thread/read\"'*) echo \"{\\\"id\\\":$id,\\\"result\\\":{\\\"thread\\\":{}}}\";;\n  esac\ndone\n",
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            b.codex_exe = Some(script);
            b.codex.path().join("asked.log")
        };

        let outcome = b.restore(&server, PARENT, None, false).await.unwrap();
        assert_eq!(outcome.sessions.len(), 2);
        let row = |id: &str| {
            outcome
                .sessions
                .iter()
                .find(|r| r.session == format!("codex:{id}"))
                .unwrap()
        };
        assert_eq!((row(PARENT).written, row(CHILD).unchanged), (1, 1));
        assert_eq!(row(PARENT).resume, format!("codex resume {PARENT}"));
        let b_parent = b.codex.path().join(parent.strip_prefix(a.codex.path()).unwrap());
        assert_eq!(fs::read(&b_parent).unwrap(), fs::read(&parent).unwrap());
        assert!(zst.is_file() && !b_child.exists());
        #[cfg(unix)]
        {
            assert_eq!(
                (row(PARENT).registered, row(CHILD).registered),
                (Some(true), Some(true))
            );
            let asked = fs::read_to_string(asked).unwrap();
            assert!(asked.contains("\"method\":\"initialize\""));
            assert!(
                asked.contains(&format!("\"threadId\":\"{PARENT}\""))
                    && asked.contains(&format!("\"threadId\":\"{CHILD}\""))
            );
            assert!(!asked.contains("thread/resume"));
        }
    }

    #[tokio::test]
    async fn without_codex_the_files_are_still_restored() {
        let server = Server::start(None).await;
        let (a, b) = (Side::new("dev-a"), Side::new("dev-b"));
        codex_rollout(&a, PARENT, None, 2);
        a.archive(&server, AgentKind::Codex, PARENT).await;
        let outcome = b.restore(&server, PARENT, None, false).await.unwrap();
        let row = &outcome.sessions[0];
        assert_eq!((row.written, row.registered), (1, Some(false)));
        assert!(
            row.note.as_deref().is_some_and(|n| n.contains("Codex")),
            "{:?}",
            row.note
        );
    }

    #[tokio::test]
    async fn a_damaged_segment_stops_the_restore_before_anything_is_written() {
        let server = Server::start(None).await;
        let (a, b) = (Side::new("dev-a"), Side::new("dev-b"));
        windows_session(&a);
        a.archive(&server, AgentKind::ClaudeCode, ID).await;
        let m = manifest_on(&server, "claude-code", ID, "dev-a-1");
        let main = m.files.iter().find(|f| f.role == "transcript").unwrap();
        let seg = &main.segments[0];
        let path = server.root().join(format!(
            "nekorisu/v1/sessions/claude-code/{ID}/files/{}/{}-{}.seg",
            main.path, seg.start, seg.sha256
        ));
        let mut bytes = fs::read(&path).unwrap();
        bytes[3] ^= 1;
        fs::write(&path, bytes).unwrap();
        let err = b.restore(&server, ID, None, false).await.unwrap_err();
        assert!(err.to_string().contains("SHA-256"), "{err}");
        assert!(!b.claude.path().join("projects").exists());
    }

    #[test]
    fn stored_paths_cannot_leave_the_agent_home() {
        let home = Path::new("home");
        assert_eq!(
            join_rel(home, "projects/p/s.jsonl").unwrap(),
            home.join("projects").join("p").join("s.jsonl")
        );
        for bad in [
            "../x",
            "projects/../../x",
            "/etc/x",
            "a//b",
            r"a\..\b",
            "C:/x",
            "a/./b",
            "",
        ] {
            assert!(join_rel(home, bad).is_err(), "{bad}");
        }
        assert_eq!(project_folder("projects/-a-b/s.jsonl").as_deref(), Some("-a-b"));
        assert_eq!(project_folder("file-history/s/x"), None);
    }

    #[test]
    fn fit_tells_same_shorter_and_different_apart() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            fs::write(&path, bytes).unwrap();
            path
        };
        let stored = file("stored", &[7u8; 200_000]);
        assert_eq!(fit(&file("same", &[7u8; 200_000]), &stored).unwrap(), Fit::Same);
        assert_eq!(fit(&file("short", &[7u8; 70_000]), &stored).unwrap(), Fit::Shorter);
        assert_eq!(fit(&file("empty", &[]), &stored).unwrap(), Fit::Shorter);
        assert_eq!(fit(&file("long", &[7u8; 200_001]), &stored).unwrap(), Fit::Differs);
        let mut changed = vec![7u8; 100];
        changed[50] = 8;
        assert_eq!(fit(&file("changed", &changed), &stored).unwrap(), Fit::Differs);
        let zst = file("short.zst", &zstd::encode_all(&[7u8; 1000][..], 3).unwrap());
        assert_eq!(fit(&zst, &stored).unwrap(), Fit::Shorter);
    }
}
