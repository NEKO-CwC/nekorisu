//! A device installed the way users install: a release built from the binary under test,
//! served from a local folder to the real install script (`install.sh`, `install.ps1`). It
//! uses the real login start (launchd; a scheduled task on Windows; a systemd unit's files on
//! Linux), the real keychain on macOS and the real user PATH on Windows, all under names of
//! its own (`NEKORISU_E2E_NAME`, read by debug builds only), and removes whatever is left of
//! them when the scenario ends, passed or not.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};

use crate::world::{Device, bin};

pub const BINARY: &str = if cfg!(windows) { "nekorisu.exe" } else { "nekorisu" };
const LAUNCHER: &str = "nekorisu-start.exe";

/// Where the install script puts the command: the default, under the device's home.
pub fn link(device: &Device) -> PathBuf {
    device.home().join(".local").join("bin").join(BINARY)
}

fn scripts() -> PathBuf {
    std::env::var_os("NEKORISU_E2E_SCRIPTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts")))
}

/// The release asset the install script asks for on this machine.
fn asset() -> String {
    let target = if cfg!(windows) {
        "x86_64-pc-windows-msvc".to_string()
    } else {
        let os = if cfg!(target_os = "macos") {
            "apple-darwin"
        } else {
            "unknown-linux-musl"
        };
        format!("{}-{os}", std::env::consts::ARCH)
    };
    format!("nekorisu-{target}.tar.gz")
}

/// A release folder like GitHub's: the archive and SHA256SUMS.
fn build_release(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut files = vec![(BINARY, bin())];
    if cfg!(windows) {
        files.push((LAUNCHER, bin().with_file_name(LAUNCHER)));
    }
    let archive = dir.join(asset());
    let gz = flate2::write::GzEncoder::new(std::fs::File::create(&archive)?, flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    for (name, from) in files {
        let bytes = std::fs::read(&from).with_context(|| format!("no {}", from.display()))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes.as_slice())?;
    }
    tar.into_inner()?.finish()?.flush()?;
    let sum = hex::encode(Sha256::digest(std::fs::read(&archive)?));
    std::fs::write(dir.join("SHA256SUMS"), format!("{sum}  {}\n", asset()))?;
    Ok(())
}

/// Installs with the real script. macOS: login start through launchd, which starts core.
/// Linux (no user session bus here): the script skips login start; its unit files are written
/// as `core enable` would leave them without one. Windows: the scheduled task could not pass
/// this device's folders to core, so the script skips it; a task with the test name stands in
/// for it, and core is started from the installed command.
pub async fn install(device: &mut Device) -> anyhow::Result<String> {
    let release = device.root.join("release");
    build_release(&release)?;
    let mut extra: Vec<(&str, OsString)> =
        vec![("NEKORISU_BIN_DIR", link(device).parent().context("a folder")?.into())];
    if !cfg!(target_os = "macos") {
        extra.push(("NEKORISU_NO_AUTOSTART", "1".into()));
    }
    let mut command = if cfg!(windows) {
        extra.push(("NEKORISU_DOWNLOAD_BASE", release.clone().into()));
        let mut c = Command::new(powershell());
        c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(scripts().join("install.ps1"));
        c
    } else {
        extra.push(("NEKORISU_DOWNLOAD_BASE", format!("file://{}", release.display()).into()));
        let mut c = Command::new("sh");
        c.arg(scripts().join("install.sh"));
        c
    };
    command.envs(device.env()).envs(extra).current_dir(&device.root);
    let out = tokio::task::spawn_blocking(move || command.output()).await??;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    ensure!(out.status.success(), "the install script failed:\n{text}");
    ensure!(
        link(device).exists(),
        "no {} after installing:\n{text}",
        link(device).display()
    );

    let name = device.system.clone().context("not a system device")?;
    if cfg!(target_os = "linux") {
        let units = device.home().join(".config").join("systemd").join("user");
        let unit = format!("nekorisu-e2e-{name}.service");
        std::fs::create_dir_all(units.join("default.target.wants"))?;
        std::fs::write(units.join(&unit), "[Service]\nExecStart=/bin/true\n")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(units.join(&unit), units.join("default.target.wants").join(&unit))?;
    }
    if cfg!(windows) {
        let made = Command::new(system32("schtasks.exe"))
            .args([
                "/create",
                "/tn",
                &task(&name),
                "/tr",
                "cmd /c exit",
                "/sc",
                "once",
                "/st",
                "00:00",
                "/f",
            ])
            .output()?;
        ensure!(
            made.status.success(),
            "schtasks /create: {}",
            String::from_utf8_lossy(&made.stderr)
        );
    }
    // launchd starts core on macOS; a machine with no login session (a CI runner) can't, and
    // then core is started from the installed command, as elsewhere.
    let by_launchd = cfg!(target_os = "macos")
        && matches!(
            tokio::time::timeout(std::time::Duration::from_secs(20), device.wait_for_core()).await,
            Ok(Ok(()))
        );
    if !by_launchd {
        device.spawn_installed(&["daemon"], true)?;
        device.wait_for_core().await?;
    }
    ensure!(
        registered(device),
        "login start is not registered after installing:\n{text}"
    );
    Ok(text)
}

pub fn task(name: &str) -> String {
    format!("nekorisu-e2e-{name}")
}

#[cfg(unix)]
fn label(name: &str) -> String {
    format!("io.github.neko-cwc.nekorisu.e2e-{name}")
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

#[cfg_attr(unix, allow(dead_code))]
fn system32(program: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    Path::new(&root).join("System32").join(program)
}

#[cfg_attr(unix, allow(dead_code))]
fn powershell() -> PathBuf {
    system32(r"WindowsPowerShell\v1.0\powershell.exe")
}

/// Whether this device's login start is registered, as the system sees it.
pub fn registered(device: &Device) -> bool {
    let Some(name) = &device.system else {
        return false;
    };
    #[cfg(unix)]
    {
        if cfg!(target_os = "macos") {
            let plist = device
                .home()
                .join("Library/LaunchAgents")
                .join(format!("{}.plist", label(name)));
            let loaded = Command::new("launchctl")
                .args(["print", &format!("gui/{}/{}", uid(), label(name))])
                .output()
                .is_ok_and(|o| o.status.success());
            plist.exists() || loaded
        } else {
            let units = device.home().join(".config").join("systemd").join("user");
            let unit = format!("nekorisu-e2e-{name}.service");
            units.join(&unit).exists()
                || units
                    .join("default.target.wants")
                    .join(&unit)
                    .symlink_metadata()
                    .is_ok()
        }
    }
    #[cfg(windows)]
    {
        Command::new(system32("schtasks.exe"))
            .args(["/query", "/tn", &task(name)])
            .output()
            .is_ok_and(|o| o.status.success())
    }
}

/// Keychain items under this device's test service names (macOS; elsewhere none).
pub fn keychain_items(device: &Device) -> usize {
    let Some(name) = &device.system else {
        return 0;
    };
    if !cfg!(target_os = "macos") {
        return 0;
    }
    ["storage", "source"]
        .iter()
        .filter(|kind| {
            Command::new("security")
                .args(["find-generic-password", "-s", &format!("nekorisu-e2e-{name}-{kind}")])
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .count()
}

/// The user's PATH (Windows, from the registry; elsewhere empty).
pub fn user_path() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let script = "$k=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment'); if ($k) { $k.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) }";
    let Ok(out) = Command::new(powershell())
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .split(';')
        .map(|p| p.trim_end_matches('\\').to_lowercase())
        .filter(|p| !p.is_empty())
        .collect()
}

pub fn in_user_path(dir: &Path) -> bool {
    let want = dir.to_string_lossy().trim_end_matches('\\').to_lowercase();
    user_path().contains(&want)
}

/// Whether a process is still there (a zombie counts on Unix).
pub fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        Command::new(system32("tasklist.exe"))
            .args(["/fi", &format!("PID eq {pid}"), "/nh", "/fo", "csv"])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
    }
}

/// Removes whatever the scenario left of the real system's things: login start, keychain
/// items, the PATH entry, Claude Code's own keychain item for this device's settings folder.
pub fn clean_up(device: &Device) {
    // The developer's Claude Code keeps its MCP logs under the real home, per working folder:
    // remove those of this scenario's folders.
    if cfg!(target_os = "macos")
        && (device.claude_signed_in || !device.extra_path.is_empty())
        && let (Some(home), Some(scenario)) = (
            std::env::var_os("HOME"),
            device
                .root
                .parent()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned()),
        )
    {
        let cache = PathBuf::from(home).join("Library/Caches/claude-cli-nodejs");
        for entry in std::fs::read_dir(cache).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().contains(&scenario) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let mut services: Vec<String> = Vec::new();
    if device.claude_signed_in {
        services.extend(claude_keychain_services(device));
    }
    if let Some(name) = &device.system {
        services.extend(
            ["storage", "source"]
                .iter()
                .map(|kind| format!("nekorisu-e2e-{name}-{kind}")),
        );
    }
    if cfg!(target_os = "macos") {
        for service in services {
            for _ in 0..20 {
                let gone = Command::new("security")
                    .args(["delete-generic-password", "-s", &service])
                    .output()
                    .map_or(true, |o| !o.status.success());
                if gone {
                    break;
                }
            }
        }
    }
    let Some(name) = &device.system else {
        return;
    };
    #[cfg(unix)]
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{}", uid(), label(name))])
            .output();
    }
    #[cfg(windows)]
    {
        let _ = Command::new(system32("schtasks.exe"))
            .args(["/delete", "/tn", &task(name), "/f"])
            .output();
        if let Some(dir) = link(device).parent()
            && in_user_path(dir)
        {
            let script = r#"
$dir = $env:E2E_DIR.TrimEnd('\')
$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
try {
  $raw = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
  $kind = $key.GetValueKind('Path')
  $kept = @("$raw" -split ';' | Where-Object { $_ -ne '' -and $_.TrimEnd('\') -ne $dir })
  $key.SetValue('Path', ($kept -join ';'), $kind)
} finally { $key.Close() }
"#;
            let _ = Command::new(powershell())
                .args(["-NoProfile", "-NonInteractive", "-Command", script])
                .env("E2E_DIR", dir)
                .output();
        }
    }
}

/// Claude Code keeps sign-ins for a settings folder other than the default in a keychain item
/// named after that folder: `Claude Code-credentials-<first 8 hex of its SHA-256>`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn claude_keychain_services(device: &Device) -> Vec<String> {
    let dir = device.claude_home();
    let mut names = vec![dir.to_string_lossy().into_owned()];
    if let Ok(real) = dir.canonicalize() {
        names.push(real.to_string_lossy().into_owned());
    }
    names
        .iter()
        .map(|n| {
            format!(
                "Claude Code-credentials-{}",
                &hex::encode(Sha256::digest(n.as_bytes()))[..8]
            )
        })
        .collect()
}

/// A program of the developer's: the variable if set, then PATH, then `~/.local/bin`.
pub fn program(name: &str, variable: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(variable).map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd")]
    } else {
        vec![name.to_string()]
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        dirs.push(PathBuf::from(home).join(".local").join("bin"));
    }
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

pub fn claude() -> Option<PathBuf> {
    program("claude", "NEKORISU_E2E_CLAUDE")
}

pub fn codex() -> Option<PathBuf> {
    program("codex", "CODEX_CLI_PATH")
}

/// Runs an agent's program with the device's folders.
pub async fn agent(
    device: &Device,
    program: &Path,
    args: &[&str],
    extra: &[(&str, &Path)],
) -> anyhow::Result<std::process::Output> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .envs(device.env())
        .current_dir(&device.root)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    for (key, value) in extra {
        command.env(key, value);
    }
    let out = tokio::time::timeout(std::time::Duration::from_secs(120), command.output())
        .await
        .with_context(|| format!("{} {} took too long", program.display(), args.join(" ")))??;
    Ok(out)
}

pub fn first_lines(out: &std::process::Output) -> String {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text.lines().take(20).collect::<Vec<_>>().join("\n")
}
