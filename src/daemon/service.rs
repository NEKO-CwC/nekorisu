//! The gRPC service. Reads (scan, status, fetch) work on the local originals, and fetch also
//! reads sessions of other devices from the storage; archive uploads to the configured WebDAV
//! storage and restore puts a stored session back, both as tasks. Search over uploads, doctor,
//! log and digest are still to do and answer "unimplemented".

use std::collections::{HashMap, HashSet};
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
use crate::cloud;
use crate::config::{self, Config, PathMap};
use crate::linemap::{Kind, LineMap};
use crate::pathmap::{self, Origin};
use crate::paths::{self, Paths};
use crate::proto::core_server::Core;
use crate::proto::{self, *};
use crate::remote::{self, LineQuery, Remote, Wanted};
use crate::restore;
use crate::settings;
use crate::storage::{self, StorageError, Webdav};
use crate::sync;
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
    /// The config in use: config.toml with the trusted source's values merged in (`settings`).
    config: RwLock<Config>,
    /// config.toml as written, to tell this device's own values from defaults.
    table: RwLock<toml::Table>,
    /// Finds every agent's sessions; tests point it at temporary directories.
    discover_all: Arc<dyn Fn() -> Vec<Discovery> + Send + Sync>,
    discovery: Mutex<Option<(Instant, Arc<Vec<Discovery>>)>>,
    linemaps: Mutex<HashMap<PathBuf, CachedMap>>,
    tasks: Registry,
    /// One archive at a time: two uploads of the same session would race for the same manifest number.
    archive_gate: tokio::sync::Mutex<()>,
    /// What each session last matched in the storage.
    records: sync::Store,
    deltas: Mutex<sync::DeltaCache>,
    /// Reads from the storage share the local caches of lines; one at a time.
    cache_gate: tokio::sync::Mutex<()>,
    places: Places,
    /// The trusted source's config in use, and what path rules each side had last time.
    source_side: Mutex<SourceSide>,
    /// One pull from the source at a time.
    pull_gate: tokio::sync::Mutex<()>,
    /// Wakes the loop that pulls from the source (`follow_source`).
    source_wake: tokio::sync::Notify,
}

#[derive(Default)]
struct SourceSide {
    /// What `held` and `memory` were read for; another source starts over.
    source: Option<cloud::Source>,
    loaded: bool,
    held: Option<cloud::Held>,
    /// Path rules each side had last time.
    memory: pathmap::Memory,
    /// The same for the other values.
    values: pathmap::Memory,
}

/// Where restore puts sessions; tests point it at temporary directories.
#[derive(Clone)]
pub struct Places {
    pub home: PathBuf,
    pub claude_home: PathBuf,
    pub codex_home: PathBuf,
    /// `None`: look for the codex program when it is needed.
    pub codex_exe: Option<PathBuf>,
}

impl Places {
    fn from_env() -> Places {
        Places {
            home: paths::home_dir(),
            claude_home: paths::claude_home(),
            codex_home: paths::codex_home(),
            codex_exe: None,
        }
    }
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
        Self::with_discovery(paths, device, config, stop, Arc::new(discover_all), Places::from_env())
    }

    fn with_discovery(
        paths: Paths,
        device: Device,
        config: Config,
        stop: watch::Sender<bool>,
        discover_all: Arc<dyn Fn() -> Vec<Discovery> + Send + Sync>,
        places: Places,
    ) -> Self {
        let table = config::load_table(&paths).unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                table: RwLock::new(table),
                records: sync::Store::new(paths.sync_dir()),
                deltas: Mutex::new(sync::DeltaCache::new()),
                cache_gate: tokio::sync::Mutex::new(()),
                places,
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
                source_side: Mutex::new(SourceSide::default()),
                pull_gate: tokio::sync::Mutex::new(()),
                source_wake: tokio::sync::Notify::new(),
            }),
        }
    }

    /// Pulls from the trusted source a few seconds after start, then every poll interval (the
    /// config says how long), and whenever woken.
    pub fn follow_source(&self) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            // What this device has, on record before anything new arrives; the source's values
            // it already holds go into the config in use.
            inner.look();
            if let Some(held) = lock(&inner.source_side).held.clone() {
                inner.apply_revokes(&held);
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            loop {
                let wait = match inner.sync_source().await {
                    Ok(None) => cloud::DEFAULT_POLL, // no source; `source add` wakes this
                    Ok(Some(_)) => inner.poll_interval(),
                    Err(err) => {
                        eprintln!("{} 没能从来源取配置：{err:#}", now_ms());
                        inner.poll_interval().min(Duration::from_secs(15 * 60))
                    }
                };
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = inner.source_wake.notified() => {}
                }
            }
        });
    }
}

impl Inner {
    /// Reads the source and what it sent, again when the source changed.
    fn source_side(&self) -> MutexGuard<'_, SourceSide> {
        let mut side = lock(&self.source_side);
        let source = cloud::Source::load(&self.paths).unwrap_or_else(|err| {
            eprintln!("{} {err:#}", now_ms());
            None
        });
        if !side.loaded || side.source != source {
            let read = |file: PathBuf| -> pathmap::Memory {
                fs::read(file)
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default()
            };
            side.held = source.as_ref().and_then(|s| cloud::load_held(&self.paths, s));
            side.memory = read(cloud::paths_memory_file(&self.paths));
            side.values = read(cloud::values_memory_file(&self.paths));
            side.source = source;
            side.loaded = true;
        }
        side
    }

    /// Writes what each side said, only with a source: without one there is nothing to tell apart.
    fn remember(&self, side: &SourceSide, memory: &pathmap::Memory, file: PathBuf) {
        if side.source.is_some()
            && let Err(err) = serde_json::to_vec_pretty(memory)
                .map_err(io::Error::other)
                .and_then(|b| remote::write_atomic(&file, &b))
        {
            eprintln!("{} 记不下 {} ：{err}", now_ms(), file.display());
        }
    }

    /// The path rules as this device applies them: its own and the source's (`pathmap::merge`).
    /// Remembers what each side said, so the next look can tell which side changed.
    fn applied_paths(&self, side: &mut SourceSide) -> Vec<pathmap::Applied> {
        let local = self.config.read().unwrap_or_else(|e| e.into_inner()).paths.map.clone();
        let theirs = side.held.as_ref().map(|h| h.path_rules().to_vec()).unwrap_or_default();
        let (applied, memory) = pathmap::merge(&local, &theirs, &self.device.id, &side.memory);
        if memory != side.memory {
            self.remember(side, &memory, cloud::paths_memory_file(&self.paths));
            side.memory = memory;
        }
        applied
    }

    /// Puts the source's values into the config in use where they apply (`settings::merge`),
    /// starting again from config.toml.
    fn applied_values(&self, side: &mut SourceSide) -> Vec<settings::Applied> {
        let table = self.table.read().unwrap_or_else(|e| e.into_inner()).clone();
        let theirs = side.held.as_ref().map(|h| h.config.values()).unwrap_or_default();
        let (applied, memory) = settings::merge(
            &settings::local_values(&table),
            &settings::locked(&table),
            &theirs,
            &side.values,
        );
        if memory != side.values {
            self.remember(side, &memory, cloud::values_memory_file(&self.paths));
            side.values = memory;
        }
        if let Ok(mut config) = toml::Value::Table(table).try_into::<Config>() {
            settings::apply(&mut config, &applied);
            *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
        }
        applied
    }

    /// Reads config.toml again and puts the source's values in.
    fn reload(&self) -> anyhow::Result<Config> {
        let table = config::load_table(&self.paths)?;
        let config: Config = toml::Value::Table(table.clone())
            .try_into()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        *self.table.write().unwrap_or_else(|e| e.into_inner()) = table;
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
        let mut side = self.source_side();
        self.applied_values(&mut side);
        self.applied_paths(&mut side);
        toml::Value::Table(self.table.read().unwrap_or_else(|e| e.into_inner()).clone())
            .try_into()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// The rules restore maps working directories with.
    fn path_rules(&self) -> Vec<PathMap> {
        let mut side = self.source_side();
        pathmap::rules(&self.applied_paths(&mut side))
    }

    /// Records every look: values and path rules.
    fn look(&self) {
        let mut side = self.source_side();
        self.applied_values(&mut side);
        self.applied_paths(&mut side);
    }

    fn poll_interval(&self) -> Duration {
        lock(&self.source_side)
            .held
            .as_ref()
            .map_or(cloud::DEFAULT_POLL, |h| h.config.poll())
    }

    /// A revoke turns the switch off at once; it needs no one's yes.
    fn apply_revokes(&self, held: &cloud::Held) {
        let handled = cloud::Handled::load(&self.paths);
        for request in held.config.exec_for(&self.device.id) {
            if request.action == cloud::ExecAction::Revoke && handled.state(&request.id).is_none() {
                let why = format!("来源收回了（请求 {}）", request.id);
                if let Err(err) = cloud::Exec::off(&self.paths, &why)
                    .and_then(|_| cloud::Handled::record(&self.paths, &request.id, cloud::Answer::Accepted))
                {
                    eprintln!("{} 关不掉执行开关：{err:#}", now_ms());
                } else {
                    eprintln!("{} 来源收回了运行脚本的许可（请求 {}）", now_ms(), request.id);
                }
            }
        }
    }

    /// What this device tells its source about itself.
    fn own_report(&self) -> cloud::DeviceReport {
        let table = self.table.read().unwrap_or_else(|e| e.into_inner()).clone();
        cloud::DeviceReport {
            paths: self.config.read().unwrap_or_else(|e| e.into_inner()).paths.map.clone(),
            values: settings::report_values(&table),
        }
    }

    /// Asks the source for a newer config now, then tells it this device's state. `None` when
    /// no source is set.
    async fn sync_source(&self) -> anyhow::Result<Option<cloud::Outcome>> {
        let _one = self.pull_gate.lock().await;
        let source = {
            // On record before the pull, so what arrives now counts as the source's change.
            self.look();
            lock(&self.source_side).source.clone()
        };
        let Some(source) = source else { return Ok(None) };
        let token = source.token(&self.paths).await.unwrap_or_else(|err| {
            eprintln!("{} 读不了来源的令牌：{err:#}", now_ms());
            None
        });
        let device = cloud::Device { id: &self.device.id };
        let result = cloud::pull(&self.paths, &source, &device, token.as_deref()).await;
        let held = cloud::load_held(&self.paths, &source);
        if let Some(held) = &held {
            self.apply_revokes(held);
        }
        lock(&self.source_side).held = held;
        self.look();
        if let Ok(cloud::Outcome::Updated(serial)) = &result {
            eprintln!("{} 从来源 {} 拿到第 {serial} 版配置", now_ms(), source.name);
        }
        if let Some(token) = token.as_deref()
            && let Err(err) = cloud::report(&self.paths, &source, &device, token, &self.own_report()).await
        {
            eprintln!("{} 没能向来源报告状态：{err:#}", now_ms());
        }
        result.map(Some)
    }

    /// The source's requests aimed at this device, with what was done with each.
    fn requests(&self, held: &cloud::Held) -> Vec<SourceRequest> {
        let handled = cloud::Handled::load(&self.paths);
        let state = |id: &str| match handled.state(id) {
            Some(cloud::Answer::Accepted) => "accepted",
            Some(cloud::Answer::Rejected) => "rejected",
            None => "pending",
        };
        let force = held
            .config
            .force_for(&self.device.id)
            .into_iter()
            .map(|f| SourceRequest {
                id: f.id.clone(),
                kind: "force".into(),
                state: state(&f.id).into(),
                changes: f
                    .changes
                    .iter()
                    .map(|c| SourceChange {
                        key: c.key.clone(),
                        value_json: c.value_json.clone(),
                        known: settings::can_force(&c.key),
                    })
                    .collect(),
                action: String::new(),
            });
        let exec = held
            .config
            .exec_for(&self.device.id)
            .into_iter()
            .map(|r| SourceRequest {
                id: r.id.clone(),
                kind: "exec".into(),
                state: state(&r.id).into(),
                changes: Vec::new(),
                action: match r.action {
                    cloud::ExecAction::Allow => "allow",
                    cloud::ExecAction::Revoke => "revoke",
                    cloud::ExecAction::Unknown => "",
                }
                .into(),
            });
        force.chain(exec).collect()
    }

    /// Accepts or rejects a request. Accepting a force request writes its values into
    /// config.toml as this device's own and unlocks them. Only after the user confirmed in a
    /// terminal (the CLI checks).
    async fn answer(&self, id: &str, accept: bool) -> Result<(), Status> {
        let held = self
            .source_side()
            .held
            .clone()
            .ok_or_else(|| Status::failed_precondition("还没有来源下发的配置"))?;
        let request = self
            .requests(&held)
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| Status::not_found(format!("在用的配置里没有给这台设备的请求 {id}")))?;
        if request.state != "pending" {
            return Err(Status::failed_precondition(format!(
                "请求 {id} 已经处理过了（{}）",
                request.state
            )));
        }
        if accept && request.kind == "exec" {
            return Err(Status::invalid_argument(match request.action.as_str() {
                "allow" => "打开运行脚本要用 nekorisu source allow-exec",
                _ => "收回的请求不用确认，已经生效",
            }));
        }
        if accept {
            if let Some(unknown) = request.changes.iter().find(|c| !c.known) {
                return Err(Status::failed_precondition(format!(
                    "这个版本不会改 {}；可以先 nekorisu source reject {id}，或者更新以后再 accept",
                    unknown.key
                )));
            }
            config::edit(&self.paths, |table| {
                for change in &request.changes {
                    settings::force(table, &change.key, &change.value_json)?;
                }
                Ok(())
            })
            .map_err(|err| Status::failed_precondition(format!("{err:#}")))?;
            self.reload()
                .map_err(|err| Status::internal(format!("config.toml 改了，但读不回来：{err:#}")))?;
        }
        let answer = if accept {
            cloud::Answer::Accepted
        } else {
            cloud::Answer::Rejected
        };
        cloud::Handled::record(&self.paths, id, answer).map_err(|err| Status::internal(format!("{err:#}")))?;
        eprintln!("{} 来源的请求 {id}：{}", now_ms(), if accept { "接受" } else { "拒绝" });
        self.source_wake.notify_one();
        Ok(())
    }

    /// Turns the switch on (with the request it answers, if any) or off.
    async fn set_exec(&self, allowed: bool, request_id: &str) -> Result<(), Status> {
        let fail = |err: anyhow::Error| Status::internal(format!("{err:#}"));
        if !allowed {
            cloud::Exec::off(&self.paths, "在这台设备上关掉").map_err(fail)?;
        } else if request_id.is_empty() {
            cloud::Exec::on(&self.paths, "在这台设备上打开").map_err(fail)?;
        } else {
            let held = self
                .source_side()
                .held
                .clone()
                .ok_or_else(|| Status::failed_precondition("还没有来源下发的配置"))?;
            let ok = self
                .requests(&held)
                .iter()
                .any(|r| r.id == request_id && r.kind == "exec" && r.action == "allow" && r.state == "pending");
            if !ok {
                return Err(Status::not_found(format!(
                    "没有给这台设备的、待确认的放行请求 {request_id}"
                )));
            }
            cloud::Exec::on(&self.paths, &format!("回应来源的请求 {request_id}")).map_err(fail)?;
            cloud::Handled::record(&self.paths, request_id, cloud::Answer::Accepted).map_err(fail)?;
        }
        self.source_wake.notify_one();
        Ok(())
    }

    async fn source_state(&self, outcome: &str, quick: bool) -> SourceState {
        let (source, held, paths, values) = {
            let mut side = self.source_side();
            let values = self.applied_values(&mut side);
            let paths = self.applied_paths(&mut side);
            (side.source.clone(), side.held.clone(), paths, values)
        };
        let origin = |o: Origin| match o {
            Origin::Local => "local",
            Origin::Source => "source",
        };
        let exec = cloud::Exec::load(&self.paths);
        let mut state = SourceState {
            exec_allowed: exec.allowed,
            exec_why: exec.why,
            device_name: self.device.name.clone(),
            paths: paths
                .into_iter()
                .map(|a| AppliedPathMap {
                    from: a.from,
                    to: a.to,
                    origin: origin(a.origin).to_string(),
                    locked: a.locked,
                    has_other: a.other.is_some(),
                    other: a.other.unwrap_or_default(),
                })
                .collect(),
            values: values
                .into_iter()
                .map(|a| AppliedValue {
                    key: a.key,
                    value_json: a.value.to_string(),
                    origin: origin(a.origin).to_string(),
                    locked: a.locked,
                    has_other: a.other.is_some(),
                    other_json: a.other.map(|v| v.to_string()).unwrap_or_default(),
                })
                .collect(),
            ..SourceState::default()
        };
        let Some(source) = source else {
            return state;
        };
        let status = cloud::Status::load(&self.paths);
        state.configured = true;
        state.name = source.name.clone();
        state.url = source.url.clone();
        state.key_ids = source.key_ids();
        state.token_set = !quick && source.token(&self.paths).await.ok().flatten().is_some();
        state.applied_serial = held.as_ref().map_or(0, |h| h.serial);
        state.applied_issued_at = held.as_ref().map(|h| h.config.issued_at.clone()).unwrap_or_default();
        state.checked_ms = status.checked_ms;
        state.error = status.error;
        state.outcome = outcome.to_string();
        state.requests = held.as_ref().map(|h| self.requests(h)).unwrap_or_default();
        state.min_client_version = held.and_then(|h| h.config.too_old()).unwrap_or_default();
        state
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
        let config = self
            .inner
            .reload()
            .map_err(|err| Status::invalid_argument(format!("config.toml 有问题：{err:#}")))?;
        let response = ReloadConfigResponse {
            config_version: config.version,
            source: config.source.clone().unwrap_or_else(|| "local".to_string()),
        };
        // The source hears about this device's own values and path rules on the next pull; make it now.
        self.inner.source_wake.notify_one();
        Ok(Response::new(response))
    }

    async fn sync_source(&self, _request: Request<SyncSourceRequest>) -> Result<Response<SourceState>, Status> {
        let (outcome, error) = match self.inner.sync_source().await {
            Ok(None) => ("", None),
            Ok(Some(cloud::Outcome::Updated(_))) => ("updated", None),
            Ok(Some(cloud::Outcome::Unchanged)) => ("unchanged", None),
            Err(err) => ("failed", Some(format!("{err:#}"))),
        };
        let mut state = self.inner.source_state(outcome, false).await;
        if let Some(error) = error {
            state.error = error;
        }
        Ok(Response::new(state))
    }

    async fn get_source_state(&self, request: Request<GetSourceStateRequest>) -> Result<Response<SourceState>, Status> {
        Ok(Response::new(
            self.inner.source_state("", request.get_ref().quick).await,
        ))
    }

    async fn answer_source_request(
        &self,
        request: Request<AnswerSourceRequestRequest>,
    ) -> Result<Response<SourceState>, Status> {
        let request = request.into_inner();
        self.inner.answer(&request.id, request.accept).await?;
        Ok(Response::new(self.inner.source_state("", true).await))
    }

    async fn set_exec(&self, request: Request<SetExecRequest>) -> Result<Response<SourceState>, Status> {
        let request = request.into_inner();
        self.inner.set_exec(request.allowed, &request.request_id).await?;
        Ok(Response::new(self.inner.source_state("", true).await))
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
        let picked: Vec<Session> = rows.into_iter().take(limit).cloned().collect();
        let counts: Vec<u32> = picked
            .iter()
            .map(|s| children.get(&(s.agent, s.id.as_str())).copied().unwrap_or(0))
            .collect();
        let inner = self.inner.clone();
        let sessions = tokio::task::spawn_blocking(move || {
            picked
                .iter()
                .zip(counts)
                .map(|(s, children)| SessionStatus {
                    session: Some(session_ref(s.agent, &s.id)),
                    parent: s.parent.as_deref().map(|p| session_ref(s.agent, p)),
                    project: s.project.clone(),
                    last_activity_ms: s.last_activity_ms,
                    bytes: s.bytes(),
                    files: s.files.len() as u32,
                    children,
                    upload: Some(inner.upload_state(s)),
                    cleanup_at_ms: s.cleanup_at_ms,
                    compressed: s.compressed,
                    archived_by_agent: s.archived_by_agent,
                })
                .collect()
        })
        .await
        .map_err(|err| Status::internal(err.to_string()))?;
        Ok(Response::new(StatusResponse { sessions }))
    }

    async fn search(&self, _request: Request<SearchRequest>) -> Result<Response<SearchResponse>, Status> {
        Err(not_yet("search"))
    }

    async fn fetch(&self, request: Request<FetchRequest>) -> Result<Response<FetchResponse>, Status> {
        let request = request.into_inner();
        let target = request
            .session
            .clone()
            .ok_or_else(|| Status::invalid_argument("缺少 session"))?;
        let kinds = request
            .kinds
            .iter()
            .map(|k| Kind::parse(k).ok_or_else(|| Status::invalid_argument(format!("不认识的种类：{k}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let local_only = request.version == "local";
        if !(request.version.is_empty() || local_only) {
            return self
                .inner
                .fetch_remote(request, &kinds, &target)
                .await
                .map(Response::new);
        }
        let found = self.inner.discover().await?;
        let session = match find_session(&found, &target) {
            Ok(session) => session,
            // Not on this machine: the newest upload, from whichever device (if there is a storage).
            Err(err) if !local_only && err.code() == tonic::Code::NotFound && self.inner.has_storage() => {
                return self
                    .inner
                    .fetch_remote(request, &kinds, &target)
                    .await
                    .map(Response::new);
            }
            Err(err) => return Err(err),
        };
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

    async fn versions(&self, request: Request<VersionsRequest>) -> Result<Response<VersionsResponse>, Status> {
        let request = request.into_inner();
        let target = request
            .session
            .ok_or_else(|| Status::invalid_argument("缺少 session"))?;
        let (_, remote) = self.inner.remote(&request.storage).await?;
        let (agent, ids) = remote
            .find(&target.id, agent_of(&target))
            .await
            .map_err(remote_status)?;
        let all = remote.manifests(agent, &target.id, &ids).await.map_err(remote_status)?;
        let devices = remote.devices().await;
        let heads: HashSet<&str> = remote::heads(&all).iter().map(|m| m.id.as_str()).collect();
        let versions = all
            .iter()
            .rev()
            .map(|m| Version {
                id: m.id.clone(),
                parent: m.parent.clone().unwrap_or_default(),
                device: m.device.clone(),
                device_name: devices.get(&m.device).map(|d| d.name.clone()).unwrap_or_default(),
                created_at_ms: m.created_at_ms,
                last_activity_ms: m.session.last_ts_ms,
                bytes: m.files.iter().map(|f| f.size).sum(),
                lines: remote::main_entry(m).map_or(0, |e| e.lines),
                head: heads.contains(m.id.as_str()),
            })
            .collect();
        Ok(Response::new(VersionsResponse {
            session: Some(session_ref(agent, &target.id)),
            versions,
        }))
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

    async fn restore(&self, request: Request<RestoreRequest>) -> Result<Response<Task>, Status> {
        let request = request.into_inner();
        let target = request
            .session
            .ok_or_else(|| Status::invalid_argument("缺少 session"))?;
        // Everything that can be said before starting is said now: no such session, a fork.
        let (name, remote) = self.inner.remote(&request.storage).await?;
        let (agent, ids) = remote
            .find(&target.id, agent_of(&target))
            .await
            .map_err(remote_status)?;
        let all = remote.manifests(agent, &target.id, &ids).await.map_err(remote_status)?;
        let devices = remote.devices().await;
        let wanted = match request.version.as_str() {
            "" | "latest" => Wanted::Latest,
            id => Wanted::Id(id),
        };
        let chosen = remote::choose(&all, wanted, &devices)
            .map_err(|e| Status::failed_precondition(e.0))?
            .clone();
        let task = self.inner.tasks.create(
            "restore",
            vec![session_ref(agent, &target.id)],
            format!(
                "从存储 {name} 恢复 {}（版本 {}）",
                archive::session_key(agent, &target.id),
                chosen.id
            ),
        );
        let (inner, id) = (self.inner.clone(), task.id.clone());
        tokio::spawn(async move {
            inner
                .run_restore(id, name, remote, agent, chosen, devices, request.replace)
                .await
        });
        Ok(Response::new(task))
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
        let ctx = archive::SyncCtx {
            records: &self.records,
            storage: &storage_name,
        };
        let outcome = archive::run(&store, &self.device, Some(&ctx), jobs, &on).await;

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

    /// Upload state from this machine's record: what was uploaded (or restored) last, and
    /// what the local files have beyond it. Blocking.
    fn upload_state(&self, session: &Session) -> UploadState {
        let Some(record) = self.records.load(session.agent, &session.id) else {
            return UploadState {
                new_bytes: session.bytes(),
                ..Default::default()
            };
        };
        let delta = sync::session_delta(session, &record, &mut lock(&self.deltas));
        UploadState {
            uploaded: true,
            last_upload_ms: record.manifest_at_ms,
            new_bytes: delta.new_bytes,
            new_lines: delta.new_lines,
            rewritten: delta.rewritten,
        }
    }

    fn has_storage(&self) -> bool {
        !self.config.read().unwrap_or_else(|e| e.into_inner()).storage.is_empty()
    }

    /// The storage a read or a restore uses, connected.
    async fn remote(&self, wanted: &str) -> Result<(String, Remote), Status> {
        let (name, storage_config) = {
            let config = self.config.read().unwrap_or_else(|e| e.into_inner());
            let (name, storage_config) =
                storage::pick(&config, wanted).map_err(|err| Status::failed_precondition(err.to_string()))?;
            (name.to_string(), storage_config.clone())
        };
        let store = storage::connect(&self.paths, &name, &storage_config)
            .await
            .map_err(|err| Status::failed_precondition(format!("{err:#}")))?;
        Ok((name, Remote::new(Arc::new(store), self.paths.clone())))
    }

    /// Lines of a session as a version in the storage has them.
    async fn fetch_remote(
        &self,
        request: FetchRequest,
        kinds: &[Kind],
        target: &SessionRef,
    ) -> Result<FetchResponse, Status> {
        let (_, remote) = self.remote(&request.storage).await?;
        let (agent, ids) = remote.find(&target.id, agent_of(target)).await.map_err(remote_status)?;
        let all = remote.manifests(agent, &target.id, &ids).await.map_err(remote_status)?;
        let wanted = match request.version.as_str() {
            "" | "latest" => Wanted::Latest,
            id => Wanted::Id(id),
        };
        let devices = if matches!(wanted, Wanted::Latest) && remote::heads(&all).len() > 1 {
            remote.devices().await
        } else {
            HashMap::new()
        };
        let manifest = remote::choose(&all, wanted, &devices).map_err(|e| Status::failed_precondition(e.0))?;
        let entry = remote::pick_entry(manifest, &request.file).map_err(Status::not_found)?;
        let dir = archive::session_dir(agent.name(), &target.id);
        let files = manifest
            .files
            .iter()
            .filter(|f| matches!(f.role.as_str(), "transcript" | "subagent"))
            .map(|f| f.path.clone())
            .collect();
        let full = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .local
            .full_copies();
        let _gate = self.cache_gate.lock().await;
        if request.whole {
            let path = remote.whole_file(&dir, entry).await.map_err(remote_status)?;
            return Ok(FetchResponse {
                path: path.display().to_string(),
                size: entry.size,
                total_lines: entry.lines,
                version: manifest.id.clone(),
                files,
                ..Default::default()
            });
        }
        let query = LineQuery {
            ranges: request.lines.iter().map(|r| (r.start, r.end)).collect(),
            kinds,
            limit: request.limit as usize,
        };
        let selected = remote.select(&dir, entry, &query, full).await.map_err(remote_status)?;
        Ok(FetchResponse {
            path: selected.path.display().to_string(),
            size: entry.size,
            total_lines: entry.lines,
            spans: selected.spans,
            truncated: selected.truncated,
            version: manifest.id.clone(),
            files,
        })
    }

    /// The restore task: planned -> running (downloading and checking) -> pending commit (putting
    /// files in place) -> committed, or abandoned with the reason.
    #[allow(clippy::too_many_arguments)]
    async fn run_restore(
        self: Arc<Self>,
        id: String,
        storage_name: String,
        remote: Remote,
        agent: AgentKind,
        chosen: archive::Manifest,
        devices: HashMap<String, remote::DeviceInfo>,
        replace: bool,
    ) {
        // Writes go one at a time: an upload and a restore of the same session would race.
        let _gate = self.archive_gate.lock().await;
        if self.tasks.advance(&id, TaskState::Running, None).is_err() {
            return;
        }
        let alive = |id: &str| self.tasks.get(id).is_some_and(|t| tasks::is_active(&t));
        let on = |event: restore::Event| {
            match event {
                restore::Event::Planned { total_bytes } => self.tasks.update(&id, |t| t.total_bytes = total_bytes),
                restore::Event::Downloaded { bytes } => self.tasks.update(&id, |t| t.done_bytes += bytes),
                restore::Event::Placing => {
                    let _ = self.tasks.advance(&id, TaskState::PendingCommit, None);
                }
            }
            alive(&id)
        };
        let result = async {
            let sessions = restore::tree(&remote, agent, chosen, &devices).await?;
            let found = self
                .discover_fresh()
                .await
                .map_err(|s| anyhow::anyhow!(s.message().to_string()))?;
            let rules = self.path_rules();
            let env = restore::Env {
                paths: &self.paths,
                home: self.places.home.clone(),
                claude_home: self.places.claude_home.clone(),
                codex_home: self.places.codex_home.clone(),
                found: &found,
                rules: &rules,
                records: &self.records,
                storage: &storage_name,
                codex_exe: self.places.codex_exe.clone(),
            };
            restore::run(&remote, &self.device, &env, sessions, &devices, replace, &on).await
        }
        .await;
        // The next discovery has to see the restored files.
        *lock(&self.discovery) = None;
        let end = match result {
            Ok(outcome) => {
                let rows = &outcome.sessions;
                let summary = format!(
                    "存储 {storage_name}：恢复 {} 个会话，{} 个文件新写入、{} 个补全、{} 个本来就一样{}",
                    rows.len(),
                    rows.iter().map(|r| r.written).sum::<usize>(),
                    rows.iter().map(|r| r.completed).sum::<usize>(),
                    rows.iter().map(|r| r.unchanged).sum::<usize>(),
                    match rows.iter().map(|r| r.replaced.len()).sum::<usize>() {
                        0 => String::new(),
                        n => format!("，{n} 个本机文件移到了 replaced"),
                    }
                );
                let result = serde_json::to_string(rows).unwrap_or_default();
                self.tasks.update(&id, |t| {
                    t.summary = summary;
                    t.result_json = result;
                });
                let _ = self.tasks.advance(&id, TaskState::PendingCommit, None);
                match outcome.log_error {
                    None => self.tasks.advance(&id, TaskState::Committed, None),
                    // The files are in place; only the record in the storage is missing.
                    Some(err) => {
                        self.tasks.update(&id, |t| {
                            t.summary
                                .push_str(&format!("；会话已恢复，但存储里的操作日志没写成功：{err}"))
                        });
                        self.tasks.advance(&id, TaskState::Committed, None)
                    }
                }
            }
            Err(err) => self.tasks.advance(&id, TaskState::Abandoned, Some(format!("{err:#}"))),
        };
        if let Err(err) = end
            && self.tasks.get(&id).is_some_and(|t| tasks::is_active(&t))
        {
            eprintln!("{} 恢复任务 {id} 没法收尾：{err}", now_ms());
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

fn agent_of(target: &SessionRef) -> Option<AgentKind> {
    proto::Agent::try_from(target.agent)
        .ok()
        .and_then(AgentKind::from_proto)
}

/// A storage problem in words for whoever asked, with a status code that fits.
fn remote_status(err: anyhow::Error) -> Status {
    let text = format!("{err:#}");
    if err.downcast_ref::<remote::Missing>().is_some() {
        Status::not_found(text)
    } else if err.downcast_ref::<remote::Choose>().is_some() {
        Status::failed_precondition(text)
    } else if err.downcast_ref::<StorageError>().is_some() {
        Status::unavailable(text)
    } else {
        Status::internal(text)
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
    use std::io::Write;

    use super::*;
    use crate::testdav::Server;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    struct Fixture {
        service: CoreService,
        claude: tempfile::TempDir,
        _home: tempfile::TempDir,
        _places: tempfile::TempDir,
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
            // As core reads it: the file, and what it says.
            fs::write(paths.config(), config).unwrap();
            let device = Device {
                id: "dev-svc".to_string(),
                name: "synthetic".to_string(),
                created_at_ms: 1,
            };
            let (stop, _) = watch::channel(false);
            let root = claude.path().to_path_buf();
            let places_dir = tempfile::tempdir().unwrap();
            let places = Places {
                home: places_dir.path().join("user"),
                claude_home: claude.path().to_path_buf(),
                codex_home: places_dir.path().join("codex"),
                codex_exe: Some(places_dir.path().join("no-codex")),
            };
            let service = CoreService::with_discovery(
                paths,
                device,
                toml::from_str(config).unwrap(),
                stop,
                Arc::new(move || vec![crate::agents::claude_code::discover_in(root.clone())]),
                places,
            );
            Fixture {
                service,
                claude,
                _home: home,
                _places: places_dir,
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

    /// Another device uploads a session of its own (working directory `/elsewhere`).
    async fn upload_from_another_device(server: &Server) -> String {
        let other = tempfile::tempdir().unwrap();
        let id = "99999999-2222-4333-8444-555555555555";
        let dir = other.path().join("projects/-elsewhere");
        fs::create_dir_all(&dir).unwrap();
        let lines = [
            r#"{"type":"user","timestamp":"2026-11-02T10:00:00.000Z","cwd":"/elsewhere","message":{"content":"q"}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-02T10:00:01.000Z","message":{"content":[{"type":"text","text":"a"}]}}"#,
        ];
        fs::write(dir.join(format!("{id}.jsonl")), lines.join("\n") + "\n").unwrap();
        let store = Webdav::new(server.spec()).unwrap();
        let device = Device {
            id: "dev-other".into(),
            name: "other".into(),
            created_at_ms: 1,
        };
        let found = vec![crate::agents::claude_code::discover_in(other.path().to_path_buf())];
        let jobs = archive::jobs_for(&found, &[(AgentKind::ClaudeCode, id.to_string())]);
        let out = archive::run(&store, &device, None, jobs, &|_| true).await;
        assert!(out.sessions[0].error.is_none());
        id.to_string()
    }

    fn fetch_request(id: &str, version: &str) -> FetchRequest {
        FetchRequest {
            session: Some(SessionRef {
                agent: proto::Agent::Unspecified as i32,
                id: id.to_string(),
            }),
            kinds: vec!["user".into()],
            version: version.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn status_shows_what_was_uploaded_and_what_came_since() {
        let server = Server::start(None).await;
        let fixture = Fixture::new(&config_for(&server));
        let status = || async {
            let response = fixture
                .service
                .status(Request::new(StatusRequest::default()))
                .await
                .unwrap()
                .into_inner();
            response.sessions[0].upload.unwrap()
        };
        let before = status().await;
        assert!(!before.uploaded);
        assert!(before.new_bytes > 0);
        let task = fixture.archive(&[ID], "").await.unwrap();
        assert_eq!(task.state(), TaskState::Committed, "{}", task.error);
        let after = status().await;
        assert!(after.uploaded && after.last_upload_ms > 0);
        assert_eq!((after.new_bytes, after.new_lines, after.rewritten), (0, 0, false));
        let main = fixture.claude.path().join(format!("projects/-synthetic/{ID}.jsonl"));
        let line = "{\"type\":\"user\",\"message\":{\"content\":\"more\"}}\n";
        fs::OpenOptions::new()
            .append(true)
            .open(&main)
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        tokio::time::sleep(DISCOVERY_TTL).await;
        let grown = status().await;
        assert_eq!(
            (grown.new_bytes, grown.new_lines, grown.rewritten),
            (line.len() as u64, 1, false)
        );
    }

    #[tokio::test]
    async fn fetch_reads_a_session_this_machine_does_not_have_from_the_storage() {
        let server = Server::start(None).await;
        let fixture = Fixture::new(&config_for(&server));
        let id = upload_from_another_device(&server).await;
        let response = fixture
            .service
            .fetch(Request::new(fetch_request(&id, "")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((response.version.as_str(), response.total_lines), ("dev-other-1", 2));
        assert_eq!(response.spans.len(), 1);
        let body = crate::render::read_spans(&response.path, &response.spans).unwrap();
        assert!(String::from_utf8_lossy(&body[0]).contains("\"q\""));
        assert_eq!(response.files, [format!("projects/-elsewhere/{id}.jsonl")]);
        let err = fixture
            .service
            .fetch(Request::new(fetch_request(&id, "local")))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        let err = fixture
            .service
            .fetch(Request::new(fetch_request(&id, "dev-other-9")))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        // A local session is read locally unless a version is named.
        let local = fixture
            .service
            .fetch(Request::new(fetch_request(ID, "")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(local.version, "local");

        let versions = fixture
            .service
            .versions(Request::new(VersionsRequest {
                session: fetch_request(&id, "").session,
                storage: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(versions.versions.len(), 1);
        let v = &versions.versions[0];
        assert_eq!(
            (v.id.as_str(), v.device_name.as_str(), v.head, v.lines),
            ("dev-other-1", "other", true, 2)
        );
    }

    #[tokio::test]
    async fn restore_runs_as_a_task_and_puts_the_session_where_claude_code_looks() {
        let server = Server::start(None).await;
        let fixture = Fixture::new(&config_for(&server));
        let id = upload_from_another_device(&server).await;
        let mut task = fixture
            .service
            .restore(Request::new(RestoreRequest {
                session: fetch_request(&id, "").session,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(task.kind, "restore");
        for _ in 0..500 {
            if !tasks::is_active(&task) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            task = fixture
                .service
                .get_task(Request::new(TaskRequest { id: task.id.clone() }))
                .await
                .unwrap()
                .into_inner();
        }
        assert_eq!(task.state(), TaskState::Committed, "{}", task.error);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&task.result_json).unwrap();
        assert_eq!(rows[0]["written"], 1);
        // `/elsewhere` is under no home: the folder name it had.
        let placed = fixture.claude.path().join(format!("projects/-elsewhere/{id}.jsonl"));
        assert!(placed.is_file());
        // Now it is local, and status knows it matches the storage.
        tokio::time::sleep(DISCOVERY_TTL).await;
        let response = fixture
            .service
            .status(Request::new(StatusRequest::default()))
            .await
            .unwrap()
            .into_inner();
        let row = response
            .sessions
            .iter()
            .find(|s| s.session.as_ref().unwrap().id == id)
            .unwrap();
        let upload = row.upload.unwrap();
        assert!(upload.uploaded && upload.new_bytes == 0);
        // Missing sessions and unknown versions are refused before a task starts.
        let err = fixture
            .service
            .restore(Request::new(RestoreRequest {
                session: fetch_request("00000000-0000-4000-8000-000000000000", "").session,
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    async fn restore_and_wait(fixture: &Fixture, id: &str) -> Task {
        let mut task = fixture
            .service
            .restore(Request::new(RestoreRequest {
                session: fetch_request(id, "").session,
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        for _ in 0..500 {
            if !tasks::is_active(&task) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            task = fixture
                .service
                .get_task(Request::new(TaskRequest { id: task.id.clone() }))
                .await
                .unwrap()
                .into_inner();
        }
        task
    }

    #[tokio::test]
    async fn force_requests_wait_for_the_user_and_revokes_do_not() {
        use crate::testsource::{TestSource, config};

        let server = Server::start(None).await;
        // A locked rule for the other device's folder and a locked copy form.
        let own = format!(
            "locked = [\"local.copy_form\"]\n{}[local]\ncopy_form = \"full\"\n\
             [[paths.map]]\nfrom = \"/elsewhere\"\nto = \"/own/locked\"\nlock = true\n",
            config_for(&server)
        );
        let fixture = Fixture::new(&own);
        let id = upload_from_another_device(&server).await;
        let src = TestSource::start(None).await;
        src.source("test-source-force")
            .save(&fixture.service.inner.paths)
            .unwrap();
        fixture.service.inner.look();

        let mapped = fixture._places.path().join("forced").join("work");
        let mapped_text = mapped.to_string_lossy().to_string();
        let mut doc = config(
            2,
            serde_json::json!([{ "from": "/elsewhere", "to": "/source/table", "devices": [] }]),
        );
        doc["local"] = serde_json::json!({ "copy_form": "pruned" });
        doc["force"] = serde_json::json!([
            { "id": "f-1", "devices": ["dev-svc"], "changes": [
                { "key": "paths.map", "value_json": serde_json::json!({ "from": "/elsewhere", "to": mapped_text }).to_string() },
            ] },
            { "id": "f-other", "devices": ["someone-else"], "changes": [ { "key": "local.copy_form", "value_json": "\"pruned\"" } ] },
            { "id": "f-unknown", "devices": [], "changes": [ { "key": "digest.excerpt", "value_json": "{}" } ] },
        ]);
        doc["requests"] = serde_json::json!([{ "id": "r-allow", "action": "EXEC_ACTION_ALLOW", "devices": [] }]);
        src.publish(&doc);

        let sync = || async {
            fixture
                .service
                .sync_source(Request::new(SyncSourceRequest {}))
                .await
                .unwrap()
                .into_inner()
        };
        let state = sync().await;
        assert_eq!(state.outcome, "updated", "{}", state.error);
        // Locked: the device's own still apply, and the requests wait.
        let ids: Vec<(&str, &str)> = state
            .requests
            .iter()
            .map(|r| (r.id.as_str(), r.state.as_str()))
            .collect();
        assert_eq!(
            ids,
            vec![("f-1", "pending"), ("f-unknown", "pending"), ("r-allow", "pending")]
        );
        assert_eq!(state.paths[0].to, "/own/locked");
        let copy = state.values.iter().find(|v| v.key == "local.copy_form").unwrap();
        assert_eq!((copy.value_json.as_str(), copy.origin.as_str()), ("\"full\"", "local"));
        assert!(!state.exec_allowed);

        let answer = |id: &'static str, accept: bool| {
            fixture
                .service
                .answer_source_request(Request::new(AnswerSourceRequestRequest { id: id.into(), accept }))
        };
        // Unknown keys are not applied; exec requests are not accepted this way.
        assert_eq!(
            answer("f-unknown", true).await.unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(
            answer("r-allow", true).await.unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        assert_eq!(answer("f-other", true).await.unwrap_err().code(), tonic::Code::NotFound);
        answer("f-unknown", false).await.unwrap();

        // Accepted: the forced rule becomes the device's own, unlocked, and a restore uses it.
        let state = answer("f-1", true).await.unwrap().into_inner();
        assert_eq!(state.requests[0].state, "accepted");
        assert_eq!(
            (state.paths[0].to.as_str(), state.paths[0].locked),
            (mapped_text.as_str(), false)
        );
        let written = fs::read_to_string(fixture.service.inner.paths.config()).unwrap();
        assert!(written.contains(&mapped_text) && !written.contains("/own/locked"));
        assert!(answer("f-1", true).await.is_err(), "handled once");
        let task = restore_and_wait(&fixture, &id).await;
        assert_eq!(task.state(), TaskState::Committed, "{}", task.error);
        let folder = crate::pathmap::claude_project_dir(&mapped_text);
        assert!(
            fixture
                .claude
                .path()
                .join("projects")
                .join(folder)
                .join(format!("{id}.jsonl"))
                .is_file()
        );

        // The copy form stays the device's own while locked; unset, the source's applies.
        assert!(fixture.service.inner.config.read().unwrap().local.full_copies());
        crate::config::edit(&fixture.service.inner.paths, |t| {
            crate::settings::set(t, "local.copy_form", None, Some(false))
        })
        .unwrap();
        fixture.service.inner.reload().unwrap();
        assert!(!fixture.service.inner.config.read().unwrap().local.full_copies());

        // The switch: on only here; a revoke turns it off at once.
        let exec = |allowed: bool, request_id: &'static str| {
            fixture.service.set_exec(Request::new(SetExecRequest {
                allowed,
                request_id: request_id.into(),
            }))
        };
        assert!(exec(true, "no-such").await.is_err());
        let state = exec(true, "r-allow").await.unwrap().into_inner();
        assert!(state.exec_allowed);
        assert!(
            state
                .requests
                .iter()
                .any(|r| r.id == "r-allow" && r.state == "accepted")
        );
        let mut doc = config(3, serde_json::json!([]));
        doc["requests"] =
            serde_json::json!([{ "id": "r-revoke", "action": "EXEC_ACTION_REVOKE", "devices": ["dev-svc"] }]);
        src.publish(&doc);
        let state = sync().await;
        assert!(!state.exec_allowed, "{}", state.exec_why);
        assert_eq!(state.requests[0].state, "accepted");
        // On again without a request, off here without asking.
        assert!(exec(true, "").await.unwrap().into_inner().exec_allowed);
        assert!(!exec(false, "").await.unwrap().into_inner().exec_allowed);
    }

    #[tokio::test]
    async fn a_path_map_from_the_source_decides_where_a_restore_goes() {
        use crate::testsource::{TestSource, config};

        let server = Server::start(None).await;
        // This device has its own rule for the other device's folder.
        let own = format!(
            "{}\n[[paths.map]]\nfrom = \"/elsewhere\"\nto = \"/own/choice\"\n",
            config_for(&server)
        );
        let fixture = Fixture::new(&own);
        let id = upload_from_another_device(&server).await;
        let src = TestSource::start(None).await;
        src.source("test-source-svc")
            .save(&fixture.service.inner.paths)
            .unwrap();

        // On record first, as core does when it starts: the device's own applies.
        assert_eq!(fixture.service.inner.path_rules()[0].to, "/own/choice");
        let state = fixture
            .service
            .get_source_state(Request::new(GetSourceStateRequest { quick: true }))
            .await
            .unwrap()
            .into_inner();
        assert!(state.configured && state.applied_serial == 0);
        assert_eq!(state.paths[0].origin, "local");

        // The source then changes it for this device.
        let mapped = fixture._places.path().join("mapped").join("work");
        let mapped_text = mapped.to_string_lossy().to_string();
        src.publish(&config(
            2,
            serde_json::json!([
                { "from": "/elsewhere", "to": mapped_text, "devices": ["dev-svc"] },
                { "from": "/elsewhere", "to": "/not/for/this/device", "devices": ["dev-other"] },
            ]),
        ));
        let state = fixture
            .service
            .sync_source(Request::new(SyncSourceRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(state.outcome, "updated", "{}", state.error);
        assert_eq!(state.applied_serial, 2);
        assert_eq!(state.paths.len(), 1);
        let p = &state.paths[0];
        assert_eq!((p.origin.as_str(), p.to.as_str()), ("source", mapped_text.as_str()));
        assert!(p.has_other && p.other == "/own/choice");

        let task = restore_and_wait(&fixture, &id).await;
        assert_eq!(task.state(), TaskState::Committed, "{}", task.error);
        let folder = crate::pathmap::claude_project_dir(&mapped_text);
        let placed = fixture
            .claude
            .path()
            .join("projects")
            .join(folder)
            .join(format!("{id}.jsonl"));
        assert!(placed.is_file(), "{}", placed.display());

        // Nothing new: unchanged, and the source's rule still applies.
        let state = fixture
            .service
            .sync_source(Request::new(SyncSourceRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(state.outcome, "unchanged");
        assert_eq!(state.paths[0].origin, "source");
        // No token: the source hears nothing.
        assert!(src.reports().is_empty());
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
