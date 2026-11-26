//! The gRPC service. Reads (scan, status, fetch) work on the local originals;
//! archive uploads to the configured WebDAV storage as a task. The rest of what
//! needs storage (search over uploads, restore, doctor, log, digest) is still
//! to do and answers "unimplemented".

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use super::device::Device;
use super::tasks::{self, Registry};
use crate::agents::{self, AgentKind, BundleFile, Discovery, FileRole, Session};
use crate::archive;
use crate::config::{self, Config};
use crate::linemap::{Kind, LineMap};
use crate::paths::Paths;
use crate::proto::core_server::Core;
use crate::proto::{self, *};
use crate::storage::{self, Webdav};
use crate::timefmt::{human_bytes, now_ms};

/// Discovery walks both agents' directories; reuse it for a few seconds so an
/// agent's burst of calls (at least 3 per model turn) stays fast.
const DISCOVERY_TTL: Duration = Duration::from_secs(3);
const DAY_MS: i64 = 86_400_000;

pub struct CoreService {
    inner: Arc<Inner>,
}

struct Inner {
    paths: Paths,
    device: Device,
    started_at_ms: i64,
    stop: watch::Sender<bool>,
    config: RwLock<Config>,
    /// Finds every agent's sessions; tests point it at temporary directories.
    discover_all: Arc<dyn Fn() -> Vec<Discovery> + Send + Sync>,
    discovery: Mutex<Option<(Instant, Arc<Vec<Discovery>>)>>,
    linemaps: Mutex<HashMap<PathBuf, CachedMap>>,
    tasks: Registry,
    /// One archive at a time: two uploads of the same session would race for the same manifest number.
    archive_gate: tokio::sync::Mutex<()>,
}

impl CoreService {
    /// True while no write task is planned, running or waiting to commit; the
    /// updater only switches versions then.
    pub fn idle_probe(&self) -> impl Fn() -> bool + Send + Sync + 'static {
        let inner = self.inner.clone();
        move || inner.tasks.list(true, 1).is_empty()
    }

    pub fn new(paths: Paths, device: Device, config: Config, stop: watch::Sender<bool>) -> Self {
        let discover_all = || AgentKind::ALL.map(agents::discover).into_iter().collect();
        Self::with_discovery(paths, device, config, stop, Arc::new(discover_all))
    }

    fn with_discovery(
        paths: Paths,
        device: Device,
        config: Config,
        stop: watch::Sender<bool>,
        discover_all: Arc<dyn Fn() -> Vec<Discovery> + Send + Sync>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                paths,
                device,
                started_at_ms: now_ms(),
                stop,
                config: RwLock::new(config),
                discover_all,
                discovery: Mutex::new(None),
                linemaps: Mutex::new(HashMap::new()),
                tasks: Registry::new(),
                archive_gate: tokio::sync::Mutex::new(()),
            }),
        }
    }
}

type TaskStream = Pin<Box<dyn Stream<Item = Result<Task, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl Core for CoreService {
    async fn hello(&self, _request: Request<HelloRequest>) -> Result<Response<HelloResponse>, Status> {
        Ok(Response::new(HelloResponse {
            protocol_version: proto::PROTOCOL_VERSION,
            core_version: env!("CARGO_PKG_VERSION").to_string(),
            device_id: self.inner.device.id.clone(),
            pid: std::process::id(),
            started_at_ms: self.inner.started_at_ms,
        }))
    }

    async fn shutdown(&self, _request: Request<ShutdownRequest>) -> Result<Response<ShutdownResponse>, Status> {
        let _ = self.inner.stop.send(true);
        Ok(Response::new(ShutdownResponse {}))
    }

    async fn reload_config(
        &self,
        _request: Request<ReloadConfigRequest>,
    ) -> Result<Response<ReloadConfigResponse>, Status> {
        let config = config::load(&self.inner.paths)
            .map_err(|err| Status::invalid_argument(format!("config.toml 有问题：{err}")))?;
        let response = ReloadConfigResponse {
            config_version: config.version,
            source: config.source.clone().unwrap_or_else(|| "local".to_string()),
        };
        *self.inner.config.write().unwrap_or_else(|e| e.into_inner()) = config;
        Ok(Response::new(response))
    }

    async fn scan(&self, request: Request<ScanRequest>) -> Result<Response<ScanResponse>, Status> {
        let wanted = wanted_agents(&request.get_ref().agents);
        let found = self.inner.discover().await?;
        let now = now_ms();
        let agents = found
            .iter()
            .filter(|d| wanted.contains(&d.agent))
            .map(|d| summarize(d, now))
            .collect();
        Ok(Response::new(ScanResponse { agents }))
    }

    async fn status(&self, request: Request<StatusRequest>) -> Result<Response<StatusResponse>, Status> {
        let request = request.into_inner();
        let wanted = wanted_agents(&request.agents);
        let found = self.inner.discover().await?;
        let mut children: HashMap<(AgentKind, &str), u32> = HashMap::new();
        for session in found.iter().flat_map(|d| &d.sessions) {
            if let Some(parent) = &session.parent {
                *children.entry((session.agent, parent.as_str())).or_default() += 1;
            }
        }
        let mut rows: Vec<&Session> = found
            .iter()
            .filter(|d| wanted.contains(&d.agent))
            .flat_map(|d| &d.sessions)
            .filter(|s| s.last_activity_ms >= request.since_ms)
            .collect();
        rows.sort_by_key(|s| std::cmp::Reverse(s.last_activity_ms));
        let limit = if request.limit == 0 {
            usize::MAX
        } else {
            request.limit as usize
        };
        let sessions = rows
            .into_iter()
            .take(limit)
            .map(|s| SessionStatus {
                session: Some(session_ref(s.agent, &s.id)),
                parent: s.parent.as_deref().map(|p| session_ref(s.agent, p)),
                project: s.project.clone(),
                last_activity_ms: s.last_activity_ms,
                bytes: s.bytes(),
                files: s.files.len() as u32,
                children: children.get(&(s.agent, s.id.as_str())).copied().unwrap_or(0),
                upload: Some(UploadState::default()), // nothing is uploaded until archive lands
                cleanup_at_ms: s.cleanup_at_ms,
                compressed: s.compressed,
                archived_by_agent: s.archived_by_agent,
            })
            .collect();
        Ok(Response::new(StatusResponse { sessions }))
    }

    async fn search(&self, _request: Request<SearchRequest>) -> Result<Response<SearchResponse>, Status> {
        Err(not_yet("search"))
    }

    async fn fetch(&self, request: Request<FetchRequest>) -> Result<Response<FetchResponse>, Status> {
        let request = request.into_inner();
        if !(request.version.is_empty() || request.version == "local") {
            return Err(Status::unimplemented("从远端读取还在开发；现在只能读本机的原件"));
        }
        let target = request
            .session
            .clone()
            .ok_or_else(|| Status::invalid_argument("缺少 session"))?;
        let kinds = request
            .kinds
            .iter()
            .map(|k| Kind::parse(k).ok_or_else(|| Status::invalid_argument(format!("不认识的种类：{k}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let found = self.inner.discover().await?;
        let session = find_session(&found, &target)?;
        let file = pick_file(session, &request.file)?.clone();
        let transcripts = session
            .files
            .iter()
            .filter(|f| matches!(f.role, FileRole::Transcript | FileRole::Subagent))
            .map(|f| f.rel.clone())
            .collect();
        let agent = session.agent;
        let inner = self.inner.clone();
        let mut response = tokio::task::spawn_blocking(move || inner.select(agent, &file, &request, &kinds))
            .await
            .map_err(|err| Status::internal(err.to_string()))??;
        response.files = transcripts;
        Ok(Response::new(response))
    }

    async fn archive(&self, request: Request<ArchiveRequest>) -> Result<Response<Task>, Status> {
        let request = request.into_inner();
        if request.sessions.is_empty() {
            return Err(Status::invalid_argument("没有指定要上传的会话"));
        }
        if !request.digest_ids.is_empty() {
            return Err(Status::unimplemented("上传时附带 digest 还在开发"));
        }
        // An upload should see the files as they are now, not as they were a few seconds ago.
        let found = self.inner.discover_fresh().await?;
        let mut wanted = Vec::new();
        for target in &request.sessions {
            let session = find_session(&found, target)?;
            wanted.push((session.agent, session.id.clone()));
        }
        let jobs = archive::jobs_for(&found, &wanted);

        let (name, storage_config) = {
            let config = self.inner.config.read().unwrap_or_else(|e| e.into_inner());
            let (name, storage_config) =
                storage::pick(&config, &request.storage).map_err(|err| Status::failed_precondition(err.to_string()))?;
            (name.to_string(), storage_config.clone())
        };
        let store = storage::connect(&self.inner.paths, &name, &storage_config)
            .await
            .map_err(|err| Status::failed_precondition(format!("{err:#}")))?;

        let refs = jobs
            .iter()
            .map(|j| session_ref(j.session.agent, &j.session.id))
            .collect();
        let task = self
            .inner
            .tasks
            .create("archive", refs, format!("上传 {} 个会话到存储 {name}", jobs.len()));
        let (inner, id) = (self.inner.clone(), task.id.clone());
        tokio::spawn(async move { inner.run_archive(id, name, store, jobs).await });
        Ok(Response::new(task))
    }

    async fn restore(&self, _request: Request<RestoreRequest>) -> Result<Response<Task>, Status> {
        Err(not_yet("restore"))
    }

    async fn doctor(&self, _request: Request<DoctorRequest>) -> Result<Response<Task>, Status> {
        Err(not_yet("doctor"))
    }

    async fn get_task(&self, request: Request<TaskRequest>) -> Result<Response<Task>, Status> {
        self.inner
            .tasks
            .get(&request.get_ref().id)
            .map(Response::new)
            .ok_or_else(|| Status::not_found("没有这个任务"))
    }

    type WatchTaskStream = TaskStream;

    async fn watch_task(&self, _request: Request<TaskRequest>) -> Result<Response<Self::WatchTaskStream>, Status> {
        Err(not_yet("watch_task"))
    }

    async fn list_tasks(&self, request: Request<ListTasksRequest>) -> Result<Response<ListTasksResponse>, Status> {
        let request = request.get_ref();
        let limit = if request.limit == 0 {
            100
        } else {
            request.limit as usize
        };
        Ok(Response::new(ListTasksResponse {
            tasks: self.inner.tasks.list(request.active_only, limit),
        }))
    }

    async fn cancel_task(&self, request: Request<TaskRequest>) -> Result<Response<Task>, Status> {
        self.inner
            .tasks
            .advance(
                &request.get_ref().id,
                TaskState::Abandoned,
                Some("用户取消".to_string()),
            )
            .map(Response::new)
            .map_err(Status::failed_precondition)
    }

    async fn append_log(&self, _request: Request<AppendLogRequest>) -> Result<Response<LogEntry>, Status> {
        Err(not_yet("log"))
    }

    async fn list_log(&self, _request: Request<ListLogRequest>) -> Result<Response<ListLogResponse>, Status> {
        Err(not_yet("log"))
    }

    async fn put_digest(&self, _request: Request<PutDigestRequest>) -> Result<Response<Digest>, Status> {
        Err(not_yet("digest"))
    }

    async fn get_digests(&self, _request: Request<GetDigestsRequest>) -> Result<Response<GetDigestsResponse>, Status> {
        Err(not_yet("digest"))
    }
}

impl Inner {
    /// The archive task: planned -> running -> pending commit (everything uploaded, the
    /// operations log is next) -> committed, or abandoned with the reason.
    async fn run_archive(self: Arc<Self>, id: String, storage_name: String, store: Webdav, jobs: Vec<archive::Job>) {
        let _gate = self.archive_gate.lock().await;
        if self.tasks.advance(&id, TaskState::Running, None).is_err() {
            return; // cancelled while waiting for its turn
        }
        let alive = |id: &str| self.tasks.get(id).is_some_and(|t| tasks::is_active(&t));
        let on = |event: archive::Event| {
            match event {
                archive::Event::Planned { total_bytes } => self.tasks.update(&id, |t| t.total_bytes = total_bytes),
                archive::Event::Uploaded { bytes } => self.tasks.update(&id, |t| t.done_bytes += bytes),
                archive::Event::Committing => {
                    let _ = self.tasks.advance(&id, TaskState::PendingCommit, None);
                }
            }
            alive(&id)
        };
        let outcome = archive::run(&store, &self.device, jobs, &on).await;

        let rows = &outcome.sessions;
        let uploaded = rows.iter().filter(|r| r.error.is_none() && !r.up_to_date).count();
        let current = rows.iter().filter(|r| r.up_to_date).count();
        let failed = outcome.failed();
        let mut parts = Vec::new();
        if uploaded > 0 {
            parts.push(format!(
                "上传 {uploaded} 个会话（新增 {}、{} 行）",
                human_bytes(rows.iter().map(|r| r.new_bytes).sum()),
                rows.iter().map(|r| r.new_lines).sum::<u64>()
            ));
        }
        if current > 0 {
            parts.push(format!("{current} 个已是最新"));
        }
        if failed > 0 {
            parts.push(format!("{failed} 个没传成功"));
        }
        let summary = format!("存储 {storage_name}：{}", parts.join("，"));
        let result = serde_json::to_string(rows).unwrap_or_default();
        self.tasks.update(&id, |t| {
            t.summary = summary;
            t.result_json = result;
        });

        let mut problems: Vec<String> = rows.iter().filter_map(|r| r.error.clone()).take(3).collect();
        if let Some(log_error) = &outcome.log_error {
            problems.push(format!("会话已经传上去了，但操作日志没写成功：{log_error}"));
        }
        // Running -> pending commit when nothing was uploaded and so nothing was logged.
        let _ = self.tasks.advance(&id, TaskState::PendingCommit, None);
        let end = if problems.is_empty() {
            self.tasks.advance(&id, TaskState::Committed, None)
        } else {
            self.tasks.advance(&id, TaskState::Abandoned, Some(problems.join("；")))
        };
        // A cancelled task is already abandoned; nothing more to say.
        if let Err(err) = end
            && self.tasks.get(&id).is_some_and(|t| tasks::is_active(&t))
        {
            eprintln!("{} 上传任务 {id} 没法收尾：{err}", now_ms());
        }
    }

    async fn discover(&self) -> Result<Arc<Vec<Discovery>>, Status> {
        let cached = lock(&self.discovery)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < DISCOVERY_TTL)
            .map(|(_, found)| found.clone());
        if let Some(found) = cached {
            return Ok(found);
        }
        self.discover_fresh().await
    }

    /// Walks the agents' directories now, and keeps the result for `discover`.
    async fn discover_fresh(&self) -> Result<Arc<Vec<Discovery>>, Status> {
        let discover_all = self.discover_all.clone();
        let found = tokio::task::spawn_blocking(move || discover_all())
            .await
            .map_err(|err| Status::internal(err.to_string()))?;
        let found = Arc::new(found);
        *lock(&self.discovery) = Some((Instant::now(), found.clone()));
        Ok(found)
    }

    /// Picks the requested lines. Runs on a blocking thread: it may read and
    /// index a large file the first time, then only what was appended.
    fn select(
        &self,
        agent: AgentKind,
        file: &BundleFile,
        request: &FetchRequest,
        kinds: &[Kind],
    ) -> Result<FetchResponse, Status> {
        let readable = if file.rel.ends_with(".zst") {
            self.decompressed(file).map_err(io_status)?
        } else {
            file.path.clone()
        };
        let path = readable.display().to_string();
        if request.whole {
            let size = fs::metadata(&readable).map_err(io_status)?.len();
            return Ok(FetchResponse {
                path,
                size,
                version: "local".to_string(),
                ..Default::default()
            });
        }
        let mut maps = lock(&self.linemaps);
        let cached = maps.entry(readable.clone()).or_insert_with(|| CachedMap::new(agent));
        cached.refresh(&readable).map_err(io_status)?;
        let map = &cached.map;
        let limit = if request.limit == 0 {
            usize::MAX
        } else {
            request.limit as usize
        };
        let mut spans = Vec::new();
        let mut truncated = false;
        for (index, line) in map.lines.iter().enumerate() {
            let number = index as u64 + 1;
            if !request.lines.is_empty()
                && !request
                    .lines
                    .iter()
                    .any(|r| number >= r.start.max(1) && (r.end == 0 || number <= r.end))
            {
                continue;
            }
            if !kinds.is_empty() && !kinds.contains(&line.kind) {
                continue;
            }
            if spans.len() == limit {
                truncated = true;
                break;
            }
            spans.push(LineSpan {
                line: number,
                offset: line.offset,
                length: line.len,
                kind: line.kind.as_str().to_string(),
                raw_type: map.raw_types[line.raw_type as usize].clone(),
                timestamp_ms: line.ts_ms,
            });
        }
        Ok(FetchResponse {
            path,
            size: map.indexed,
            total_lines: map.lines.len() as u64,
            spans,
            truncated,
            version: "local".to_string(),
            files: Vec::new(),
        })
    }

    /// Codex's own `.jsonl.zst`, decompressed once into the cache. The
    /// original stays as it is.
    fn decompressed(&self, file: &BundleFile) -> io::Result<PathBuf> {
        let dir = self.paths.cache_dir().join("zst");
        fs::create_dir_all(&dir)?;
        let name = file
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("rollout.jsonl.zst");
        let target = dir.join(format!(
            "{}-{}-{}",
            file.size,
            file.mtime_ms,
            name.trim_end_matches(".zst")
        ));
        if target.exists() {
            return Ok(target);
        }
        let tmp = target.with_extension("tmp");
        let mut decoder = zstd::stream::read::Decoder::new(File::open(&file.path)?)?;
        io::copy(&mut decoder, &mut File::create(&tmp)?)?;
        fs::rename(&tmp, &target)?;
        Ok(target)
    }
}

/// A line map plus the last indexed bytes, to notice when a file was rewritten
/// rather than appended to (Claude Code can do that; research doc, risk 2).
struct CachedMap {
    map: LineMap,
    tail: Vec<u8>,
}

impl CachedMap {
    fn new(agent: AgentKind) -> Self {
        Self {
            map: LineMap::new(agent),
            tail: Vec::new(),
        }
    }

    fn refresh(&mut self, path: &Path) -> io::Result<()> {
        let size = fs::metadata(path)?.len();
        let indexed = self.map.indexed;
        if size < indexed || read_range(path, indexed - self.tail.len() as u64, indexed)? != self.tail {
            *self = CachedMap::new(self.map.agent);
        }
        if size > self.map.indexed {
            self.map.update_from_file(path)?;
            let end = self.map.indexed;
            self.tail = read_range(path, end.saturating_sub(64), end)?;
        }
        Ok(())
    }
}

fn read_range(path: &Path, start: u64, end: u64) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0; (end - start) as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

fn wanted_agents(agents: &[i32]) -> Vec<AgentKind> {
    let picked: Vec<AgentKind> = agents
        .iter()
        .filter_map(|a| proto::Agent::try_from(*a).ok())
        .filter_map(AgentKind::from_proto)
        .collect();
    if picked.is_empty() {
        AgentKind::ALL.to_vec()
    } else {
        picked
    }
}

fn summarize(d: &Discovery, now: i64) -> AgentSummary {
    let files = || d.sessions.iter().flat_map(|s| s.files.iter());
    let roots = d.sessions.iter().filter(|s| s.parent.is_none()).count() as u64;
    AgentSummary {
        agent: d.agent.to_proto() as i32,
        home: d.home.display().to_string(),
        found: d.found,
        sessions: roots,
        child_sessions: d.sessions.len() as u64 - roots,
        subagent_files: files().filter(|f| f.role == FileRole::Subagent).count() as u64,
        files: files().count() as u64,
        bytes: files().map(|f| f.size).sum(),
        compressed_files: files().filter(|f| f.rel.ends_with(".jsonl.zst")).count() as u64,
        archived_by_agent: d.sessions.iter().filter(|s| s.archived_by_agent).count() as u64,
        cleanup_within_7d: d
            .sessions
            .iter()
            .filter(|s| s.cleanup_at_ms > 0 && s.cleanup_at_ms <= now + 7 * DAY_MS)
            .count() as u64,
        oldest_activity_ms: d
            .sessions
            .iter()
            .map(|s| s.last_activity_ms)
            .filter(|t| *t > 0)
            .min()
            .unwrap_or(0),
    }
}

fn find_session<'a>(found: &'a [Discovery], target: &SessionRef) -> Result<&'a Session, Status> {
    let agent = proto::Agent::try_from(target.agent)
        .ok()
        .and_then(AgentKind::from_proto);
    let mut hits = found
        .iter()
        .filter(|d| agent.is_none_or(|a| a == d.agent))
        .flat_map(|d| &d.sessions)
        .filter(|s| s.id == target.id);
    let first = hits
        .next()
        .ok_or_else(|| Status::not_found(format!("本机没有会话 {}", target.id)))?;
    if hits.next().is_some() {
        return Err(Status::failed_precondition(format!(
            "{} 对应不止一个会话，请写成 <agent>:<id>",
            target.id
        )));
    }
    Ok(first)
}

fn pick_file<'a>(session: &'a Session, file: &str) -> Result<&'a BundleFile, Status> {
    if file.is_empty() {
        return session
            .files
            .iter()
            .find(|f| f.path == session.main)
            .ok_or_else(|| Status::internal("找不到主记录"));
    }
    // Stored paths use `/`; let a Windows user type `\` as well.
    let file = if cfg!(windows) {
        file.replace('\\', "/")
    } else {
        file.to_string()
    };
    let file = file.as_str();
    let suffix = format!("/{file}");
    let matches: Vec<&BundleFile> = session
        .files
        .iter()
        .filter(|f| f.rel == file || f.rel.ends_with(&suffix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(Status::not_found(format!("会话包里没有 {file}"))),
        _ => Err(Status::failed_precondition(format!(
            "{file} 对应不止一个文件，请写完整的相对路径"
        ))),
    }
}

fn session_ref(agent: AgentKind, id: &str) -> SessionRef {
    SessionRef {
        agent: agent.to_proto() as i32,
        id: id.to_string(),
    }
}

fn not_yet(what: &str) -> Status {
    Status::unimplemented(format!("{what} 还在开发"))
}

fn io_status(err: io::Error) -> Status {
    if err.kind() == io::ErrorKind::NotFound {
        Status::not_found(err.to_string())
    } else {
        Status::internal(err.to_string())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdav::Server;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    struct Fixture {
        service: CoreService,
        claude: tempfile::TempDir,
        _home: tempfile::TempDir,
    }

    impl Fixture {
        /// A core over a synthetic Claude Code home with one session, and the given config.
        fn new(config: &str) -> Self {
            let claude = tempfile::tempdir().unwrap();
            let dir = claude.path().join("projects/-synthetic");
            fs::create_dir_all(&dir).unwrap();
            let lines = [
                r#"{"type":"user","timestamp":"2026-11-01T10:00:00.000Z","cwd":"/synthetic","version":"9.9.9","message":{"content":"synthetic question"}}"#,
                r#"{"type":"assistant","timestamp":"2026-11-01T10:00:01.000Z","message":{"content":[{"type":"text","text":"synthetic answer"}]}}"#,
            ];
            fs::write(dir.join(format!("{ID}.jsonl")), lines.join("\n") + "\n").unwrap();
            let home = tempfile::tempdir().unwrap();
            let paths = Paths {
                home: home.path().to_path_buf(),
            };
            let device = Device {
                id: "dev-svc".to_string(),
                name: "synthetic".to_string(),
                created_at_ms: 1,
            };
            let (stop, _) = watch::channel(false);
            let root = claude.path().to_path_buf();
            let service = CoreService::with_discovery(
                paths,
                device,
                toml::from_str(config).unwrap(),
                stop,
                Arc::new(move || vec![crate::agents::claude_code::discover_in(root.clone())]),
            );
            Fixture {
                service,
                claude,
                _home: home,
            }
        }

        async fn archive(&self, sessions: &[&str], storage: &str) -> Result<Task, Status> {
            let sessions = sessions
                .iter()
                .map(|id| SessionRef {
                    agent: proto::Agent::Unspecified as i32,
                    id: id.to_string(),
                })
                .collect();
            let request = ArchiveRequest {
                sessions,
                storage: storage.to_string(),
                digest_ids: Vec::new(),
            };
            let mut task = self.service.archive(Request::new(request)).await?.into_inner();
            for _ in 0..500 {
                if !tasks::is_active(&task) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                task = self
                    .service
                    .get_task(Request::new(TaskRequest { id: task.id.clone() }))
                    .await?
                    .into_inner();
            }
            Ok(task)
        }
    }

    fn config_for(server: &Server) -> String {
        format!(
            "[storage.test]\nkind = \"webdav\"\nurl = \"http://127.0.0.1:{}\"\nresolve = \"127.0.0.1\"\n",
            server.port()
        )
    }

    #[tokio::test]
    async fn archive_runs_as_a_task_and_reports_per_session() {
        let server = Server::start(None).await;
        let fixture = Fixture::new(&config_for(&server));

        let started = fixture
            .service
            .archive(Request::new(ArchiveRequest {
                sessions: vec![SessionRef {
                    agent: proto::Agent::ClaudeCode as i32,
                    id: ID.to_string(),
                }],
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((started.kind.as_str(), started.sessions.len()), ("archive", 1));
        assert!(tasks::is_active(&started) || started.state() == TaskState::Committed);

        let task = fixture.archive(&[ID], "").await.unwrap(); // a bare id works, and it is the second run
        assert_eq!(task.state(), TaskState::Committed, "{}", task.error);
        // Wait for the first task too; both are done, one uploaded and one found nothing new.
        let mut all = Vec::new();
        for _ in 0..200 {
            all = fixture
                .service
                .list_tasks(Request::new(ListTasksRequest {
                    active_only: false,
                    limit: 10,
                }))
                .await
                .unwrap()
                .into_inner()
                .tasks;
            if all.iter().all(|t| !tasks::is_active(t)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|t| t.state() == TaskState::Committed));
        let first = all.iter().find(|t| t.id == started.id).unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&first.result_json).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["session"], format!("claude-code:{ID}"));
        assert_eq!(rows[0]["manifest"], "dev-svc-1");
        assert_eq!(rows[0]["up_to_date"], false);
        assert_eq!(
            (rows[0]["files"].as_u64(), rows[0]["new_lines"].as_u64()),
            (Some(1), Some(2))
        );
        assert!(first.total_bytes > 0 && first.done_bytes == first.total_bytes);
        assert!(first.summary.contains("上传 1 个会话"), "{}", first.summary);

        let rows: Vec<serde_json::Value> = serde_json::from_str(&task.result_json).unwrap();
        assert_eq!(rows[0]["up_to_date"], true);
        assert!(task.summary.contains("1 个已是最新"), "{}", task.summary);
        assert!(
            server
                .root()
                .join(format!(
                    "nekorisu/v1/sessions/claude-code/{ID}/manifests/dev-svc-1.json"
                ))
                .is_file()
        );
        drop(fixture.claude);
    }

    #[tokio::test]
    async fn archive_says_clearly_what_is_wrong_before_starting() {
        let server = Server::start(None).await;
        let none = Fixture::new("");
        let err = none.archive(&[ID], "").await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("nekorisu storage add"), "{}", err.message());

        let two = Fixture::new(&format!(
            "{}[storage.other]\nkind = \"webdav\"\nurl = \"http://127.0.0.1:1\"\n",
            config_for(&server)
        ));
        let err = two.archive(&[ID], "").await.unwrap_err();
        assert!(err.message().contains("--storage"), "{}", err.message());
        let err = two.archive(&[ID], "nope").await.unwrap_err();
        assert!(err.message().contains("有：other、test"), "{}", err.message());

        let err = two.archive(&["no-such-session"], "test").await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        let err = two.archive(&[], "test").await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        let err = two
            .service
            .archive(Request::new(ArchiveRequest {
                sessions: vec![SessionRef {
                    agent: proto::Agent::ClaudeCode as i32,
                    id: ID.to_string(),
                }],
                storage: "test".into(),
                digest_ids: vec!["d1".into()],
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);

        // A user name without a password: say how to set one, and do not start a task.
        let name = format!("synthetic-{}", uuid::Uuid::new_v4().simple());
        let authed = Fixture::new(&format!(
            "[storage.{name}]\nkind = \"webdav\"\nurl = \"http://127.0.0.1:1\"\nusername = \"me\"\n"
        ));
        let err = authed.archive(&[ID], "").await.unwrap_err();
        assert!(
            err.message().contains(&format!("nekorisu storage login {name}")),
            "{}",
            err.message()
        );
        assert!(authed.service.inner.tasks.list(false, 10).is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_storage_abandons_the_task_with_the_reason() {
        // Nothing listens on port 1.
        let fixture =
            Fixture::new("[storage.down]\nkind = \"webdav\"\nurl = \"http://127.0.0.1:1\"\nresolve = \"127.0.0.1\"\n");
        let task = fixture.archive(&[ID], "").await.unwrap();
        assert_eq!(task.state(), TaskState::Abandoned);
        assert!(task.error.contains("连不上存储"), "{}", task.error);
        assert!(task.summary.contains("1 个没传成功"), "{}", task.summary);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&task.result_json).unwrap();
        assert!(rows[0]["error"].as_str().is_some());
        // The task is over, so core counts as idle again (the updater waits for that).
        assert!((fixture.service.idle_probe())());
    }

    #[tokio::test]
    async fn a_cancelled_task_stays_abandoned() {
        let server = Server::start(None).await;
        let fixture = Fixture::new(&config_for(&server));
        // Cancelled before it gets its turn: the upload never starts.
        let task = fixture
            .service
            .archive(Request::new(ArchiveRequest {
                sessions: vec![SessionRef {
                    agent: proto::Agent::ClaudeCode as i32,
                    id: ID.to_string(),
                }],
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        let cancelled = fixture
            .service
            .cancel_task(Request::new(TaskRequest { id: task.id.clone() }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(cancelled.state(), TaskState::Abandoned);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let after = fixture
            .service
            .get_task(Request::new(TaskRequest { id: task.id }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            (after.state(), after.error.as_str()),
            (TaskState::Abandoned, "用户取消")
        );
        assert!(server.puts().is_empty());
    }
}
