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

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, bail};
use nekorisu::release::{self, Asset, Manifest};
use nekorisu::sign::PublicKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::UpdateConfig;
use crate::paths::{self, Paths};
use crate::timefmt::now_ms;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TARGET: &str = env!("NEKORISU_TARGET");
const BUILTIN_URL: &str = "https://github.com/NEKO-CwC/nekorisu/releases/latest/download/{channel}.json";
const MAX_MANIFEST_BYTES: usize = 1 << 20;
const MAX_ASSET_BYTES: u64 = 512 << 20;
const BINARY_NAME: &str = "nekorisu";
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

/// Downloads, checks and unpacks the new version; returns the path of its binary.
pub async fn stage(paths: &Paths, candidate: &Candidate) -> anyhow::Result<PathBuf> {
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
        let (archive, binary) = (archive.clone(), binary.clone());
        tokio::task::spawn_blocking(move || unpack_binary(&archive, &binary))
    };
    let unpacked = unpack.await?;
    let _ = fs::remove_file(&archive);
    unpacked?;

    self_test(&binary, version).await?;
    Ok(binary)
}

fn unpack_binary(archive: &Path, binary: &Path) -> anyhow::Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(archive)?));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let is_binary =
            entry.header().entry_type().is_file() && entry.path()?.file_name().is_some_and(|n| n == BINARY_NAME);
        if !is_binary {
            continue;
        }
        let part = binary.with_extension("part");
        let mut out = fs::File::create(&part)?;
        let mut limited = (&mut entry).take(MAX_ASSET_BYTES);
        std::io::copy(&mut limited, &mut out)?;
        out.sync_all()?;
        set_mode(&part, 0o755)?;
        fs::rename(&part, binary)?;
        return Ok(());
    }
    bail!("压缩包里没有 {BINARY_NAME}")
}

/// Runs the new binary once, so a build that can't run on this machine never gets switched in.
async fn self_test(binary: &Path, version: &str) -> anyhow::Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(binary)
            .arg("--version")
            .env_remove(LOCK_FD_ENV)
            .output(),
    )
    .await
    .context("新版本自检超时")??;
    let printed = String::from_utf8_lossy(&output.stdout);
    let expected = format!("{BINARY_NAME} {version}");
    if !output.status.success() || printed.trim() != expected {
        bail!("新版本自检不通过：应该打印 {expected}，实际是 {}", printed.trim());
    }
    Ok(())
}

/// The symlink this installation runs through, if the install script made it.
pub fn installed_link(paths: &Paths, config: &UpdateConfig) -> Option<PathBuf> {
    let link = config.link.clone().unwrap_or_else(paths::default_link);
    let target = fs::read_link(&link).ok()?;
    let target = if target.is_absolute() {
        target
    } else {
        link.parent()?.join(target)
    };
    target.starts_with(paths.versions_dir()).then_some(link)
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

/// Backs up local state, writes the marker and repoints the symlink.
pub fn install(paths: &Paths, link: &Path, binary: &Path, to_version: &str) -> anyhow::Result<()> {
    let from_target = fs::read_link(link).with_context(|| format!("读不了 {}", link.display()))?;
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
    repoint(link, binary)
}

/// Atomically makes `link` point at `target` (new symlink, then rename over the old one).
fn repoint(link: &Path, target: &Path) -> anyhow::Result<()> {
    let tmp = link.with_extension("nekorisu-new");
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    fs::rename(&tmp, link).with_context(|| format!("换不了 {}", link.display()))?;
    Ok(())
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
    repoint(&marker.link, &marker.from_target)?;
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
pub const LOCK_FD_ENV: &str = "NEKORISU_LOCK_FD";

/// Replaces this process with `link daemon`, passing the lock along so no other
/// core can slip in between. Only returns on failure.
pub fn exec_core(link: &Path, lock: Option<&fs::File>) -> std::io::Error {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(link);
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

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
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

    fn tarball(script: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, "nekorisu", script.as_bytes()).unwrap();
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
        let binary = stage(&f.paths, &candidate).await.unwrap();
        assert_eq!(binary, f.paths.versions_dir().join("99.0.0").join("nekorisu"));

        // Pretend the install script put the current version in place.
        let old = f.paths.versions_dir().join(VERSION).join("nekorisu");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, "old").unwrap();
        let link = f.paths.home.join("bin-nekorisu");
        std::os::unix::fs::symlink(&old, &link).unwrap();
        let config = UpdateConfig {
            link: Some(link.clone()),
            ..UpdateConfig::default()
        };
        assert_eq!(installed_link(&f.paths, &config), Some(link.clone()));

        fs::write(f.paths.home.join("state.db"), "v1 state").unwrap();
        install(&f.paths, &link, &binary, "99.0.0").unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), binary);
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
        assert!(!f.paths.versions_dir().join("99.0.0").join("nekorisu").exists());
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
        let old = paths.versions_dir().join("0.0.1").join("nekorisu");
        let new = paths.versions_dir().join(VERSION).join("nekorisu");
        for p in [&old, &new] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "bin").unwrap();
        }
        let link = paths.home.join("bin-nekorisu");
        std::os::unix::fs::symlink(&new, &link).unwrap();
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
        assert_eq!(fs::read_link(&link).unwrap(), old);
        assert_eq!(fs::read_to_string(paths.home.join("state.db")).unwrap(), "old state");
        assert!(!paths.upgrade_marker().exists());
        assert!(State::load(&paths).bad_versions.contains(&VERSION.to_string()));
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
