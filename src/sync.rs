//! What this machine last uploaded or restored, per session: the manifest its files match and,
//! per file, enough to tell cheaply what is new since then (`status`), plus which manifest the
//! next upload continues and under which names (`archive`). One small JSON file per session
//! under `~/.nekorisu/sync/<agent>/<id>.json`, replaced whole each time.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agents::{self, AgentKind, BundleFile, FileRole, Session};
use crate::archive::Manifest;
use crate::timefmt::now_ms;

/// The head and tail fingerprints cover this many bytes each.
const PRINT: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Storage name in config.toml.
    pub storage: String,
    /// "archive" or "restore".
    pub op: String,
    /// The manifest the local files matched then.
    pub manifest: String,
    /// When that manifest was written to the storage.
    pub manifest_at_ms: i64,
    /// When this record was written.
    pub at_ms: i64,
    /// Folders that have another name here than in the storage (a Claude Code project folder
    /// of another device). Uploads keep the storage's names.
    #[serde(default)]
    pub renames: Vec<Rename>,
    pub files: Vec<Synced>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rename {
    /// Agent-relative prefix here, `/`-separated and ending in `/`.
    pub local: String,
    /// The same prefix in the storage.
    pub stored: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Synced {
    /// Path in the storage (the manifest's).
    pub path: String,
    /// Bytes in the storage: the start of the local file that matched them.
    pub size: u64,
    pub sha256: String,
    /// SHA-256 of the first and of the last `PRINT` bytes of those `size` bytes; not kept for
    /// files Codex compressed.
    pub head: Option<String>,
    pub tail: Option<String>,
    /// The file on disk when the record was written.
    pub disk: Option<Disk>,
    pub line_based: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub size: u64,
    pub mtime_ms: i64,
}

/// The storage path of an agent-relative local path: Codex's `.zst` dropped, renames applied.
pub fn stored_path(rel: &str, renames: &[Rename]) -> String {
    let rel = rel
        .strip_suffix(".zst")
        .filter(|r| r.ends_with(".jsonl"))
        .unwrap_or(rel);
    for rename in renames {
        if let Some(rest) = rel.strip_prefix(&rename.local) {
            return format!("{}{rest}", rename.stored);
        }
    }
    rel.to_string()
}

/// The agent-relative local path of a storage path: renames applied backwards.
pub fn local_path(stored: &str, renames: &[Rename]) -> String {
    for rename in renames {
        if let Some(rest) = stored.strip_prefix(&rename.stored) {
            return format!("{}{rest}", rename.local);
        }
    }
    stored.to_string()
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store { dir }
    }

    fn file(&self, agent: AgentKind, id: &str) -> PathBuf {
        self.dir.join(agent.name()).join(format!("{id}.json"))
    }

    pub fn load(&self, agent: AgentKind, id: &str) -> Option<Record> {
        let bytes = fs::read(self.file(agent, id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self, agent: AgentKind, id: &str, record: &Record) -> io::Result<()> {
        let file = self.file(agent, id);
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = file.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
        fs::rename(&tmp, &file)
    }
}

/// A local file that now holds what a manifest entry says.
pub struct LocalCopy {
    pub path: PathBuf,
    /// Codex's `.jsonl.zst`.
    pub compressed: bool,
    pub line_based: bool,
}

/// The record for local files that match `manifest`. `locals` is keyed by storage path; files
/// of the manifest that are not here are recorded without a disk state. Blocking: reads the
/// fingerprints.
pub fn record(
    storage: &str,
    op: &str,
    manifest: &Manifest,
    renames: Vec<Rename>,
    locals: &HashMap<String, LocalCopy>,
) -> Record {
    let files = manifest
        .files
        .iter()
        .map(|entry| {
            let local = locals.get(&entry.path);
            let line_based = matches!(entry.role.as_str(), "transcript" | "subagent");
            let (head, tail, disk) = match local {
                Some(local) => {
                    let disk = fs::metadata(&local.path).ok().map(|m| Disk {
                        size: m.len(),
                        mtime_ms: agents::mtime_ms(&m),
                    });
                    if local.compressed {
                        (None, None, disk)
                    } else {
                        let (head, tail) = fingerprints(&local.path, entry.size).unwrap_or((None, None));
                        (head, tail, disk)
                    }
                }
                None => (None, None, None),
            };
            Synced {
                path: entry.path.clone(),
                size: entry.size,
                sha256: entry.sha256.clone(),
                head,
                tail,
                disk,
                line_based: local.map_or(line_based, |l| l.line_based),
            }
        })
        .collect();
    Record {
        storage: storage.to_string(),
        op: op.to_string(),
        manifest: manifest.id.clone(),
        manifest_at_ms: manifest.created_at_ms,
        at_ms: now_ms(),
        renames,
        files,
    }
}

type Print = (Option<String>, Option<String>);

fn fingerprints(path: &Path, size: u64) -> io::Result<Print> {
    let span = size.min(PRINT);
    Ok((
        Some(hash_range(path, 0, span)?),
        Some(hash_range(path, size - span, size)?),
    ))
}

fn hash_range(path: &Path, start: u64, end: u64) -> io::Result<String> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0; (end - start) as usize];
    file.read_exact(&mut buf)?;
    Ok(hex::encode(Sha256::digest(&buf)))
}

/// What a session has that its record does not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delta {
    pub new_bytes: u64,
    pub new_lines: u64,
    /// Bytes recorded as uploaded are not at the start of the local file any more.
    pub rewritten: bool,
}

impl Delta {
    fn add(&mut self, other: Delta) {
        self.new_bytes += other.new_bytes;
        self.new_lines += other.new_lines;
        self.rewritten |= other.rewritten;
    }
}

/// Results per (file, size, mtime, recorded size, recorded hash): an unchanged file is read once.
pub type DeltaCache = HashMap<(PathBuf, u64, i64, u64, String), Delta>;

/// What `session` has beyond `record`: new files, appended complete lines, rewritten files.
/// Blocking. A file that still matches its disk state is not read at all; an appended one is
/// read from where the upload ended; only a rewritten or new file is read whole.
pub fn session_delta(session: &Session, record: &Record, cache: &mut DeltaCache) -> Delta {
    let synced: HashMap<&str, &Synced> = record.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut total = Delta::default();
    for file in &session.files {
        let stored = stored_path(&file.rel, &record.renames);
        let synced = synced.get(stored.as_str()).copied();
        let key = (
            file.path.clone(),
            file.size,
            file.mtime_ms,
            synced.map_or(0, |s| s.size),
            synced.map(|s| s.sha256.clone()).unwrap_or_default(),
        );
        let delta = match cache.get(&key) {
            Some(delta) => *delta,
            None => match file_delta(file, synced) {
                Ok(delta) => {
                    cache.insert(key, delta);
                    delta
                }
                Err(_) => Delta::default(), // gone since discovery: nothing to say about it
            },
        };
        total.add(delta);
    }
    total
}

fn file_delta(file: &BundleFile, synced: Option<&Synced>) -> io::Result<Delta> {
    let compressed = file.rel.ends_with(".jsonl.zst");
    let line_based = matches!(file.role, FileRole::Transcript | FileRole::Subagent);
    let open = || -> io::Result<Box<dyn Read>> {
        let raw = File::open(&file.path)?;
        Ok(if compressed {
            Box::new(zstd::stream::read::Decoder::new(raw)?)
        } else {
            Box::new(raw)
        })
    };
    let whole = |rewritten: bool| -> io::Result<Delta> {
        let (new_bytes, new_lines) = count(open()?, line_based)?;
        Ok(Delta {
            new_bytes,
            new_lines,
            rewritten,
        })
    };
    let Some(synced) = synced else {
        return whole(false);
    };
    if let Some(disk) = &synced.disk
        && disk.size == file.size
        && disk.mtime_ms == file.mtime_ms
        && (compressed || disk.size == synced.size)
    {
        return Ok(Delta::default());
    }
    if compressed || synced.head.is_none() || synced.tail.is_none() {
        // No fingerprints: hash the recorded part.
        let mut reader = open()?;
        let mut hasher = Sha256::new();
        let copied = io::copy(&mut reader.by_ref().take(synced.size), &mut HashWriter(&mut hasher))?;
        if copied < synced.size || hex::encode(hasher.finalize()) != synced.sha256 {
            return whole(true);
        }
        let (new_bytes, new_lines) = count(reader, line_based)?;
        return Ok(Delta {
            new_bytes,
            new_lines,
            rewritten: false,
        });
    }
    if file.size < synced.size || fingerprints(&file.path, synced.size)? != (synced.head.clone(), synced.tail.clone()) {
        return whole(true);
    }
    let mut reader = File::open(&file.path)?;
    reader.seek(SeekFrom::Start(synced.size))?;
    let (new_bytes, new_lines) = count(reader, line_based)?;
    Ok(Delta {
        new_bytes,
        new_lines,
        rewritten: false,
    })
}

/// Bytes and lines in `reader`: for line-based files only complete lines count (what an upload
/// would send); other files count all their bytes and no lines.
fn count(reader: impl Read, line_based: bool) -> io::Result<(u64, u64)> {
    let mut reader = BufReader::with_capacity(1 << 20, reader);
    let (mut total, mut complete, mut lines) = (0u64, 0u64, 0u64);
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let n = buf.len();
        if line_based {
            for (i, &b) in buf.iter().enumerate() {
                if b == b'\n' {
                    lines += 1;
                    complete = total + i as u64 + 1;
                }
            }
        }
        total += n as u64;
        reader.consume(n);
    }
    Ok(if line_based { (complete, lines) } else { (total, 0) })
}

struct HashWriter<'a>(&'a mut Sha256);

impl io::Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::FileEntry;

    fn entry(path: &str, bytes: &[u8], role: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            role: role.to_string(),
            size: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(bytes)),
            mtime_ms: 0,
            form: "as-is".to_string(),
            segments: Vec::new(),
            lines: 0,
        }
    }

    fn manifest(files: Vec<FileEntry>) -> Manifest {
        Manifest {
            format: "nekorisu.manifest/1".to_string(),
            id: "dev-1".to_string(),
            parent: None,
            session: Default::default(),
            device: "dev".to_string(),
            created_at_ms: 42,
            files,
            links: Default::default(),
            rewrite: false,
            encryption: "none".to_string(),
        }
    }

    fn bundle_file(home: &Path, rel: &str, role: FileRole) -> BundleFile {
        let path = home.join(rel);
        let meta = fs::metadata(&path).unwrap();
        BundleFile {
            rel: rel.to_string(),
            role,
            size: meta.len(),
            mtime_ms: agents::mtime_ms(&meta),
            path,
        }
    }

    fn session(files: Vec<BundleFile>) -> Session {
        Session {
            agent: AgentKind::ClaudeCode,
            id: "s".to_string(),
            main: files[0].path.clone(),
            files,
            parent: None,
            project: String::new(),
            last_activity_ms: 0,
            compressed: false,
            archived_by_agent: false,
            cleanup_at_ms: 0,
        }
    }

    #[test]
    fn renames_go_both_ways_and_zst_is_dropped() {
        let renames = vec![Rename {
            local: "projects/-Users-me-a/".into(),
            stored: "projects/C--Users-x-a/".into(),
        }];
        assert_eq!(
            stored_path("projects/-Users-me-a/s.jsonl", &renames),
            "projects/C--Users-x-a/s.jsonl"
        );
        assert_eq!(
            local_path("projects/C--Users-x-a/s/subagents/a.jsonl", &renames),
            "projects/-Users-me-a/s/subagents/a.jsonl"
        );
        assert_eq!(stored_path("sessions/2026/r.jsonl.zst", &[]), "sessions/2026/r.jsonl");
        assert_eq!(
            stored_path("generated_images/x.png.zst", &[]),
            "generated_images/x.png.zst"
        );
        assert_eq!(local_path("file-history/s/a", &renames), "file-history/s/a");
    }

    #[test]
    fn the_delta_counts_appended_lines_new_files_and_rewrites() {
        let home = tempfile::tempdir().unwrap();
        let main = "projects/p/s.jsonl";
        fs::create_dir_all(home.path().join("projects/p/s/tool-results")).unwrap();
        let uploaded = b"{\"a\":1}\n{\"b\":2}\n";
        fs::write(home.path().join(main), uploaded).unwrap();
        let locals: HashMap<String, LocalCopy> = [(
            main.to_string(),
            LocalCopy {
                path: home.path().join(main),
                compressed: false,
                line_based: true,
            },
        )]
        .into();
        let rec = record(
            "test",
            "archive",
            &manifest(vec![entry(main, uploaded, "transcript")]),
            Vec::new(),
            &locals,
        );
        assert_eq!((rec.manifest.as_str(), rec.manifest_at_ms), ("dev-1", 42));
        let mut cache = DeltaCache::new();
        let now = |home: &Path| session(vec![bundle_file(home, main, FileRole::Transcript)]);
        assert_eq!(session_delta(&now(home.path()), &rec, &mut cache), Delta::default());

        // Appended: one complete line and a partial one.
        let mut grown = uploaded.to_vec();
        grown.extend_from_slice(b"{\"c\":3}\n{\"partial\"");
        fs::write(home.path().join(main), &grown).unwrap();
        let delta = session_delta(&now(home.path()), &rec, &mut cache);
        assert_eq!(
            delta,
            Delta {
                new_bytes: 8,
                new_lines: 1,
                rewritten: false
            }
        );

        // A new side file counts whole, without lines.
        fs::write(home.path().join("projects/p/s/tool-results/t.txt"), b"12345").unwrap();
        let with_side = session(vec![
            bundle_file(home.path(), main, FileRole::Transcript),
            bundle_file(home.path(), "projects/p/s/tool-results/t.txt", FileRole::ToolResult),
        ]);
        assert_eq!(session_delta(&with_side, &rec, &mut cache).new_bytes, 13);

        // Rewritten: the start changed; everything counts again.
        let mut rewritten = b"{\"A\":1}\n{\"b\":2}\n".to_vec();
        rewritten.extend_from_slice(b"{\"c\":3}\n");
        fs::write(home.path().join(main), &rewritten).unwrap();
        let delta = session_delta(&now(home.path()), &rec, &mut cache);
        assert_eq!(
            delta,
            Delta {
                new_bytes: 24,
                new_lines: 3,
                rewritten: true
            }
        );
        // Shorter than what was uploaded.
        fs::write(home.path().join(main), b"{\"a\":1}\n").unwrap();
        assert!(session_delta(&now(home.path()), &rec, &mut cache).rewritten);
    }

    #[test]
    fn compressed_files_are_compared_decompressed() {
        let home = tempfile::tempdir().unwrap();
        let rel = "sessions/2026/11/01/r.jsonl.zst";
        fs::create_dir_all(home.path().join("sessions/2026/11/01")).unwrap();
        let text = b"{\"a\":1}\n{\"b\":2}\n";
        fs::write(home.path().join(rel), zstd::encode_all(&text[..], 3).unwrap()).unwrap();
        // Uploaded as plain text before Codex compressed it: no disk state that matches.
        let rec = Record {
            storage: "t".into(),
            op: "archive".into(),
            manifest: "d-1".into(),
            manifest_at_ms: 1,
            at_ms: 1,
            renames: Vec::new(),
            files: vec![Synced {
                path: "sessions/2026/11/01/r.jsonl".into(),
                size: text.len() as u64,
                sha256: hex::encode(Sha256::digest(text)),
                head: Some("x".into()),
                tail: Some("y".into()),
                disk: Some(Disk { size: 1, mtime_ms: 1 }),
                line_based: true,
            }],
        };
        let s = session(vec![bundle_file(home.path(), rel, FileRole::Transcript)]);
        assert_eq!(session_delta(&s, &rec, &mut DeltaCache::new()), Delta::default());
    }
}
