//! Discovery: find each agent's sessions and the files that belong to them
//!. Read-only. Reads file names, sizes,
//! mtimes and a few structural fields (ids, timestamps, parent links), never
//! message content.

pub mod claude_code;
pub mod codex;

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::proto;
use crate::timefmt::parse_rfc3339_ms;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
}

impl AgentKind {
    pub const ALL: [AgentKind; 2] = [AgentKind::ClaudeCode, AgentKind::Codex];

    pub fn name(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude-code" | "claude" => Some(AgentKind::ClaudeCode),
            "codex" => Some(AgentKind::Codex),
            _ => None,
        }
    }

    pub fn to_proto(self) -> proto::Agent {
        match self {
            AgentKind::ClaudeCode => proto::Agent::ClaudeCode,
            AgentKind::Codex => proto::Agent::Codex,
        }
    }

    pub fn from_proto(agent: proto::Agent) -> Option<Self> {
        match agent {
            proto::Agent::ClaudeCode => Some(AgentKind::ClaudeCode),
            proto::Agent::Codex => Some(AgentKind::Codex),
            proto::Agent::Unspecified => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileRole {
    Transcript,
    Subagent,
    ToolResult,
    FileHistory,
    Asset,
    Sidecar,
}

#[derive(Clone, Debug)]
pub struct BundleFile {
    pub path: PathBuf,
    /// Path relative to the agent's home directory, `/`-separated on every system: it becomes
    /// part of what is stored, which must not depend on the uploading machine.
    pub rel: String,
    pub role: FileRole,
    pub size: u64,
    pub mtime_ms: i64,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub agent: AgentKind,
    pub id: String,
    /// The main transcript (for Codex: the newest rollout of the thread).
    pub main: PathBuf,
    /// Every file of the bundle, the main transcript included.
    pub files: Vec<BundleFile>,
    /// Parent thread, for Codex sub-agent threads.
    pub parent: Option<String>,
    pub project: String,
    pub last_activity_ms: i64,
    pub compressed: bool,
    pub archived_by_agent: bool,
    /// When the agent itself starts deleting files of this bundle; 0 = never.
    pub cleanup_at_ms: i64,
}

impl Session {
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

pub struct Discovery {
    pub agent: AgentKind,
    pub home: PathBuf,
    pub found: bool,
    pub sessions: Vec<Session>,
}

pub fn discover(agent: AgentKind) -> Discovery {
    match agent {
        AgentKind::ClaudeCode => claude_code::discover(),
        AgentKind::Codex => codex::discover(),
    }
}

/// Every regular file under `dir`, recursively. Symlinks are not followed.
pub(crate) fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            walk_files(&entry.path(), out);
        } else if kind.is_file() {
            out.push(entry.path());
        }
    }
}

pub(crate) fn mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn relative(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(inside) => slash_path(inside),
        // Not under the home: kept as it was, rather than joining an absolute path's parts.
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// A relative path written with `/` between its parts, whatever this system uses (`\` on Windows).
pub(crate) fn slash_path(path: &Path) -> String {
    let parts: Vec<_> = path.iter().map(|part| part.to_string_lossy()).collect();
    parts.join("/")
}

/// Adds every file under `dir` to `files`. `role` gets the path inside `dir`.
pub(crate) fn add_dir(home: &Path, dir: &Path, files: &mut Vec<BundleFile>, role: impl Fn(&str) -> FileRole) {
    let mut found = Vec::new();
    walk_files(dir, &mut found);
    found.sort();
    for path in found {
        let Ok(meta) = fs::metadata(&path) else { continue };
        let inner = path.strip_prefix(dir).map(slash_path).unwrap_or_default();
        files.push(BundleFile {
            rel: relative(home, &path),
            role: role(&inner),
            size: meta.len(),
            mtime_ms: mtime_ms(&meta),
            path,
        });
    }
}

/// At most `max` bytes from the start of a file.
pub(crate) fn read_head(path: &Path, max: u64) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    File::open(path).ok()?.take(max).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// At most `max` bytes from the end of a file.
pub(crate) fn read_tail(path: &Path, max: u64) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// The last parseable `"timestamp":"…"` in `buf`, in ms. Quotes inside JSON
/// strings are escaped, so this only matches real fields.
pub(crate) fn last_timestamp(buf: &[u8]) -> Option<i64> {
    const NEEDLE: &[u8] = b"\"timestamp\":\"";
    let mut end = buf.len();
    while let Some(pos) = rfind(&buf[..end], NEEDLE) {
        let start = pos + NEEDLE.len();
        if let Some(len) = buf[start..].iter().position(|&c| c == b'"')
            && let Some(ms) = std::str::from_utf8(&buf[start..start + len])
                .ok()
                .and_then(parse_rfc3339_ms)
        {
            return Some(ms);
        }
        end = pos;
    }
    None
}

pub(crate) fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len())
        .rev()
        .find(|&i| &hay[i..i + needle.len()] == needle)
}

pub fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_last_timestamp() {
        let buf = br#"{"timestamp":"2000-03-01T00:00:00Z"}
{"type":"custom-title"}
{"timestamp":"2000-03-01T00:00:01Z","text":"say \"timestamp\":\"x\""}
"#;
        assert_eq!(last_timestamp(buf), Some(951_868_801_000));
    }

    #[test]
    fn relative_paths_use_slashes_on_every_system() {
        let home = Path::new("agent-home");
        let file = home.join("projects").join("C--Users-x-proj").join("a.jsonl");
        assert_eq!(relative(home, &file), "projects/C--Users-x-proj/a.jsonl");
        let dir = home.join("projects").join("p").join("id");
        let inner = dir.join("subagents").join("agent-1.jsonl");
        assert_eq!(slash_path(inner.strip_prefix(&dir).unwrap()), "subagents/agent-1.jsonl");
        #[cfg(windows)]
        assert_eq!(
            relative(Path::new(r"C:\h"), Path::new(r"C:\h\sessions\2026\11\01\r.jsonl")),
            "sessions/2026/11/01/r.jsonl"
        );
    }

    #[test]
    fn recognises_uuids() {
        assert!(is_uuid("73ffc2c4-aa52-4618-9f84-7cd27a0d3c16"));
        assert!(!is_uuid("agent-a1b2"));
    }
}
