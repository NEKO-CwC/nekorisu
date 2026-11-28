//! The sentences scenarios are written in. In quoted text, `~A` is device A's home folder,
//! `@A` its device ID, `%A` its device name; session names given in a scenario stand for their IDs.

use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail, ensure};
use cucumber::gherkin::Step;
use cucumber::{given, then, when};

use crate::standins::{Dav, RealServer, Source};
use crate::system;
use crate::world::{Device, E2e, Ran, bin, claude_folder, find_claude_session, write_claude_session};

fn docstring(step: &Step) -> anyhow::Result<&str> {
    step.docstring.as_deref().context("this step needs a \"\"\" text block")
}

/// `%A` becomes device A's name.
fn expand_names(w: &E2e, text: &str) -> anyhow::Result<String> {
    let mut out = w.expand(text);
    for (name, device) in &w.devices {
        let file = device.nekorisu_home().join("device.json");
        if out.contains(&format!("%{name}")) {
            let json: serde_json::Value = serde_json::from_slice(&std::fs::read(file)?)?;
            out = out.replace(&format!("%{name}"), json["name"].as_str().unwrap_or_default());
        }
    }
    Ok(out)
}

/// Expands every string in a JSON document (so paths with `\` stay valid JSON). A force
/// change may give its value as `"value"`; it becomes the `value_json` text the protocol wants.
fn expand_json(w: &E2e, value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => *s = w.expand(s),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| expand_json(w, v)),
        serde_json::Value::Object(map) => {
            map.values_mut().for_each(|v| expand_json(w, v));
            if let Some(v) = map.remove("value") {
                map.insert("value_json".into(), serde_json::Value::String(v.to_string()));
            }
        }
        _ => {}
    }
}

// ---- what there is ----

#[given("有一个 WebDAV 存储")]
async fn storage(w: &mut E2e) {
    w.dav = Some(Dav::start().await);
}

#[given(expr = "有一个配置来源，设备令牌是 {string}")]
async fn source(w: &mut E2e, token: String) {
    w.source = Some(Source::start(&token).await);
}

#[given(expr = "设备 {string} 连着这个存储")]
async fn device_with_storage(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let root = w.root()?;
    let url = w.dav.as_ref().context("no storage yet: 有一个 WebDAV 存储")?.url();
    let device = Device::new(&root, &name)?;
    let config = format!("[update]\nauto = false\n\n[storage.test]\nkind = \"webdav\"\nurl = \"{url}\"\n");
    std::fs::write(device.config_file(), config)?;
    w.devices.insert(name, device);
    Ok(())
}

#[given(expr = "设备 {string} 的 core 在运行")]
async fn core_running(w: &mut E2e, name: String) -> anyhow::Result<()> {
    w.device_mut(&name)?.start().await
}

#[given(expr = "设备 {string} 已连上这个存储，core 在运行")]
async fn device_running(w: &mut E2e, name: String) -> anyhow::Result<()> {
    device_with_storage(w, name.clone()).await?;
    core_running(w, name).await
}

/// Appends to config.toml; a running core is restarted to read it, as a person would.
#[given(expr = "设备 {string} 的 config.toml 里加上：")]
async fn add_to_config(w: &mut E2e, name: String, step: &Step) -> anyhow::Result<()> {
    let text = w.expand(docstring(step)?);
    let device = w.device_mut(&name)?;
    let mut config = std::fs::read_to_string(device.config_file())?;
    config.push('\n');
    config.push_str(&text);
    config.push('\n');
    std::fs::write(device.config_file(), config)?;
    if !device.id.is_empty() {
        device.stop().await;
        device.start().await?;
    }
    Ok(())
}

#[given(expr = "设备 {string} 的 Claude Code 里有会话 {string}，工作目录是 {string}")]
async fn has_session(w: &mut E2e, name: String, session: String, cwd: String) -> anyhow::Result<()> {
    let cwd = w.expand(&cwd);
    let id = w
        .sessions
        .entry(session)
        .or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    write_claude_session(w.device(&name)?, &id, &cwd)?;
    Ok(())
}

/// Address, public key and device token of the source in this scenario: the real server if
/// there is one, otherwise the stand-in.
fn source_endpoint(w: &E2e) -> anyhow::Result<(String, String, String)> {
    if let Some(server) = &w.server {
        return Ok((server.grpc.clone(), server.public_key.clone(), server.token.clone()));
    }
    let src = w.source.as_ref().context("no source yet: 有一个配置来源")?;
    Ok((src.url.clone(), src.public_key(), src.token.clone()))
}

#[given(expr = "设备 {string} 设置了这个来源")]
async fn add_source(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let (url, key, _) = source_endpoint(w)?;
    let args: Vec<String> = vec!["source".into(), "add".into(), "home".into(), url, "--key".into(), key];
    let ran = w.device(&name)?.run(&args, None).await?;
    ensure!(ran.success, "source add failed: {}", ran.text());
    w.last = Some(ran);
    Ok(())
}

#[given(expr = "设备 {string} 设置了这个来源并登录")]
async fn add_source_and_login(w: &mut E2e, name: String) -> anyhow::Result<()> {
    add_source(w, name.clone()).await?;
    let token = source_endpoint(w)?.2;
    let ran = w
        .device(&name)?
        .run(&["source".into(), "login".into()], Some(&format!("{token}\n")))
        .await?;
    ensure!(ran.success, "source login failed: {}", ran.text());
    w.last = Some(ran);
    Ok(())
}

#[given("来源下发配置：")]
#[when("来源下发配置：")]
async fn publish(w: &mut E2e, step: &Step) -> anyhow::Result<()> {
    let mut body: serde_json::Value = serde_json::from_str(docstring(step)?).context("the config is not JSON")?;
    expand_json(w, &mut body);
    w.source.as_mut().context("no source yet")?.publish(body);
    Ok(())
}

// ---- a real server (tests/e2e/server.sh) ----

#[given("有一个真的服务端")]
async fn real_server(w: &mut E2e) -> anyhow::Result<()> {
    let server = RealServer::connect().await?;
    server.reset().await?;
    w.server = Some(server);
    Ok(())
}

fn server(w: &E2e) -> anyhow::Result<&RealServer> {
    w.server.as_ref().context("no real server: 有一个真的服务端")
}

#[when(expr = "管理员在 panel 里把设备 {string} 的 {string} 映射到 {string}")]
async fn panel_map(w: &mut E2e, name: String, from: String, to: String) -> anyhow::Result<()> {
    let id = w.device(&name)?.id.clone();
    let item = serde_json::json!({ "from": w.expand(&from), "to": w.expand(&to), "devices": [id] });
    let reply = server(w)?
        .post("/api/paths", serde_json::json!({ "items": [item] }))
        .await?;
    ensure!(reply["published"] == true, "not published: {reply}");
    Ok(())
}

#[when(expr = "管理员在 panel 里请求设备 {string} 把 {string} 强制改成 {string}，记为 {string}")]
async fn panel_force(w: &mut E2e, name: String, from: String, to: String, alias: String) -> anyhow::Result<()> {
    let body = serde_json::json!({ "device": w.device(&name)?.id, "from": w.expand(&from), "to": w.expand(&to) });
    let reply = server(w)?.post("/api/paths/force", body).await?;
    ensure!(reply["published"] == true, "not published: {reply}");
    let id = reply["id"].as_str().context("no request ID")?.to_string();
    w.sessions.insert(alias, id);
    Ok(())
}

#[when(expr = "管理员在 panel 里撤回请求 {string}")]
async fn panel_withdraw(w: &mut E2e, alias: String) -> anyhow::Result<()> {
    let id = w.sessions.get(&alias).cloned().unwrap_or(alias);
    server(w)?
        .post(&format!("/api/paths/force/{id}/withdraw"), serde_json::json!({}))
        .await?;
    Ok(())
}

#[then(expr = "panel 上设备 {string} 上报了路径映射 {string}")]
async fn panel_shows_rule(w: &mut E2e, name: String, from: String) -> anyhow::Result<()> {
    let id = w.device(&name)?.id.clone();
    let paths = server(w)?.get("/api/paths").await?;
    let device = paths["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["id"] == id.as_str())
        .ok_or_else(|| anyhow!("the panel does not list device {name}"))?;
    let from = w.expand(&from);
    ensure!(
        device["local"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r["from"] == from.as_str()),
        "the panel has no rule {from} from device {name}: {}",
        device["local"]
    );
    Ok(())
}

#[then(expr = "panel 上请求 {string} 是 {string}")]
async fn panel_request_state(w: &mut E2e, alias: String, state: String) -> anyhow::Result<()> {
    let want = match state.as_str() {
        "待确认" => "pending",
        "已接受" => "accepted",
        "已拒绝" => "rejected",
        "设备还没上报" => "unknown",
        other => bail!("states are 待确认, 已接受, 已拒绝, 设备还没上报, not {other}"),
    };
    let id = w.sessions.get(&alias).cloned().unwrap_or(alias);
    let paths = server(w)?.get("/api/paths").await?;
    let got = paths["force"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["id"] == id.as_str())
        .map(|f| f["state"].clone());
    ensure!(got == Some(serde_json::json!(want)), "request {id}: {got:?}");
    Ok(())
}

/// Claude Code's cache folder on this system, under the device's home.
fn claude_cache(device: &Device) -> std::path::PathBuf {
    let home = device.home();
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/claude-cli-nodejs")
    } else if cfg!(windows) {
        home.join("AppData")
            .join("Local")
            .join("claude-cli-nodejs")
            .join("Cache")
    } else {
        home.join(".cache").join("claude-cli-nodejs")
    }
}

fn skill_dir(device: &Device, name: &str) -> std::path::PathBuf {
    device.claude_home().join("skills").join(name)
}

#[given(expr = "设备 {string} 上的会话 {string} 删掉了")]
async fn session_deleted(w: &mut E2e, name: String, session: String) -> anyhow::Result<()> {
    let id = w.sessions.get(&session).context("no such session in this scenario")?;
    let file = find_claude_session(w.device(&name)?, id).context("the session is not there")?;
    std::fs::remove_file(file)?;
    Ok(())
}

#[given(expr = "设备 {string} 上的会话 {string} 又多了一行")]
async fn session_grew(w: &mut E2e, name: String, session: String) -> anyhow::Result<()> {
    use std::io::Write;
    let id = w.sessions.get(&session).context("no such session in this scenario")?;
    let file = find_claude_session(w.device(&name)?, id).context("the session is not there")?;
    let mut f = std::fs::OpenOptions::new().append(true).open(file)?;
    writeln!(
        f,
        "{}",
        serde_json::json!({"type": "user", "sessionId": id, "message": {"role": "user", "content": "one more"}})
    )?;
    Ok(())
}

#[given(expr = "设备 {string} 的 Claude Code 装了 skill {string}")]
async fn skill_installed(w: &mut E2e, name: String, skill: String) -> anyhow::Result<()> {
    let dir = skill_dir(w.device(&name)?, &skill);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {skill}\ndescription: x\n---\nbody\n"),
    )?;
    Ok(())
}

#[given(expr = "设备 {string} 的 Claude Code 里有一个别人的 skill 叫 {string}")]
async fn foreign_skill(w: &mut E2e, name: String, skill: String) -> anyhow::Result<()> {
    let dir = skill_dir(w.device(&name)?, &skill);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), "---\nname: something-else\n---\nbody\n")?;
    Ok(())
}

#[given(expr = "设备 {string} 的 Claude Code 给 MCP {string} 记过日志")]
async fn mcp_logs(w: &mut E2e, name: String, server: String) -> anyhow::Result<()> {
    let dir = claude_cache(w.device(&name)?)
        .join("-some-project")
        .join(format!("mcp-logs-{server}"));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("2026-11-28.jsonl"), "{}\n")?;
    Ok(())
}

#[given(expr = "设备 {string} 的 nekorisu 目录里有文件 {string}")]
async fn file_in_home(w: &mut E2e, name: String, rel: String) -> anyhow::Result<()> {
    let file = w.device(&name)?.nekorisu_home().join(rel);
    std::fs::create_dir_all(file.parent().context("a file name")?)?;
    std::fs::write(file, "set aside\n")?;
    Ok(())
}

/// A stand-in `claude` first in line (`~/.local/bin`): it notes its arguments, and removing an
/// MCP entry empties Claude Code's settings file, as the real one would for the last entry.
#[given(expr = "设备 {string} 有一个替身 claude 命令")]
async fn fake_claude(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let device = w.device(&name)?;
    let bin = device.home().join(".local").join("bin");
    std::fs::create_dir_all(&bin)?;
    let log = device.root.join("claude-calls.txt");
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$2\" in remove) echo '{{}}' > \"$CLAUDE_CONFIG_DIR/.claude.json\" ;; esac\nexit 0\n",
        log.display()
    );
    let file = bin.join("claude");
    std::fs::write(&file, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[given(expr = "设备 {string} 的 Claude Code 里接了 MCP nekorisu 和 nekorisu-server")]
async fn claude_entries(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let device = w.device(&name)?;
    let settings = serde_json::json!({
        "mcpServers": {
            "nekorisu": {"type": "stdio", "command": "/somewhere/.local/bin/nekorisu", "args": ["mcp"]},
            "nekorisu-server": {"type": "http", "url": "https://example.test/mcp"},
        }
    });
    std::fs::write(device.claude_home().join(".claude.json"), settings.to_string())?;
    Ok(())
}

#[then(expr = "设备 {string} 的替身 claude 命令依次收到：")]
async fn fake_claude_calls(w: &mut E2e, name: String, step: &Step) -> anyhow::Result<()> {
    let device = w.device(&name)?;
    let calls = std::fs::read_to_string(device.root.join("claude-calls.txt")).unwrap_or_default();
    let got: Vec<&str> = calls.lines().collect();
    let want: Vec<&str> = docstring(step)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    ensure!(
        got == want,
        "claude was run with:\n{calls}\nwanted:\n{}",
        want.join("\n")
    );
    Ok(())
}

// ---- installed with the install script (system.rs) ----

#[given(expr = "设备 {string} 用安装脚本装好了 nekorisu")]
async fn installed(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let root = w.root()?;
    let mut device = Device::new(&root, &name)?;
    device.system = Some(uuid::Uuid::new_v4().simple().to_string()[..8].to_string());
    std::fs::write(device.config_file(), "[update]\nauto = false\n")?;
    // In the scenario before installing, so what it registers is cleaned up even if it fails.
    w.devices.insert(name.clone(), device);
    let device = w.device_mut(&name)?;
    let text = system::install(device).await?;
    if cfg!(target_os = "macos") {
        let status = device.run(&["core".into(), "status".into()], None).await?;
        device.system_core_pid = status
            .stdout
            .split("pid ")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|pid| pid.parse().ok());
        ensure!(
            device.system_core_pid.is_some(),
            "launchd did not start core: {}",
            status.text()
        );
    }
    if cfg!(windows) {
        let dir = system::link(device).parent().context("a folder")?.to_path_buf();
        ensure!(
            system::in_user_path(&dir),
            "the install script did not add {} to PATH",
            dir.display()
        );
    }
    w.last = Some(Ran {
        success: true,
        stdout: text,
        stderr: String::new(),
    });
    Ok(())
}

#[given(expr = "设备 {string} 的存储 {string} 设好了密码")]
async fn storage_password(w: &mut E2e, name: String, storage: String) -> anyhow::Result<()> {
    let url = w.dav.as_ref().context("no storage yet: 有一个 WebDAV 存储")?.url();
    let device = w.device(&name)?;
    let add: Vec<String> = ["storage", "add", &storage, "--url", &url, "--username", "tester"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let ran = device.run(&add, None).await?;
    ensure!(ran.success, "storage add: {}", ran.text());
    let ran = device
        .run(
            &["storage".into(), "login".into(), storage.clone()],
            Some("e2e-not-a-real-password\n"),
        )
        .await?;
    ensure!(ran.success, "storage login: {}", ran.text());
    if cfg!(target_os = "macos") && device.system.is_some() {
        ensure!(
            system::keychain_items(device) == 1,
            "the password is not in the keychain: {}",
            ran.text()
        );
    }
    Ok(())
}

#[given(expr = "设备 {string} 上有一个 Agent 正连着 nekorisu mcp")]
async fn agent_running_mcp(w: &mut E2e, name: String) -> anyhow::Result<()> {
    w.device_mut(&name)?.spawn_installed(&["mcp"], false)?;
    Ok(())
}

#[when(expr = "在设备 {string} 上用装好的 nekorisu 运行 {string}")]
async fn run_installed(w: &mut E2e, name: String, line: String) -> anyhow::Result<()> {
    let args = w.args(&line);
    w.last = Some(w.device(&name)?.run_installed(&args).await?);
    Ok(())
}

#[then(expr = "设备 {string} 上装的东西都不在了")]
async fn nothing_installed_left(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let device = w.device(&name)?;
    let link = system::link(device);
    let bin_dir = link.parent().context("a folder")?.to_path_buf();
    // Windows deletes the command that ran the uninstall once it has exited.
    let wait = Duration::from_secs(if cfg!(windows) { 120 } else { 10 });
    let started = Instant::now();
    loop {
        let mut left: Vec<String> = Vec::new();
        for path in [
            link.clone(),
            bin_dir.join("nekorisu-start.exe"),
            device.nekorisu_home(),
            bin_dir.clone(),
        ] {
            if path.symlink_metadata().is_ok() {
                left.push(path.display().to_string());
            }
        }
        if system::registered(device) {
            left.push("login start".into());
        }
        let items = system::keychain_items(device);
        if items > 0 {
            left.push(format!("{items} keychain items"));
        }
        if system::in_user_path(&bin_dir) {
            left.push(format!("{} in PATH", bin_dir.display()));
        }
        for spawned in device.spawned.iter().filter(|s| !s.exited()) {
            left.push(format!("nekorisu {} (pid {})", spawned.what, spawned.pid));
        }
        if let Some(pid) = device.system_core_pid
            && system::alive(pid)
        {
            left.push(format!("core started by the system (pid {pid})"));
        }
        if left.is_empty() {
            return Ok(());
        }
        if started.elapsed() > wait {
            bail!(
                "still there:\n  {}\nthe uninstall said:\n{}",
                left.join("\n  "),
                w.last()?.text()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ---- the developer's own Claude Code and Codex, with the device's settings folders ----

fn real_claude() -> anyhow::Result<std::path::PathBuf> {
    system::claude().context("no claude program here")
}

fn real_codex() -> anyhow::Result<std::path::PathBuf> {
    system::codex().context("no codex program here")
}

#[given(expr = "设备 {string} 的 Claude Code 用 claude 命令接了 nekorisu 的 MCP")]
async fn claude_added(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let claude = real_claude()?;
    let device = w.device_mut(&name)?;
    device
        .extra_path
        .push(claude.parent().context("a folder")?.to_path_buf());
    let nekorisu = bin().to_string_lossy().into_owned();
    let out = system::agent(
        device,
        &claude,
        &["mcp", "add", "-s", "user", "nekorisu", "--", &nekorisu, "mcp"],
        &[],
    )
    .await?;
    ensure!(out.status.success(), "claude mcp add: {}", system::first_lines(&out));
    skill_installed(w, name, "nekorisu".into()).await
}

#[given(expr = "设备 {string} 的 Codex 用 codex 命令接了 nekorisu 和 nekorisu-server 的 MCP")]
async fn codex_added(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let codex = real_codex()?;
    let device = w.device_mut(&name)?;
    device.extra_env.push(("CODEX_CLI_PATH", codex.clone().into()));
    let nekorisu = bin().to_string_lossy().into_owned();
    for args in [
        vec!["mcp", "add", "nekorisu", "--", &nekorisu, "mcp"],
        vec!["mcp", "add", "nekorisu-server", "--url", "http://127.0.0.1:9/mcp"],
    ] {
        let out = system::agent(device, &codex, &args, &[]).await?;
        ensure!(
            out.status.success(),
            "codex {}: {}",
            args.join(" "),
            system::first_lines(&out)
        );
    }
    let dir = device.root.join("x").join("skills").join("nekorisu");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), "---\nname: nekorisu\ndescription: x\n---\nbody\n")?;
    Ok(())
}

#[then(expr = "设备 {string} 的 Claude Code 里没有 MCP {string} 了")]
async fn claude_lacks(w: &mut E2e, name: String, server: String) -> anyhow::Result<()> {
    let out = system::agent(w.device(&name)?, &real_claude()?, &["mcp", "get", &server], &[]).await?;
    ensure!(
        !out.status.success(),
        "Claude Code still has {server}:\n{}",
        system::first_lines(&out)
    );
    Ok(())
}

#[then(expr = "设备 {string} 的 Codex 里没有 MCP {string} 了")]
async fn codex_lacks(w: &mut E2e, name: String, server: String) -> anyhow::Result<()> {
    let out = system::agent(w.device(&name)?, &real_codex()?, &["mcp", "get", &server], &[]).await?;
    ensure!(
        !out.status.success(),
        "Codex still has {server}:\n{}",
        system::first_lines(&out)
    );
    Ok(())
}

#[then(expr = "设备 {string} 的 Codex 的 skill {string} 不在了")]
async fn codex_skill_gone(w: &mut E2e, name: String, skill: String) -> anyhow::Result<()> {
    let dir = w.device(&name)?.root.join("x").join("skills").join(&skill);
    ensure!(!dir.exists(), "{} is still there", dir.display());
    Ok(())
}

/// Form encoding, for the consent page's answer.
fn form_body(pairs: &[(&str, &str)]) -> String {
    let encode = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Signs Claude Code in the way a person does, with the browser played by the scenario: a
/// script given as `BROWSER` writes down the address Claude Code opens; the scenario approves
/// it on the test server's consent page with the test administrator password and follows the
/// redirect back to Claude Code.
#[given(expr = "设备 {string} 的 Claude Code 用 claude 命令接了这个服务端的 MCP，并登录了")]
async fn claude_signed_in(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let server = w.server.as_ref().context("no server yet: 有一个真的服务端")?;
    let http = server.http.clone();
    let password = std::env::var("NEKORISU_E2E_SERVER_PASSWORD").context("NEKORISU_E2E_SERVER_PASSWORD")?;
    let claude = real_claude()?;
    let device = w.device_mut(&name)?;
    device
        .extra_path
        .push(claude.parent().context("a folder")?.to_path_buf());
    device.claude_signed_in = true;
    let mcp = format!("{http}/mcp");
    let out = system::agent(
        device,
        &claude,
        &[
            "mcp",
            "add",
            "--transport",
            "http",
            "-s",
            "user",
            "nekorisu-server",
            &mcp,
        ],
        &[],
    )
    .await?;
    ensure!(out.status.success(), "claude mcp add: {}", system::first_lines(&out));

    let url_file = device.root.join("browser-url.txt");
    let browser = device.root.join("browser.sh");
    std::fs::write(
        &browser,
        format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", url_file.display()),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755))?;
    }
    // Claude Code signs in only from a terminal.
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    let mut command = CommandBuilder::new(&claude);
    command.args(["mcp", "login", "nekorisu-server"]);
    command.cwd(&device.root);
    for (key, value) in device.env() {
        command.env(key, value);
    }
    command.env("BROWSER", &browser);
    let pair = native_pty_system().openpty(PtySize {
        rows: 40,
        cols: 400,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut login = pair.slave.spawn_command(command)?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let mut typing = pair.master.take_writer()?;
    let said = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink = said.clone();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let shown = || String::from_utf8_lossy(&said.lock().unwrap()).into_owned();
    let started = Instant::now();
    while !url_file.is_file() {
        if login.try_wait()?.is_some() || started.elapsed() > Duration::from_secs(60) {
            let _ = login.kill();
            bail!("Claude Code opened no sign-in page:\n{}", shown());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let url = std::fs::read_to_string(&url_file)?;
    let client_id = reqwest::Url::parse(&url)?
        .query_pairs()
        .find(|(k, _)| k == "client_id")
        .map(|(_, v)| v.into_owned())
        .context("no client_id in the sign-in address")?;
    let browser = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let page = browser.get(&url).send().await?.text().await?;
    let hidden = |field: &str| -> anyhow::Result<String> {
        let marker = format!("name=\"{field}\" value=\"");
        let start = page
            .find(&marker)
            .with_context(|| format!("no {field} on the consent page:\n{page}"))?
            + marker.len();
        Ok(page[start..].split('"').next().unwrap_or_default().to_string())
    };
    let body = form_body(&[
        ("request_id", &hidden("request_id")?),
        ("csrf", &hidden("csrf")?),
        ("password", &password),
        ("action", "approve"),
    ]);
    let reply = browser
        .post(format!("{http}/oauth/authorize"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;
    ensure!(reply.status().is_redirection(), "approving gave {}", reply.status());
    let back = reply
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .context("no redirect back to Claude Code")?
        .to_string();
    // Back to Claude Code: its own listener, or, when it asks for it, the address typed in.
    let _ = tokio::time::timeout(Duration::from_secs(10), browser.get(&back).send()).await;
    let started = Instant::now();
    let mut typed = false;
    let status = loop {
        if let Some(status) = login.try_wait()? {
            break status;
        }
        if !typed && started.elapsed() > Duration::from_secs(5) {
            use std::io::Write;
            typing.write_all(format!("{back}\r").as_bytes())?;
            typing.flush()?;
            typed = true;
        }
        if started.elapsed() > Duration::from_secs(60) {
            let _ = login.kill();
            bail!("claude mcp login did not finish:\n{}", shown());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    drop(typing);
    let said_text = shown();
    std::thread::spawn(move || drop(pair.master));
    ensure!(status.success(), "claude mcp login failed:\n{said_text}");

    let apps = server_apps(w).await?;
    ensure!(
        apps.contains(&client_id),
        "the panel does not list Claude Code: {apps:?}"
    );
    w.sessions.insert("claude-app".into(), client_id);
    Ok(())
}

async fn server_apps(w: &E2e) -> anyhow::Result<Vec<String>> {
    let server = w.server.as_ref().context("no server")?;
    let apps = server.get("/api/oauth/apps").await?;
    Ok(apps["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["client_id"].as_str().map(str::to_string))
        .collect())
}

#[then("panel 上没有这个 Claude Code 的授权了")]
async fn claude_app_gone(w: &mut E2e) -> anyhow::Result<()> {
    let id = w
        .sessions
        .get("claude-app")
        .context("Claude Code was not signed in")?
        .clone();
    let apps = server_apps(w).await?;
    ensure!(!apps.contains(&id), "the panel still lists Claude Code ({id})");
    Ok(())
}

// ---- doing ----

#[given(expr = "在设备 {string} 上运行 {string}")]
#[when(expr = "在设备 {string} 上运行 {string}")]
async fn run(w: &mut E2e, name: String, line: String) -> anyhow::Result<()> {
    let args = w.args(&line);
    w.last = Some(w.device(&name)?.run(&args, None).await?);
    Ok(())
}

#[when(expr = "在设备 {string} 的终端里运行 {string}，输入 {string}")]
async fn run_in_terminal(w: &mut E2e, name: String, line: String, input: String) -> anyhow::Result<()> {
    let args = w.args(&line);
    let input = expand_names(w, &input)?;
    let input = w.sessions.get(&input).cloned().unwrap_or(input);
    w.last = Some(w.device(&name)?.run_in_terminal(&args, &input).await?);
    Ok(())
}

// ---- what should be true ----

#[then("命令成功")]
async fn succeeded(w: &mut E2e) -> anyhow::Result<()> {
    let last = w.last()?;
    ensure!(last.success, "the command failed:\n{}", last.text());
    Ok(())
}

#[then("命令失败")]
async fn failed(w: &mut E2e) -> anyhow::Result<()> {
    let last = w.last()?;
    ensure!(!last.success, "the command should have failed:\n{}", last.text());
    Ok(())
}

#[then(expr = "输出里有 {string}")]
async fn output_has(w: &mut E2e, text: String) -> anyhow::Result<()> {
    let text = expand_names(w, &text)?;
    let last = w.last()?;
    ensure!(last.text().contains(&text), "no {text:?} in:\n{}", last.text());
    Ok(())
}

#[then(expr = "输出里没有 {string}")]
async fn output_lacks(w: &mut E2e, text: String) -> anyhow::Result<()> {
    let last = w.last()?;
    ensure!(
        !last.text().contains(&text),
        "{text:?} should not be in:\n{}",
        last.text()
    );
    Ok(())
}

#[then(expr = "设备 {string} 上的会话 {string} 在工作目录 {string} 对应的项目文件夹里")]
async fn session_in_folder(w: &mut E2e, name: String, session: String, cwd: String) -> anyhow::Result<()> {
    let id = w.sessions.get(&session).context("no such session in this scenario")?;
    let device = w.device(&name)?;
    let wanted = device
        .claude_home()
        .join("projects")
        .join(claude_folder(&w.expand(&cwd)))
        .join(format!("{id}.jsonl"));
    if !wanted.is_file() {
        bail!(
            "not at {}; it is at {:?}",
            wanted.display(),
            find_claude_session(device, id).map(|p| p.display().to_string())
        );
    }
    Ok(())
}

#[then(expr = "设备 {string} 上的会话 {string} 和设备 {string} 上的逐字节相同")]
async fn same_bytes(w: &mut E2e, a: String, session: String, b: String) -> anyhow::Result<()> {
    let id = w.sessions.get(&session).context("no such session in this scenario")?;
    let read = |name: &str| -> anyhow::Result<Vec<u8>> {
        let device = w.device(name)?;
        let file = find_claude_session(device, id).ok_or_else(|| anyhow!("device {name} has no session {session}"))?;
        Ok(std::fs::read(file)?)
    };
    ensure!(read(&a)? == read(&b)?, "the two copies differ");
    Ok(())
}

#[then(expr = "设备 {string} 的 nekorisu 目录不在了")]
async fn home_gone(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let home = w.device(&name)?.nekorisu_home();
    ensure!(!home.exists(), "{} is still there", home.display());
    Ok(())
}

#[then(expr = "设备 {string} 的 nekorisu 目录还在")]
async fn home_kept(w: &mut E2e, name: String) -> anyhow::Result<()> {
    let home = w.device(&name)?.nekorisu_home();
    ensure!(home.join("config.toml").is_file(), "{} is gone", home.display());
    Ok(())
}

#[then(expr = "设备 {string} 的主目录里有文件 {string}")]
async fn file_in_user_home(w: &mut E2e, name: String, rel: String) -> anyhow::Result<()> {
    let file = w.device(&name)?.home().join(rel);
    ensure!(file.is_file(), "no {}", file.display());
    Ok(())
}

#[then(expr = "设备 {string} 的 skill {string} 不在了")]
async fn skill_gone(w: &mut E2e, name: String, skill: String) -> anyhow::Result<()> {
    let dir = skill_dir(w.device(&name)?, &skill);
    ensure!(!dir.exists(), "{} is still there", dir.display());
    Ok(())
}

#[then(expr = "设备 {string} 的 skill {string} 还在")]
async fn skill_kept(w: &mut E2e, name: String, skill: String) -> anyhow::Result<()> {
    let dir = skill_dir(w.device(&name)?, &skill);
    ensure!(dir.join("SKILL.md").is_file(), "{} is gone", dir.display());
    Ok(())
}

#[then(expr = "设备 {string} 的 Claude Code 没有 MCP {string} 的日志了")]
async fn mcp_logs_gone(w: &mut E2e, name: String, server: String) -> anyhow::Result<()> {
    let dir = claude_cache(w.device(&name)?)
        .join("-some-project")
        .join(format!("mcp-logs-{server}"));
    ensure!(!dir.exists(), "{} is still there", dir.display());
    Ok(())
}

#[then(expr = "设备 {string} 的 config.toml 里有 {string}")]
async fn config_has(w: &mut E2e, name: String, text: String) -> anyhow::Result<()> {
    let config = std::fs::read_to_string(w.device(&name)?.config_file())?;
    ensure!(
        config.contains(&w.expand(&text)),
        "no {text:?} in config.toml:\n{config}"
    );
    Ok(())
}

#[then(expr = "设备 {string} 的 config.toml 里没有 {string}")]
async fn config_lacks(w: &mut E2e, name: String, text: String) -> anyhow::Result<()> {
    let config = std::fs::read_to_string(w.device(&name)?.config_file())?;
    ensure!(
        !config.contains(&w.expand(&text)),
        "{text:?} should not be in config.toml:\n{config}"
    );
    Ok(())
}

fn report(w: &E2e, name: &str) -> anyhow::Result<crate::standins::proto::ReportStateRequest> {
    let device = w.device(name)?;
    w.source
        .as_ref()
        .context("no source")?
        .report_from(&device.id)
        .ok_or_else(|| anyhow!("the source has no report from device {name}"))
}

#[then(expr = "来源收到设备 {string} 的上报，请求 {string} 是 {string}")]
async fn reported_request(w: &mut E2e, name: String, id: String, state: String) -> anyhow::Result<()> {
    let want = match state.as_str() {
        "待确认" => 1,
        "已接受" => 2,
        "已拒绝" => 3,
        other => bail!("states are 待确认, 已接受, 已拒绝, not {other}"),
    };
    let report = report(w, &name)?;
    let got = report.requests.iter().find(|r| r.id == id).map(|r| r.state);
    ensure!(got == Some(want), "request {id}: {got:?} in {:?}", report.requests);
    Ok(())
}

#[then(expr = "来源收到设备 {string} 的上报，里面有路径映射 {string}")]
async fn reported_path(w: &mut E2e, name: String, from: String) -> anyhow::Result<()> {
    let report = report(w, &name)?;
    ensure!(
        report.paths.iter().any(|p| p.from == w.expand(&from)),
        "no rule for {from} in {:?}",
        report.paths
    );
    Ok(())
}

#[then(expr = "来源收到设备 {string} 的上报，允许运行脚本")]
async fn reported_exec_on(w: &mut E2e, name: String) -> anyhow::Result<()> {
    ensure!(
        report(w, &name)?.exec_allowed,
        "the device says it does not run scripts"
    );
    Ok(())
}

#[then(expr = "来源收到设备 {string} 的上报，不允许运行脚本")]
async fn reported_exec_off(w: &mut E2e, name: String) -> anyhow::Result<()> {
    ensure!(!report(w, &name)?.exec_allowed, "the device says it runs scripts");
    Ok(())
}
