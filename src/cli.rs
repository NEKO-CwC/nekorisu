//! The command line: a thin client of core.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::agents::AgentKind;
use crate::client;
use crate::paths::Paths;
use crate::proto::{self, *};
use crate::render;
use crate::timefmt::{ago, human_bytes, now_ms};
use crate::update;

#[derive(Args)]
pub struct StatusArgs {
    /// 只看一个 Agent：claude-code 或 codex
    #[arg(long)]
    pub agent: Option<String>,
    /// 最多列多少个会话
    #[arg(long, default_value_t = 30)]
    pub limit: u32,
    /// 输出 JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct FetchArgs {
    /// 会话：<agent>:<id>，或者只写 id
    pub session: String,
    /// 会话包里的哪个文件，比如 subagents/agent-a1b2.jsonl；不写就是主记录
    #[arg(long)]
    pub file: Option<String>,
    /// 行号，比如 1-20,35,40-
    #[arg(long)]
    pub lines: Option<String>,
    /// 只要这些种类：user、assistant、reasoning、tool、meta，用逗号分隔
    #[arg(long, value_delimiter = ',')]
    pub kind: Vec<String>,
    /// 最多多少行；0 表示不限
    #[arg(long, default_value_t = 0)]
    pub limit: u32,
    /// 每个字段最多多少个字符；0 表示原样输出
    #[arg(long, default_value_t = 0)]
    pub max_field_chars: usize,
    /// 只把整份文件准备在本机，打印路径
    #[arg(long)]
    pub whole: bool,
    /// 每行输出一个 JSON 对象
    #[arg(long)]
    pub json: bool,
    /// 读存储里的哪个版本：manifest id，或 latest（最新上传的）；local 只读本机。不写：本机有就读本机，没有就读存储里最新的
    #[arg(long)]
    pub version: Option<String>,
    /// 用哪个存储；只配了一个时不用写
    #[arg(long)]
    pub storage: Option<String>,
}

#[derive(Args)]
pub struct VersionsArgs {
    /// 会话：<agent>:<id>，或者只写 id
    pub session: String,
    /// 用哪个存储；只配了一个时不用写
    #[arg(long)]
    pub storage: Option<String>,
    /// 输出 JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct RestoreArgs {
    /// 会话：<agent>:<id>，或者只写 id
    pub session: String,
    /// 恢复哪个版本（manifest id）；不写就是最新的，有好几个最新版本时会列出来让你选
    #[arg(long)]
    pub version: Option<String>,
    /// 本机这份有存储里没有的内容时，仍用存储里的替换（本机这份移到 ~/.nekorisu/replaced/，不删）
    #[arg(long)]
    pub replace: bool,
    /// 用哪个存储；只配了一个时不用写
    #[arg(long)]
    pub storage: Option<String>,
}

#[derive(Subcommand)]
pub enum CoreAction {
    /// core 在不在运行
    Status,
    /// 请系统启动 core（要先 nekorisu core enable）
    Start,
    /// 让 core 退出，等它退出完
    Stop,
    /// 登录时自动启动 core，并且现在就启动（macOS 用 launchd，Linux 用 systemd --user，Windows 用计划任务）
    Enable,
    /// 取消登录时自动启动，并让 core 退出
    Disable,
}

pub async fn scan(json_out: bool) -> anyhow::Result<()> {
    let mut core = client::connect("cli").await?;
    let response = core.scan(ScanRequest::default()).await.map_err(rpc_error)?.into_inner();
    if json_out {
        let rows: Vec<Value> = response.agents.iter().map(summary_json).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let now = now_ms();
    for a in &response.agents {
        let name = agent_name(a.agent);
        if !a.found {
            println!("{name}：没找到（{}）", a.home);
            continue;
        }
        let extra = if a.agent() == proto::Agent::Codex {
            format!("，另有子线程 {} 个", a.child_sessions)
        } else {
            format!("，另有子 Agent 记录 {} 个", a.subagent_files)
        };
        println!("{name}（{}）", a.home);
        println!(
            "  会话 {} 个{extra}；文件 {} 个，共 {}",
            a.sessions,
            a.files,
            human_bytes(a.bytes)
        );
        if a.compressed_files > 0 || a.archived_by_agent > 0 {
            println!(
                "  {} 个文件被 {name} 自己压缩过，{} 个会话被它自己归档",
                a.compressed_files, a.archived_by_agent
            );
        }
        if a.cleanup_within_7d > 0 {
            println!("  {} 个会话 7 天内会被 {name} 自己删掉部分文件", a.cleanup_within_7d);
        }
        if a.oldest_activity_ms > 0 {
            println!("  最早一个会话的最后活动：{}", ago(a.oldest_activity_ms, now));
        }
    }
    Ok(())
}

pub async fn status(args: StatusArgs) -> anyhow::Result<()> {
    let agents = match &args.agent {
        Some(name) => vec![parse_agent(name)? as i32],
        None => Vec::new(),
    };
    let mut core = client::connect("cli").await?;
    let response = core
        .status(StatusRequest {
            agents,
            since_ms: 0,
            limit: args.limit,
        })
        .await
        .map_err(rpc_error)?
        .into_inner();
    if args.json {
        let rows: Vec<Value> = response.sessions.iter().map(status_json).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for line in source_reminders(&mut core).await {
        println!("{line}");
    }
    let now = now_ms();
    for s in &response.sessions {
        let reference = s.session.as_ref().map(ref_text).unwrap_or_default();
        let upload = match &s.upload {
            Some(u) if u.uploaded => upload_text(u, now),
            _ => "未上传".to_string(),
        };
        let mut notes = Vec::new();
        if let Some(parent) = &s.parent {
            notes.push(format!("子线程，父线程 {}", parent.id));
        }
        if s.children > 0 {
            notes.push(format!("{} 个子线程", s.children));
        }
        if s.compressed {
            notes.push("被 Agent 压缩过".to_string());
        }
        if s.archived_by_agent {
            notes.push("被 Agent 归档".to_string());
        }
        if s.cleanup_at_ms > 0 {
            notes.push(if s.cleanup_at_ms <= now {
                "Agent 随时可能删掉部分文件".to_string()
            } else {
                format!("{} 天后 Agent 会删掉部分文件", (s.cleanup_at_ms - now) / 86_400_000)
            });
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!("（{}）", notes.join("，"))
        };
        println!(
            "{reference}  {}  {}  {} 个文件  {upload}  {}{notes}",
            ago(s.last_activity_ms, now),
            human_bytes(s.bytes),
            s.files,
            s.project
        );
    }
    Ok(())
}

/// What the trusted source is waiting on, or asks for, that the person should hear about.
/// Empty when there is no source or core does not know this call yet.
pub async fn source_reminders(core: &mut client::Client) -> Vec<String> {
    let Ok(state) = core.get_source_state(GetSourceStateRequest { quick: true }).await else {
        return Vec::new();
    };
    let state = state.into_inner();
    let mut out = Vec::new();
    let pending = state.requests.iter().filter(|r| r.state == "pending").count();
    if pending > 0 {
        out.push(format!(
            "来源 {} 有 {pending} 个请求等你本人在终端里确认：nekorisu source status",
            state.name
        ));
    }
    if !state.min_client_version.is_empty() {
        out.push(format!(
            "来源要求客户端至少是 {}，这台是 {}：nekorisu update",
            state.min_client_version,
            env!("CARGO_PKG_VERSION")
        ));
    }
    out
}

/// "3 小时前上传，之后新增 12.0 KB、8 行", "1 天前上传，已是最新", plus a note when the
/// uploaded part changed locally.
fn upload_text(u: &UploadState, now: i64) -> String {
    let mut text = format!("{}上传", ago(u.last_upload_ms, now));
    if u.rewritten {
        text.push_str("，本机文件改写过，下次整份重传");
    } else if u.new_bytes == 0 {
        text.push_str("，已是最新");
    } else if u.new_lines > 0 {
        text.push_str(&format!("，之后新增 {}、{} 行", human_bytes(u.new_bytes), u.new_lines));
    } else {
        text.push_str(&format!("，之后新增 {}", human_bytes(u.new_bytes)));
    }
    text
}

pub async fn fetch(args: FetchArgs) -> anyhow::Result<()> {
    let request = FetchRequest {
        session: Some(parse_ref(&args.session)?),
        file: args.file.clone().unwrap_or_default(),
        lines: match &args.lines {
            Some(spec) => parse_lines(spec)?,
            None => Vec::new(),
        },
        kinds: args.kind.clone(),
        limit: args.limit,
        whole: args.whole,
        version: args.version.clone().unwrap_or_default(),
        storage: args.storage.clone().unwrap_or_default(),
    };
    let mut core = client::connect("cli").await?;
    let response = core.fetch(request).await.map_err(rpc_error)?.into_inner();
    if args.whole {
        println!("{}", response.path);
        return Ok(());
    }
    if response.version != "local" {
        eprintln!("读的是存储里的版本 {}", response.version);
    }
    let bodies = render::read_spans(&response.path, &response.spans)?;
    for (span, body) in response.spans.iter().zip(&bodies) {
        let text = render::line_text(body, args.max_field_chars);
        if args.json {
            let record = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
            println!(
                "{}",
                json!({ "line": span.line, "kind": span.kind, "raw_type": span.raw_type, "timestamp_ms": span.timestamp_ms, "record": record })
            );
        } else {
            println!("{}\t{}\t{}", span.line, span.kind, text);
        }
    }
    if response.truncated {
        eprintln!("还有更多符合条件的行；调大 --limit 再看");
    }
    Ok(())
}

pub async fn search(query: String) -> anyhow::Result<()> {
    let mut core = client::connect("cli").await?;
    core.search(SearchRequest {
        query,
        ..Default::default()
    })
    .await
    .map_err(rpc_error)?;
    Ok(())
}

pub async fn archive(sessions: Vec<String>, storage: Option<String>) -> anyhow::Result<()> {
    let sessions = sessions
        .iter()
        .map(|s| parse_ref(s))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut core = client::connect("cli").await?;
    let request = ArchiveRequest {
        sessions,
        storage: storage.unwrap_or_default(),
        ..Default::default()
    };
    let task = run_archive(&mut core, request, true, None).await?;
    for line in archive_lines(&task) {
        println!("{line}");
    }
    if !task.summary.is_empty() {
        println!("{}", task.summary);
    }
    match task.state() {
        TaskState::Committed => Ok(()),
        _ if !task.error.is_empty() => bail!("{}", task.error),
        _ => bail!("上传没有完成"),
    }
}

/// Starts an archive task in core and waits for it, polling. `max_wait` of `None` waits until
/// it is done; with a limit the task may come back still running. With `progress`, a
/// terminal gets a "sent so far" line on stderr.
pub async fn run_archive(
    core: &mut client::Client,
    request: ArchiveRequest,
    progress: bool,
    max_wait: Option<Duration>,
) -> anyhow::Result<Task> {
    let task = core.archive(request).await.map_err(rpc_error)?.into_inner();
    wait_task(core, task, progress, max_wait).await
}

/// Waits for a task in core, polling. `max_wait` of `None` waits until it is done; with a limit
/// the task may come back still running. With `progress`, a terminal gets a "so far" line on
/// stderr.
pub async fn wait_task(
    core: &mut client::Client,
    mut task: Task,
    progress: bool,
    max_wait: Option<Duration>,
) -> anyhow::Result<Task> {
    let show = progress && std::io::stderr().is_terminal();
    let started = Instant::now();
    while is_running(&task) {
        if max_wait.is_some_and(|limit| started.elapsed() > limit) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        task = core
            .get_task(TaskRequest { id: task.id.clone() })
            .await
            .map_err(rpc_error)?
            .into_inner();
        if show && task.total_bytes > 0 {
            eprint!(
                "\r  已传输 {} / {}   ",
                human_bytes(task.done_bytes),
                human_bytes(task.total_bytes)
            );
        }
    }
    if show {
        eprint!("\r\x1b[2K");
    }
    Ok(task)
}

pub fn is_running(task: &Task) -> bool {
    matches!(
        task.state(),
        TaskState::Planned | TaskState::Running | TaskState::PendingCommit
    )
}

/// One line per session of an archive task's result.
pub fn archive_lines(task: &Task) -> Vec<String> {
    let rows: Vec<Value> = serde_json::from_str(&task.result_json).unwrap_or_default();
    rows.iter()
        .map(|row| {
            let session = row["session"].as_str().unwrap_or("?");
            let manifest = row["manifest"].as_str().unwrap_or("-");
            if let Some(error) = row["error"].as_str() {
                return format!("{session}  失败：{error}");
            }
            if row["up_to_date"].as_bool() == Some(true) {
                return format!("{session}  已是最新（manifest {manifest}）");
            }
            let rewritten = if row["rewrite"].as_bool() == Some(true) {
                "；本机文件的前面部分变了，这些文件整份重传"
            } else {
                ""
            };
            format!(
                "{session}  已上传：{} 个文件，新增 {}、{} 行；manifest {manifest}{rewritten}",
                row["files"].as_u64().unwrap_or(0),
                human_bytes(row["new_bytes"].as_u64().unwrap_or(0)),
                row["new_lines"].as_u64().unwrap_or(0)
            )
        })
        .collect()
}

pub async fn versions(args: VersionsArgs) -> anyhow::Result<()> {
    let mut core = client::connect("cli").await?;
    let response = core
        .versions(VersionsRequest {
            session: Some(parse_ref(&args.session)?),
            storage: args.storage.unwrap_or_default(),
        })
        .await
        .map_err(rpc_error)?
        .into_inner();
    if args.json {
        let rows: Vec<Value> = response.versions.iter().map(version_json).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let now = now_ms();
    let heads = response.versions.iter().filter(|v| v.head).count();
    for v in &response.versions {
        let device = if v.device_name.is_empty() {
            &v.device
        } else {
            &v.device_name
        };
        println!(
            "{}{}  设备 {device}  {}上传  最后一轮 {}  {}、主记录 {} 行{}",
            if v.head { "* " } else { "  " },
            v.id,
            ago(v.created_at_ms, now),
            ago(v.last_activity_ms, now),
            human_bytes(v.bytes),
            v.lines,
            if v.parent.is_empty() {
                String::new()
            } else {
                format!("  接着 {}", v.parent)
            }
        );
    }
    if heads > 1 {
        println!("有 {heads} 个最新版本（标 *）：不同设备各自接着做过。恢复时用 --version 选一个。");
    }
    Ok(())
}

pub fn version_json(v: &Version) -> Value {
    json!({
        "id": v.id,
        "parent": v.parent,
        "device": v.device,
        "device_name": v.device_name,
        "created_at_ms": v.created_at_ms,
        "last_activity_ms": v.last_activity_ms,
        "bytes": v.bytes,
        "lines": v.lines,
        "newest": v.head,
    })
}

pub async fn restore(args: RestoreArgs) -> anyhow::Result<()> {
    let request = RestoreRequest {
        session: Some(parse_ref(&args.session)?),
        version: args.version.unwrap_or_default(),
        replace: args.replace,
        storage: args.storage.unwrap_or_default(),
    };
    let mut core = client::connect("cli").await?;
    let task = core.restore(request).await.map_err(rpc_error)?.into_inner();
    let task = wait_task(&mut core, task, true, None).await?;
    for line in restore_lines(&task) {
        println!("{line}");
    }
    if !task.summary.is_empty() && task.state() == TaskState::Committed {
        println!("{}", task.summary);
    }
    match task.state() {
        TaskState::Committed => Ok(()),
        _ if !task.error.is_empty() => bail!("{}", task.error),
        _ => bail!("恢复没有完成"),
    }
}

/// One line per session of a restore task's result, with how to go on.
pub fn restore_lines(task: &Task) -> Vec<String> {
    let rows: Vec<Value> = serde_json::from_str(&task.result_json).unwrap_or_default();
    let mut out = Vec::new();
    for row in &rows {
        let session = row["session"].as_str().unwrap_or("?");
        let count = |key: &str| row[key].as_u64().unwrap_or(0);
        out.push(format!(
            "{session}  版本 {}：{} 个文件新写入、{} 个补全、{} 个本来就一样",
            row["manifest"].as_str().unwrap_or("-"),
            count("written"),
            count("completed"),
            count("unchanged")
        ));
        if let Some(folder) = row["folder"].as_str() {
            out.push(format!("  放在 Claude Code 的项目文件夹 {folder}"));
        }
        for path in row["replaced"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            out.push(format!("  本机原来那份移到了 {path}"));
        }
        if let Some(note) = row["note"].as_str() {
            out.push(format!("  {note}"));
        }
        if let Some(resume) = row["resume"].as_str() {
            match row["resume_in"].as_str() {
                Some(dir) => out.push(format!("  接着做：在 {dir} 里运行 {resume}")),
                None => out.push(format!("  接着做：{resume}")),
            }
        }
    }
    out
}

pub async fn doctor() -> anyhow::Result<()> {
    let mut core = client::connect("cli").await?;
    core.doctor(DoctorRequest::default()).await.map_err(rpc_error)?;
    Ok(())
}

/// `nekorisu update [--check]`.
pub async fn update(check_only: bool) -> anyhow::Result<()> {
    let paths = Paths::from_env();
    let config = crate::cloud::effective_config(&paths)?;
    let roots = nekorisu::sign::embedded_root_keys();
    let candidate = match update::check(&paths, &config.update, &roots).await? {
        update::Outcome::UpToDate { latest } => {
            println!(
                "已经是最新：当前 {}，{} 渠道最新的是 {latest}",
                update::VERSION,
                config.update.channel
            );
            return Ok(());
        }
        update::Outcome::Available(candidate) => candidate,
    };
    let version = candidate.manifest.version.clone();
    println!("有新版本 {version}（当前 {}）", update::VERSION);
    if check_only {
        return Ok(());
    }
    let Some(link) = update::installed_link(&paths, &config.update) else {
        bail!(
            "这份 nekorisu 不是安装脚本装的（{}），请用你安装它的方式更新",
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
    };
    let mut running = client::connect_existing().await;
    if let Some(core) = running.as_mut() {
        let active = core
            .list_tasks(ListTasksRequest {
                active_only: true,
                limit: 1,
            })
            .await
            .map_err(rpc_error)?
            .into_inner();
        if !active.tasks.is_empty() {
            bail!("core 正在执行任务，等它做完再更新（core 自己也会在空闲时更新）");
        }
    }
    let Some(_guard) = update::try_lock(&paths)? else {
        bail!("另一个更新正在进行");
    };
    println!("正在下载并核对……");
    let binary = update::stage(&paths, &candidate).await?;
    update::install(&paths, &link, &binary, &version)?;
    println!("已更新到 {version}");
    if let Some(mut core) = running {
        // Core still runs the old version: stop it, and the system starts the new one through the link.
        stop_core(&mut core).await?;
        start_core().await?;
    }
    Ok(())
}

pub fn not_yet(what: &str) -> anyhow::Result<()> {
    bail!("{what} 还在开发")
}

pub async fn core(action: CoreAction) -> anyhow::Result<()> {
    match action {
        CoreAction::Status => {
            let Some(mut core) = client::connect_existing().await else {
                println!("core 没在运行");
                return Ok(());
            };
            let hello = client::hello(&mut core, "cli").await?;
            println!(
                "core {} 在运行：pid {}，{}启动，协议版本 {}，设备 {}",
                hello.core_version,
                hello.pid,
                ago(hello.started_at_ms, now_ms()),
                hello.protocol_version,
                hello.device_id
            );
        }
        CoreAction::Start => {
            if let Some(mut core) = client::connect_existing().await {
                let hello = client::hello(&mut core, "cli").await?;
                println!("core {} 已经在运行：pid {}", hello.core_version, hello.pid);
                return Ok(());
            }
            start_core().await?;
        }
        CoreAction::Stop => match client::connect_existing().await {
            Some(mut core) => stop_core(&mut core).await?,
            None => println!("core 没在运行"),
        },
        CoreAction::Enable => {
            crate::autostart::enable()?;
            // launchd and systemd have started it already; the Task Scheduler is asked here.
            if cfg!(windows) && client::connect_existing().await.is_none() {
                start_core().await?;
            }
        }
        CoreAction::Disable => {
            crate::autostart::disable()?;
            // launchd and systemd stop it with the registration; on Windows it is stopped here.
            if let Some(mut core) = client::connect_existing().await {
                stop_core(&mut core).await?;
            }
        }
    }
    Ok(())
}

/// Asks the system to start core (`autostart::start`), then waits until it answers.
async fn start_core() -> anyhow::Result<()> {
    crate::autostart::start()?;
    // launchd holds a restart back for up to ten seconds after the previous start.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(mut core) = client::connect_existing().await {
            let hello = client::hello(&mut core, "cli").await?;
            println!("core {} 已启动：pid {}", hello.core_version, hello.pid);
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!(
                "已请系统启动 core，但 20 秒内没连上；日志在 {}",
                Paths::from_env().core_log().display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Asks core to exit and waits until its process is gone, so whatever starts core next finds
/// nothing in the way. Exiting this way counts as stopped: the system doesn't restart it.
async fn stop_core(core: &mut client::Client) -> anyhow::Result<()> {
    let pid = client::hello(core, "cli").await?.pid;
    core.shutdown(ShutdownRequest {}).await.map_err(rpc_error)?;
    if !client::wait_for_exit(pid, Duration::from_secs(30)).await {
        bail!("已让 core 退出，但它 30 秒后还在收尾（pid {pid}）");
    }
    println!("core 已退出");
    Ok(())
}

/// Keeps the gRPC status inside (so callers can check its code) but shows only its message.
pub fn rpc_error(status: tonic::Status) -> anyhow::Error {
    let message = status.message().to_string();
    anyhow::Error::new(status).context(message)
}

pub fn parse_agent(name: &str) -> anyhow::Result<proto::Agent> {
    AgentKind::parse(name)
        .map(AgentKind::to_proto)
        .ok_or_else(|| anyhow!("不认识的 Agent：{name}（可以是 claude-code 或 codex）"))
}

/// "claude-code:<id>", "codex:<id>", or a bare id.
pub fn parse_ref(text: &str) -> anyhow::Result<SessionRef> {
    let (agent, id) = match text.split_once(':') {
        Some((agent, id)) => (parse_agent(agent)?, id),
        None => (proto::Agent::Unspecified, text),
    };
    if id.trim().is_empty() {
        bail!("会话 ID 是空的");
    }
    Ok(SessionRef {
        agent: agent as i32,
        id: id.trim().to_string(),
    })
}

pub fn ref_text(session: &SessionRef) -> String {
    format!("{}:{}", agent_name(session.agent), session.id)
}

/// "1-20,35,40-": 1-based, inclusive, an open end means "to the last line".
pub fn parse_lines(spec: &str) -> anyhow::Result<Vec<LineRange>> {
    spec.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (start, end) = match part.split_once('-') {
                Some((a, b)) => {
                    let end = if b.trim().is_empty() {
                        0
                    } else {
                        b.trim().parse::<u64>()?
                    };
                    (a.trim().parse::<u64>()?, end)
                }
                None => {
                    let n = part.parse::<u64>()?;
                    (n, n)
                }
            };
            if start == 0 || (end != 0 && end < start) {
                bail!("行号范围不对：{part}");
            }
            Ok(LineRange { start, end })
        })
        .collect()
}

fn agent_name(agent: i32) -> &'static str {
    proto::Agent::try_from(agent)
        .ok()
        .and_then(AgentKind::from_proto)
        .map(AgentKind::name)
        .unwrap_or("unknown")
}

pub fn summary_json(a: &AgentSummary) -> Value {
    json!({
        "agent": agent_name(a.agent),
        "home": a.home,
        "found": a.found,
        "sessions": a.sessions,
        "child_sessions": a.child_sessions,
        "subagent_files": a.subagent_files,
        "files": a.files,
        "bytes": a.bytes,
        "compressed_files": a.compressed_files,
        "archived_by_agent": a.archived_by_agent,
        "cleanup_within_7d": a.cleanup_within_7d,
        "oldest_activity_ms": a.oldest_activity_ms,
    })
}

pub fn status_json(s: &SessionStatus) -> Value {
    let upload = s.upload.unwrap_or_default();
    json!({
        "session": s.session.as_ref().map(ref_text),
        "parent": s.parent.as_ref().map(ref_text),
        "project": s.project,
        "last_activity_ms": s.last_activity_ms,
        "bytes": s.bytes,
        "files": s.files,
        "children": s.children,
        "uploaded": upload.uploaded,
        "last_upload_ms": upload.last_upload_ms,
        "new_bytes": upload.new_bytes,
        "new_lines": upload.new_lines,
        "rewritten": upload.rewritten,
        "cleanup_at_ms": s.cleanup_at_ms,
        "compressed": s.compressed,
        "archived_by_agent": s.archived_by_agent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_line_specs() {
        let ranges = parse_lines("1-20, 35,40-").unwrap();
        assert_eq!(
            ranges.iter().map(|r| (r.start, r.end)).collect::<Vec<_>>(),
            [(1, 20), (35, 35), (40, 0)]
        );
        assert!(parse_lines("0-3").is_err());
        assert!(parse_lines("9-3").is_err());
        assert!(parse_lines("x").is_err());
    }

    #[test]
    fn parses_session_refs() {
        let r = parse_ref("codex:abc").unwrap();
        assert_eq!((r.agent(), r.id.as_str()), (proto::Agent::Codex, "abc"));
        assert_eq!(parse_ref("abc").unwrap().agent(), proto::Agent::Unspecified);
        assert!(parse_ref("gemini:abc").is_err());
    }
}
