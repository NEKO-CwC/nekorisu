//! Windows login launcher, run by the scheduled task `nekorisu core enable` registers: at
//! login, and whenever `nekorisu core start` asks the Task Scheduler for it. It stands in for
//! launchd and systemd: it runs `nekorisu.exe daemon` from its own folder (the install
//! script's link, so a version switch takes effect on the next start), hidden, its log going to
//! `run/core.log`, waits for it, and starts it again when it exits with an error
//! (`supervise`). It is a window-less (GUI subsystem) program, since a console one would open
//! a terminal window at every login.
//!
//! On other systems it is a stub: launchd and systemd do this there.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[allow(dead_code)]
#[path = "../paths.rs"]
mod paths;
#[cfg(windows)]
#[path = "../supervise.rs"]
mod supervise;

use std::process::ExitCode;

#[cfg(windows)]
fn main() -> ExitCode {
    use std::time::Instant;

    use supervise::{Next, Policy};

    let Some(core) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(paths::BINARY_NAME)))
    else {
        return ExitCode::FAILURE;
    };
    let paths = paths::Paths::from_env();
    let mut policy = Policy::default();
    loop {
        let started = Instant::now();
        let code = match run(&core, &paths) {
            Ok(status) => status.code(),
            Err(err) => {
                log(&paths, &format!("启动器运行不了 {}：{err}", core.display()));
                None
            }
        };
        match policy.after(code, started.elapsed()) {
            Next::Stop => return ExitCode::SUCCESS,
            Next::Again(wait) => {
                if !wait.is_zero() {
                    let code = code.map_or("没有".to_string(), |c| c.to_string());
                    log(
                        &paths,
                        &format!(
                            "core 异常退出（退出码 {code}），启动器 {} 秒后重新启动它",
                            wait.as_secs()
                        ),
                    );
                    std::thread::sleep(wait);
                }
            }
            Next::GiveUp => {
                log(
                    &paths,
                    "core 一再启动后很快退出，启动器不再重启它；原因见上面的日志，处理好后运行 nekorisu core start",
                );
                return ExitCode::FAILURE;
            }
        }
    }
}

/// Runs core once, hidden, and waits for it. Nothing here holds handles core could inherit
/// besides its stdio, so no handle list is needed.
#[cfg(windows)]
fn run(core: &std::path::Path, paths: &paths::Paths) -> std::io::Result<std::process::ExitStatus> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    /// No console window for the console-subsystem `nekorisu.exe`.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    std::fs::create_dir_all(paths.run_dir())?;
    let log = open_log(paths)?;
    Command::new(core)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
}

#[cfg(windows)]
fn open_log(paths: &paths::Paths) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.core_log())
}

/// One line in core's log, stamped like core's own.
#[cfg(windows)]
fn log(paths: &paths::Paths, message: &str) {
    use std::io::Write;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    if let Ok(mut file) = open_log(paths) {
        let _ = writeln!(file, "{now} {message}");
    }
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!(
        "nekorisu-start 只在 Windows 上用：登录时在后台运行 core，异常退出时重启它。这个系统上由 nekorisu core enable 注册的 launchd 或 systemd 来做"
    );
    ExitCode::FAILURE
}
