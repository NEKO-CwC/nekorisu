//! The resident core: holds local state, owns storage connections and
//! the write queue, and serves the thin clients over a Unix socket (a named pipe
//! on Windows, see `ipc`). One core per user, guarded by a lock file.
//!
//! It also updates itself: a background task checks
//! once a day, stages the new version, waits until no task is running, then the
//! core stops serving and execs the new version in place, handing over its lock.
//! Windows has no exec: there the old core lets go of the lock and exits, and the
//! login launcher that runs it starts the new version (`supervise`).
//!
//! Core never starts itself again: launchd, systemd or that launcher do, after a failure.

pub(crate) mod device;
mod service;
mod tasks;

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use rand_core::RngCore;
use tokio::sync::watch;
use tonic::transport::Server;

use crate::config::{self, Config};
use crate::ipc;
use crate::paths::Paths;
use crate::proto::core_server::CoreServer;
use crate::timefmt::now_ms;
use crate::update::{self, Outcome, Startup};

pub async fn run() -> anyhow::Result<()> {
    let paths = Paths::from_env();

    // A version that keeps failing to start gets swapped back before anything else runs.
    if let Startup::Exec(link) = update::on_core_start(&paths) {
        let err = update::hand_over(&link, None);
        eprintln!("{} 启动不了旧版本 {}：{err}", now_ms(), link.display());
    }

    let run_dir = paths.run_dir();
    fs::create_dir_all(&run_dir).with_context(|| format!("建不了 {}", run_dir.display()))?;
    make_private(&paths.home)?;
    make_private(&run_dir)?;

    let lock = match adopt_lock(&paths) {
        Some(lock) => lock,
        None => OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(paths.lock())?,
    };
    if lock.try_lock().is_err() {
        // Not an error, and exiting cleanly keeps the system from starting it again: a second
        // start (the Windows task run once more, say) finds the first core running.
        eprintln!(
            "{} core 已经在运行（{}），这个不再启动",
            now_ms(),
            ipc::describe(&paths)
        );
        return Ok(());
    }
    let incoming = ipc::listen(&paths).await?;
    update::tidy_install(&paths);

    let device = device::load_or_create(&paths)?;
    let config = config::load(&paths).unwrap_or_else(|err| {
        eprintln!("config.toml 读不了，先用默认配置：{err}");
        Config::default()
    });
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let service = service::CoreService::new(paths.clone(), device, config, stop_tx);
    let idle = service.idle_probe();
    eprintln!(
        "{} core {} 已启动，pid {}，监听 {}",
        now_ms(),
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        ipc::describe(&paths)
    );
    // Listening and configured: if this is a freshly installed version, the switch worked.
    update::confirm_started(&paths);

    let (handover_tx, mut handover_rx) = watch::channel::<Option<PathBuf>>(None);
    let handover_target = handover_rx.clone();
    tokio::spawn(auto_update(paths.clone(), idle, handover_tx));

    let stop_signal = stop_signal()?;
    Server::builder()
        .add_service(CoreServer::new(service).max_encoding_message_size(64 << 20))
        .serve_with_incoming_shutdown(incoming, async move {
            tokio::select! {
                _ = stop_signal => {}
                _ = stop_rx.changed() => {}
                _ = handover_rx.changed() => {}
            }
        })
        .await?;

    ipc::after_shutdown(&paths);
    let handover = handover_target.borrow().clone();
    if let Some(link) = handover {
        eprintln!("{} core 交接给新版本", now_ms());
        let err = update::hand_over(&link, Some(lock));
        // Exec failed. Exiting with an error has launchd or systemd start the link again.
        anyhow::bail!("交接失败，core 退出，由系统重新启动：{err}");
    }
    eprintln!("{} core 已退出", now_ms());
    drop(lock);
    Ok(())
}

/// Resolves when the system asks core to stop: Ctrl-C or SIGTERM on Unix; Ctrl-C and the
/// console's close, logoff and shutdown events on Windows. The shutdown RPC works everywhere.
#[cfg(unix)]
fn stop_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    })
}

#[cfg(windows)]
fn stop_signal() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::windows::{ctrl_close, ctrl_logoff, ctrl_shutdown};
    let (mut close, mut logoff, mut shutdown) = (ctrl_close()?, ctrl_logoff()?, ctrl_shutdown()?);
    Ok(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = close.recv() => {}
            _ = logoff.recv() => {}
            _ = shutdown.recv() => {}
        }
    })
}

/// Takes over the lock an older core passed along when it exec'd this one.
#[cfg(unix)]
fn adopt_lock(paths: &Paths) -> Option<File> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let fd: i32 = std::env::var(update::LOCK_FD_ENV).ok()?.parse().ok()?;
    // SAFETY: the previous core passed this descriptor on purpose; it is checked below
    // to be our lock file before use, and forgotten (not closed) otherwise.
    let file = unsafe { File::from_raw_fd(fd) };
    let (got, want) = (file.metadata().ok(), fs::metadata(paths.lock()).ok());
    match (got, want) {
        (Some(got), Some(want)) if got.dev() == want.dev() && got.ino() == want.ino() => {
            // SAFETY: fcntl on a descriptor we now own; children must not inherit the lock.
            unsafe {
                let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFD);
                libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
            Some(file)
        }
        _ => {
            std::mem::forget(file);
            None
        }
    }
}

/// Windows passes no lock along: the old core lets go of it before starting the new one.
#[cfg(not(unix))]
fn adopt_lock(_paths: &Paths) -> Option<File> {
    None
}

/// Checks once a day (first check 5–35 minutes after start) and switches to a
/// newer version when no task is running. With `update.auto = false` it only records what's available.
async fn auto_update(paths: Paths, idle: impl Fn() -> bool, handover: watch::Sender<Option<PathBuf>>) {
    let mut delay = match std::env::var("NEKORISU_UPDATE_FIRST_CHECK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(secs) => Duration::from_secs(secs), // for end-to-end tests only
        None => jitter(Duration::from_secs(5 * 60), Duration::from_secs(30 * 60)),
    };
    loop {
        tokio::time::sleep(delay).await;
        delay = jitter(Duration::from_secs(24 * 3600), Duration::from_secs(3600));

        let roots = nekorisu::sign::embedded_root_keys();
        if roots.is_empty() {
            continue; // no root keys built in yet: self-update is off
        }
        let config = config::load(&paths).unwrap_or_default();
        let candidate = match update::check(&paths, &config.update, &roots).await {
            Ok(Outcome::Available(candidate)) => candidate,
            Ok(Outcome::UpToDate { .. }) => continue,
            Err(err) => {
                eprintln!("{} 检查更新失败：{err:#}", now_ms());
                continue;
            }
        };
        let version = candidate.manifest.version.clone();
        if !config.update.auto {
            eprintln!(
                "{} 有新版本 {version}；自动更新关着，运行 nekorisu update 更新",
                now_ms()
            );
            continue;
        }
        let Some(link) = update::installed_link(&paths, &config.update) else {
            eprintln!("{} 有新版本 {version}；这份不是安装脚本装的，不自动更新", now_ms());
            continue;
        };
        let Ok(Some(_guard)) = update::try_lock(&paths) else {
            continue; // `nekorisu update` is doing it right now
        };
        let binary = match update::stage(&paths, &candidate).await {
            Ok(binary) => binary,
            Err(err) => {
                eprintln!("{} 准备新版本 {version} 失败：{err:#}", now_ms());
                continue;
            }
        };
        while !idle() {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        if let Err(err) = update::install(&paths, &link, &binary, &version) {
            eprintln!("{} 换到新版本 {version} 失败：{err:#}", now_ms());
            continue;
        }
        let _ = handover.send(Some(link));
        return;
    }
}

fn jitter(base: Duration, spread: Duration) -> Duration {
    let spread_ms = spread.as_millis().max(1) as u64;
    base + Duration::from_millis(rand_core::OsRng.next_u64() % spread_ms)
}

/// Unix: only the owner may enter. Windows: a no-op; the profile folder is private already.
pub(crate) fn make_private(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}
