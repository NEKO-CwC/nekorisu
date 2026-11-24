//! Claude Code: `projects/<project dir>/<session id>.jsonl`, the directory of
//! the same name (sub-agents, tool results, title), `file-history/<id>/` and
//! `tasks/<id>/`.

use std::fs;
use std::path::{Path, PathBuf};

use super::{
    AgentKind, BundleFile, Discovery, FileRole, Session, add_dir, contains, is_uuid, last_timestamp, mtime_ms,
    read_head, read_tail, relative,
};
use crate::paths;

const DAY_MS: i64 = 86_400_000;

pub fn discover() -> Discovery {
    discover_in(paths::claude_home())
}

/// Discovery in a given Claude Code home (`CLAUDE_CONFIG_DIR`): what tests use.
pub fn discover_in(home: PathBuf) -> Discovery {
    let projects = home.join("projects");
    let cleanup_days = cleanup_period_days(&home);
    let mut sessions = Vec::new();
    if let Ok(dirs) = fs::read_dir(&projects) {
        for project in dirs.flatten() {
            let project_dir = project.path();
            if !project_dir.is_dir() {
                continue;
            }
            let project_name = project.file_name().to_string_lossy().into_owned();
            let Ok(entries) = fs::read_dir(&project_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Some(id) = name.strip_suffix(".jsonl") else {
                    continue;
                };
                if !is_uuid(id) {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let mut files = vec![BundleFile {
                    path: path.clone(),
                    rel: relative(&home, &path),
                    role: FileRole::Transcript,
                    size: meta.len(),
                    mtime_ms: mtime_ms(&meta),
                }];
                add_dir(&home, &project_dir.join(id), &mut files, role_in_session_dir);
                add_dir(&home, &home.join("file-history").join(id), &mut files, |_| {
                    FileRole::FileHistory
                });
                add_dir(&home, &home.join("tasks").join(id), &mut files, |_| FileRole::Sidecar);

                let tail = read_tail(&path, 256 * 1024).unwrap_or_default();
                let last_activity_ms = last_timestamp(&tail).unwrap_or(files[0].mtime_ms);
                let desktop = is_desktop(&path, &tail);
                sessions.push(Session {
                    agent: AgentKind::ClaudeCode,
                    id: id.to_string(),
                    cleanup_at_ms: cleanup_at(&files, desktop, cleanup_days),
                    main: path,
                    files,
                    parent: None,
                    project: project_name.clone(),
                    last_activity_ms,
                    compressed: false,
                    archived_by_agent: false,
                });
            }
        }
    }
    Discovery {
        agent: AgentKind::ClaudeCode,
        found: projects.is_dir(),
        home,
        sessions,
    }
}

fn role_in_session_dir(inner: &str) -> FileRole {
    if inner.starts_with("subagents/") && inner.ends_with(".jsonl") {
        FileRole::Subagent
    } else if inner.starts_with("tool-results/") {
        FileRole::ToolResult
    } else {
        FileRole::Sidecar
    }
}

/// `cleanupPeriodDays` from settings.json; Claude Code's default is 30.
fn cleanup_period_days(home: &Path) -> i64 {
    fs::read(home.join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|v| v.get("cleanupPeriodDays").and_then(serde_json::Value::as_i64))
        .filter(|days| *days > 0)
        .unwrap_or(30)
}

/// Transcripts whose first or last record says they came from the desktop app
/// (or a local agent) are exempt from the cleanup; their other files are not.
fn is_desktop(path: &Path, tail: &[u8]) -> bool {
    let head = read_head(path, 64 * 1024).unwrap_or_default();
    let first = head.split(|&c| c == b'\n').next().unwrap_or_default();
    let last = tail.split(|&c| c == b'\n').rfind(|l| !l.is_empty()).unwrap_or_default();
    const MARKERS: [&[u8]; 3] = [
        b"\"entrypoint\":\"claude-desktop\"",
        b"\"entrypoint\":\"claude-desktop-3p\"",
        b"\"entrypoint\":\"local-agent\"",
    ];
    [first, last]
        .iter()
        .any(|line| MARKERS.iter().any(|m| contains(line, m)))
}

/// Claude Code deletes a file once its mtime is older than the cleanup period
/// (checked from its binary).
fn cleanup_at(files: &[BundleFile], desktop: bool, days: i64) -> i64 {
    files
        .iter()
        .enumerate()
        .filter(|(i, _)| !(desktop && *i == 0))
        .map(|(_, f)| f.mtime_ms + days * DAY_MS)
        .min()
        .unwrap_or(0)
}
