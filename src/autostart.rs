//! Starting core at login: a launchd agent on
//! macOS, a `systemd --user` unit on Linux. Both run the install script's
//! symlink, so a version switch takes effect on the next start.
//!
//! Clients still start core on demand when it isn't running; a second core
//! finds the lock taken and exits quietly, so the two never fight.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::config;
use crate::paths::{self, Paths};
use crate::update;

const LABEL: &str = "io.github.neko-cwc.nekorisu";
const UNIT: &str = "nekorisu.service";
/// Passed through to core so it looks where the user's shell does.
const PASS_ENV: &[&str] = &["NEKORISU_HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME"];

pub fn enable() -> anyhow::Result<()> {
    let paths = Paths::from_env();
    let program = program(&paths)?;
    if cfg!(target_os = "macos") {
        let plist = launch_agent_path();
        fs::create_dir_all(plist.parent().expect("has a parent"))?;
        fs::write(&plist, launchd_plist(&program, &paths.core_log()))?;
        let domain = format!("gui/{}", uid());
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{LABEL}")])
            .output();
        run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&plist))?;
        println!("已注册开机自启：{}", plist.display());
    } else {
        let unit = systemd_unit_path();
        fs::create_dir_all(unit.parent().expect("has a parent"))?;
        fs::write(&unit, systemd_unit(&program, &paths.core_log()))?;
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(Command::new("systemctl").args(["--user", "enable", "--now", UNIT]))?;
        println!("已注册开机自启：{}", unit.display());
    }
    if update::installed_link(&paths, &config::load(&paths).unwrap_or_default().update).is_none() {
        println!("注意：这份 nekorisu 不是安装脚本装的，自动更新不会生效");
    }
    Ok(())
}

pub fn disable() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let plist = launch_agent_path();
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
            .output();
        if plist.exists() {
            fs::remove_file(&plist)?;
        }
        println!("已取消开机自启");
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", UNIT])
            .output();
        let unit = systemd_unit_path();
        if unit.exists() {
            fs::remove_file(&unit)?;
        }
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
        println!("已取消开机自启");
    }
    Ok(())
}

/// The install script's symlink when there is one, otherwise this binary.
fn program(paths: &Paths) -> anyhow::Result<PathBuf> {
    let config = config::load(paths).unwrap_or_default();
    match update::installed_link(paths, &config.update) {
        Some(link) => Ok(link),
        None => std::env::current_exe()?
            .canonicalize()
            .context("找不到自己的可执行文件"),
    }
}

fn launch_agent_path() -> PathBuf {
    paths::home_dir()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn systemd_unit_path() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".config"));
    config.join("systemd/user").join(UNIT)
}

fn launchd_plist(program: &Path, log: &Path) -> String {
    let env: String = PASS_ENV
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|v| (name, v)))
        .map(|(name, value)| format!("    <key>{name}</key><string>{}</string>\n", xml_escape(&value)))
        .collect();
    let env = if env.is_empty() {
        String::new()
    } else {
        format!("  <key>EnvironmentVariables</key>\n  <dict>\n{env}  </dict>\n")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{program}</string>
    <string>daemon</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Background</string>
  <key>StandardErrorPath</key><string>{log}</string>
{env}</dict>
</plist>
"#,
        program = xml_escape(&program.display().to_string()),
        log = xml_escape(&log.display().to_string()),
    )
}

fn systemd_unit(program: &Path, log: &Path) -> String {
    let env: String = PASS_ENV
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|v| format!("Environment=\"{name}={v}\"\n"))
        })
        .collect();
    format!(
        "[Unit]\nDescription=nekorisu core\n\n[Service]\nExecStart=\"{}\" daemon\nRestart=on-failure\nStandardError=append:{}\n{env}\n[Install]\nWantedBy=default.target\n",
        program.display(),
        log.display()
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn run(command: &mut Command) -> anyhow::Result<()> {
    let output = command.output().with_context(|| format!("运行不了 {command:?}"))?;
    if !output.status.success() {
        bail!("{command:?} 失败：{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_the_link_and_restarts_only_on_failure() {
        let plist = launchd_plist(Path::new("/Users/me/.local/bin/nekorisu"), Path::new("/tmp/core.log"));
        assert!(plist.contains("<string>/Users/me/.local/bin/nekorisu</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
    }

    #[test]
    fn unit_runs_the_link() {
        let unit = systemd_unit(Path::new("/home/me/.local/bin/nekorisu"), Path::new("/tmp/core.log"));
        assert!(unit.contains("ExecStart=\"/home/me/.local/bin/nekorisu\" daemon"));
        assert!(unit.contains("Restart=on-failure"));
    }
}
