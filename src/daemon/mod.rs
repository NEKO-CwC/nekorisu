//! The resident core: holds local state, owns storage connections and
//! the write queue, and serves the thin clients over a Unix socket. One core
//! per user, guarded by a lock file.
//!
//! It also updates itself: a background task checks
//! once a day, stages the new version, waits until no task is running, then the
//! core stops serving and execs the new version in place, handing over its lock.

pub(crate) mod device;
mod service;
mod tasks;

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use rand_core::RngCore;
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

use crate::config::{self, Config};
use crate::paths::Paths;
use crate::proto::core_server::CoreServer;
use crate::timefmt::now_ms;
use crate::update::{self, Outcome, Startup};

pub async fn run() -> anyhow::Result<()> {
    let paths = Paths::from_env();

    // A version that keeps failing to start gets swapped back before anything else runs.
    if let Startup::Exec(link) = update::on_core_start(&paths) {
        let err = update::exec_core(&link, None);
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
        // Not an error: launchd / systemd may start one while a client-started core runs.
        eprintln!(
            "{} core 已经在运行（{}），这个不再启动",
            now_ms(),
            paths.socket().display()
        );
        return Ok(());
    }
    let socket = paths.socket();
    let _ = fs::remove_file(&socket); // left over from a core that didn't exit cleanly
    let listener = UnixListener::bind(&socket).with_context(|| format!("监听不了 {}", socket.display()))?;

    let device = device::load_or_create(&paths)?;
    let config = config::load(&paths).unwrap_or_else(|err| {
        eprintln!("config.toml 读不了，先用默认配置：{err}");
        Config::default()
    });
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let service = service::CoreService::new(paths.clone(), device, config, stop_tx);
    let idle = service.idle_probe();
    eprintln!(
        "{} core {} 已启动，pid {}，socket {}",
        now_ms(),
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        socket.display()
    );
    // Listening and configured: if this is a freshly installed version, the switch worked.
    update::confirm_started(&paths);

    let (handover_tx, mut handover_rx) = watch::channel::<Option<PathBuf>>(None);
    let handover_target = handover_rx.clone();
    tokio::spawn(auto_update(paths.clone(), idle, handover_tx));

    let mut terminate = signal(SignalKind::terminate())?;
    Server::builder()
        .add_service(CoreServer::new(service).max_encoding_message_size(64 << 20))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
                _ = stop_rx.changed() => {}
                _ = handover_rx.changed() => {}
            }
        })
        .await?;

    let _ = fs::remove_file(&socket);
    let handover = handover_target.borrow().clone();
    if let Some(link) = handover {
        eprintln!("{} core 交接给新版本", now_ms());
        let err = update::exec_core(&link, Some(&lock));
        eprintln!("{} 交接失败，core 退出，下次使用时再启动：{err}", now_ms());
    }
    eprintln!("{} core 已退出", now_ms());
    drop(lock);
    Ok(())
}

/// Takes over the lock an older core passed along when it exec'd this one.
fn adopt_lock(paths: &Paths) -> Option<File> {
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

fn make_private(dir: &Path) -> std::io::Result<()> {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}
