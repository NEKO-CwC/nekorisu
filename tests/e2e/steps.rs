//! The sentences scenarios are written in. In quoted text, `~A` is device A's home folder,
//! `@A` its device ID, `%A` its device name; session names given in a scenario stand for their IDs.

use anyhow::{Context, anyhow, bail, ensure};
use cucumber::gherkin::Step;
use cucumber::{given, then, when};

use crate::standins::{Dav, RealServer, Source};
use crate::world::{Device, E2e, claude_folder, find_claude_session, write_claude_session};

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
