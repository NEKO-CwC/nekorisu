//! Self-update.
//!
//! check   fetch `<channel>.json` from the configured addresses, then GitHub;
//!         verify root key → signing-key certificate → manifest
//! stage   download this platform's archive, check size and SHA-256, unpack
//!         into ~/.nekorisu/versions/<version>/, run `--version` on it
//! install back up local state, write the upgrade marker, repoint the symlink
//! startup the new core counts its attempts in the marker; after too many
//!         failed starts it puts the old version back
//!
//! Only installs made by the install script (a symlink into versions/) are
//! updated; a binary built from source or installed another way is left alone.
//!
//! Windows swaps only the platform pieces: the "symlink" is a hard link (or copy) of the
//! versioned `nekorisu.exe` plus a pointer file naming it (`repoint_copy`), and the handover
//! exits so the login launcher starts the new core, instead of exec'ing it (`hand_over`).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use nekorisu::release::{self, Asset, Manifest};
use nekorisu::sign::PublicKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::UpdateConfig;
use crate::paths::{self, BINARY_NAME, Paths};
use crate::timefmt::now_ms;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TARGET: &str = env!("NEKORISU_TARGET");
const BUILTIN_URL: &str = "https://github.com/NEKO-CwC/nekorisu/releases/latest/download/{channel}.json";
const MAX_MANIFEST_BYTES: usize = 1 << 20;
const MAX_ASSET_BYTES: u64 = 512 << 20;
/// Unpacked from the release archive into the version's folder; the first one must be there.
#[cfg(unix)]
const ARCHIVE_FILES: &[&str] = &[BINARY_NAME];
#[cfg(windows)]
const ARCHIVE_FILES: &[&str] = &[BINARY_NAME, paths::LAUNCHER_NAME];
/// A new core that fails to come up this many times in a row is rolled back.
const MAX_START_ATTEMPTS: u32 = 3;
/// Local state copied aside before switching versions (the new version may migrate it).
const STATE_FILES: &[&str] = &["state.db", "state.db-wal", "state.db-shm"];

/// What the updater remembers between runs (~/.nekorisu/update.json).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub last_check_ms: i64,
    /// Highest signing-key certificate serial accepted; lower ones are refused.
    pub cert_serial: u64,
    /// Highest version ever seen on the channel; an older manifest is treated as stale.
    pub highest_seen: Option<String>,
    /// Versions that failed to start here; never tried again automatically.
    pub bad_versions: Vec<String>,
    pub available: Option<String>,
    pub last_error: Option<String>,
}

impl State {
    pub fn load(paths: &Paths) -> Self {
        fs::read(paths.update_state())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> anyhow::Result<()> {
        write_atomic(&paths.update_state(), &serde_json::to_vec_pretty(self)?)
    }
}

#[derive(Debug)]
pub struct Candidate {
    pub manifest: Manifest,
    pub asset: Asset,
}

#[derive(Debug)]
pub enum Outcome {
    UpToDate { latest: String },
    Available(Candidate),
}

/// The addresses to try, in order: configured ones (mirrors), then GitHub.
pub fn sources(config: &UpdateConfig) -> Vec<String> {
    let mut urls = config.urls.clone();
    urls.push(BUILTIN_URL.replace("{channel}", &config.channel));
    urls
}

pub fn http_client() -> anyhow::Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .user_agent(format!("nekorisu/{VERSION}"))
        .connect_timeout(Duration::from_secs(15))
        .build()?)
}

pub async fn check(paths: &Paths, config: &UpdateConfig, roots: &[PublicKey]) -> anyhow::Result<Outcome> {
    let mut state = State::load(paths);
    let result = check_inner(&sources(config), &config.channel, roots, &mut state).await;
    state.last_check_ms = now_ms();
    match &result {
        Ok(Outcome::Available(c)) => {
            state.available = Some(c.manifest.version.clone());
            state.last_error = None;
        }
        Ok(Outcome::UpToDate { .. }) => {
            state.available = None;
            state.last_error = None;
        }
        Err(err) => state.last_error = Some(format!("{err:#}")),
    }
    state.save(paths)?;
    result
}

async fn check_inner(
    urls: &[String],
    channel: &str,
    roots: &[PublicKey],
    state: &mut State,
) -> anyhow::Result<Outcome> {
    if roots.is_empty() {
        bail!("这个程序里没有内置的根公钥（维护者还没生成），自动更新关闭");
    }
    let client = http_client()?;
    let mut errors = Vec::new();
    let mut verified = None;
    for url in urls {
        match fetch_limited(&client, url, MAX_MANIFEST_BYTES).await {
            Ok(bytes) => match release::verify(&bytes, roots, now_ms(), state.cert_serial) {
                Ok(v) => {
                    verified = Some(v);
                    break;
                }
                Err(err) => errors.push(format!("{url}：{err}")),
            },
            Err(err) => errors.push(format!("{url}：{err:#}")),
        }
    }
    let Some(verified) = verified else {
        bail!("所有更新地址都不可用：\n  {}", errors.join("\n  "));
    };
    let manifest = verified.manifest;
    if manifest.channel != channel {
        bail!("版本说明是 {} 渠道的，这台设备用的是 {channel}", manifest.channel);
    }
    state.cert_serial = state.cert_serial.max(verified.cert_serial);

    let offered = semver::Version::parse(&manifest.version).context("版本说明里的版本号不合法")?;
    let highest = state
        .highest_seen
        .as_deref()
        .and_then(|v| semver::Version::parse(v).ok());
    if highest.as_ref().is_none_or(|h| offered > *h) {
        state.highest_seen = Some(manifest.version.clone());
    }
    let current = semver::Version::parse(VERSION).expect("own version is valid");
    // Only ever move forward, and never to a version older than one already seen
    // (a stale or replayed manifest), nor to one that failed to start here.
    let stale = highest.is_some_and(|h| offered < h);
    if offered <= current || stale || state.bad_versions.contains(&manifest.version) {
        return Ok(Outcome::UpToDate {
            latest: manifest.version,
        });
    }
    let asset = manifest
        .assets
        .get(TARGET)
        .cloned()
        .with_context(|| format!("{} 没有 {TARGET} 平台的版本", manifest.version))?;
    Ok(Outcome::Available(Candidate { manifest, asset }))
}

async fn fetch_limited(client: &reqwest::Client, url: &str, limit: usize) -> anyhow::Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(60))
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > limit {
            bail!("内容太大（超过 {limit} 字节）");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Downloads, checks and unpacks the new version, then runs it once; returns the path of its binary.
pub async fn stage(paths: &Paths, candidate: &Candidate) -> anyhow::Result<PathBuf> {
    let binary = download(paths, candidate).await?;
    self_test(&binary, &candidate.manifest.version).await?;
    Ok(binary)
}

/// `stage` without the self-test.
async fn download(paths: &Paths, candidate: &Candidate) -> anyhow::Result<PathBuf> {
    let version = &candidate.manifest.version;
    let asset = &candidate.asset;
    if asset.size > MAX_ASSET_BYTES {
        bail!("{version} 的压缩包大得不正常（{} 字节）", asset.size);
    }
    let dir = paths.versions_dir().join(version);
    let binary = dir.join(BINARY_NAME);
    fs::create_dir_all(&dir)?;
    let archive = dir.join("download.tar.gz.part");

    let client = http_client()?;
    let mut response = client
        .get(&asset.url)
        .timeout(Duration::from_secs(600))
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("下载不了 {}", asset.url))?;
    let mut file = fs::File::create(&archive)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = response.chunk().await? {
        size += chunk.len() as u64;
        if size > asset.size {
            bail!("下载的内容比版本说明里写的大");
        }
        hasher.update(&chunk);
        std::io::Write::write_all(&mut file, &chunk)?;
    }
    drop(file);
    let sha256 = hex::encode(hasher.finalize());
    if size != asset.size || sha256 != asset.sha256 {
        let _ = fs::remove_file(&archive);
        bail!("下载的文件和版本说明对不上（大小或 SHA-256 不一致），已丢弃");
    }

    let unpack = {
        let (archive, dir) = (archive.clone(), dir.clone());
        tokio::task::spawn_blocking(move || unpack(&archive, &dir, ARCHIVE_FILES))
    };
    let unpacked = unpack.await?;
    let _ = fs::remove_file(&archive);
    unpacked?;
    Ok(binary)
}

/// Unpacks the files called `names` (wherever they sit in the archive) into `dir`.
/// The first name is required; the others are taken when present.
fn unpack(archive: &Path, dir: &Path, names: &[&str]) -> anyhow::Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(archive)?));
    let mut found = Vec::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = {
            let path = entry.path()?;
            names
                .iter()
                .copied()
                .find(|n| path.file_name().is_some_and(|f| f == *n))
        };
        let Some(name) = name else {
            continue;
        };
        let target = dir.join(name);
        let part = target.with_extension("part");
        let mut out = fs::File::create(&part)?;
        let mut limited = (&mut entry).take(MAX_ASSET_BYTES);
        std::io::copy(&mut limited, &mut out)?;
        out.sync_all()?;
        set_mode(&part, 0o755)?;
        fs::rename(&part, &target)?;
        found.push(name);
    }
    if !found.contains(&names[0]) {
        bail!("压缩包里没有 {}", names[0]);
    }
    Ok(())
}

/// Runs the new binary once, so a build that can't run on this machine never gets switched in.
async fn self_test(binary: &Path, version: &str) -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new(binary);
    command.arg("--version");
    #[cfg(unix)]
    command.env_remove(LOCK_FD_ENV);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .context("新版本自检超时")??;
    let printed = String::from_utf8_lossy(&output.stdout);
    let expected = format!("nekorisu {version}");
    if !output.status.success() || printed.trim() != expected {
        bail!("新版本自检不通过：应该打印 {expected}，实际是 {}", printed.trim());
    }
    Ok(())
}

/// The link this installation runs through, if the install script made it.
pub fn installed_link(paths: &Paths, config: &UpdateConfig) -> Option<PathBuf> {
    let link = config.link.clone().unwrap_or_else(paths::default_link);
    let target = link_target(paths, &link)?;
    target.starts_with(paths.versions_dir()).then_some(link)
}

/// The versioned binary `link` stands for: the symlink's target on Unix; on Windows
/// what the pointer file names, as long as the link itself is there.
pub fn link_target(paths: &Paths, link: &Path) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        let _ = paths;
        let target = fs::read_link(link).ok()?;
        Some(if target.is_absolute() {
            target
        } else {
            link.parent()?.join(target)
        })
    }
    #[cfg(not(unix))]
    copy_target(paths, link)
}

#[cfg_attr(unix, allow(dead_code))]
fn copy_target(paths: &Paths, link: &Path) -> Option<PathBuf> {
    if !link.is_file() {
        return None;
    }
    let text = fs::read_to_string(paths.current_pointer()).ok()?;
    // Tolerate a BOM or a line break from an editor or PowerShell.
    let text = text.trim_start_matches('\u{feff}').trim_end();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// Marker written just before switching; the new core reads it on startup.
#[derive(Debug, Serialize, Deserialize)]
pub struct Marker {
    pub from_version: String,
    pub from_target: PathBuf,
    pub to_version: String,
    pub link: PathBuf,
    pub attempts: u32,
    pub at_ms: i64,
}

/// Backs up local state, writes the marker and repoints the link.
pub fn install(paths: &Paths, link: &Path, binary: &Path, to_version: &str) -> anyhow::Result<()> {
    let from_target = link_target(paths, link).with_context(|| format!("读不了 {} 指向的版本", link.display()))?;
    let backup = paths.backup_dir().join(VERSION);
    fs::create_dir_all(&backup)?;
    for name in STATE_FILES {
        let src = paths.home.join(name);
        if src.exists() {
            fs::copy(&src, backup.join(name))?;
        }
    }
    let marker = Marker {
        from_version: VERSION.to_string(),
        from_target,
        to_version: to_version.to_string(),
        link: link.to_path_buf(),
        attempts: 0,
        at_ms: now_ms(),
    };
    fs::create_dir_all(paths.run_dir())?;
    write_atomic(&paths.upgrade_marker(), &serde_json::to_vec_pretty(&marker)?)?;
    repoint(paths, link, binary)
}

/// Makes `link` stand for `target`.
fn repoint(paths: &Paths, link: &Path, target: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let _ = paths;
        repoint_symlink(link, target)
    }
    #[cfg(not(unix))]
    repoint_copy(paths, link, target)
}

/// Atomically makes `link` point at `target` (new symlink, then rename over the old one).
#[cfg(unix)]
fn repoint_symlink(link: &Path, target: &Path) -> anyhow::Result<()> {
    let tmp = link.with_extension("nekorisu-new");
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    fs::rename(&tmp, link).with_context(|| format!("换不了 {}", link.display()))?;
    Ok(())
}

/// The Windows link: ordinary users can't make symlinks, so `link` is a hard link to `target`
/// (a copy when they sit on different volumes) and the pointer file names `target`. A running
/// binary can't be replaced but can be renamed: the new one is put next to it, the old one is
/// renamed aside to `<link>.old-<ms>` (removed on a later core start), the new one takes its
/// name, then the pointer is written. On failure the old one goes back.
#[cfg_attr(unix, allow(dead_code))]
fn repoint_copy(paths: &Paths, link: &Path, target: &Path) -> anyhow::Result<()> {
    let fresh = with_suffix(link, ".nekorisu-new");
    let _ = fs::remove_file(&fresh);
    if fs::hard_link(target, &fresh).is_err() {
        fs::copy(target, &fresh).with_context(|| format!("复制不了 {}", target.display()))?;
    }
    let aside = with_suffix(link, &format!(".old-{}", now_ms()));
    let moved = match fs::rename(link, &aside) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            let _ = fs::remove_file(&fresh);
            return Err(err).with_context(|| format!("挪不开 {}", link.display()));
        }
    };
    let switched = fs::rename(&fresh, link)
        .with_context(|| format!("换不了 {}", link.display()))
        .and_then(|()| write_atomic(&paths.current_pointer(), target.to_string_lossy().as_bytes()));
    if let Err(err) = switched {
        let _ = fs::remove_file(&fresh);
        if moved {
            let _ = fs::remove_file(link);
            let _ = fs::rename(&aside, link);
        }
        return Err(err);
    }
    Ok(())
}

/// `link` with `suffix` added to its file name.
#[cfg_attr(unix, allow(dead_code))]
fn with_suffix(link: &Path, suffix: &str) -> PathBuf {
    let mut name = link.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    link.with_file_name(name)
}

/// Removes binaries earlier switches renamed aside, once nothing runs them any more.
#[cfg_attr(unix, allow(dead_code))]
fn sweep_aside(link: &Path) {
    let (Some(dir), Some(name)) = (link.parent(), link.file_name()) else {
        return;
    };
    let prefix = format!("{}.old-", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = fs::remove_file(entry.path()); // still running: next time
        }
    }
}

/// Copies the current version's login launcher next to `link` when it differs. The one there
/// is usually running (it runs core), so it is renamed aside first, like the binary in
/// `repoint_copy`; the new one takes over at the next login.
#[cfg_attr(unix, allow(dead_code))]
fn refresh_launcher(paths: &Paths, link: &Path) {
    let (Some(target), Some(dir)) = (copy_target(paths, link), link.parent()) else {
        return;
    };
    let Some(source) = target.parent().map(|d| d.join(paths::LAUNCHER_NAME)) else {
        return;
    };
    let installed = dir.join(paths::LAUNCHER_NAME);
    let (Ok(new), old) = (fs::read(&source), fs::read(&installed)) else {
        return;
    };
    if old.as_ref().is_ok_and(|old| *old == new) {
        return;
    }
    let aside = with_suffix(&installed, &format!(".old-{}", now_ms()));
    if old.is_ok() && fs::rename(&installed, &aside).is_err() {
        return;
    }
    if fs::copy(&source, &installed).is_err() && old.is_ok() {
        let _ = fs::rename(&aside, &installed);
    }
}

/// On core start, Windows only: removes binaries renamed aside by earlier switches and
/// refreshes the login launcher from the current version. Errors are ignored.
pub fn tidy_install(paths: &Paths) {
    #[cfg(windows)]
    {
        let config = crate::config::load(paths).unwrap_or_default();
        let link = config.update.link.unwrap_or_else(paths::default_link);
        sweep_aside(&link);
        sweep_aside(&link.with_file_name(paths::LAUNCHER_NAME));
        refresh_launcher(paths, &link);
    }
    #[cfg(not(windows))]
    let _ = paths;
}

pub enum Startup {
    Continue,
    /// Rolled back: run this (the old version) instead.
    Exec(PathBuf),
}

/// First thing a core does. Counts start attempts of a freshly installed version
/// and puts the previous one back after too many failures.
pub fn on_core_start(paths: &Paths) -> Startup {
    let Ok(bytes) = fs::read(paths.upgrade_marker()) else {
        return Startup::Continue;
    };
    let Ok(mut marker) = serde_json::from_slice::<Marker>(&bytes) else {
        let _ = fs::remove_file(paths.upgrade_marker());
        return Startup::Continue;
    };
    if marker.to_version != VERSION {
        // Not the version being tried (say, an old binary run directly): leave it to that one.
        return Startup::Continue;
    }
    marker.attempts += 1;
    if marker.attempts <= MAX_START_ATTEMPTS {
        if let Ok(bytes) = serde_json::to_vec_pretty(&marker) {
            let _ = write_atomic(&paths.upgrade_marker(), &bytes);
        }
        return Startup::Continue;
    }
    match rollback(paths, &marker) {
        Ok(()) => {
            eprintln!(
                "{} 新版本 {} 连续 {} 次没能启动，已换回 {}",
                now_ms(),
                marker.to_version,
                MAX_START_ATTEMPTS,
                marker.from_version
            );
            Startup::Exec(marker.link)
        }
        Err(err) => {
            eprintln!("{} 回滚到 {} 失败：{err:#}", now_ms(), marker.from_version);
            Startup::Continue
        }
    }
}

fn rollback(paths: &Paths, marker: &Marker) -> anyhow::Result<()> {
    repoint(paths, &marker.link, &marker.from_target)?;
    let backup = paths.backup_dir().join(&marker.from_version);
    for name in STATE_FILES {
        let saved = backup.join(name);
        let live = paths.home.join(name);
        if saved.exists() {
            fs::copy(&saved, &live)?;
        } else {
            let _ = fs::remove_file(&live);
        }
    }
    let mut state = State::load(paths);
    if !state.bad_versions.contains(&marker.to_version) {
        state.bad_versions.push(marker.to_version.clone());
    }
    state.available = None;
    state.save(paths)?;
    fs::remove_file(paths.upgrade_marker())?;
    Ok(())
}

/// Called once the new core is listening: the switch worked. Drops the marker
/// and keeps only this version and the one it replaced.
pub fn confirm_started(paths: &Paths) {
    let Ok(bytes) = fs::read(paths.upgrade_marker()) else {
        return;
    };
    let Ok(marker) = serde_json::from_slice::<Marker>(&bytes) else {
        return;
    };
    if marker.to_version != VERSION {
        return;
    }
    let _ = fs::remove_file(paths.upgrade_marker());
    eprintln!("{} 已从 {} 换到 {}", now_ms(), marker.from_version, VERSION);
    prune_versions(paths, &[VERSION, &marker.from_version]);
}

fn prune_versions(paths: &Paths, keep: &[&str]) {
    let Ok(entries) = fs::read_dir(paths.versions_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if semver::Version::parse(&name).is_ok() && !keep.contains(&name.as_str()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    let Ok(entries) = fs::read_dir(paths.backup_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name.as_str()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Environment variable through which a core hands its lock file to the version it execs.
#[cfg(unix)]
pub const LOCK_FD_ENV: &str = "NEKORISU_LOCK_FD";

/// Hands this core's place to `link daemon`. Unix: exec, passing the lock along so no other
/// core can slip in between; returns only on failure. Windows has no exec: core lets go of
/// the lock and exits with `supervise::RESTART`, and the login launcher that runs it starts
/// the link again at once; never returns.
pub fn hand_over(link: &Path, lock: Option<fs::File>) -> std::io::Error {
    #[cfg(unix)]
    {
        exec_core(link, lock.as_ref())
    }
    #[cfg(not(unix))]
    {
        drop(lock);
        eprintln!("{} core 退出，由启动器运行 {}", now_ms(), link.display());
        std::process::exit(crate::supervise::RESTART)
    }
}

/// Replaces this process with `link daemon`, passing the lock along so no other
/// core can slip in between. Only returns on failure.
#[cfg(unix)]
fn exec_core(link: &Path, lock: Option<&fs::File>) -> std::io::Error {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(link);
    command.arg("daemon");
    match lock {
        Some(file) => {
            let fd = file.as_raw_fd();
            // SAFETY: fcntl on a descriptor we own; clears close-on-exec so the lock survives exec.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            command.env(LOCK_FD_ENV, fd.to_string());
        }
        None => {
            command.env_remove(LOCK_FD_ENV);
        }
    }
    command.exec()
}

/// Takes the update lock, so the CLI and core never switch versions at the same time.
pub fn try_lock(paths: &Paths) -> anyhow::Result<Option<fs::File>> {
    fs::create_dir_all(paths.run_dir())?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.update_lock())?;
    Ok(file.try_lock().is_ok().then_some(file))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Unix file mode; a no-op on Windows.
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nekorisu::release::{MANIFEST_SCHEMA, SignedRelease, SigningCert};
    use nekorisu::sign::{CONTEXT_RELEASE_CERT, CONTEXT_RELEASE_MANIFEST, SecretKey};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const DAY: i64 = 86_400_000;

    fn temp_home() -> Paths {
        let home = std::env::temp_dir().join(format!("nekorisu-update-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();
        Paths { home }
    }

    /// Serves fixed files over plain HTTP; returns the base URL.
    async fn serve(files: BTreeMap<String, Vec<u8>>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let files = files.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let response = match files.get(&path) {
                        Some(body) => {
                            let mut r = format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            r.extend_from_slice(body);
                            r
                        }
                        None => b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = stream.write_all(&response).await;
                });
            }
        });
        base
    }

    /// A release archive: `ARCHIVE_FILES` with `script` as the binary.
    fn tarball(script: &str) -> Vec<u8> {
        let files: Vec<(&str, &[u8])> = ARCHIVE_FILES
            .iter()
            .map(|name| {
                (
                    *name,
                    if *name == BINARY_NAME {
                        script.as_bytes()
                    } else {
                        b"launcher"
                    },
                )
            })
            .collect();
        tar_of(&files)
    }

    fn tar_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (name, body) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, *body).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    struct Fixture {
        paths: Paths,
        config: UpdateConfig,
        roots: Vec<PublicKey>,
    }

    /// A channel offering `version`, whose archive for this platform is `archive`
    /// but whose manifest claims `claimed` as the archive's bytes.
    async fn fixture(version: &str, archive: Vec<u8>, claimed: &[u8]) -> Fixture {
        let root = SecretKey::generate();
        let signer = SecretKey::generate();
        let cert = SigningCert {
            purpose: "release".into(),
            key: signer.public().to_base64(),
            serial: 1,
            not_before_ms: 0,
            not_after_ms: now_ms() + DAY,
        };
        let mut files = BTreeMap::new();
        files.insert(format!("/nekorisu-{TARGET}.tar.gz"), archive);
        let base = serve(files.clone()).await;
        let mut assets = BTreeMap::new();
        assets.insert(
            TARGET.to_string(),
            Asset {
                url: format!("{base}/nekorisu-{TARGET}.tar.gz"),
                sha256: nekorisu::sign::sha256_hex(claimed),
                size: claimed.len() as u64,
            },
        );
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA,
            channel: "stable".into(),
            version: version.into(),
            released_at_ms: now_ms(),
            assets,
            notes_url: None,
        };
        let signed = SignedRelease {
            cert: root.sign(CONTEXT_RELEASE_CERT, &serde_json::to_vec(&cert).unwrap()),
            manifest: signer.sign(CONTEXT_RELEASE_MANIFEST, &serde_json::to_vec(&manifest).unwrap()),
        };
        files.insert("/stable.json".into(), serde_json::to_vec(&signed).unwrap());
        let base = serve(files).await;
        let config = UpdateConfig {
            urls: vec![format!("{base}/stable.json")],
            ..UpdateConfig::default()
        };
        Fixture {
            paths: temp_home(),
            config,
            roots: vec![root.public()],
        }
    }

    #[tokio::test]
    async fn checks_stages_and_installs() {
        let archive = tarball("#!/bin/sh\necho nekorisu 99.0.0\n");
        let f = fixture("99.0.0", archive.clone(), &archive).await;
        let Outcome::Available(candidate) = check(&f.paths, &f.config, &f.roots).await.unwrap() else {
            panic!("expected an update");
        };
        #[cfg(unix)]
        let binary = stage(&f.paths, &candidate).await.unwrap();
        #[cfg(windows)]
        let binary = {
            // A shell script can't run here, so the self-test refuses it; the rest is the same.
            let binary = download(&f.paths, &candidate).await.unwrap();
            assert!(self_test(&binary, "99.0.0").await.is_err());
            assert!(binary.with_file_name(paths::LAUNCHER_NAME).is_file());
            binary
        };
        assert_eq!(binary, f.paths.versions_dir().join("99.0.0").join(BINARY_NAME));

        // Pretend the install script put the current version in place.
        let old = f.paths.versions_dir().join(VERSION).join(BINARY_NAME);
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, "old").unwrap();
        let link = f.paths.home.join("bin").join(BINARY_NAME);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        repoint(&f.paths, &link, &old).unwrap();
        let config = UpdateConfig {
            link: Some(link.clone()),
            ..UpdateConfig::default()
        };
        assert_eq!(installed_link(&f.paths, &config), Some(link.clone()));

        fs::write(f.paths.home.join("state.db"), "v1 state").unwrap();
        install(&f.paths, &link, &binary, "99.0.0").unwrap();
        assert_eq!(link_target(&f.paths, &link), Some(binary.clone()));
        assert_eq!(fs::read(&link).unwrap(), fs::read(&binary).unwrap());
        assert!(f.paths.upgrade_marker().exists());
        assert!(f.paths.backup_dir().join(VERSION).join("state.db").exists());
        assert_eq!(State::load(&f.paths).available.as_deref(), Some("99.0.0"));
    }

    #[tokio::test]
    async fn refuses_an_archive_that_does_not_match_the_manifest() {
        let archive = tarball("#!/bin/sh\necho nekorisu 99.0.0\n");
        let other = tarball("#!/bin/sh\necho evil\n");
        let f = fixture("99.0.0", other, &archive).await;
        let Outcome::Available(candidate) = check(&f.paths, &f.config, &f.roots).await.unwrap() else {
            panic!("expected an update");
        };
        assert!(stage(&f.paths, &candidate).await.is_err());
        assert!(!f.paths.versions_dir().join("99.0.0").join(BINARY_NAME).exists());
    }

    #[tokio::test]
    async fn refuses_a_binary_that_fails_its_self_test() {
        let archive = tarball("#!/bin/sh\necho nekorisu 1.2.3\n");
        let f = fixture("99.0.0", archive.clone(), &archive).await;
        let Outcome::Available(candidate) = check(&f.paths, &f.config, &f.roots).await.unwrap() else {
            panic!("expected an update");
        };
        assert!(stage(&f.paths, &candidate).await.is_err());
    }

    #[tokio::test]
    async fn never_goes_back_to_an_older_or_failed_version() {
        let archive = tarball("#!/bin/sh\necho nekorisu 99.0.0\n");
        let f = fixture("99.0.0", archive.clone(), &archive).await;
        let state = State {
            highest_seen: Some("100.0.0".into()),
            ..State::default()
        };
        state.save(&f.paths).unwrap();
        assert!(matches!(
            check(&f.paths, &f.config, &f.roots).await.unwrap(),
            Outcome::UpToDate { .. }
        ));

        let state = State {
            bad_versions: vec!["99.0.0".into()],
            ..State::default()
        };
        state.save(&f.paths).unwrap();
        assert!(matches!(
            check(&f.paths, &f.config, &f.roots).await.unwrap(),
            Outcome::UpToDate { .. }
        ));
    }

    #[tokio::test]
    async fn untrusted_roots_mean_no_update() {
        let archive = tarball("#!/bin/sh\necho nekorisu 99.0.0\n");
        let f = fixture("99.0.0", archive.clone(), &archive).await;
        let stranger = vec![SecretKey::generate().public()];
        let result = check_inner(&f.config.urls, "stable", &stranger, &mut State::default()).await;
        assert!(result.is_err(), "signed by a root this build doesn't trust");
        assert!(
            check_inner(&f.config.urls, "stable", &[], &mut State::default())
                .await
                .is_err()
        );
        assert!(
            check_inner(&f.config.urls, "beta", &f.roots, &mut State::default())
                .await
                .is_err(),
            "wrong channel"
        );
    }

    #[test]
    fn rolls_back_after_repeated_failed_starts() {
        let paths = temp_home();
        let old = paths.versions_dir().join("0.0.1").join(BINARY_NAME);
        let new = paths.versions_dir().join(VERSION).join(BINARY_NAME);
        for (p, body) in [(&old, "old bin"), (&new, "new bin")] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        let link = paths.home.join("bin").join(BINARY_NAME);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        repoint(&paths, &link, &new).unwrap();
        fs::create_dir_all(paths.backup_dir().join("0.0.1")).unwrap();
        fs::write(paths.backup_dir().join("0.0.1").join("state.db"), "old state").unwrap();
        fs::write(paths.home.join("state.db"), "migrated state").unwrap();
        fs::create_dir_all(paths.run_dir()).unwrap();
        let marker = Marker {
            from_version: "0.0.1".into(),
            from_target: old.clone(),
            to_version: VERSION.into(),
            link: link.clone(),
            attempts: 0,
            at_ms: now_ms(),
        };
        fs::write(paths.upgrade_marker(), serde_json::to_vec(&marker).unwrap()).unwrap();

        for _ in 0..MAX_START_ATTEMPTS {
            assert!(matches!(on_core_start(&paths), Startup::Continue));
        }
        let Startup::Exec(exec) = on_core_start(&paths) else {
            panic!("expected a rollback")
        };
        assert_eq!(exec, link);
        assert_eq!(link_target(&paths, &link), Some(old.clone()));
        assert_eq!(fs::read_to_string(&link).unwrap(), "old bin");
        assert_eq!(fs::read_to_string(paths.home.join("state.db")).unwrap(), "old state");
        assert!(!paths.upgrade_marker().exists());
        assert!(State::load(&paths).bad_versions.contains(&VERSION.to_string()));
    }

    #[test]
    fn unpacks_the_named_files_wherever_they_sit() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        let names = ["nekorisu.exe", "nekorisu-start.exe"];
        fs::write(
            &archive,
            tar_of(&[
                ("pkg/nekorisu.exe", b"bin"),
                ("nekorisu-start.exe", b"launcher"),
                ("README", b"other"),
            ]),
        )
        .unwrap();
        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        unpack(&archive, &out, &names).unwrap();
        assert_eq!(fs::read(out.join("nekorisu.exe")).unwrap(), b"bin");
        assert_eq!(fs::read(out.join("nekorisu-start.exe")).unwrap(), b"launcher");
        assert!(!out.join("README").exists());

        // The launcher is optional, the binary is not.
        fs::write(&archive, tar_of(&[("nekorisu-start.exe", b"launcher")])).unwrap();
        assert!(unpack(&archive, &out, &names).is_err());
    }

    /// The Windows link (hard link or copy plus pointer file), exercised on every system.
    #[test]
    fn copy_link_switches_versions_and_sets_the_old_binary_aside() {
        let paths = temp_home();
        let v1 = paths.versions_dir().join("0.0.1").join(BINARY_NAME);
        let v2 = paths.versions_dir().join("0.0.2").join(BINARY_NAME);
        for (p, body) in [(&v1, "one"), (&v2, "two")] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
            fs::write(p.with_file_name(paths::LAUNCHER_NAME), format!("launcher {body}")).unwrap();
        }
        let bin = paths.home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let link = bin.join(BINARY_NAME);
        let launcher = bin.join(paths::LAUNCHER_NAME);
        let aside = || {
            fs::read_dir(&bin)
                .unwrap()
                .filter(|e| {
                    let name = e.as_ref().unwrap().file_name();
                    name.to_string_lossy().starts_with(&format!("{BINARY_NAME}.old-"))
                })
                .count()
        };

        repoint_copy(&paths, &link, &v1).unwrap();
        assert_eq!(fs::read_to_string(&link).unwrap(), "one");
        assert_eq!(copy_target(&paths, &link), Some(v1.clone()));
        assert_eq!(aside(), 0, "nothing to set aside on a first install");
        refresh_launcher(&paths, &link);
        assert_eq!(fs::read_to_string(&launcher).unwrap(), "launcher one");

        // The running binary is renamed aside, never written over.
        let running = fs::File::open(&link).unwrap();
        repoint_copy(&paths, &link, &v2).unwrap();
        drop(running);
        assert_eq!(fs::read_to_string(&link).unwrap(), "two");
        assert_eq!(copy_target(&paths, &link), Some(v2.clone()));
        assert_eq!(
            fs::read_to_string(&v1).unwrap(),
            "one",
            "the version folder is untouched"
        );
        assert_eq!(aside(), 1);
        assert!(!bin.join(format!("{BINARY_NAME}.nekorisu-new")).exists());
        refresh_launcher(&paths, &link);
        assert_eq!(fs::read_to_string(&launcher).unwrap(), "launcher two");

        sweep_aside(&link);
        assert_eq!(aside(), 0);

        // A pointer written by PowerShell may carry a BOM and a line break.
        fs::write(paths.current_pointer(), format!("\u{feff}{}\r\n", v1.display())).unwrap();
        assert_eq!(copy_target(&paths, &link), Some(v1.clone()));
        // No link, no installation, whatever the pointer says.
        fs::remove_file(&link).unwrap();
        assert_eq!(copy_target(&paths, &link), None);
    }

    #[test]
    fn confirming_a_start_clears_the_marker_and_old_versions() {
        let paths = temp_home();
        for v in ["0.0.1", "0.0.2", VERSION] {
            fs::create_dir_all(paths.versions_dir().join(v)).unwrap();
        }
        fs::create_dir_all(paths.run_dir()).unwrap();
        let marker = Marker {
            from_version: "0.0.2".into(),
            from_target: PathBuf::new(),
            to_version: VERSION.into(),
            link: PathBuf::new(),
            attempts: 1,
            at_ms: 0,
        };
        fs::write(paths.upgrade_marker(), serde_json::to_vec(&marker).unwrap()).unwrap();
        confirm_started(&paths);
        assert!(!paths.upgrade_marker().exists());
        assert!(!paths.versions_dir().join("0.0.1").exists());
        assert!(paths.versions_dir().join("0.0.2").exists());
        assert!(paths.versions_dir().join(VERSION).exists());
    }
}
