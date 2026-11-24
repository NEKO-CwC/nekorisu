//! Codex: rollouts under `sessions/YYYY/MM/DD/` and `archived_sessions/`
//! (`.jsonl` or Codex's own `.jsonl.zst`), plus `generated_images/<tid>/` and
//! `visualizations/YYYY/MM/DD/<tid>/`.
//! Sub-agent threads are sessions of their own that point at their parent.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{
    AgentKind, BundleFile, Discovery, FileRole, Session, add_dir, is_uuid, last_timestamp, mtime_ms, read_tail,
    relative, walk_files,
};
use crate::paths;

pub fn discover() -> Discovery {
    discover_in(paths::codex_home())
}

/// Discovery in a given Codex home (`CODEX_HOME`): what tests use.
pub fn discover_in(home: PathBuf) -> Discovery {
    let mut rollouts = Vec::new();
    walk_files(&home.join("sessions"), &mut rollouts);
    let live = rollouts.len();
    walk_files(&home.join("archived_sessions"), &mut rollouts);

    let mut threads: HashMap<String, Session> = HashMap::new();
    for (index, path) in rollouts.iter().enumerate() {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(thread_id) = thread_id_from_name(name) else {
            continue;
        };
        let compressed = name.ends_with(".jsonl.zst");
        let Ok(meta) = fs::metadata(path) else { continue };
        let file = BundleFile {
            path: path.clone(),
            rel: relative(&home, path),
            role: FileRole::Transcript,
            size: meta.len(),
            mtime_ms: mtime_ms(&meta),
        };
        let archived = index >= live;
        let session = threads.entry(thread_id.to_string()).or_insert_with(|| {
            let head = read_session_meta(path, compressed);
            Session {
                agent: AgentKind::Codex,
                id: thread_id.to_string(),
                main: path.clone(),
                files: Vec::new(),
                parent: head.as_ref().and_then(|h| h.parent.clone()),
                project: head.and_then(|h| h.cwd).unwrap_or_default(),
                last_activity_ms: 0,
                compressed,
                archived_by_agent: archived,
                cleanup_at_ms: 0, // Codex compresses old rollouts but doesn't delete them
            }
        });
        // A revert writes a second rollout for the same thread; the newest one is live.
        if session.files.iter().all(|f| f.mtime_ms <= file.mtime_ms) {
            session.main = path.clone();
            session.compressed = compressed;
            session.archived_by_agent = archived;
        }
        session.files.push(file);
    }

    let visualizations = visualization_dirs(&home.join("visualizations"));
    let mut sessions: Vec<Session> = threads.into_values().collect();
    for session in &mut sessions {
        add_dir(
            &home,
            &home.join("generated_images").join(&session.id),
            &mut session.files,
            |_| FileRole::Asset,
        );
        if let Some(dirs) = visualizations.get(&session.id) {
            for dir in dirs {
                add_dir(&home, dir, &mut session.files, |_| FileRole::Asset);
            }
        }
        let fallback = session.files.iter().map(|f| f.mtime_ms).max().unwrap_or(0);
        // TODO: a compressed rollout's tail needs decompressing; until core caches
        // that, its last activity falls back to mtime, which can be later than the
        // real last turn.
        session.last_activity_ms = if session.compressed {
            fallback
        } else {
            read_tail(&session.main, 256 * 1024)
                .and_then(|t| last_timestamp(&t))
                .unwrap_or(fallback)
        };
    }
    Discovery {
        agent: AgentKind::Codex,
        found: home.join("sessions").is_dir(),
        home,
        sessions,
    }
}

/// `rollout-<YYYY-MM-DDTHH-MM-SS>-<thread id>[_<rollout id>].jsonl[.zst]`
fn thread_id_from_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("rollout-")?;
    if !(rest.ends_with(".jsonl") || rest.ends_with(".jsonl.zst")) {
        return None;
    }
    let id = rest.get(20..56)?;
    is_uuid(id).then_some(id)
}

struct Head {
    parent: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct MetaLine {
    #[serde(rename = "type")]
    kind: String,
    payload: MetaPayload,
}

#[derive(Deserialize)]
struct MetaPayload {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    parent_thread_id: Option<String>,
    #[serde(default)]
    source: Option<serde_json::Value>,
}

/// The first record of a rollout is `session_meta`: thread id, cwd, source.
fn read_session_meta(path: &Path, compressed: bool) -> Option<Head> {
    let file = File::open(path).ok()?;
    let reader: Box<dyn Read> = if compressed {
        Box::new(zstd::stream::read::Decoder::new(file).ok()?)
    } else {
        Box::new(file)
    };
    let mut line = Vec::new();
    BufReader::new(reader.take(64 << 20))
        .read_until(b'\n', &mut line)
        .ok()?;
    let meta: MetaLine = serde_json::from_slice(&line).ok()?;
    if meta.kind != "session_meta" {
        return None;
    }
    let parent = meta.payload.parent_thread_id.or_else(|| {
        meta.payload
            .source
            .as_ref()?
            .pointer("/subagent/thread_spawn/parent_thread_id")?
            .as_str()
            .map(str::to_owned)
    });
    Some(Head {
        parent,
        cwd: meta.payload.cwd,
    })
}

/// `visualizations/YYYY/MM/DD/<thread id>/`, keyed by thread id.
fn visualization_dirs(root: &Path) -> HashMap<String, Vec<PathBuf>> {
    let mut found: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut level = vec![root.to_path_buf()];
    for _ in 0..3 {
        level = level
            .iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flat_map(|entries| entries.flatten().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
    }
    for day in level {
        let Ok(entries) = fs::read_dir(&day) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_uuid(&name) && entry.path().is_dir() {
                found.entry(name).or_default().push(entry.path());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_id_comes_from_the_file_name() {
        let id = "019a1b2c-3d4e-5f60-7182-93a4b5c6d7e8";
        assert_eq!(
            thread_id_from_name(&format!("rollout-2026-11-01T10-20-30-{id}.jsonl")),
            Some(id)
        );
        assert_eq!(
            thread_id_from_name(&format!("rollout-2026-11-01T10-20-30-{id}.jsonl.zst")),
            Some(id)
        );
        assert_eq!(
            thread_id_from_name(&format!("rollout-2026-11-01T10-20-30-{id}_abc.jsonl")),
            Some(id)
        );
        assert_eq!(thread_id_from_name("rollout-compress-1.tmp"), None);
        assert_eq!(thread_id_from_name("session_index.jsonl"), None);
    }
}
