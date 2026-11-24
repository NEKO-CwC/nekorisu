//! Connecting a thin client (CLI, MCP, ...) to core over the Unix
//! socket. If no core is running, start one in the background and wait for it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use crate::paths::Paths;
use crate::proto::core_client::CoreClient;
use crate::proto::{HelloRequest, PROTOCOL_VERSION, ShutdownRequest};
use crate::update;

pub type Client = CoreClient<Channel>;

const MAX_MESSAGE: usize = 64 << 20;

pub async fn connect(kind: &str) -> anyhow::Result<Client> {
    let paths = Paths::from_env();
    for attempt in 0..2 {
        let mut client = match open(&paths.socket()).await {
            Ok(client) => client,
            Err(_) => start_and_open(&paths).await?,
        };
        let hello = client
            .hello(HelloRequest {
                protocol_version: PROTOCOL_VERSION,
                client_version: env!("CARGO_PKG_VERSION").to_string(),
                client_kind: kind.to_string(),
            })
            .await
            .context("core 没有回应握手")?
            .into_inner();
        if hello.protocol_version == PROTOCOL_VERSION {
            return Ok(client);
        }
        if attempt == 0 {
            // An older core is still running: ask it to exit, then start ours.
            let _ = client.shutdown(ShutdownRequest {}).await;
            wait_until_gone(&paths.socket()).await;
        }
    }
    bail!("core 的协议版本和这个程序对不上，请运行 nekorisu core stop 后重试")
}

/// Connects only if a core is already running; never starts one.
pub async fn connect_existing() -> Option<Client> {
    open(&Paths::from_env().socket()).await.ok()
}

async fn open(socket: &Path) -> anyhow::Result<Client> {
    let socket = socket.to_path_buf();
    // The URI is required but unused: the connector always dials the socket.
    let channel = Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move { Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(socket).await?)) }
        }))
        .await?;
    Ok(CoreClient::new(channel).max_decoding_message_size(MAX_MESSAGE))
}

async fn start_and_open(paths: &Paths) -> anyhow::Result<Client> {
    spawn_core(paths)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if let Ok(client) = open(&paths.socket()).await {
            return Ok(client);
        }
        if Instant::now() > deadline {
            bail!("core 没有启动起来，日志在 {}", paths.core_log().display());
        }
    }
}

fn spawn_core(paths: &Paths) -> anyhow::Result<()> {
    std::fs::create_dir_all(paths.run_dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.core_log())?;
    let exe = core_executable(paths)?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("daemon")
        .env_remove(update::LOCK_FD_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    // Its own process group, so the agent stopping its MCP server doesn't take core down.
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().context("启动不了 core")?;
    Ok(())
}

/// Starts core through the install script's symlink when that is what we are
/// running, so a later version switch (which repoints the link) takes effect.
fn core_executable(paths: &Paths) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("找不到自己的可执行文件")?;
    let config = crate::config::load(paths).unwrap_or_default();
    if let Some(link) = update::installed_link(paths, &config.update) {
        let same = match (std::fs::canonicalize(&link), std::fs::canonicalize(&exe)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if same {
            return Ok(link);
        }
    }
    Ok(exe)
}

async fn wait_until_gone(socket: &Path) {
    for _ in 0..100 {
        if !socket.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
