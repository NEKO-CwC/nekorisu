//! nekorisu: archive agent sessions to the user's own storage, read them back
//! by line, and restore them byte-exact. The process layout is a resident core plus thin clients
//! over gRPC.

mod agents;
mod archive;
mod autostart;
mod cli;
mod client;
mod config;
mod daemon;
mod linemap;
mod mcp;
mod paths;
mod proto;
mod render;
mod secrets;
mod storage;
mod storage_cli;
#[cfg(test)]
mod testdav;
mod timefmt;
mod update;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nekorisu",
    version,
    about = "把 Agent 会话原样存到你自己的存储，按行读回来，逐字节恢复。"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 本机会话的数量和体积
    Scan {
        /// 输出 JSON
        #[arg(long)]
        json: bool,
    },
    /// 每个会话的大小、最后活动时间和上传状态
    Status(cli::StatusArgs),
    /// 按行读取一个会话
    Fetch(cli::FetchArgs),
    /// 搜索会话（开发中）
    Search { query: String },
    /// 上传会话（子 Agent 的会话一起）到你的存储，只传新增的部分
    Archive {
        /// 会话：<agent>:<id>，或者只写 id
        #[arg(required = true)]
        sessions: Vec<String>,
        /// 用哪个存储；只配了一个时不用写
        #[arg(long)]
        storage: Option<String>,
    },
    /// 把会话放回 Agent 原来的目录（开发中）
    Restore { session: String },
    /// 操作日志（开发中）
    Log,
    /// digest（开发中）
    Digest,
    /// 校验已上传的会话（开发中）
    Doctor,
    /// 配置存储：add 写地址、login 存密码、test 只读检查
    Storage {
        #[command(subcommand)]
        action: storage_cli::StorageAction,
    },
    /// 管理常驻的 core
    Core {
        #[command(subcommand)]
        action: cli::CoreAction,
    },
    /// 以 MCP 服务器方式运行（stdio），给 Claude Code、Codex 用
    Mcp,
    /// 运行常驻的 core（平时由 CLI 和 MCP 按需拉起）
    Daemon,
    /// 检查并更新到新版本（core 平时每天自己检查一次）
    Update {
        /// 只检查，不下载
        #[arg(long)]
        check: bool,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run(Cli::parse().command).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            // Errors from core already carry a readable message; show the full chain only for local ones.
            if err.downcast_ref::<tonic::Status>().is_some() {
                eprintln!("错误：{err}");
            } else {
                eprintln!("错误：{err:#}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Scan { json } => cli::scan(json).await,
        Command::Status(args) => cli::status(args).await,
        Command::Fetch(args) => cli::fetch(args).await,
        Command::Search { query } => cli::search(query).await,
        Command::Archive { sessions, storage } => cli::archive(sessions, storage).await,
        Command::Storage { action } => storage_cli::run(action).await,
        Command::Restore { session } => cli::restore(session).await,
        Command::Log => cli::not_yet("log"),
        Command::Digest => cli::not_yet("digest"),
        Command::Doctor => cli::doctor().await,
        Command::Core { action } => cli::core(action).await,
        Command::Mcp => mcp::run().await,
        Command::Daemon => daemon::run().await,
        Command::Update { check } => cli::update(check).await,
    }
}
