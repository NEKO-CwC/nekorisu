//! `nekorisu uninstall`: takes off this computer everything installing and using nekorisu put
//! on it, then looks again to check it is gone. What is the user's stays: the sessions in the
//! agents' folders, and everything in storage (their backup).
//!
//! Nothing is removed before it has listed what will go and settled two questions, at a
//! terminal by asking, otherwise from options: whether to first put back from storage the
//! sessions this computer uploaded, and what to do with the local copies `restore --replace`
//! set aside in `~/.nekorisu/replaced/`.
//!
//! What goes, in this order: the agents' MCP entries (signing out of the server first, which
//! revokes the grant where the agent asks the server to), the skills, and Claude Code's logs
//! for those entries; login start and core; other processes of this install (MCP servers of
//! open sessions); passwords and tokens in the keychain; the command link, the Windows
//! launcher and PATH entry; `~/.nekorisu`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::bail;
use clap::{Args, ValueEnum};
use serde_json::Value;

use crate::archive::ROOT;
use crate::cloud::Source;
use crate::paths::{self, Paths};
use crate::proto::{RestoreRequest, TaskState};
use crate::timefmt::human_bytes;
use crate::{autostart, cli, client, config, restore, secrets, storage, update};

/// The names the install manual gives the MCP entries and the skills.
const NAMES: [&str; 2] = ["nekorisu", "nekorisu-server"];
/// Where `--replaced keep` puts the set-aside copies, under the home folder.
const KEPT_REPLACED: &str = "nekorisu-replaced";
/// How long one `claude` or `codex` command may take (signing out asks the server).
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Args)]
pub struct UninstallArgs {
    /// 先把这台电脑上传过的会话从存储放回 Agent 的目录（本机已有、更完整的不动）
    #[arg(long, conflicts_with = "no_restore")]
    restore: bool,
    /// 不从存储放回会话
    #[arg(long)]
    no_restore: bool,
    /// restore --replace 留在 ~/.nekorisu/replaced/ 的本机旧副本：keep 移到主目录下的 nekorisu-replaced，delete 一起删掉
    #[arg(long, value_enum)]
    replaced: Option<Replaced>,
    /// 不再确认，直接卸载
    #[arg(long, short = 'y')]
    yes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Replaced {
    Keep,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    Claude,
    Codex,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Claude => "Claude Code",
            Tool::Codex => "Codex",
        }
    }

    fn command(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
        }
    }
}

/// An MCP entry of ours in an agent's settings.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    tool: Tool,
    name: String,
    /// Claude Code's local scope: the project folder the entry belongs to.
    project: Option<PathBuf>,
    /// Reached over HTTP (the server), so the agent may hold a sign-in for it.
    remote: bool,
}

impl Entry {
    fn describe(&self) -> String {
        match &self.project {
            Some(dir) => format!("{}（只在 {}）", self.name, dir.display()),
            None => format!("{}（所有项目）", self.name),
        }
    }
}

/// A running process of this install other than this one.
#[derive(Clone, Debug, PartialEq)]
struct Process {
    pid: u32,
    command: String,
}

/// What this computer has, looked at before anything is removed.
struct Plan {
    paths: Paths,
    home: bool,
    home_bytes: u64,
    replaced_files: usize,
    replaced_bytes: u64,
    core_pid: Option<u32>,
    autostart: bool,
    link: Option<PathBuf>,
    bin_extras: Vec<PathBuf>,
    processes: Vec<Process>,
    entries: Vec<Entry>,
    skills: Vec<PathBuf>,
    foreign_skills: Vec<PathBuf>,
    mcp_logs: Vec<PathBuf>,
    keychain: bool,
    source: Option<Source>,
    /// Per storage: the sessions this computer uploaded, each with its newest version from here.
    uploads: BTreeMap<String, BTreeMap<String, String>>,
    unreadable: Vec<String>,
    /// Windows: the folder of the link, while it is in the user's PATH.
    path_entry: Option<PathBuf>,
}

impl Plan {
    async fn gather(paths: Paths) -> Plan {
        let config = config::load(&paths).unwrap_or_default();
        let link = update::installed_link(&paths, &config.update);
        let mut core_pid = None;
        if let Some(mut core) = client::connect_existing().await
            && let Ok(hello) = client::hello(&mut core, "cli").await
        {
            core_pid = Some(hello.pid);
        }
        let (replaced_files, replaced_bytes) = tally(&paths.replaced_dir());
        let mut entries = claude_entries(&fs::read_to_string(claude_config()).unwrap_or_default());
        entries.extend(codex_entries(
            &fs::read_to_string(paths::codex_home().join("config.toml")).unwrap_or_default(),
        ));
        let (skills, foreign_skills) = skills();
        let mut names: Vec<String> = NAMES.iter().map(|n| n.to_string()).collect();
        names.extend(entries.iter().map(|e| e.name.clone()));
        let (uploads, unreadable) = uploads(&paths, &config).await;
        Plan {
            home: paths.home.is_dir(),
            home_bytes: tally(&paths.home).1,
            replaced_files,
            replaced_bytes,
            core_pid,
            autostart: login_start(link.is_some()),
            bin_extras: link.as_deref().map(bin_extras).unwrap_or_default(),
            processes: processes(&paths, link.as_deref()),
            entries,
            skills,
            foreign_skills,
            mcp_logs: mcp_logs(&names),
            keychain: secrets::any_in_keychain(),
            source: Source::load(&paths).ok().flatten(),
            uploads,
            unreadable,
            path_entry: link.as_deref().and_then(path_entry),
            link,
            paths,
        }
    }

    fn uploaded(&self) -> usize {
        self.uploads.values().map(BTreeMap::len).sum()
    }

    fn print(&self) {
        let mut gone = Vec::new();
        if self.autostart {
            gone.push(format!("登录时启动 core 的设置（{}）", autostart::describe()));
        }
        if let Some(pid) = self.core_pid {
            gone.push(format!("在运行的 core（pid {pid}）"));
        }
        if !self.processes.is_empty() {
            gone.push(format!(
                "{} 个还在运行的 nekorisu 进程（别的 Agent 会话里的 nekorisu 工具会断开）",
                self.processes.len()
            ));
        }
        for tool in [Tool::Claude, Tool::Codex] {
            let mine: Vec<String> = self
                .entries
                .iter()
                .filter(|e| e.tool == tool)
                .map(Entry::describe)
                .collect();
            if !mine.is_empty() {
                let signed = self.entries.iter().any(|e| e.tool == tool && e.remote);
                gone.push(format!(
                    "{} 里的 MCP：{}{}",
                    tool.label(),
                    mine.join("、"),
                    if signed {
                        "；连服务端的那个先退出登录"
                    } else {
                        ""
                    }
                ));
            }
        }
        for dir in &self.skills {
            gone.push(format!("skill {}", dir.display()));
        }
        if !self.mcp_logs.is_empty() {
            gone.push(format!(
                "Claude Code 给这些 MCP 记的日志（{} 个文件夹）",
                self.mcp_logs.len()
            ));
        }
        if self.keychain {
            gone.push("钥匙串里 nekorisu 存的存储密码和来源令牌".to_string());
        }
        if let Some(link) = &self.link {
            gone.push(format!("命令 {}", link.display()));
        }
        for path in &self.bin_extras {
            gone.push(format!("{}", path.display()));
        }
        if let Some(dir) = &self.path_entry {
            gone.push(format!("用户 PATH 里的 {}（那个文件夹删空了才去掉）", dir.display()));
        }
        if self.home {
            gone.push(format!(
                "{}（程序、配置、设备 ID、来源、日志、缓存，{}）",
                self.paths.home.display(),
                human_bytes(self.home_bytes)
            ));
        }
        if gone.is_empty() {
            println!("这台电脑上没找到 nekorisu 留下的东西。");
        } else {
            println!("卸载 nekorisu 会删掉这些：");
            for line in &gone {
                println!("  - {line}");
            }
        }
        let mut kept = vec!["Agent 目录里的会话（包括从存储放回来的）".to_string()];
        for name in self.uploads.keys() {
            kept.push(format!("存储 {name} 里的会话和记录：那是你的备份"));
        }
        if let Some(source) = &self.source {
            kept.push(format!(
                "服务端 {}（{}）上这台电脑上报过的状态：客户端删不了",
                source.name, source.url
            ));
        }
        for dir in &self.foreign_skills {
            kept.push(format!("{}：里面不是 nekorisu 的 SKILL.md", dir.display()));
        }
        println!("不动的：");
        for line in &kept {
            println!("  - {line}");
        }
    }
}

pub async fn run(args: UninstallArgs) -> anyhow::Result<()> {
    let paths = Paths::from_env();
    let plan = Plan::gather(paths.clone()).await;
    plan.print();
    let at_terminal = std::io::stdin().is_terminal();

    let restore_wanted = if plan.uploaded() == 0 && plan.unreadable.is_empty() {
        false
    } else if args.restore || args.no_restore {
        args.restore
    } else if at_terminal {
        for (name, sessions) in &plan.uploads {
            println!("这台电脑往存储 {name} 上传过 {} 个会话。", sessions.len());
        }
        for line in &plan.unreadable {
            println!("存储读不了：{line}");
        }
        ask_yes("卸载前，要不要先把它们从存储放回 Agent 的目录？本机已有、更完整的不动")?
    } else {
        return missing(&plan, &args);
    };
    if restore_wanted && !plan.unreadable.is_empty() {
        bail!(
            "存储读不了，放不回会话（{}）；什么都没删。修好再试，或者用 --no-restore",
            plan.unreadable.join("；")
        );
    }

    let replaced = if plan.replaced_files == 0 {
        Replaced::Delete
    } else if let Some(choice) = args.replaced {
        choice
    } else if at_terminal {
        println!(
            "{} 里有 {} 个文件（{}），是以前 restore --replace 替换下来的本机旧副本。",
            paths.replaced_dir().display(),
            plan.replaced_files,
            human_bytes(plan.replaced_bytes)
        );
        let kept_at = paths::home_dir().join(KEPT_REPLACED);
        ask_replaced(&format!("留着（移到 {}）还是删掉？", kept_at.display()))?
    } else {
        return missing(&plan, &args);
    };

    if !args.yes {
        if !at_terminal {
            return missing(&plan, &args);
        }
        if !ask_yes("确认卸载？")? {
            println!("没有卸载，什么都没动。");
            return Ok(());
        }
    }

    if restore_wanted {
        put_back(&plan).await?;
    }
    let mut todo: Vec<String> = Vec::new();
    take_off_agents(&plan, &mut todo).await;
    stop_everything(&plan).await;
    match secrets::forget_keychain() {
        Ok(0) => {}
        Ok(n) => println!("已从钥匙串删掉 {n} 条"),
        Err(err) => todo.push(format!("钥匙串里的 nekorisu 条目没删掉（{err:#}）")),
    }
    if replaced == Replaced::Keep && plan.replaced_files > 0 {
        match keep_replaced(&paths) {
            Ok(to) => println!("restore 替换下来的旧副本移到了 {}", to.display()),
            Err(err) => bail!(
                "旧副本没能移出 {}（{err:#}），停在这里，{} 没删",
                paths.replaced_dir().display(),
                paths.home.display()
            ),
        }
    }
    remove_files(&plan, &mut todo);

    let left = leftovers(&plan);
    if left.is_empty() {
        println!("卸载完了，又查了一遍，上面列的都不在了。");
    } else {
        println!("还在的：");
        for line in &left {
            println!("  - {line}");
        }
    }
    if let Some(source) = &plan.source {
        todo.push(format!(
            "服务端 {} 的 panel 上，设备页还会显示这台电脑上报过的状态",
            source.name
        ));
    }
    for dir in &plan.foreign_skills {
        todo.push(format!("{} 不是 nekorisu 的 skill，没删；不要了自己删", dir.display()));
    }
    if !todo.is_empty() {
        println!("还要你看一下的：");
        for line in &todo {
            println!("  - {line}");
        }
    }
    if left.is_empty() {
        Ok(())
    } else {
        bail!("有 {} 样没删掉", left.len())
    }
}

/// Not at a terminal and a question unanswered: say which options answer it, remove nothing.
fn missing(plan: &Plan, args: &UninstallArgs) -> anyhow::Result<()> {
    let mut need = Vec::new();
    if (plan.uploaded() > 0 || !plan.unreadable.is_empty()) && !(args.restore || args.no_restore) {
        let counts: Vec<String> = plan
            .uploads
            .iter()
            .map(|(name, s)| format!("{name} 里 {} 个", s.len()))
            .collect();
        need.push(format!(
            "--restore 或 --no-restore：要不要先把这台电脑上传过的会话从存储放回来（{}）",
            if counts.is_empty() {
                "存储现在读不了".to_string()
            } else {
                counts.join("，")
            }
        ));
    }
    if plan.replaced_files > 0 && args.replaced.is_none() {
        need.push(format!(
            "--replaced keep 或 --replaced delete：restore 替换下来的 {} 个旧副本留着还是删掉",
            plan.replaced_files
        ));
    }
    if !args.yes {
        need.push("--yes：确认卸载".to_string());
    }
    bail!("什么都没删。不在终端里运行，要用选项回答：\n  {}", need.join("\n  "))
}

fn ask(question: &str) -> anyhow::Result<String> {
    print!("{question} ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

fn ask_yes(question: &str) -> anyhow::Result<bool> {
    let answer = ask(&format!("{question}[y/N]"))?;
    Ok(matches!(answer.as_str(), "y" | "yes" | "是" | "要"))
}

fn ask_replaced(question: &str) -> anyhow::Result<Replaced> {
    loop {
        match ask(&format!("{question}[k 留着 / d 删掉]"))?.as_str() {
            "k" | "keep" | "留" | "留着" => return Ok(Replaced::Keep),
            "d" | "delete" | "删" | "删掉" => return Ok(Replaced::Delete),
            _ => println!("请输入 k 或 d"),
        }
    }
}

// ---- putting sessions back ----

/// Restores, through core, the newest version this computer uploaded of each session. A
/// session whose local copy has something storage lacks is left as it is; any other failure
/// stops the uninstall before anything is removed.
async fn put_back(plan: &Plan) -> anyhow::Result<()> {
    let mut core = match client::connect_existing().await {
        Some(core) => core,
        None if plan.autostart => {
            cli::start_core().await?;
            client::connect("cli").await?
        }
        None => bail!("core 没在运行，放不回会话；什么都没删。先运行 nekorisu core start，或者用 --no-restore"),
    };
    let (mut wrote, mut same, mut newer, mut failed) = (0, 0, Vec::new(), Vec::new());
    for (storage, sessions) in &plan.uploads {
        for (session, version) in sessions {
            let request = RestoreRequest {
                session: Some(cli::parse_ref(session)?),
                version: version.clone(),
                replace: false,
                storage: storage.clone(),
            };
            let task = match core.restore(request).await {
                Ok(task) => cli::wait_task(&mut core, task.into_inner(), false, None).await?,
                Err(status) => {
                    failed.push(format!("{session}：{}", status.message()));
                    continue;
                }
            };
            if task.state() == TaskState::Committed {
                let rows: Vec<Value> = serde_json::from_str(&task.result_json).unwrap_or_default();
                let count = |key: &str| rows.iter().map(|r| r[key].as_u64().unwrap_or(0)).sum::<u64>();
                if count("written") + count("completed") > 0 {
                    wrote += 1;
                } else {
                    same += 1;
                }
            } else if task.error.starts_with(restore::CONFLICT_LEAD) {
                newer.push(session.clone());
            } else {
                failed.push(format!("{session}：{}", first_line(&task.error)));
            }
        }
    }
    println!("从存储放回会话：{wrote} 个放回或补全，{same} 个本来就在");
    if !newer.is_empty() {
        println!("  {} 个本机这份比存储里的多，没动：{}", newer.len(), newer.join("、"));
    }
    if !failed.is_empty() {
        bail!(
            "{} 个会话没放回去，卸载停在这里，什么都没删：\n  {}\n可以再试一次，或者用 --no-restore 跳过",
            failed.len(),
            failed.join("\n  ")
        );
    }
    Ok(())
}

/// Per storage, the sessions this device uploaded: its operations log in storage lists every
/// upload with the manifest it wrote (`<device>-<n>`); the highest `n` is the newest.
async fn uploads(paths: &Paths, config: &config::Config) -> (BTreeMap<String, BTreeMap<String, String>>, Vec<String>) {
    let mut found = BTreeMap::new();
    let mut unreadable = Vec::new();
    let Some(device) = fs::read(paths.device_file())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["id"].as_str().map(str::to_string))
    else {
        return (found, unreadable);
    };
    for (name, storage) in &config.storage {
        match device_uploads(paths, name, storage, &device).await {
            Ok(sessions) if sessions.is_empty() => {}
            Ok(sessions) => {
                found.insert(name.clone(), sessions);
            }
            Err(err) => unreadable.push(format!("{name}：{err:#}")),
        }
    }
    (found, unreadable)
}

async fn device_uploads(
    paths: &Paths,
    name: &str,
    storage: &config::StorageConfig,
    device: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let store = storage::connect(paths, name, storage).await?;
    let dir = format!("{ROOT}/logs/{device}");
    let mut text = String::new();
    for entry in store.list(&dir).await? {
        if !entry.is_dir && entry.name.ends_with(".jsonl") {
            let bytes = store.get(&format!("{dir}/{}", entry.name), None).await?;
            text.push_str(&String::from_utf8_lossy(&bytes));
            text.push('\n');
        }
    }
    Ok(newest_uploads(&text, device))
}

fn newest_uploads(log: &str, device: &str) -> BTreeMap<String, String> {
    let mut newest: BTreeMap<String, (u64, String)> = BTreeMap::new();
    for line in log.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record["op"] != "archive" {
            continue;
        }
        let Some(manifest) = record["detail"]["manifest"].as_str() else {
            continue;
        };
        let stem = manifest
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .trim_end_matches(".json");
        let Some(n) = stem
            .strip_prefix(device)
            .and_then(|rest| rest.strip_prefix('-'))
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        for session in record["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let slot = newest.entry(session.to_string()).or_insert((0, String::new()));
            if n > slot.0 {
                *slot = (n, stem.to_string());
            }
        }
    }
    newest.into_iter().map(|(session, (_, id))| (session, id)).collect()
}

// ---- agents: MCP entries, skills, logs ----

/// Claude Code's own settings file: `~/.claude.json`, or in `CLAUDE_CONFIG_DIR` when set.
fn claude_config() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => paths::home_dir().join(".claude.json"),
    }
}

fn claude_entries(text: &str) -> Vec<Entry> {
    let Ok(doc) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut out = entries_in(Tool::Claude, &doc["mcpServers"], None);
    for (dir, project) in doc["projects"].as_object().into_iter().flatten() {
        out.extend(entries_in(
            Tool::Claude,
            &project["mcpServers"],
            Some(PathBuf::from(dir)),
        ));
    }
    out
}

fn codex_entries(text: &str) -> Vec<Entry> {
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::to_value(table) else {
        return Vec::new();
    };
    entries_in(Tool::Codex, &doc["mcp_servers"], None)
}

fn entries_in(tool: Tool, servers: &Value, project: Option<PathBuf>) -> Vec<Entry> {
    let mut out = Vec::new();
    for (name, server) in servers.as_object().into_iter().flatten() {
        let runs_us = server["command"]
            .as_str()
            .and_then(|c| Path::new(c).file_name())
            .is_some_and(|f| f == paths::BINARY_NAME || f == "nekorisu")
            && server["args"].as_array().is_some_and(|a| a.len() == 1 && a[0] == "mcp");
        if NAMES.contains(&name.as_str()) || runs_us {
            out.push(Entry {
                tool,
                name: name.clone(),
                project: project.clone(),
                remote: server["url"].is_string() || matches!(server["type"].as_str(), Some("http" | "sse")),
            });
        }
    }
    out
}

/// Signs out of remote entries (Claude Code then asks the server to revoke its grant), removes
/// every entry, the skills and Claude Code's logs for them.
async fn take_off_agents(plan: &Plan, todo: &mut Vec<String>) {
    for tool in [Tool::Claude, Tool::Codex] {
        let mine: Vec<&Entry> = plan.entries.iter().filter(|e| e.tool == tool).collect();
        let mut signed_out = Vec::new();
        for entry in mine.iter().filter(|e| e.remote) {
            if signed_out.contains(&entry.name) {
                continue;
            }
            signed_out.push(entry.name.clone());
            match run_tool(tool, &["mcp", "logout", &entry.name], None).await {
                Ok(()) => println!("已在 {} 里退出 {} 的登录", tool.label(), entry.name),
                Err(err) => todo.push(format!(
                    "{} 没能退出 {} 的登录（{err}）；服务端那条授权可能还在，去 panel 的“应用”页撤销",
                    tool.label(),
                    entry.name
                )),
            }
            if tool == Tool::Codex {
                todo.push(format!(
                    "在 Codex 里登录过 {} 的话：Codex 退出登录时不通知服务端，去服务端 panel 的“应用”页撤销 Codex 那条授权",
                    entry.name
                ));
            }
        }
        for entry in mine {
            let result = match (tool, &entry.project) {
                (Tool::Claude, None) => run_tool(tool, &["mcp", "remove", "-s", "user", &entry.name], None).await,
                (Tool::Claude, Some(dir)) if dir.is_dir() => {
                    run_tool(tool, &["mcp", "remove", "-s", "local", &entry.name], Some(dir)).await
                }
                (Tool::Claude, Some(_)) => Err("那个项目文件夹不在了".to_string()),
                (Tool::Codex, _) => run_tool(tool, &["mcp", "remove", &entry.name], None).await,
            };
            match result {
                Ok(()) => println!("已从 {} 删掉 MCP {}", tool.label(), entry.describe()),
                Err(err) => todo.push(format!(
                    "{} 里的 MCP {} 没删掉（{err}）",
                    tool.label(),
                    entry.describe()
                )),
            }
        }
    }
    for dir in plan.skills.iter().chain(&plan.mcp_logs) {
        if let Err(err) = fs::remove_dir_all(dir)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            todo.push(format!("{} 没删掉（{err}）", dir.display()));
        }
    }
    if !plan.skills.is_empty() {
        println!("已删掉 {} 个 skill", plan.skills.len());
    }
}

/// Runs `args` with the first of the tool's programs that does it.
async fn run_tool(tool: Tool, args: &[&str], dir: Option<&Path>) -> Result<(), String> {
    let programs = tool_programs(tool);
    if programs.is_empty() {
        return Err(format!("没找到 {} 命令", tool.command()));
    }
    let mut last = String::new();
    for program in programs {
        let mut command = tokio::process::Command::new(&program);
        command.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        match tokio::time::timeout(TOOL_TIMEOUT, command.output()).await {
            Ok(Ok(out)) if out.status.success() => return Ok(()),
            Ok(Ok(out)) => {
                let said = String::from_utf8_lossy(if out.stderr.is_empty() {
                    &out.stdout
                } else {
                    &out.stderr
                });
                last = format!(
                    "{} 退出码 {}：{}",
                    program.display(),
                    out.status.code().unwrap_or(-1),
                    first_line(&said)
                );
            }
            Ok(Err(err)) => last = format!("{} 运行不了：{err}", program.display()),
            Err(_) => last = format!("{} 一分钟没结束", program.display()),
        }
    }
    Err(last)
}

/// The agent's command-line programs, most likely to work first: the native installers' place
/// before `PATH`, where an old npm copy can come first and be broken.
fn tool_programs(tool: Tool) -> Vec<PathBuf> {
    let names: &[&str] = match (tool, cfg!(windows)) {
        (Tool::Claude, true) => &["claude.exe", "claude.cmd"],
        (Tool::Claude, false) => &["claude"],
        (Tool::Codex, true) => &["codex.exe", "codex.cmd"],
        (Tool::Codex, false) => &["codex"],
    };
    let mut out = Vec::new();
    if tool == Tool::Codex
        && let Some(path) = std::env::var_os("CODEX_CLI_PATH")
    {
        out.push(PathBuf::from(path));
    }
    let local_bin = paths::home_dir().join(".local").join("bin");
    out.extend(names.iter().map(|n| local_bin.join(n)));
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            out.extend(names.iter().map(|n| dir.join(n)));
        }
    }
    if tool == Tool::Codex
        && let Some(found) = restore::codex::find()
    {
        out.push(found);
    }
    let mut unique: Vec<PathBuf> = Vec::new();
    for program in out {
        if program.is_file() && !unique.contains(&program) {
            unique.push(program);
        }
    }
    unique
}

/// The skill folders of ours, and folders with our names holding someone else's skill.
fn skills() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut ours, mut foreign) = (Vec::new(), Vec::new());
    for home in [paths::claude_home(), paths::codex_home()] {
        for name in NAMES {
            let dir = home.join("skills").join(name);
            if !dir.is_dir() {
                continue;
            }
            let text = fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
            if skill_name(&text).as_deref() == Some(name) {
                ours.push(dir);
            } else {
                foreign.push(dir);
            }
        }
    }
    (ours, foreign)
}

/// The `name:` of a SKILL.md's front matter.
fn skill_name(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("name:") {
            return Some(value.trim().trim_matches(['"', '\'']).to_string());
        }
    }
    None
}

/// Claude Code's MCP logs for our entries: `<cache>/<project>/mcp-logs-<name>/`.
fn mcp_logs(names: &[String]) -> Vec<PathBuf> {
    let Some(cache) = claude_cache() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for project in fs::read_dir(&cache).into_iter().flatten().flatten() {
        for name in names {
            let dir = project.path().join(format!("mcp-logs-{name}"));
            if dir.is_dir() && !out.contains(&dir) {
                out.push(dir);
            }
        }
    }
    out
}

fn claude_cache() -> Option<PathBuf> {
    let home = paths::home_dir();
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Caches/claude-cli-nodejs"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("claude-cli-nodejs").join("Cache"))
    } else {
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"));
        Some(cache.join("claude-cli-nodejs"))
    }
}

// ---- core, login start, processes ----

/// Whether login start is registered for this install. On Windows the scheduled task is the
/// account's, so it counts as this install's only when the install script put the link there.
fn login_start(installed: bool) -> bool {
    autostart::registered() && (cfg!(unix) || installed)
}

async fn stop_everything(plan: &Plan) {
    if plan.autostart
        && let Err(err) = autostart::disable()
    {
        println!("没能取消开机自启：{err:#}");
    }
    if let Some(mut core) = client::connect_existing().await
        && let Err(err) = cli::stop_core(&mut core).await
    {
        println!("core 没能退出：{err:#}");
    }
    let mut ended = 0;
    for process in processes(&plan.paths, plan.link.as_deref()) {
        if end_process(process.pid).await {
            ended += 1;
        } else {
            println!("进程 {} 没结束：{}", process.pid, process.command);
        }
    }
    if ended > 0 {
        println!("已结束 {ended} 个 nekorisu 进程");
    }
}

/// Whether a command line or program path is this install's: a versioned binary, the link, or
/// the Windows launcher next to it.
fn belongs(command: &str, paths: &Paths, link: Option<&Path>) -> bool {
    let fold = |p: &Path| {
        let s = p.to_string_lossy().into_owned();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    let command = if cfg!(windows) {
        command.to_lowercase()
    } else {
        command.to_string()
    };
    // launchd and systemd start core by its resolved path, which can differ from the one
    // configured when a folder on the way is a link (macOS's /var is /private/var).
    let both = |p: PathBuf| {
        let real = p
            .parent()
            .and_then(|d| d.canonicalize().ok())
            .map(|d| d.join(p.file_name().unwrap_or_default()));
        std::iter::once(p).chain(real)
    };
    let sep = std::path::MAIN_SEPARATOR;
    if both(paths.versions_dir()).any(|v| command.starts_with(&format!("{}{sep}", fold(&v)))) {
        return true;
    }
    let Some(link) = link else {
        return false;
    };
    both(link.to_path_buf())
        .chain(both(link.with_file_name(paths::LAUNCHER_NAME)))
        .map(|p| fold(&p))
        .any(|p| command == p || command.starts_with(&format!("{p} ")))
}

fn processes(paths: &Paths, link: Option<&Path>) -> Vec<Process> {
    if link.is_none() && !paths.versions_dir().is_dir() {
        return Vec::new();
    }
    let me = std::process::id();
    list_processes()
        .into_iter()
        .filter(|p| p.pid != me && belongs(&p.command, paths, link))
        .collect()
}

/// This user's processes with their command lines.
#[cfg(unix)]
fn list_processes() -> Vec<Process> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "uid=", "-o", "args="])
        .output()
    else {
        return Vec::new();
    };
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() }.to_string();
    parse_ps(&String::from_utf8_lossy(&out.stdout), &uid)
}

#[cfg(unix)]
fn parse_ps(text: &str, uid: &str) -> Vec<Process> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some((pid, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some((owner, command)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        if let Ok(pid) = pid.parse()
            && owner == uid
        {
            out.push(Process {
                pid,
                command: command.trim().to_string(),
            });
        }
    }
    out
}

/// Processes named like ours, with their program path (empty for other accounts' processes).
#[cfg(windows)]
fn list_processes() -> Vec<Process> {
    let script = "Get-CimInstance Win32_Process -Filter \"Name='nekorisu.exe' or Name='nekorisu-start.exe'\" | ForEach-Object { \"$($_.ProcessId)`t$($_.ExecutablePath)\" }";
    let Ok(out) = powershell(script, &[]) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, path) = line.trim().split_once('\t')?;
            Some(Process {
                pid: pid.parse().ok()?,
                command: path.to_string(),
            })
        })
        .filter(|p| !p.command.is_empty())
        .collect()
}

#[cfg(unix)]
async fn end_process(pid: u32) -> bool {
    let Ok(id) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: sending a signal to a process id has no memory-safety preconditions.
    unsafe { libc::kill(id, libc::SIGTERM) };
    if client::wait_for_exit(pid, Duration::from_secs(3)).await {
        return true;
    }
    // SAFETY: as above.
    unsafe { libc::kill(id, libc::SIGKILL) };
    client::wait_for_exit(pid, Duration::from_secs(3)).await
}

#[cfg(windows)]
async fn end_process(pid: u32) -> bool {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let _ = std::process::Command::new(Path::new(&root).join("System32").join("taskkill.exe"))
        .args(["/pid", &pid.to_string(), "/f"])
        .output();
    client::wait_for_exit(pid, Duration::from_secs(5)).await
}

#[cfg(windows)]
fn powershell(script: &str, env: &[(&str, &std::ffi::OsStr)]) -> std::io::Result<std::process::Output> {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let mut command =
        std::process::Command::new(Path::new(&root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"));
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(std::process::Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    command.output()
}

// ---- files ----

/// Next to the link: what updates and installs leave there (`.old-<ms>`, `.nekorisu-new`) and,
/// on Windows, the launcher.
fn bin_extras(link: &Path) -> Vec<PathBuf> {
    let Some(dir) = link.parent() else {
        return Vec::new();
    };
    let binary = link.file_name().unwrap_or_default().to_string_lossy().to_string();
    let launcher = paths::LAUNCHER_NAME;
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let leftover = name.starts_with(&format!("{binary}.old-"))
            || name == format!("{binary}.nekorisu-new")
            || (cfg!(windows) && (name == launcher || name.starts_with(&format!("{launcher}.old-"))));
        if leftover {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

/// Windows: the link's folder while the user's PATH names it.
#[cfg(windows)]
fn path_entry(link: &Path) -> Option<PathBuf> {
    let dir = link.parent()?.to_path_buf();
    let script =
        "$k=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment'); if ($k) { $k.GetValue('Path', '') }";
    let out = powershell(script, &[]).ok()?;
    let want = dir.to_string_lossy().trim_end_matches('\\').to_lowercase();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .split(';')
        .any(|p| p.trim_end_matches('\\').to_lowercase() == want)
        .then_some(dir)
}

#[cfg(unix)]
fn path_entry(_link: &Path) -> Option<PathBuf> {
    None
}

/// Takes `dir` out of the user's PATH, keeping the value's kind, and tells running programs.
#[cfg(windows)]
fn drop_path_entry(dir: &Path) -> anyhow::Result<()> {
    let script = r#"
$dir = $env:NEKORISU_UNINSTALL_DIR.TrimEnd('\')
$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
try {
  $raw = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
  if (-not "$raw") { exit 0 }
  $kind = $key.GetValueKind('Path')
  $kept = @("$raw" -split ';' | Where-Object { $_ -ne '' -and [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ne $dir })
  if ($kept.Count -eq 0) { $key.DeleteValue('Path') } else { $key.SetValue('Path', ($kept -join ';'), $kind) }
} finally { $key.Close() }
try {
  Add-Type -Namespace NekorisuUninstall -Name Native -MemberDefinition '[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint msg, System.UIntPtr wParam, string lParam, uint flags, uint timeout, out System.UIntPtr result);'
  $result = [UIntPtr]::Zero
  [NekorisuUninstall.Native]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result) | Out-Null
} catch { }
"#;
    let out = powershell(script, &[("NEKORISU_UNINSTALL_DIR", dir.as_os_str())])?;
    if !out.status.success() {
        bail!("PowerShell 退出码 {}", out.status.code().unwrap_or(-1));
    }
    Ok(())
}

fn keep_replaced(paths: &Paths) -> anyhow::Result<PathBuf> {
    let home = paths::home_dir();
    let mut to = home.join(KEPT_REPLACED);
    let mut n = 2;
    while to.exists() {
        to = home.join(format!("{KEPT_REPLACED}-{n}"));
        n += 1;
    }
    if fs::rename(paths.replaced_dir(), &to).is_err() {
        copy_tree(&paths.replaced_dir(), &to)?;
        fs::remove_dir_all(paths.replaced_dir())?;
    }
    Ok(to)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn remove_files(plan: &Plan, todo: &mut Vec<String>) {
    let mut files: Vec<&PathBuf> = plan.link.iter().collect();
    files.extend(&plan.bin_extras);
    let mut stuck = Vec::new();
    for file in files {
        match fs::remove_file(file) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => stuck.push(file.clone()),
        }
    }
    if let Some(dir) = plan.link.as_deref().and_then(Path::parent) {
        let empty = fs::read_dir(dir).map(|mut d| d.next().is_none()).unwrap_or(false);
        if empty {
            let _ = fs::remove_dir(dir);
        }
        #[cfg(windows)]
        if plan.path_entry.is_some() {
            if empty {
                match drop_path_entry(dir) {
                    Ok(()) => println!("已从用户 PATH 去掉 {}", dir.display()),
                    Err(err) => todo.push(format!("用户 PATH 里的 {} 没去掉（{err:#}）", dir.display())),
                }
            } else {
                todo.push(format!("{} 里还有别的程序，用户 PATH 里的这一项没去掉", dir.display()));
            }
        }
        #[cfg(unix)]
        let _ = empty;
    }
    if plan.home && fs::remove_dir_all(&plan.paths.home).is_err() && plan.paths.home.exists() {
        stuck.push(plan.paths.home.clone());
    }
    if stuck.is_empty() {
        if plan.home || plan.link.is_some() {
            println!("已删掉程序和 {}", plan.paths.home.display());
        }
        return;
    }
    // A running program (this one, on Windows) can't be deleted: finish once it has exited.
    if cfg!(windows) && later(&stuck) {
        todo.push(format!(
            "{} 正被占用，这个命令退出后会在后台再删",
            stuck
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("、")
        ));
    } else {
        for path in stuck {
            todo.push(format!("{} 没删掉", path.display()));
        }
    }
}

/// Windows: a hidden `cmd` that keeps trying for a minute after this process has gone.
#[cfg(windows)]
fn later(paths: &[PathBuf]) -> bool {
    use std::os::windows::process::CommandExt;
    let mut steps = Vec::new();
    let mut checks = Vec::new();
    for path in paths {
        let p = path.display();
        if path.is_dir() {
            steps.push(format!("rmdir /s /q \"{p}\" 2>nul"));
        } else {
            steps.push(format!("del /f /q \"{p}\" 2>nul"));
        }
        checks.push(format!("if exist \"{p}\" set \"left=1\""));
    }
    let line = format!(
        "for /l %i in (1,1,30) do (ping -n 3 127.0.0.1 >nul & {} & set \"left=\" & {} & if not defined left exit)",
        steps.join(" & "),
        checks.join(" & ")
    );
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(Path::new(&root).join("System32").join("cmd.exe"))
        .raw_arg(format!("/d /q /v:off /c \"{line}\""))
        .current_dir(std::env::temp_dir())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(unix)]
fn later(_paths: &[PathBuf]) -> bool {
    false
}

/// What is still there after removing, looked at afresh.
fn leftovers(plan: &Plan) -> Vec<String> {
    let mut left = Vec::new();
    if login_start(plan.link.is_some()) {
        left.push(format!("登录时启动 core 的设置（{}）", autostart::describe()));
    }
    let running = processes(&plan.paths, plan.link.as_deref());
    if !running.is_empty() {
        left.push(format!("{} 个 nekorisu 进程还在运行", running.len()));
    }
    let mut entries = claude_entries(&fs::read_to_string(claude_config()).unwrap_or_default());
    entries.extend(codex_entries(
        &fs::read_to_string(paths::codex_home().join("config.toml")).unwrap_or_default(),
    ));
    for entry in entries {
        left.push(format!("{} 里的 MCP {}", entry.tool.label(), entry.describe()));
    }
    for dir in plan.skills.iter().chain(&plan.mcp_logs) {
        if dir.exists() {
            left.push(dir.display().to_string());
        }
    }
    if secrets::any_in_keychain() {
        left.push("钥匙串里的 nekorisu 条目".to_string());
    }
    if let Some(link) = &plan.link
        && (link.exists() || link.symlink_metadata().is_ok())
        && !cfg!(windows)
    {
        left.push(link.display().to_string());
    }
    if plan.paths.home.exists() && !cfg!(windows) {
        left.push(plan.paths.home.display().to_string());
    }
    left
}

/// Files under `dir` and their total size; links are not followed.
fn tally(dir: &Path) -> (usize, u64) {
    let mut files = 0;
    let mut bytes = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    (files, bytes)
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "6a1c0f3e-1111-4222-8333-444455556666";

    #[test]
    fn the_newest_upload_of_each_session_from_this_device() {
        let log = format!(
            r#"{{"seq":1,"device":"{DEV}","op":"archive","sessions":["claude-code:a"],"detail":{{"manifest":"nekorisu/v1/sessions/claude-code/a/manifests/{DEV}-1.json","bytes":10}}}}
{{"seq":2,"device":"{DEV}","op":"archive","sessions":["codex:b"],"detail":{{"manifest":"nekorisu/v1/sessions/codex/b/manifests/{DEV}-2.json","bytes":10}}}}
{{"seq":3,"device":"{DEV}","op":"archive","sessions":["claude-code:a"],"detail":{{"manifest":"nekorisu/v1/sessions/claude-code/a/manifests/{DEV}-12.json","bytes":10}}}}
{{"seq":4,"device":"{DEV}","op":"restore","sessions":["claude-code:z"],"detail":{{"manifest":"nekorisu/v1/sessions/claude-code/z/manifests/other-3.json","bytes":10}}}}
not json
{{"seq":5,"device":"{DEV}","op":"archive","sessions":["claude-code:a"],"detail":{{"manifest":"nekorisu/v1/sessions/claude-code/a/manifests/{DEV}-3.json","bytes":10}}}}"#
        );
        let found = newest_uploads(&log, DEV);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found["claude-code:a"], format!("{DEV}-12"));
        assert_eq!(found["codex:b"], format!("{DEV}-2"));
    }

    #[test]
    fn finds_our_entries_in_claude_code_settings_in_every_scope() {
        let text = r#"{
          "mcpServers": {
            "nekorisu": {"type": "stdio", "command": "/Users/me/.local/bin/nekorisu", "args": ["mcp"]},
            "nekorisu-server": {"type": "http", "url": "https://example.test/mcp"},
            "other": {"type": "stdio", "command": "node", "args": ["x.js"]}
          },
          "projects": {
            "/Users/me/work": {"mcpServers": {"neko": {"command": "C:\\Users\\me\\.local\\bin\\nekorisu.exe", "args": ["mcp"]}}},
            "/Users/me/else": {"mcpServers": {"tool": {"command": "nekorisu", "args": ["archive"]}}}
          }
        }"#;
        let found = claude_entries(text);
        let names: Vec<(&str, bool, Option<&Path>)> = found
            .iter()
            .map(|e| (e.name.as_str(), e.remote, e.project.as_deref()))
            .collect();
        assert!(names.contains(&("nekorisu", false, None)), "{names:?}");
        assert!(names.contains(&("nekorisu-server", true, None)), "{names:?}");
        assert_eq!(found.len(), if cfg!(windows) { 3 } else { 2 }, "{names:?}");
        assert!(claude_entries("not json").is_empty());
    }

    #[test]
    fn finds_our_entries_in_codex_settings() {
        let text = r#"
model = "x"
[mcp_servers.nekorisu]
command = "/home/me/.local/bin/nekorisu"
args = ["mcp"]
[mcp_servers.nekorisu-server]
url = "https://example.test/mcp"
[mcp_servers.other]
command = "npx"
"#;
        let found = codex_entries(text);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found
                .iter()
                .any(|e| e.name == "nekorisu-server" && e.remote && e.tool == Tool::Codex)
        );
        assert!(found.iter().any(|e| e.name == "nekorisu" && !e.remote));
    }

    #[test]
    fn a_skill_is_ours_only_by_its_front_matter_name() {
        assert_eq!(
            skill_name("---\nname: nekorisu\ndescription: x\n---\nbody").as_deref(),
            Some("nekorisu")
        );
        assert_eq!(
            skill_name("---\nname: \"nekorisu-server\"\n---\n").as_deref(),
            Some("nekorisu-server")
        );
        assert_eq!(skill_name("# no front matter\nname: nekorisu"), None);
        assert_eq!(skill_name("---\ndescription: x\n---\nname: nekorisu"), None);
    }

    #[test]
    fn only_this_installs_programs_belong() {
        let paths = Paths {
            home: PathBuf::from(if cfg!(windows) {
                r"C:\Users\me\.nekorisu"
            } else {
                "/home/me/.nekorisu"
            }),
        };
        let link = PathBuf::from(if cfg!(windows) {
            r"C:\Users\me\.local\bin\nekorisu.exe"
        } else {
            "/home/me/.local/bin/nekorisu"
        });
        let versioned = paths.versions_dir().join("0.2.1").join(paths::BINARY_NAME);
        assert!(belongs(&format!("{} daemon", versioned.display()), &paths, Some(&link)));
        assert!(belongs(&format!("{} mcp", link.display()), &paths, Some(&link)));
        assert!(belongs(&link.display().to_string(), &paths, Some(&link)));
        assert!(!belongs(&format!("{}x mcp", link.display()), &paths, Some(&link)));
        assert!(!belongs(
            "/usr/bin/vim /home/me/.local/bin/nekorisu",
            &paths,
            Some(&link)
        ));
        assert!(!belongs("/opt/target/debug/nekorisu mcp", &paths, Some(&link)));
        assert!(!belongs(&format!("{} mcp", link.display()), &paths, None));
        if cfg!(windows) {
            assert!(belongs(
                r"C:\Users\me\.local\bin\nekorisu-start.exe",
                &paths,
                Some(&link)
            ));
            assert!(belongs(
                r"c:\users\me\.nekorisu\versions\0.2.1\nekorisu.exe",
                &paths,
                Some(&link)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn reads_this_users_processes_from_ps() {
        let text = "  101   501 /home/me/.local/bin/nekorisu mcp\n  102     0 /home/me/.local/bin/nekorisu mcp\nbad line\n  103   501 /bin/zsh -l\n";
        let found = parse_ps(text, "501");
        assert_eq!(
            found,
            vec![
                Process {
                    pid: 101,
                    command: "/home/me/.local/bin/nekorisu mcp".into()
                },
                Process {
                    pid: 103,
                    command: "/bin/zsh -l".into()
                },
            ]
        );
    }

    #[test]
    fn leftovers_from_updates_next_to_the_link() {
        let dir = std::env::temp_dir().join(format!("nekorisu-uninstall-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        let link = dir.join(paths::BINARY_NAME);
        for name in [
            paths::BINARY_NAME.to_string(),
            format!("{}.old-123", paths::BINARY_NAME),
            format!("{}.nekorisu-new", paths::BINARY_NAME),
            format!("{}.sh", paths::BINARY_NAME),
            "claude".to_string(),
            paths::LAUNCHER_NAME.to_string(),
        ] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        let found: Vec<String> = bin_extras(&link)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        let mut want = vec![
            format!("{}.nekorisu-new", paths::BINARY_NAME),
            format!("{}.old-123", paths::BINARY_NAME),
        ];
        if cfg!(windows) {
            want.push(paths::LAUNCHER_NAME.to_string());
        }
        want.sort();
        assert_eq!(found, want);
        fs::remove_dir_all(&dir).unwrap();
    }
}
