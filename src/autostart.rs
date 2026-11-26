//! Keeping core running is the system's job, not the clients' (`client`): a launchd agent on
//! macOS and a `systemd --user` unit on Linux start it at login and again after it fails. On
//! Windows a scheduled task runs `nekorisu-start.exe` at login, a window-less launcher next to
//! the binary that runs core and restarts it after a failure (`supervise`); the Task Scheduler
//! also starts it on request, outside whatever process tree or job asked (an agent may end its
//! whole job when it exits). All of them run the install script's link, so a version switch
//! takes effect on the next start.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::config;
use crate::paths::{self, Paths};
use crate::update;

#[cfg(unix)]
const LABEL: &str = "io.github.neko-cwc.nekorisu";
#[cfg(unix)]
const UNIT: &str = "nekorisu.service";
/// The scheduled task's name, in the Task Scheduler's top folder.
#[cfg_attr(unix, allow(dead_code))]
const TASK: &str = "nekorisu";
/// Passed through to core so it looks where the user's shell does.
const PASS_ENV: &[&str] = &["NEKORISU_HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME"];

/// Registers core to start at login. launchd and systemd also start it right away; on Windows
/// that is `start`'s job.
pub fn enable() -> anyhow::Result<()> {
    let paths = Paths::from_env();
    let program = program(&paths)?;
    // launchd and systemd open core's log in run/ themselves, before core could create the
    // folder; systemd refuses to start it at all when the folder is missing.
    let run_dir = paths.run_dir();
    fs::create_dir_all(&run_dir).with_context(|| format!("建不了 {}", run_dir.display()))?;
    crate::daemon::make_private(&paths.home)?;
    crate::daemon::make_private(&run_dir)?;
    register(&paths, &program)?;
    if update::installed_link(&paths, &config::load(&paths).unwrap_or_default().update).is_none() {
        println!("注意：这份 nekorisu 不是安装脚本装的，自动更新不会生效");
    }
    Ok(())
}

/// Removes the registration. launchd and systemd stop core with it; on Windows it keeps running
/// until it is stopped.
pub fn disable() -> anyhow::Result<()> {
    unregister()?;
    println!("已取消开机自启");
    Ok(())
}

/// Asks the system to start core now, the way it does at login.
pub fn start() -> anyhow::Result<()> {
    if !registered() {
        bail!("还没注册开机自启，没法交给系统启动 core；先运行 nekorisu core enable，它会注册并启动 core");
    }
    start_registered()
}

#[cfg(unix)]
fn register(paths: &Paths, program: &Path) -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let plist = launch_agent_path();
        fs::create_dir_all(plist.parent().expect("has a parent"))?;
        fs::write(&plist, launchd_plist(program, &paths.core_log()))?;
        let domain = format!("gui/{}", uid());
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{LABEL}")])
            .output();
        run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&plist))?;
        println!("已注册开机自启：{}", plist.display());
    } else {
        let unit = systemd_unit_path();
        fs::create_dir_all(unit.parent().expect("has a parent"))?;
        fs::write(&unit, systemd_unit(program, &paths.core_log()))?;
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(Command::new("systemctl").args(["--user", "enable", "--now", UNIT]))?;
        println!("已注册开机自启：{}", unit.display());
    }
    Ok(())
}

#[cfg(unix)]
fn unregister() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let plist = launch_agent_path();
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
            .output();
        if plist.exists() {
            fs::remove_file(&plist)?;
        }
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", UNIT])
            .output();
        let unit = systemd_unit_path();
        if unit.exists() {
            fs::remove_file(&unit)?;
        }
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
    }
    Ok(())
}

#[cfg(unix)]
fn registered() -> bool {
    if cfg!(target_os = "macos") {
        launch_agent_path().is_file()
    } else {
        systemd_unit_path().is_file()
    }
}

#[cfg(unix)]
fn start_registered() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let domain = format!("gui/{}", uid());
        if run(Command::new("launchctl").args(["kickstart", &format!("{domain}/{LABEL}")])).is_err() {
            // Registered but not loaded (say, after a bootout): loading starts it (RunAtLoad).
            run(Command::new("launchctl")
                .args(["bootstrap", &domain])
                .arg(launch_agent_path()))?;
        }
    } else {
        run(Command::new("systemctl").args(["--user", "start", UNIT]))?;
    }
    Ok(())
}

/// Windows: a task that runs the launcher next to `program` at this user's login, only while
/// the user is logged on and without elevation, like a `Run` entry, but which the Task Scheduler
/// can also start on request. Environment variables can't ride along: core sees the account's own.
#[cfg(windows)]
fn register(paths: &Paths, program: &Path) -> anyhow::Result<()> {
    let launcher = program.with_file_name(paths::LAUNCHER_NAME);
    if !launcher.is_file() {
        bail!(
            "找不到 {}，它应该和 nekorisu.exe 在同一个文件夹；用安装脚本重装一次就有了",
            launcher.display()
        );
    }
    let user = crate::ipc::user_sid().context("查不到当前账户")?;
    let file = paths.run_dir().join("task.xml");
    // schtasks reads the file in the encoding its declaration names: UTF-16 with a byte-order mark.
    let utf16: Vec<u8> = [0xFEFFu16]
        .into_iter()
        .chain(task_xml(&launcher, &user).encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    fs::write(&file, utf16)?;
    let created = run(schtasks().args(["/create", "/tn", TASK, "/f", "/xml"]).arg(&file));
    let _ = fs::remove_file(&file);
    created.context("建不了登录时运行的计划任务")?;
    println!("已注册开机自启：计划任务 {TASK}，登录时运行 {}", launcher.display());
    let set: Vec<&str> = PASS_ENV
        .iter()
        .copied()
        .filter(|name| std::env::var_os(name).is_some())
        .collect();
    if !set.is_empty() {
        println!(
            "注意：计划任务启动的 core 只看得到账户自己的环境变量；{} 要在那时也生效，请用 setx 设到账户上",
            set.join("、")
        );
    }
    Ok(())
}

#[cfg(windows)]
fn unregister() -> anyhow::Result<()> {
    if registered() {
        run(schtasks().args(["/delete", "/tn", TASK, "/f"])).context("删不了登录时运行的计划任务")?;
    }
    Ok(())
}

#[cfg(windows)]
fn registered() -> bool {
    schtasks()
        .args(["/query", "/tn", TASK])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The Task Scheduler starts the launcher in this user's session, in no process tree or job of
/// ours, whoever asks.
#[cfg(windows)]
fn start_registered() -> anyhow::Result<()> {
    run(schtasks().args(["/run", "/tn", TASK])).context("计划任务启动不了 core")
}

#[cfg(windows)]
fn schtasks() -> Command {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let mut command = Command::new(Path::new(&root).join("System32").join("schtasks.exe"));
    command.stdin(std::process::Stdio::null());
    command
}

/// The task: at `user`'s login (a SID), as that user, only while logged on, not elevated; on
/// battery too, with no time limit, and a new instance whenever asked (one that finds core
/// already running exits at once, see `daemon`).
#[cfg_attr(unix, allow(dead_code))]
fn task_xml(launcher: &Path, user: &str) -> String {
    let launcher = xml_escape(&launcher.display().to_string());
    let user = xml_escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>nekorisu core</Description></RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author">
    <Exec><Command>{launcher}</Command></Exec>
  </Actions>
</Task>
"#
    )
}

/// The install script's link when there is one, otherwise this binary.
fn program(paths: &Paths) -> anyhow::Result<PathBuf> {
    let config = config::load(paths).unwrap_or_default();
    if let Some(link) = update::installed_link(paths, &config.update) {
        return Ok(link);
    }
    let exe = std::env::current_exe().context("找不到自己的可执行文件")?;
    if cfg!(windows) {
        // Already a full path; a canonical one would start with \\?\, which the task shouldn't.
        return Ok(exe);
    }
    exe.canonicalize().context("找不到自己的可执行文件")
}

#[cfg(unix)]
fn launch_agent_path() -> PathBuf {
    paths::home_dir()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

#[cfg(unix)]
fn systemd_unit_path() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".config"));
    config.join("systemd/user").join(UNIT)
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn run(command: &mut Command) -> anyhow::Result<()> {
    let output = command.output().with_context(|| format!("运行不了 {command:?}"))?;
    if !output.status.success() {
        // schtasks writes in the console's code page, not UTF-8: then show only the exit code.
        let detail = std::str::from_utf8(&output.stderr)
            .map(|text| text.trim().to_string())
            .unwrap_or_default();
        let code = output.status.code().map_or("无".to_string(), |c| c.to_string());
        bail!("{command:?} 失败（退出码 {code}）{detail}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_runs_the_launcher_at_this_users_login_only() {
        let xml = task_xml(
            Path::new(r"C:\Users\A&B\.local\bin\nekorisu-start.exe"),
            "S-1-5-21-1-2-3-1001",
        );
        assert!(xml.contains(r"<Command>C:\Users\A&amp;B\.local\bin\nekorisu-start.exe</Command>"));
        assert!(
            xml.contains("<LogonTrigger><Enabled>true</Enabled><UserId>S-1-5-21-1-2-3-1001</UserId></LogonTrigger>")
        );
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
    }

    #[cfg(unix)]
    #[test]
    fn plist_runs_the_link_and_restarts_only_on_failure() {
        let plist = launchd_plist(Path::new("/Users/me/.local/bin/nekorisu"), Path::new("/tmp/core.log"));
        assert!(plist.contains("<string>/Users/me/.local/bin/nekorisu</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key><false/>"));
    }

    #[cfg(unix)]
    #[test]
    fn unit_runs_the_link() {
        let unit = systemd_unit(Path::new("/home/me/.local/bin/nekorisu"), Path::new("/tmp/core.log"));
        assert!(unit.contains("ExecStart=\"/home/me/.local/bin/nekorisu\" daemon"));
        assert!(unit.contains("Restart=on-failure"));
    }
}
