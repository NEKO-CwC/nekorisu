//! `nekorisu source add | login | remove | sync | status | accept | reject | allow-exec | deny-exec`: the
//! trusted source is set only here, on the device, and its token goes into the keychain.
//! Pulling from it is core's job; these commands write the local files and ask core to pull.
//! Saying yes to the source (`accept`, `allow-exec`) needs the person at a terminal typing the
//! request ID, or the device name, again: an agent running a command has no terminal.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::bail;
use clap::Subcommand;
use serde_json::json;

use crate::cli::rpc_error;
use crate::client;
use crate::cloud::Source;
use crate::paths::Paths;
use crate::proto::{
    AnswerSourceRequestRequest, GetSourceStateRequest, SetExecRequest, SourceRequest, SourceState, SyncSourceRequest,
};
use crate::secrets::{self, Kind};
use crate::timefmt::{ago, now_ms};

#[derive(Subcommand)]
pub enum SourceAction {
    /// 设置可信来源（服务端）：地址和它签配置用的公钥。只能在本机设置；一台设备只认一个来源，再运行一次就是替换
    Add {
        /// 来源的名字，比如 home
        name: String,
        /// 地址，比如 https://nekorisu.example.com:8443
        url: String,
        /// 来源的公钥（base64），服务端的配置页上有；换钥期间可以写两把
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
    },
    /// 设置来源的令牌：终端里输入，不回显；不在终端里就从标准输入读一行。存进系统钥匙串，没有钥匙串的系统存进只有本人能读的文件
    Login,
    /// 撤掉来源：删掉它的地址、公钥、令牌和它发来的配置，关掉“接受所有内容”。config.toml 里本机写的设置不动
    Remove,
    /// 现在就问来源有没有新配置（core 平时按配置的间隔自己问）
    Sync,
    /// 来源、在用的配置、上次检查的结果、待确认的请求，以及各项设置和路径映射最后用的是哪边的
    Status {
        /// 输出 JSON
        #[arg(long)]
        json: bool,
    },
    /// 接受来源的强制覆盖请求：它要改的值写进 config.toml，成为这台设备自己的值并解锁。要在终端里再输入一次请求 ID 确认
    Accept { id: String },
    /// 拒绝来源的一个请求（强制覆盖或放行运行脚本）
    Reject { id: String },
    /// 允许运行来源下发的脚本（“接受所有内容”）。有来源的放行请求时输入它的 ID 确认，没有时输入这台设备的名字确认
    AllowExec,
    /// 不再运行来源下发的脚本，马上生效，不用确认
    DenyExec,
}

pub async fn run(action: SourceAction) -> anyhow::Result<()> {
    let paths = Paths::from_env();
    match action {
        SourceAction::Add { name, url, keys } => add(&paths, name, url, keys).await,
        SourceAction::Login => login(&paths).await,
        SourceAction::Remove => remove(&paths).await,
        SourceAction::Sync => sync().await,
        SourceAction::Status { json } => status(json).await,
        SourceAction::Accept { id } => answer(id, true).await,
        SourceAction::Reject { id } => answer(id, false).await,
        SourceAction::AllowExec => allow_exec().await,
        SourceAction::DenyExec => deny_exec().await,
    }
}

/// Asks the person at the terminal to type `expected` again. Refuses without a terminal.
fn confirm(prompt: &str, expected: &str) -> anyhow::Result<()> {
    if !io::stdin().is_terminal() {
        bail!("这一步要你本人在终端里确认：请自己在终端里运行这个命令（Agent 不能替你确认）");
    }
    eprint!("{prompt}");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    if line.trim() != expected {
        bail!("输入的和 {expected} 不一样，什么都没改");
    }
    Ok(())
}

async fn state(core: &mut client::Client) -> anyhow::Result<SourceState> {
    Ok(core
        .get_source_state(GetSourceStateRequest { quick: true })
        .await
        .map_err(rpc_error)?
        .into_inner())
}

/// A value for people: text without its JSON quotes, lists and objects as JSON.
fn plain(value_json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(value_json) {
        Ok(serde_json::Value::String(s)) => s,
        _ => value_json.to_string(),
    }
}

fn describe(r: &SourceRequest) -> String {
    match r.kind.as_str() {
        "force" => {
            let changes: Vec<String> = r
                .changes
                .iter()
                .map(|c| {
                    format!(
                        "{} 改成 {}{}",
                        c.key,
                        plain(&c.value_json),
                        if c.known { "" } else { "（这个版本不会改）" }
                    )
                })
                .collect();
            format!("强制覆盖：{}", changes.join("；"))
        }
        _ => match r.action.as_str() {
            "allow" => "请这台设备允许运行来源下发的脚本".to_string(),
            "revoke" => "收回运行脚本的许可（已经生效）".to_string(),
            other => format!("不认识的请求 {other}"),
        },
    }
}

async fn answer(id: String, accept: bool) -> anyhow::Result<()> {
    let mut core = client::connect("cli").await?;
    let s = state(&mut core).await?;
    let Some(request) = s.requests.iter().find(|r| r.id == id) else {
        bail!("在用的配置里没有给这台设备的请求 {id}；nekorisu source status 看有哪些");
    };
    println!("请求 {id}：{}", describe(request));
    if accept {
        confirm(&format!("确认接受的话，再输入一次请求 ID（{id}）："), &id)?;
    }
    core.answer_source_request(AnswerSourceRequestRequest { id: id.clone(), accept })
        .await
        .map_err(rpc_error)?;
    println!("已{}请求 {id}", if accept { "接受" } else { "拒绝" });
    Ok(())
}

async fn allow_exec() -> anyhow::Result<()> {
    let mut core = client::connect("cli").await?;
    let s = state(&mut core).await?;
    if s.exec_allowed {
        println!("已经允许运行来源下发的脚本（{}）", s.exec_why);
        return Ok(());
    }
    let pending: Vec<&SourceRequest> = s
        .requests
        .iter()
        .filter(|r| r.kind == "exec" && r.action == "allow" && r.state == "pending")
        .collect();
    println!(
        "打开以后，来源下发的脚本在校验签名和 SHA-256 之后会在这台设备上运行。来源随时可以关掉它，你也可以用 nekorisu source deny-exec 关掉。"
    );
    let request_id = match pending.as_slice() {
        [] => {
            confirm(
                &format!("确认的话，输入这台设备的名字（{}）：", s.device_name),
                &s.device_name,
            )?;
            String::new()
        }
        [one] => {
            confirm(
                &format!("来源发来了放行请求 {}；确认的话，输入这个请求 ID：", one.id),
                &one.id,
            )?;
            one.id.clone()
        }
        many => {
            let ids: Vec<&str> = many.iter().map(|r| r.id.as_str()).collect();
            if !io::stdin().is_terminal() {
                bail!("这一步要你本人在终端里确认：请自己在终端里运行这个命令（Agent 不能替你确认）");
            }
            eprint!(
                "来源发来了几个放行请求（{}）；确认的话，输入其中一个请求 ID：",
                ids.join("、")
            );
            io::stderr().flush().ok();
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            let typed = line.trim().to_string();
            if !ids.contains(&typed.as_str()) {
                bail!("输入的不是其中的请求 ID，什么都没改");
            }
            typed
        }
    };
    core.set_exec(SetExecRequest {
        allowed: true,
        request_id,
    })
    .await
    .map_err(rpc_error)?;
    println!("已允许运行来源下发的脚本");
    Ok(())
}

async fn deny_exec() -> anyhow::Result<()> {
    let mut core = client::connect("cli").await?;
    core.set_exec(SetExecRequest {
        allowed: false,
        request_id: String::new(),
    })
    .await
    .map_err(rpc_error)?;
    println!("已关掉：不会运行来源下发的脚本");
    Ok(())
}

async fn add(paths: &Paths, name: String, url: String, keys: Vec<String>) -> anyhow::Result<()> {
    let source = Source {
        name,
        url: url.trim_end_matches('/').to_string(),
        keys: keys.iter().map(|k| k.trim().to_string()).collect(),
    };
    let changed = source.save(paths)?;
    println!(
        "已{}来源 {}：{}（公钥 {}）",
        if changed { "设置" } else { "确认" },
        source.name,
        source.url,
        source.key_ids().join("、")
    );
    let has_token = {
        let (paths, name) = (paths.clone(), source.name.clone());
        tokio::task::spawn_blocking(move || secrets::load_kind(Kind::Source, &paths, &name)).await??
    }
    .is_some();
    if !has_token {
        println!("下一步：nekorisu source login（输入服务端给设备用的令牌）");
        return Ok(());
    }
    sync_if_running().await;
    Ok(())
}

async fn login(paths: &Paths) -> anyhow::Result<()> {
    let Some(source) = Source::load(paths)? else {
        bail!("还没有设置来源；先运行 nekorisu source add <名字> <地址> --key <公钥>");
    };
    let token = crate::storage_cli::read_password(&format!("来源 {} 的令牌（输入时不显示）：", source.name))?;
    let (store_paths, name) = (paths.clone(), source.name.clone());
    tokio::task::spawn_blocking(move || secrets::store_kind(Kind::Source, &store_paths, &name, &token)).await??;
    println!("已把来源 {} 的令牌存进{}", source.name, secrets::place());
    sync_if_running().await;
    Ok(())
}

async fn remove(paths: &Paths) -> anyhow::Result<()> {
    let source = Source::load(paths)?;
    let (blocking_paths, name) = (paths.clone(), source.as_ref().map(|s| s.name.clone()));
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // The token first: when that fails the source is still set, and running this again finishes.
        if let Some(name) = name {
            secrets::delete_kind(Kind::Source, &blocking_paths, &name)?;
        }
        // Without a source this only clears what an earlier one may have left.
        Source::remove(&blocking_paths)
    })
    .await??;
    let Some(source) = source else {
        println!("没有设置来源，不用撤掉");
        return Ok(());
    };
    println!(
        "已撤掉来源 {}（{}）：它的令牌和发来的配置都删了，config.toml 里本机写的设置没动",
        source.name, source.url
    );
    // A running core goes back to config.toml alone now, not at its next look.
    if let Some(mut core) = client::connect_existing().await
        && let Err(err) = core.sync_source(SyncSourceRequest {}).await
    {
        eprintln!(
            "没能通知 core（{}）；运行 nekorisu core stop，再运行 nekorisu core start，来源下发的设置就不再生效",
            err.message()
        );
    }
    Ok(())
}

/// Asks a running core to pull now; without one, says so.
async fn sync_if_running() {
    match client::connect_existing().await {
        Some(mut core) => match core.sync_source(SyncSourceRequest {}).await {
            Ok(state) => print_sync(&state.into_inner()),
            Err(err) => eprintln!(
                "没能让 core 去取配置（{}）；core 是旧版本的话，运行 nekorisu core stop，再运行 nekorisu core start",
                err.message()
            ),
        },
        None => println!("core 没在运行；它启动后会去取配置（nekorisu core start）"),
    }
}

fn print_sync(state: &SourceState) {
    match state.outcome.as_str() {
        "updated" => println!("已从来源取到第 {} 版配置", state.applied_serial),
        "unchanged" => println!("来源没有新配置，在用的是第 {} 版", state.applied_serial),
        "failed" => eprintln!("没能从来源取配置：{}", state.error),
        _ => println!("还没有设置来源"),
    }
    if !state.min_client_version.is_empty() {
        eprintln!(
            "来源要求客户端至少是 {}，这台是 {}：运行 nekorisu update",
            state.min_client_version,
            env!("CARGO_PKG_VERSION")
        );
    }
}

async fn sync() -> anyhow::Result<()> {
    let mut core = client::connect("cli").await?;
    let state = core
        .sync_source(SyncSourceRequest {})
        .await
        .map_err(rpc_error)?
        .into_inner();
    if !state.configured {
        bail!("还没有设置来源；先运行 nekorisu source add <名字> <地址> --key <公钥>");
    }
    print_sync(&state);
    if state.outcome == "failed" {
        bail!("取配置失败");
    }
    Ok(())
}

async fn status(json_out: bool) -> anyhow::Result<()> {
    let mut core = client::connect("cli").await?;
    let s = core
        .get_source_state(GetSourceStateRequest { quick: false })
        .await
        .map_err(rpc_error)?
        .into_inner();
    if json_out {
        let paths: Vec<_> = s
            .paths
            .iter()
            .map(|p| {
                json!({
                    "from": p.from, "to": p.to, "origin": p.origin, "locked": p.locked,
                    "other": if p.has_other { json!(p.other) } else { json!(null) },
                })
            })
            .collect();
        let requests: Vec<_> = s
            .requests
            .iter()
            .map(|r| {
                json!({
                    "id": r.id, "kind": r.kind, "state": r.state, "action": r.action,
                    "changes": r.changes.iter().map(|c| json!({ "key": c.key, "value_json": c.value_json, "known": c.known })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let values: Vec<_> = s
            .values
            .iter()
            .map(|v| {
                json!({
                    "key": v.key, "value_json": v.value_json, "origin": v.origin, "locked": v.locked,
                    "other_json": if v.has_other { json!(v.other_json) } else { json!(null) },
                })
            })
            .collect();
        let out = json!({
            "configured": s.configured, "name": s.name, "url": s.url, "key_ids": s.key_ids,
            "token_set": s.token_set, "applied_serial": s.applied_serial,
            "applied_issued_at": s.applied_issued_at, "checked_ms": s.checked_ms,
            "error": s.error, "min_client_version": s.min_client_version, "paths": paths,
            "requests": requests, "values": values,
            "exec": { "allowed": s.exec_allowed, "why": s.exec_why },
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if s.configured {
        println!("来源 {}：{}", s.name, s.url);
        println!("公钥：{}", s.key_ids.join("、"));
        println!(
            "令牌：{}",
            if s.token_set {
                "已设置"
            } else {
                "没设置（nekorisu source login）"
            }
        );
        if s.applied_serial == 0 {
            println!("在用的配置：还没有");
        } else {
            println!("在用的配置：第 {} 版，签发于 {}", s.applied_serial, s.applied_issued_at);
        }
        if s.checked_ms > 0 {
            println!("上次检查：{}", ago(s.checked_ms, now_ms()));
        }
        if !s.error.is_empty() {
            println!("上次检查出错：{}", s.error);
        }
        if !s.min_client_version.is_empty() {
            println!("来源要求客户端至少是 {}：运行 nekorisu update", s.min_client_version);
        }
    } else {
        println!("还没有设置来源（nekorisu source add <名字> <地址> --key <公钥>）");
    }
    let pending: Vec<&SourceRequest> = s.requests.iter().filter(|r| r.state == "pending").collect();
    if !pending.is_empty() {
        println!("待你确认的请求：");
        for r in &pending {
            let how = if r.kind == "exec" {
                "nekorisu source allow-exec 或 nekorisu source reject"
            } else {
                "nekorisu source accept 或 nekorisu source reject"
            };
            println!(
                "  {}：{}（{how} {}）",
                r.id,
                describe(r),
                if r.kind == "exec" { "" } else { &r.id }
            );
        }
    }
    for r in s.requests.iter().filter(|r| r.state != "pending") {
        println!(
            "  已{} {}：{}",
            if r.state == "accepted" { "接受" } else { "拒绝" },
            r.id,
            describe(r)
        );
    }
    println!(
        "运行来源下发的脚本：{}{}",
        if s.exec_allowed { "允许" } else { "不允许" },
        if s.exec_why.is_empty() {
            String::new()
        } else {
            format!("（{}）", s.exec_why)
        }
    );
    for v in &s.values {
        let whose = match (v.origin.as_str(), v.locked) {
            ("local", true) => "本机设的，已锁定",
            ("local", false) => "本机设的",
            _ => "来源下发的",
        };
        let other = if v.has_other {
            format!("；另一边是 {}", plain(&v.other_json))
        } else {
            String::new()
        };
        println!("{}：{}（{whose}{other}）", v.key, plain(&v.value_json));
    }
    if s.paths.is_empty() {
        println!("路径映射：没有");
        return Ok(());
    }
    println!("路径映射（恢复别的设备的会话时用）：");
    for p in &s.paths {
        let to = if p.to.is_empty() { "不映射" } else { p.to.as_str() };
        let whose = match (p.origin.as_str(), p.locked) {
            ("local", true) => "本机写的，已锁定",
            ("local", false) => "本机写的",
            _ => "来源下发的",
        };
        let other = if !p.has_other {
            String::new()
        } else if p.origin == "local" {
            format!(
                "；来源下发的是 {}",
                if p.other.is_empty() { "不映射" } else { &p.other }
            )
        } else {
            format!("；本机写的是 {}", p.other)
        };
        let drop_note = if p.to.is_empty() {
            "，来源删掉了本机的这条"
        } else {
            ""
        };
        println!("  {} → {to}（{whose}{drop_note}{other}）", p.from);
    }
    Ok(())
}
