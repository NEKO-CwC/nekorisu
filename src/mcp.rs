//! MCP over stdio: newline-delimited JSON-RPC 2.0, tools only. A thin client:
//! every tool call goes to core over the Unix socket. Tool descriptions
//! stay short because they ride along with every model request. Hand-written rather than an SDK: the surface
//! is small, and this keeps startup fast.

use anyhow::anyhow;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::cli::{
    is_running, parse_agent, parse_lines, parse_ref, rpc_error, run_archive, status_json, summary_json, wait_task,
};
use crate::client::{self, Client};
use crate::proto::{ArchiveRequest, FetchRequest, RestoreRequest, ScanRequest, StatusRequest, TaskState};
use crate::render;

const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_FETCH_LIMIT: u32 = 200;
const DEFAULT_MAX_FIELD_CHARS: usize = 2_000;
/// An archive or restore call waits this long; after that it answers "still running".
const ARCHIVE_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

pub async fn run() -> anyhow::Result<()> {
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    let mut core: Option<Client> = None;
    while let Some(line) = input.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(message) => handle(&mut core, message).await,
            Err(err) => Some(error_reply(Value::Null, -32700, &format!("parse error: {err}"))),
        };
        if let Some(reply) = reply {
            let mut text = reply.to_string();
            text.push('\n');
            output.write_all(text.as_bytes()).await?;
            output.flush().await?;
        }
    }
    Ok(())
}

async fn handle(core: &mut Option<Client>, message: Value) -> Option<Value> {
    let method = message.get("method")?.as_str()?.to_string();
    let id = message.get("id")?.clone(); // notifications carry no id and get no reply
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method.as_str() {
        "initialize" => initialize(&params),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => call(core, &params).await,
        _ => return Some(error_reply(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn initialize(params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    let version = if PROTOCOL_VERSIONS.contains(&asked) {
        asked
    } else {
        PROTOCOL_VERSIONS[0]
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "nekorisu", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "The user's past agent sessions (Claude Code, Codex), kept byte-exact. Read a digest first, then fetch only what the person said (kinds=[\"user\"]), then more lines if needed. Upload, restore or delete only when the user asks."
    })
}

fn tools() -> Value {
    let session = json!({ "type": "string", "description": "<agent>:<id>, or just <id>" });
    let agent = json!({ "type": "string", "enum": ["claude-code", "codex"] });
    let sessions = json!({ "type": "array", "items": { "type": "string" } });
    let storage = json!({ "type": "string", "description": "storage name; omit when only one is configured" });
    json!([
        {
            "name": "scan",
            "description": "Count local sessions per agent, and their size.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "status",
            "description": "Local sessions, newest first: size, last activity, upload state, and when the agent itself will delete files.",
            "inputSchema": { "type": "object", "properties": { "agent": agent, "limit": { "type": "integer", "minimum": 1 } } }
        },
        {
            "name": "search",
            "description": "Find sessions by words, time, device or agent.",
            "inputSchema": { "type": "object", "properties": {
                "query": { "type": "string" }, "agent": agent, "device": { "type": "string" },
                "since": { "type": "string", "description": "RFC 3339" }, "limit": { "type": "integer" }
            }, "required": ["query"] }
        },
        {
            "name": "fetch",
            "description": "Read lines of a session by line number or kind (user = what the person said; assistant; reasoning; tool; meta). Sessions not on this machine are read from the storage. String fields are cut to max_field_chars (default 2000). whole=true only returns a local path to the full file.",
            "inputSchema": { "type": "object", "properties": {
                "session": session,
                "file": { "type": "string", "description": "a file of the bundle, e.g. subagents/agent-x.jsonl; default: the main transcript" },
                "lines": { "type": "string", "description": "e.g. 1-20,35,40-" },
                "kinds": { "type": "array", "items": { "type": "string", "enum": ["user", "assistant", "reasoning", "tool", "meta"] } },
                "limit": { "type": "integer", "description": "max lines, default 200" },
                "max_field_chars": { "type": "integer", "description": "0 = no cut" },
                "whole": { "type": "boolean" },
                "version": { "type": "string", "description": "a version in the storage, or \"latest\"; default: this machine's copy, else the latest" },
                "storage": storage.clone()
            }, "required": ["session"] }
        },
        {
            "name": "restore",
            "description": "Put a session from the storage back where its agent keeps it, byte-exact, then tell the user the resume command. If it lists several newest versions, ask the user which. If the local copy has more, ask before replace=true.",
            "inputSchema": { "type": "object", "properties": {
                "session": session, "version": { "type": "string" },
                "replace": { "type": "boolean", "description": "replace a local copy that has more; it is moved aside, not deleted" },
                "storage": storage.clone()
            }, "required": ["session"] }
        },
        {
            "name": "archive",
            "description": "Upload sessions (with their sub-agent sessions) to the user's storage, optionally with digests.",
            "inputSchema": { "type": "object", "properties": {
                "sessions": sessions, "storage": storage,
                "digest_ids": sessions
            }, "required": ["sessions"] }
        },
        {
            "name": "digest",
            "description": "Get, write or replace a digest. What goes in it: see the nekorisu skill.",
            "inputSchema": { "type": "object", "properties": {
                "action": { "type": "string", "enum": ["get", "put"] }, "id": { "type": "string" },
                "sessions": sessions, "body": { "type": "string" }
            }, "required": ["action"] }
        },
        {
            "name": "log",
            "description": "Append or list organizing notes: tags, remarks, which sessions belong together.",
            "inputSchema": { "type": "object", "properties": {
                "action": { "type": "string", "enum": ["append", "list"] }, "sessions": sessions,
                "tags": { "type": "array", "items": { "type": "string" } }, "note": { "type": "string" }
            }, "required": ["action"] }
        }
    ])
}

async fn call(core: &mut Option<Client>, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let client = match core {
        Some(client) => client,
        None => match client::connect("mcp").await {
            Ok(client) => core.insert(client),
            Err(err) => return tool_result(Err(anyhow!("连不上 nekorisu core：{err}"))),
        },
    };
    let result = match name {
        "scan" => scan(client).await,
        "status" => status(client, &args).await,
        "fetch" => fetch(client, &args).await,
        "archive" => archive(client, &args).await,
        "restore" => restore(client, &args).await,
        "search" | "digest" | "log" => Err(anyhow!("{name} is not implemented yet (in progress)")),
        other => Err(anyhow!("unknown tool: {other}")),
    };
    if let Err(err) = &result {
        // core went away (restarted, upgraded): reconnect on the next call.
        if err
            .downcast_ref::<tonic::Status>()
            .is_some_and(|s| s.code() == tonic::Code::Unavailable)
        {
            *core = None;
            return tool_result(result);
        }
    }
    // What the trusted source waits on: the agent tells the person, who answers in a terminal.
    let reminders = crate::cli::source_reminders(client).await;
    let result = if reminders.is_empty() {
        result
    } else {
        let note = format!(
            "\n\n[nekorisu] Tell the user (they must answer in their own terminal; you cannot): {}",
            reminders.join("; ")
        );
        result.map(|text| text + &note).map_err(|err| anyhow!("{err}{note}"))
    };
    tool_result(result)
}

async fn scan(client: &mut Client) -> anyhow::Result<String> {
    let response = client
        .scan(ScanRequest::default())
        .await
        .map_err(rpc_error)?
        .into_inner();
    let rows: Vec<String> = response.agents.iter().map(|a| summary_json(a).to_string()).collect();
    Ok(rows.join("\n"))
}

async fn status(client: &mut Client, args: &Value) -> anyhow::Result<String> {
    let agents = match args.get("agent").and_then(Value::as_str) {
        Some(name) => vec![parse_agent(name)? as i32],
        None => Vec::new(),
    };
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(30) as u32;
    let response = client
        .status(StatusRequest {
            agents,
            since_ms: 0,
            limit,
        })
        .await
        .map_err(rpc_error)?
        .into_inner();
    let rows: Vec<String> = response.sessions.iter().map(|s| status_json(s).to_string()).collect();
    Ok(if rows.is_empty() {
        "no local sessions".to_string()
    } else {
        rows.join("\n")
    })
}

async fn fetch(client: &mut Client, args: &Value) -> anyhow::Result<String> {
    let session = args
        .get("session")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("session is required"))?;
    let kinds = args
        .get("kinds")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    let request = FetchRequest {
        session: Some(parse_ref(session)?),
        file: args.get("file").and_then(Value::as_str).unwrap_or_default().to_string(),
        lines: match args.get("lines").and_then(Value::as_str) {
            Some(spec) => parse_lines(spec)?,
            None => Vec::new(),
        },
        kinds,
        limit: args
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_FETCH_LIMIT, |n| n as u32),
        whole: args.get("whole").and_then(Value::as_bool).unwrap_or(false),
        version: text_arg(args, "version"),
        storage: text_arg(args, "storage"),
    };
    let max_chars = args
        .get("max_field_chars")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_MAX_FIELD_CHARS, |n| n as usize);
    let whole = request.whole;
    let response = client.fetch(request).await.map_err(rpc_error)?.into_inner();
    if whole {
        return Ok(format!("path: {}\nsize: {} bytes", response.path, response.size));
    }
    let bodies = render::read_spans(&response.path, &response.spans)?;
    let source = if response.version == "local" {
        String::new()
    } else {
        format!(" (version {} in the storage)", response.version)
    };
    let mut text = format!(
        "{} of {} lines{source}{}; transcripts in this session: {}\n",
        response.spans.len(),
        response.total_lines,
        if response.truncated {
            " (more match: raise limit or narrow lines)"
        } else {
            ""
        },
        response.files.join(", ")
    );
    for (span, body) in response.spans.iter().zip(&bodies) {
        text.push_str(&format!(
            "L{} {} {}\n",
            span.line,
            span.kind,
            render::line_text(body, max_chars)
        ));
    }
    Ok(text)
}

async fn archive(client: &mut Client, args: &Value) -> anyhow::Result<String> {
    let strings = |key: &str| -> Vec<String> {
        args.get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let sessions = strings("sessions")
        .iter()
        .map(|s| parse_ref(s))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if sessions.is_empty() {
        return Err(anyhow!("sessions is required"));
    }
    let request = ArchiveRequest {
        sessions,
        storage: args
            .get("storage")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        digest_ids: strings("digest_ids"),
    };
    let task = run_archive(client, request, false, Some(ARCHIVE_WAIT)).await?;
    // One JSON object per session: manifest, files, new_bytes, new_lines, up_to_date, rewrite, error.
    let rows: Vec<Value> = serde_json::from_str(&task.result_json).unwrap_or_default();
    let mut text: Vec<String> = rows.iter().map(Value::to_string).collect();
    if is_running(&task) {
        text.push(format!("still running in core (task {}); check status later", task.id));
    } else if task.state() != TaskState::Committed {
        text.push(format!("failed: {}", task.error));
        return Err(anyhow!(text.join("\n")));
    }
    Ok(text.join("\n"))
}

async fn restore(client: &mut Client, args: &Value) -> anyhow::Result<String> {
    let session = args
        .get("session")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("session is required"))?;
    let request = RestoreRequest {
        session: Some(parse_ref(session)?),
        version: text_arg(args, "version"),
        replace: args.get("replace").and_then(Value::as_bool).unwrap_or(false),
        storage: text_arg(args, "storage"),
    };
    let task = client.restore(request).await.map_err(rpc_error)?.into_inner();
    let task = wait_task(client, task, false, Some(ARCHIVE_WAIT)).await?;
    // One JSON object per session: manifest, written, completed, unchanged, replaced, folder,
    // resume (the command to go on), registered (Codex), note.
    let rows: Vec<Value> = serde_json::from_str(&task.result_json).unwrap_or_default();
    let mut text: Vec<String> = rows.iter().map(Value::to_string).collect();
    if is_running(&task) {
        text.push(format!("still running in core (task {}); try again later", task.id));
    } else if task.state() != TaskState::Committed {
        return Err(anyhow!("{}", task.error));
    } else {
        text.push(task.summary.clone());
    }
    Ok(text.join("\n"))
}

fn text_arg(args: &Value, key: &str) -> String {
    args.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn tool_result(result: anyhow::Result<String>) -> Value {
    match result {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
        Err(err) => json!({ "content": [{ "type": "text", "text": err.to_string() }], "isError": true }),
    }
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
