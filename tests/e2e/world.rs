//! What one scenario works with: devices (each its own folders and its own running core), the
//! storage and the trusted source, and session names given in the scenario.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};

use crate::standins::{Dav, RealServer, Source};

/// The binary under test: the one cargo built, or `NEKORISU_E2E_BIN` (to run a copy elsewhere).
pub fn bin() -> PathBuf {
    std::env::var_os("NEKORISU_E2E_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_nekorisu")))
}
const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);

/// The output of the last command a step ran.
#[derive(Debug, Default, Clone)]
pub struct Ran {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    pub fn text(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// One device: a home, a nekorisu home, agent folders, and its core.
#[derive(Debug)]
pub struct Device {
    pub name: String,
    pub root: PathBuf,
    pub id: String,
    /// Running core: a task waits on it (reaping it when it exits, as launchd or systemd
    /// would); sending on the channel stops it.
    core: Option<(tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>)>,
    /// Installed with the install script (`system.rs`): the test name its login start and
    /// keychain items go by.
    pub system: Option<String>,
    /// Folders put in front of PATH (the developer's `claude`, `codex`).
    pub extra_path: Vec<PathBuf>,
    pub extra_env: Vec<(&'static str, OsString)>,
    /// Programs of the installed copy the scenario started (core, an agent's MCP server).
    pub spawned: Vec<Spawned>,
    /// Set when the scenario signed Claude Code in somewhere (cleanup deletes its keychain item).
    pub claude_signed_in: bool,
    /// Core started by the system (launchd) rather than by the scenario.
    pub system_core_pid: Option<u32>,
}

/// A program the scenario started; a task waits on it (so it is reaped when it exits).
#[derive(Debug)]
pub struct Spawned {
    pub pid: u32,
    pub what: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    waiter: tokio::task::JoinHandle<()>,
}

impl Spawned {
    pub fn exited(&self) -> bool {
        self.waiter.is_finished()
    }
}

impl Device {
    /// Folders kept short: a Unix socket path must stay under about 100 bytes.
    pub fn new(root: &Path, name: &str) -> anyhow::Result<Device> {
        let root = root.join(name);
        for sub in ["h", "n", "c", "x"] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        Ok(Device {
            name: name.to_string(),
            root,
            id: String::new(),
            core: None,
            system: None,
            extra_path: Vec::new(),
            extra_env: Vec::new(),
            spawned: Vec::new(),
            claude_signed_in: false,
            system_core_pid: None,
        })
    }

    /// Stands in for the user's home folder.
    pub fn home(&self) -> PathBuf {
        self.root.join("h")
    }

    pub fn nekorisu_home(&self) -> PathBuf {
        self.root.join("n")
    }

    pub fn claude_home(&self) -> PathBuf {
        self.root.join("c")
    }

    pub fn config_file(&self) -> PathBuf {
        self.nekorisu_home().join("config.toml")
    }

    pub fn env(&self) -> Vec<(&'static str, OsString)> {
        let mut env = self.base_env();
        if let Some(name) = &self.system {
            env.push(("NEKORISU_E2E_NAME", name.into()));
            if cfg!(target_os = "macos") {
                // The real keychain, under this device's own service names.
                env.retain(|(key, _)| *key != "NEKORISU_SECRETS");
            }
        }
        if !self.extra_path.is_empty() {
            let mut dirs = self.extra_path.clone();
            dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
            env.push(("PATH", std::env::join_paths(dirs).unwrap_or_default()));
        }
        env.extend(self.extra_env.iter().cloned());
        env
    }

    fn base_env(&self) -> Vec<(&'static str, OsString)> {
        let mut env: Vec<(&'static str, OsString)> = vec![
            ("NEKORISU_HOME", self.nekorisu_home().into()),
            ("CLAUDE_CONFIG_DIR", self.claude_home().into()),
            ("CODEX_HOME", self.root.join("x").into()),
            // nekorisu's idea of the home folder (debug builds only).
            ("NEKORISU_E2E_HOME", self.home().into()),
            ("USERPROFILE", self.home().into()),
            // Where login start and Claude Code's caches are looked for (`uninstall` removes
            // them): never the developer's own.
            ("LOCALAPPDATA", self.home().join("AppData").join("Local").into()),
            ("XDG_CONFIG_HOME", self.home().join(".config").into()),
            ("XDG_CACHE_HOME", self.home().join(".cache").into()),
            // Secrets in files under NEKORISU_HOME, never the developer's keychain (debug builds only).
            ("NEKORISU_SECRETS", "file".into()),
            // No self-update during a scenario.
            ("NEKORISU_UPDATE_FIRST_CHECK_SECS", "86400".into()),
            ("NO_COLOR", "1".into()),
        ];
        // macOS keeps the real HOME: the keychain is found through it, and with another one
        // `security` asks the person, in a dialog, which keychain to use.
        if !cfg!(target_os = "macos") {
            env.push(("HOME", self.home().into()));
        }
        env
    }

    fn command(&self, args: &[String]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(bin());
        cmd.args(args)
            .envs(self.env())
            .current_dir(&self.root)
            .kill_on_drop(true);
        cmd
    }

    /// Starts this device's core and waits until it answers.
    pub async fn start(&mut self) -> anyhow::Result<()> {
        let log = std::fs::File::create(self.root.join("core.out"))?;
        let mut child = self
            .command(&["daemon".to_string()])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("core did not start")?;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let waiter = tokio::spawn(async move {
            let exited = tokio::select! {
                _ = child.wait() => true,
                _ = stopped => false,
            };
            if !exited {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        });
        self.core = Some((stop, waiter));
        self.wait_for_core().await
    }

    /// Waits until this device's core answers, and notes the device ID it made.
    pub async fn wait_for_core(&mut self) -> anyhow::Result<()> {
        let started = Instant::now();
        loop {
            let ran = self.run(&["core".into(), "status".into()], None).await?;
            if ran.stdout.contains("pid") {
                break;
            }
            if started.elapsed() > Duration::from_secs(30) {
                let log =
                    std::fs::read_to_string(self.nekorisu_home().join("run").join("core.log")).unwrap_or_default();
                bail!("core of {} did not answer: {}{log}", self.name, self.core_log());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let device: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.nekorisu_home().join("device.json"))?)?;
        self.id = device["id"].as_str().unwrap_or_default().to_string();
        Ok(())
    }

    /// Starts the installed command (`~/.local/bin/nekorisu`) with `args` and leaves it running
    /// with its input open, as an agent keeps its MCP server; `log` sends its output to core.out.
    pub fn spawn_installed(&mut self, args: &[&str], log: bool) -> anyhow::Result<u32> {
        let mut command = tokio::process::Command::new(crate::system::link(self));
        command
            .args(args)
            .envs(self.env())
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .kill_on_drop(true);
        if log {
            let file = std::fs::File::create(self.root.join("core.out"))?;
            command.stdout(file.try_clone()?).stderr(file);
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let mut child = command.spawn().context("the installed nekorisu did not start")?;
        let pid = child.id().context("no pid")?;
        let input = child.stdin.take();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let waiter = tokio::spawn(async move {
            let _input = input;
            let exited = tokio::select! {
                _ = child.wait() => true,
                _ = stopped => false,
            };
            if !exited {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        });
        self.spawned.push(Spawned {
            pid,
            what: args.join(" "),
            stop: Some(stop),
            waiter,
        });
        Ok(pid)
    }

    /// Runs the installed command, as a person or an agent would after installing.
    pub async fn run_installed(&self, args: &[String]) -> anyhow::Result<Ran> {
        let out = tokio::process::Command::new(crate::system::link(self))
            .args(args)
            .envs(self.env())
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(COMMAND_TIMEOUT, out)
            .await
            .map_err(|_| anyhow!("nekorisu {} took over {COMMAND_TIMEOUT:?}", args.join(" ")))??;
        Ok(Ran {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    pub fn core_log(&self) -> String {
        std::fs::read_to_string(self.root.join("core.out")).unwrap_or_default()
    }

    /// Runs the CLI on this device; `stdin` is fed as if piped.
    pub async fn run(&self, args: &[String], stdin: Option<&str>) -> anyhow::Result<Ran> {
        let mut child = self
            .command(args)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(text) = stdin {
            use tokio::io::AsyncWriteExt;
            let mut pipe = child.stdin.take().context("no stdin")?;
            pipe.write_all(text.as_bytes()).await?;
            drop(pipe);
        }
        let out = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| anyhow!("nekorisu {} took over {COMMAND_TIMEOUT:?}", args.join(" ")))??;
        Ok(Ran {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Runs the CLI in a terminal and types `input` and Enter, the way a person confirms.
    pub async fn run_in_terminal(&self, args: &[String], input: &str) -> anyhow::Result<Ran> {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        let mut cmd = CommandBuilder::new(bin());
        cmd.args(args);
        cmd.cwd(&self.root);
        for (key, value) in self.env() {
            cmd.env(key, value);
        }
        let input = format!("{input}\r\n");
        tokio::task::spawn_blocking(move || -> anyhow::Result<Ran> {
            let pair = native_pty_system().openpty(PtySize {
                rows: 40,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })?;
            let mut child = pair.slave.spawn_command(cmd)?;
            drop(pair.slave);
            let mut reader = pair.master.try_clone_reader()?;
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });
            let mut writer = pair.master.take_writer()?;
            let mut text: Vec<u8> = Vec::new();
            // Windows' pseudo console first asks where the cursor is (ESC [ 6 n) and holds the
            // input back until a terminal answers; answer it, then type.
            let ready = Instant::now();
            let mut answered = false;
            while ready.elapsed() < Duration::from_millis(if cfg!(windows) { 5000 } else { 500 }) {
                if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) {
                    text.extend(chunk);
                }
                if !answered && text.windows(4).any(|w| w == b"\x1b[6n") {
                    writer.write_all(b"\x1b[1;1R")?;
                    writer.flush()?;
                    answered = true;
                }
                if answered && ready.elapsed() > Duration::from_millis(300) {
                    break;
                }
            }
            writer.write_all(input.as_bytes())?;
            writer.flush()?;
            let started = Instant::now();
            let status = loop {
                while let Ok(chunk) = rx.try_recv() {
                    text.extend(chunk);
                }
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                if started.elapsed() > COMMAND_TIMEOUT {
                    let _ = child.kill();
                    bail!(
                        "the command in the terminal did not finish; it printed: {}",
                        String::from_utf8_lossy(&text)
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            std::thread::sleep(Duration::from_millis(200));
            while let Ok(chunk) = rx.try_recv() {
                text.extend(chunk);
            }
            drop(writer);
            // Closing the pseudo console waits for its output to be read: keep the reader going.
            std::thread::spawn(move || drop(pair.master));
            Ok(Ran {
                success: status.success(),
                stdout: String::from_utf8_lossy(&text).into_owned(),
                stderr: String::new(),
            })
        })
        .await?
    }

    pub async fn stop(&mut self) {
        if let Some((stop, waiter)) = self.core.take() {
            let _ = stop.send(());
            let _ = tokio::time::timeout(Duration::from_secs(10), waiter).await;
        }
        for spawned in &mut self.spawned {
            if let Some(stop) = spawned.stop.take() {
                let _ = stop.send(());
            }
        }
        for spawned in self.spawned.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(10), spawned.waiter).await;
        }
        crate::system::clean_up(self);
    }
}

use std::io::{Read, Write};

#[derive(Debug, Default, cucumber::World)]
pub struct E2e {
    root: Option<tempfile::TempDir>,
    pub dav: Option<Dav>,
    pub source: Option<Source>,
    pub server: Option<RealServer>,
    pub devices: BTreeMap<String, Device>,
    /// Names given in the scenario (sessions, requests) → their IDs.
    pub sessions: BTreeMap<String, String>,
    pub last: Option<Ran>,
}

impl E2e {
    pub fn root(&mut self) -> anyhow::Result<PathBuf> {
        if self.root.is_none() {
            self.root = Some(tempfile::Builder::new().prefix("e2e").tempdir()?);
        }
        Ok(self.root.as_ref().expect("made above").path().to_path_buf())
    }

    pub fn device(&self, name: &str) -> anyhow::Result<&Device> {
        self.devices
            .get(name)
            .ok_or_else(|| anyhow!("no device {name:?} in this scenario"))
    }

    pub fn device_mut(&mut self, name: &str) -> anyhow::Result<&mut Device> {
        self.devices
            .get_mut(name)
            .ok_or_else(|| anyhow!("no device {name:?} in this scenario"))
    }

    pub fn last(&self) -> anyhow::Result<&Ran> {
        self.last.as_ref().ok_or_else(|| anyhow!("no command has run yet"))
    }

    /// `~A` becomes device A's home folder, `@A` its device ID, and a session name its ID.
    pub fn expand(&self, text: &str) -> String {
        let mut out = text.to_string();
        // Longer names first, so `~AB` is not read as `~A` + "B".
        let mut names: Vec<&String> = self.devices.keys().collect();
        names.sort_by_key(|n| std::cmp::Reverse(n.len()));
        for name in names {
            let device = &self.devices[name];
            out = out.replace(&format!("~{name}"), &device.home().to_string_lossy());
            out = out.replace(&format!("@{name}"), &device.id);
        }
        out
    }

    /// A command line from a step: words split on spaces, session names replaced by IDs.
    pub fn args(&self, line: &str) -> Vec<String> {
        let line = line.strip_prefix("nekorisu ").unwrap_or(line);
        line.split_whitespace()
            .map(|word| {
                let word = self.expand(word);
                self.sessions.get(&word).cloned().unwrap_or(word)
            })
            .collect()
    }

    pub async fn shutdown(&mut self) {
        for device in self.devices.values_mut() {
            device.stop().await;
        }
    }
}

/// Claude Code's folder name for a working directory: every character that is not an ASCII
/// letter or digit becomes `-` (paths here are short and ASCII).
pub fn claude_folder(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Writes a synthetic Claude Code session with `cwd` as its working directory.
pub fn write_claude_session(device: &Device, id: &str, cwd: &str) -> anyhow::Result<PathBuf> {
    let dir = device.claude_home().join("projects").join(claude_folder(cwd));
    std::fs::create_dir_all(&dir)?;
    let lines = [
        serde_json::json!({
            "type": "user", "timestamp": "2026-11-01T10:00:00.000Z", "cwd": cwd, "sessionId": id,
            "version": "9.9.9", "message": { "role": "user", "content": "synthetic question" },
        }),
        serde_json::json!({
            "type": "assistant", "timestamp": "2026-11-01T10:00:01.000Z", "cwd": cwd, "sessionId": id,
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "synthetic answer" }] },
        }),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let file = dir.join(format!("{id}.jsonl"));
    std::fs::write(&file, text)?;
    Ok(file)
}

/// Where a restored Claude Code session with this ID is on `device`, if anywhere.
pub fn find_claude_session(device: &Device, id: &str) -> Option<PathBuf> {
    let projects = device.claude_home().join("projects");
    std::fs::read_dir(projects).ok()?.flatten().find_map(|entry| {
        let file = entry.path().join(format!("{id}.jsonl"));
        file.is_file().then_some(file)
    })
}
