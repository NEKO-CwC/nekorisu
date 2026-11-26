//! Connecting a thin client (CLI, MCP, ...) to core over the local socket or pipe (`ipc`).
//!
//! Clients never start core themselves. Keeping it running is the system's job: launchd on
//! macOS, `systemd --user` on Linux, a scheduled task and its launcher on Windows
//! (`autostart`). A client that can't reach core says so and points at that, rather than
//! starting a core whose life would then hang on the client's (an agent may end its MCP
//! server's whole process tree, or job, when it exits).

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use hyper_util::rt::TokioIo;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use crate::ipc;
use crate::paths::Paths;
use crate::proto::core_client::CoreClient;
use crate::proto::{HelloRequest, HelloResponse, PROTOCOL_VERSION};

pub type Client = CoreClient<Channel>;

const MAX_MESSAGE: usize = 64 << 20;

pub async fn connect(kind: &str) -> anyhow::Result<Client> {
    let paths = Paths::from_env();
    let Ok(mut client) = open(&paths).await else {
        bail!(
            "core 没在运行：可能还没启动，也可能意外退出了（日志在 {}）。启动它：nekorisu core start",
            paths.core_log().display()
        );
    };
    let hello = hello(&mut client, kind).await?;
    if hello.protocol_version > PROTOCOL_VERSION {
        bail!(
            "core（{}）比这个程序新，协议对不上；重启 Agent 或重新打开终端，用新版本",
            hello.core_version
        );
    }
    if hello.protocol_version < PROTOCOL_VERSION {
        bail!(
            "core（{}）比这个程序旧，协议对不上；运行 nekorisu core stop，再运行 nekorisu core start",
            hello.core_version
        );
    }
    Ok(client)
}

pub async fn hello(client: &mut Client, kind: &str) -> anyhow::Result<HelloResponse> {
    let response = client
        .hello(HelloRequest {
            protocol_version: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            client_kind: kind.to_string(),
        })
        .await
        .context("core 没有回应握手")?;
    Ok(response.into_inner())
}

/// Connects only if a core is already running.
pub async fn connect_existing() -> Option<Client> {
    open(&Paths::from_env()).await.ok()
}

async fn open(paths: &Paths) -> anyhow::Result<Client> {
    let paths = paths.clone();
    // The URI is required but unused: the connector always dials the local socket or pipe.
    let channel = Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let paths = paths.clone();
            async move { Ok::<_, std::io::Error>(TokioIo::new(ipc::connect(&paths).await?)) }
        }))
        .await?;
    Ok(CoreClient::new(channel).max_decoding_message_size(MAX_MESSAGE))
}

/// Waits up to `limit` for core's process to be gone. Its socket or pipe goes a moment before
/// the process does, and whoever restarts core next must not find the old one still holding
/// the lock. False if it is still there.
pub async fn wait_for_exit(pid: u32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while process_exists(pid) {
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// Whether `pid` is still a process. A zombie counts, so on Unix this turns false only once its
/// parent (launchd, systemd) has collected it and knows it is gone.
#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists and may be signalled.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    alive || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

    // SAFETY: plain Win32 calls; the handle, when there is one, is closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            // Gone, or by now another account's process: core runs as this user.
            return false;
        }
        let running = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
        CloseHandle(handle);
        running
    }
}
