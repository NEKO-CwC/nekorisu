//! Reading what is in the storage: a session's manifests and its newest versions, device files,
//! line maps, segments and single lines. Manifests, device files and line maps never change once
//! written, so each is downloaded once and kept under `~/.nekorisu/`. Whatever is downloaded
//! whole is checked against its SHA-256; a single line is checked against its line map (it must
//! end with a line break exactly where the map says).
//!
//! Reading a session to look at it fetches only lines, not whole files: by default the lines of
//! what the person said and the agent's replies of every segment it touches (the pruned copy),
//! plus whatever else was asked for. `copy_form = "full"` fetches whole segments instead.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::agents::AgentKind;
use crate::archive::{self, FileEntry, Manifest, ROOT, SegmentInfo};
use crate::linemap::Kind;
use crate::paths::Paths;
use crate::proto::LineSpan;
use crate::storage::Webdav;
use crate::timefmt::{ago, human_bytes, now_ms};

/// Line requests run this many at a time.
const PARALLEL: usize = 8;
/// Two wanted lines closer than this are fetched in one request.
const GAP: u64 = 32 * 1024;
/// A segment this small, or needed at least half, is fetched whole. Tests use a small one, as
/// their segments are small.
const SMALL_SEGMENT: u64 = if cfg!(test) { 1024 } else { 256 * 1024 };
/// Answers to line requests are kept this long for the client to read.
const VIEW_TTL: Duration = Duration::from_secs(3600);

/// Names read from the storage become parts of local paths: only plain ones are taken.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn check_sha(sha: &str) -> anyhow::Result<()> {
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("存储里的 manifest 记了一个不像 SHA-256 的名字：{sha}");
    }
    Ok(())
}

/// The session is not in the storage.
#[derive(Debug)]
pub struct Missing(pub String);

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Missing {}

/// The caller has to choose: several newest versions, or an id that is not there.
#[derive(Debug)]
pub struct Choose(pub String);

impl std::fmt::Display for Choose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Choose {}

/// What a device file says that is used here. Devices registered before the client wrote `home`
/// have only their name.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeviceInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub home: Option<String>,
}

/// One segment's line map as stored: `[offset, length, kind, raw type index, ts_ms]`.
#[derive(Debug, Clone, Deserialize)]
pub struct SegMap {
    #[serde(default)]
    pub raw_types: Vec<String>,
    #[serde(default)]
    pub lines: Vec<(u64, u64, String, u16, i64)>,
}

/// Which version to read.
pub enum Wanted<'a> {
    /// The newest one; when there are several (a fork), the caller must choose.
    Latest,
    Id(&'a str),
}

/// What to pick out of a file.
pub struct LineQuery<'a> {
    /// 1-based, inclusive; an end of 0 means to the last line. Empty: every line.
    pub ranges: Vec<(u64, u64)>,
    pub kinds: &'a [Kind],
    /// 0 = no limit.
    pub limit: usize,
}

pub struct Selected {
    /// A local file holding exactly the selected lines, each followed by `\n`.
    pub path: PathBuf,
    pub spans: Vec<LineSpan>,
    pub truncated: bool,
}

pub struct Remote {
    pub store: Arc<Webdav>,
    paths: Paths,
}

impl Remote {
    pub fn new(store: Arc<Webdav>, paths: Paths) -> Remote {
        Remote { store, paths }
    }

    /// The agent the session belongs to and the ids of its manifests. A bare id that both
    /// agents have needs the agent named.
    pub async fn find(&self, id: &str, agent: Option<AgentKind>) -> anyhow::Result<(AgentKind, Vec<String>)> {
        if !plain(id) {
            return Err(Missing(format!("会话 ID 不对：{id}")).into());
        }
        let candidates: Vec<AgentKind> = match agent {
            Some(agent) => vec![agent],
            None => AgentKind::ALL.to_vec(),
        };
        let mut found = Vec::new();
        for agent in candidates {
            let ids = archive::list_manifests(&self.store, &archive::session_dir(agent.name(), id)).await?;
            if !ids.is_empty() {
                found.push((agent, ids));
            }
        }
        match found.len() {
            0 => Err(Missing(format!("存储里没有会话 {id}")).into()),
            1 => Ok(found.remove(0)),
            _ => Err(Choose(format!("{id} 在存储里对应不止一个会话，请写成 <agent>:<id>")).into()),
        }
    }

    /// Every manifest named in `ids`, from the local copies where there are some.
    pub async fn manifests(&self, agent: AgentKind, id: &str, ids: &[String]) -> anyhow::Result<Vec<Manifest>> {
        let local_dir = self.paths.manifests_dir().join(agent.name()).join(id);
        let dir = archive::session_dir(agent.name(), id);
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for mid in ids.iter().filter(|mid| plain(mid)) {
            let local = local_dir.join(format!("{mid}.json"));
            match fs::read(&local)
                .ok()
                .and_then(|b| archive::parse_manifest(&b, mid).ok())
            {
                Some(manifest) => out.push(manifest),
                None => missing.push(mid.clone()),
            }
        }
        let mut tasks = tokio::task::JoinSet::new();
        let gate = Arc::new(tokio::sync::Semaphore::new(PARALLEL));
        for mid in missing {
            let (store, gate, path) = (self.store.clone(), gate.clone(), format!("{dir}/manifests/{mid}.json"));
            tasks.spawn(async move {
                let _permit = gate.acquire_owned().await;
                let bytes = store.get(&path, None).await;
                (mid, bytes)
            });
        }
        while let Some(joined) = tasks.join_next().await {
            let (mid, bytes) = joined?;
            let bytes = bytes?;
            let manifest = archive::parse_manifest(&bytes, &mid)?;
            // A copy that cannot be kept only means downloading it again next time.
            let _ = write_atomic(&local_dir.join(format!("{mid}.json")), &bytes);
            out.push(manifest);
        }
        out.sort_by(|a, b| (a.created_at_ms, &a.id).cmp(&(b.created_at_ms, &b.id)));
        Ok(out)
    }

    /// Every device file in the storage, from the local copies where there are some.
    pub async fn devices(&self) -> HashMap<String, DeviceInfo> {
        let local_dir = self.paths.home.join("devices");
        let mut out = HashMap::new();
        let Ok(entries) = self.store.list(&format!("{ROOT}/devices")).await else {
            return out;
        };
        for entry in entries.into_iter().filter(|e| !e.is_dir) {
            let Some(id) = entry.name.strip_suffix(".json").filter(|id| plain(id)) else {
                continue;
            };
            let local = local_dir.join(&entry.name);
            let bytes = match fs::read(&local) {
                Ok(bytes) => bytes,
                Err(_) => match self.store.get(&format!("{ROOT}/devices/{}", entry.name), None).await {
                    Ok(bytes) => {
                        let _ = write_atomic(&local, &bytes);
                        bytes
                    }
                    Err(_) => continue,
                },
            };
            if let Ok(info) = serde_json::from_slice::<DeviceInfo>(&bytes) {
                out.insert(id.to_string(), info);
            }
        }
        out
    }

    /// A segment's line map.
    pub async fn linemap(&self, dir: &str, sha: &str) -> anyhow::Result<SegMap> {
        check_sha(sha)?;
        let local = self.paths.cache_dir().join("linemaps").join(format!("{sha}.json"));
        if let Ok(bytes) = fs::read(&local)
            && let Ok(map) = serde_json::from_slice(&bytes)
        {
            return Ok(map);
        }
        let bytes = self.store.get(&format!("{dir}/linemaps/{sha}.json"), None).await?;
        let map = serde_json::from_slice(&bytes).with_context(|| format!("行号表 {sha} 读不懂"))?;
        let _ = write_atomic(&local, &bytes);
        Ok(map)
    }

    /// A whole segment, checked against its name and size. With `keep`, it stays in the cache.
    pub async fn segment(&self, dir: &str, file: &str, seg: &SegmentInfo, keep: bool) -> anyhow::Result<Vec<u8>> {
        check_sha(&seg.sha256)?;
        let cached = self.segment_cache(&seg.sha256);
        if let Ok(bytes) = fs::read(&cached)
            && bytes.len() as u64 == seg.len
        {
            return Ok(bytes);
        }
        let path = format!("{dir}/files/{file}/{}-{}.seg", seg.start, seg.sha256);
        let bytes = self.store.get(&path, None).await?;
        if bytes.len() as u64 != seg.len || hex::encode(Sha256::digest(&bytes)) != seg.sha256 {
            bail!(
                "存储里的分段 {file}@{} 和 manifest 记的对不上（大小或 SHA-256 不同）",
                seg.start
            );
        }
        if keep {
            let _ = write_atomic(&cached, &bytes);
        }
        Ok(bytes)
    }

    fn segment_cache(&self, sha: &str) -> PathBuf {
        self.paths.cache_dir().join("segments").join(sha)
    }

    /// The whole file of `entry`, assembled from its segments and checked against the entry's
    /// size and SHA-256. Kept in the cache, named after that hash.
    pub async fn whole_file(&self, dir: &str, entry: &FileEntry) -> anyhow::Result<PathBuf> {
        check_sha(&entry.sha256)?;
        let name = entry.path.rsplit('/').next().filter(|n| plain(n)).unwrap_or("file");
        let target = self
            .paths
            .cache_dir()
            .join("files")
            .join(format!("{}-{name}", &entry.sha256[..16]));
        if fs::metadata(&target).is_ok_and(|m| m.len() == entry.size) {
            return Ok(target);
        }
        let tmp = target.with_extension("part");
        if let Some(parent) = tmp.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut size = 0;
        for seg in &entry.segments {
            if seg.start != size {
                bail!("{} 的分段不连续（{} 处）", entry.path, seg.start);
            }
            let bytes = self.segment(dir, &entry.path, seg, false).await?;
            hasher.update(&bytes);
            out.write_all(&bytes)?;
            size += bytes.len() as u64;
        }
        out.sync_all()?;
        drop(out);
        if size != entry.size || hex::encode(hasher.finalize()) != entry.sha256 {
            let _ = fs::remove_file(&tmp);
            bail!("{} 拼起来和 manifest 记的对不上（大小或 SHA-256 不同）", entry.path);
        }
        fs::rename(&tmp, &target)?;
        Ok(target)
    }

    /// Picks lines of `entry` and writes them to a local file. Segments are visited in order and
    /// only until the limit is reached; their line maps are read as needed.
    pub async fn select(
        &self,
        dir: &str,
        entry: &FileEntry,
        query: &LineQuery<'_>,
        full: bool,
    ) -> anyhow::Result<Selected> {
        let limit = if query.limit == 0 { usize::MAX } else { query.limit };
        let in_ranges =
            |n: u64| query.ranges.is_empty() || query.ranges.iter().any(|&(s, e)| n >= s.max(1) && (e == 0 || n <= e));
        let mut picked: Vec<(LineSpan, usize, usize)> = Vec::new(); // span, segment, line in it
        let mut bodies: HashMap<(usize, usize), Vec<u8>> = HashMap::new();
        let mut truncated = false;
        let mut first = 1u64; // global number of the segment's first line
        for (s, seg) in entry.segments.iter().enumerate() {
            let last = first + seg.lines; // one past
            let overlaps = seg.lines > 0
                && (query.ranges.is_empty()
                    || query
                        .ranges
                        .iter()
                        .any(|&(s, e)| s.max(1) < last && (e == 0 || e >= first)));
            if !overlaps {
                first = last;
                continue;
            }
            let map = self.linemap(dir, &seg.sha256).await?;
            if map.lines.len() as u64 != seg.lines {
                bail!(
                    "分段 {} 的行号表有 {} 行，manifest 记的是 {} 行",
                    seg.sha256,
                    map.lines.len(),
                    seg.lines
                );
            }
            let mut wanted = Vec::new();
            for (i, line) in map.lines.iter().enumerate() {
                let number = first + i as u64;
                let kind = Kind::parse(&line.2).unwrap_or(Kind::Meta);
                if !in_ranges(number) || !(query.kinds.is_empty() || query.kinds.contains(&kind)) {
                    continue;
                }
                if picked.len() == limit {
                    truncated = true;
                    break;
                }
                picked.push((
                    LineSpan {
                        line: number,
                        offset: 0,
                        length: line.1,
                        kind: kind.as_str().to_string(),
                        raw_type: map.raw_types.get(line.3 as usize).cloned().unwrap_or_default(),
                        timestamp_ms: line.4,
                    },
                    s,
                    i,
                ));
                wanted.push(i);
            }
            // The pruned copy of this segment comes along: what the person said and the replies.
            let ahead: Vec<usize> = map
                .lines
                .iter()
                .enumerate()
                .filter(|(_, l)| matches!(l.2.as_str(), "user" | "assistant"))
                .map(|(i, _)| i)
                .collect();
            let mut need: Vec<usize> = wanted.iter().copied().chain(ahead).collect();
            need.sort_unstable();
            need.dedup();
            let got = self.lines(dir, &entry.path, seg, &map, &need, full).await?;
            for i in wanted {
                let body = got
                    .get(&i)
                    .cloned()
                    .ok_or_else(|| anyhow!("缺第 {} 行", first + i as u64))?;
                bodies.insert((s, i), body);
            }
            first = last;
            if truncated {
                break;
            }
        }

        let views = self.paths.cache_dir().join("views");
        fs::create_dir_all(&views)?;
        sweep(&views, VIEW_TTL);
        let path = views.join(format!("{}.jsonl", uuid::Uuid::new_v4().simple()));
        let mut out = io::BufWriter::new(File::create(&path)?);
        let mut offset = 0;
        let mut spans = Vec::with_capacity(picked.len());
        for (mut span, s, i) in picked {
            let body = &bodies[&(s, i)];
            out.write_all(body)?;
            out.write_all(b"\n")?;
            span.offset = offset;
            offset += body.len() as u64 + 1;
            spans.push(span);
        }
        out.flush()?;
        Ok(Selected { path, spans, truncated })
    }

    /// The bytes of lines `need` (indexes into the segment's map), without line breaks: from the
    /// whole segment if it is cached, else from the line cache, fetching what is missing.
    async fn lines(
        &self,
        dir: &str,
        file: &str,
        seg: &SegmentInfo,
        map: &SegMap,
        need: &[usize],
        full: bool,
    ) -> anyhow::Result<HashMap<usize, Vec<u8>>> {
        check_sha(&seg.sha256)?;
        let cut = |whole: &[u8]| -> anyhow::Result<HashMap<usize, Vec<u8>>> {
            need.iter()
                .map(|&i| {
                    let (off, len) = (map.lines[i].0 as usize, map.lines[i].1 as usize);
                    let body = whole.get(off..off + len).ok_or_else(|| anyhow!("行号表超出了分段"))?;
                    Ok((i, body.to_vec()))
                })
                .collect()
        };
        if let Ok(whole) = fs::read(self.segment_cache(&seg.sha256))
            && whole.len() as u64 == seg.len
        {
            return cut(&whole);
        }
        let mut cache = LineCache::open(&self.paths.cache_dir().join("lines"), &seg.sha256);
        let missing: Vec<usize> = need
            .iter()
            .copied()
            .filter(|i| !cache.index.contains_key(&(*i as u32)))
            .collect();
        let missing_bytes: u64 = missing.iter().map(|&i| map.lines[i].1 + 2).sum();
        if !missing.is_empty() && (full || seg.len <= SMALL_SEGMENT || missing_bytes * 2 >= seg.len) {
            let whole = self.segment(dir, file, seg, true).await?;
            let _ = cache.forget();
            return cut(&whole);
        }

        // Fetch the missing lines in a few range requests, one past each line to see its break.
        let mut ranges: Vec<(u64, u64, Vec<usize>)> = Vec::new();
        for &i in &missing {
            let (off, len) = (map.lines[i].0, map.lines[i].1);
            let end = (off + len + 1).min(seg.len);
            match ranges.last_mut() {
                Some((_, last_end, members)) if off <= *last_end + GAP => {
                    *last_end = (*last_end).max(end);
                    members.push(i);
                }
                _ => ranges.push((off, end, vec![i])),
            }
        }
        let path = format!("{dir}/files/{file}/{}-{}.seg", seg.start, seg.sha256);
        let gate = Arc::new(tokio::sync::Semaphore::new(PARALLEL));
        let mut tasks = tokio::task::JoinSet::new();
        for (n, (start, end, _)) in ranges.iter().enumerate() {
            let (store, gate, path, range) = (self.store.clone(), gate.clone(), path.clone(), *start..*end);
            tasks.spawn(async move {
                let _permit = gate.acquire_owned().await;
                (n, store.get(&path, Some(range)).await)
            });
        }
        let mut fetched: Vec<Option<Vec<u8>>> = vec![None; ranges.len()];
        while let Some(joined) = tasks.join_next().await {
            let (n, bytes) = joined?;
            fetched[n] = Some(bytes?);
        }
        for ((start, end, members), bytes) in ranges.iter().zip(fetched) {
            let bytes = bytes.unwrap_or_default();
            if bytes.len() as u64 != end - start {
                bail!("存储对按字节范围读的回应长度不对（{file}@{}）", seg.start);
            }
            for &i in members {
                let (off, len) = ((map.lines[i].0 - start) as usize, map.lines[i].1 as usize);
                let ends_right = matches!(bytes.get(off + len), Some(b'\n' | b'\r'));
                if !ends_right {
                    bail!("存储里的 {file} 第 {} 段和它的行号表对不上", seg.start);
                }
                cache.add(i as u32, &bytes[off..off + len])?;
            }
        }
        cache.save()?;
        need.iter()
            .map(|&i| Ok((i, cache.read(i as u32, map.lines[i].1)?)))
            .collect()
    }
}

/// Lines of one segment fetched so far: their bytes one after another in `<sha>.bin`, and where
/// each starts in `<sha>.json`.
struct LineCache {
    bin: PathBuf,
    idx: PathBuf,
    index: BTreeMap<u32, u64>,
    appended: Vec<u8>,
    end: u64,
}

impl LineCache {
    fn open(dir: &Path, sha: &str) -> LineCache {
        let bin = dir.join(format!("{sha}.bin"));
        let idx = dir.join(format!("{sha}.json"));
        let end = fs::metadata(&bin).map(|m| m.len()).unwrap_or(0);
        let index: BTreeMap<u32, u64> = fs::read(&idx)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        // An index that points past the bytes (a write cut short) is not trusted.
        let index = if index.values().all(|&o| o <= end) {
            index
        } else {
            BTreeMap::new()
        };
        LineCache {
            bin,
            idx,
            index,
            appended: Vec::new(),
            end,
        }
    }

    fn add(&mut self, line: u32, body: &[u8]) -> io::Result<()> {
        if self.index.contains_key(&line) {
            return Ok(());
        }
        self.index.insert(line, self.end + self.appended.len() as u64);
        self.appended.extend_from_slice(body);
        Ok(())
    }

    fn save(&mut self) -> io::Result<()> {
        if self.appended.is_empty() {
            return Ok(());
        }
        if let Some(dir) = self.bin.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&self.bin)?;
        file.write_all(&self.appended)?;
        file.sync_all()?;
        self.end += self.appended.len() as u64;
        self.appended.clear();
        write_atomic(&self.idx, &serde_json::to_vec(&self.index)?)
    }

    fn read(&self, line: u32, len: u64) -> io::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let offset = *self
            .index
            .get(&line)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "line not cached"))?;
        let mut file = File::open(&self.bin)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0; len as usize];
        file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// The whole segment is cached now; single lines are not needed.
    fn forget(&self) -> io::Result<()> {
        let _ = fs::remove_file(&self.idx);
        let _ = fs::remove_file(&self.bin);
        Ok(())
    }
}

/// The newest versions: manifests no other manifest continues. Newest upload first.
pub fn heads(all: &[Manifest]) -> Vec<&Manifest> {
    let parents: HashSet<&str> = all.iter().filter_map(|m| m.parent.as_deref()).collect();
    let mut heads: Vec<&Manifest> = all.iter().filter(|m| !parents.contains(m.id.as_str())).collect();
    heads.sort_by(|a, b| (b.created_at_ms, &b.id).cmp(&(a.created_at_ms, &a.id)));
    heads
}

/// The version to read or restore. Several newest versions mean a fork: the caller is told
/// which there are and has to name one.
pub fn choose<'m>(
    all: &'m [Manifest],
    wanted: Wanted<'_>,
    devices: &HashMap<String, DeviceInfo>,
) -> Result<&'m Manifest, Choose> {
    match wanted {
        Wanted::Id(id) => all.iter().find(|m| m.id == id).ok_or_else(|| {
            Choose(format!(
                "存储里没有版本 {id}；有：{}",
                all.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join("、")
            ))
        }),
        Wanted::Latest => {
            let heads = heads(all);
            match heads.as_slice() {
                [] => Err(Choose("存储里的版本互相指向，找不到最新的一个".to_string())),
                [one] => Ok(one),
                many => {
                    let lines: Vec<String> = many.iter().map(|m| format!("  {}", describe(m, devices))).collect();
                    Err(Choose(format!(
                        "这个会话在存储里有 {} 个最新版本（不同设备各自接着做过），请选一个，用 version 指定：\n{}",
                        many.len(),
                        lines.join("\n")
                    )))
                }
            }
        }
    }
}

/// "dev-3：设备 mac，2 小时前上传，最后一轮 3 小时前，主记录 1.2 MB、340 行"
pub fn describe(m: &Manifest, devices: &HashMap<String, DeviceInfo>) -> String {
    let now = now_ms();
    let device = devices
        .get(&m.device)
        .map(|d| d.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| m.device.clone());
    let main = main_entry(m);
    let size = main.map_or(String::new(), |e| {
        format!("，主记录 {}、{} 行", human_bytes(e.size), e.lines)
    });
    format!(
        "{}：设备 {device}，{}上传，最后一轮 {}{size}",
        m.id,
        ago(m.created_at_ms, now),
        ago(m.session.last_ts_ms, now)
    )
}

/// The main transcript: Claude Code's `<id>.jsonl`; for Codex the newest rollout of the thread.
pub fn main_entry(m: &Manifest) -> Option<&FileEntry> {
    let transcripts = m.files.iter().filter(|f| f.role == "transcript");
    let own = format!("/{}.jsonl", m.session.id);
    let by_name = m
        .files
        .iter()
        .find(|f| f.role == "transcript" && f.path.ends_with(&own));
    by_name.or_else(|| transcripts.max_by_key(|f| (f.mtime_ms, f.path.clone())))
}

/// A file of the manifest by the same rules as a local one: empty = the main transcript, else
/// the whole path or its end after a `/`.
pub fn pick_entry<'m>(m: &'m Manifest, file: &str) -> Result<&'m FileEntry, String> {
    if file.is_empty() {
        return main_entry(m).ok_or_else(|| format!("版本 {} 里没有主记录", m.id));
    }
    let file = file.replace('\\', "/");
    let suffix = format!("/{file}");
    let found: Vec<&FileEntry> = m
        .files
        .iter()
        .filter(|f| f.path == file || f.path.ends_with(&suffix))
        .collect();
    match found.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("版本 {} 里没有 {file}", m.id)),
        _ => Err(format!("{file} 对应不止一个文件，请写完整的相对路径")),
    }
}

/// Writes a file whole or not at all.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// Deletes files in `dir` older than `ttl`.
fn sweep(dir: &Path, ttl: Duration) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > ttl);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::claude_code;
    use crate::daemon::device::Device;
    use crate::testdav::Server;

    const ID: &str = "11111111-2222-4333-8444-555555555555";

    /// 48 lines: per four, a user line, a reply, and two long tool lines, so that what the person
    /// said and the replies are a small part of each segment.
    fn transcript() -> String {
        (0..48u32)
            .map(|n| {
                let t = format!("2026-11-01T10:{:02}:{:02}.000Z", n / 60, n % 60);
                let line = match n % 4 {
                    0 => format!(r#"{{"type":"user","timestamp":"{t}","cwd":"/s","message":{{"content":"q{n}"}}}}"#),
                    1 => format!(
                        r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"text","text":"a{n}"}}]}}}}"#
                    ),
                    _ => format!(
                        r#"{{"type":"assistant","timestamp":"{t}","message":{{"content":[{{"type":"tool_use","name":"{}"}}]}}}}"#,
                        "x".repeat(1500)
                    ),
                };
                line + "\n"
            })
            .collect()
    }

    struct Setup {
        server: Server,
        remote: Remote,
        manifest: Manifest,
        text: String,
        _claude: tempfile::TempDir,
        _nk: tempfile::TempDir,
    }

    async fn setup() -> Setup {
        let server = Server::start(None).await;
        let claude = tempfile::tempdir().unwrap();
        let dir = claude.path().join("projects/-s");
        fs::create_dir_all(&dir).unwrap();
        let text = transcript();
        fs::write(dir.join(format!("{ID}.jsonl")), &text).unwrap();
        let store = Webdav::new(server.spec()).unwrap();
        let device = Device {
            id: "dev-r".into(),
            name: "r".into(),
            created_at_ms: 1,
        };
        let found = vec![claude_code::discover_in(claude.path().to_path_buf())];
        let jobs = archive::jobs_for(&found, &[(AgentKind::ClaudeCode, ID.to_string())]);
        archive::run(&store, &device, None, jobs, &|_| true).await;
        let nk = tempfile::tempdir().unwrap();
        let remote = Remote::new(
            Arc::new(store),
            Paths {
                home: nk.path().to_path_buf(),
            },
        );
        let (agent, ids) = remote.find(ID, None).await.unwrap();
        assert_eq!(
            (agent, ids.as_slice()),
            (AgentKind::ClaudeCode, &["dev-r-1".to_string()][..])
        );
        let all = remote.manifests(agent, ID, &ids).await.unwrap();
        let manifest = all[0].clone();
        assert!(main_entry(&manifest).unwrap().segments.len() >= 4);
        Setup {
            server,
            remote,
            manifest,
            text,
            _claude: claude,
            _nk: nk,
        }
    }

    fn dir() -> String {
        archive::session_dir("claude-code", ID)
    }

    /// The text of the selected lines, read back the way clients do.
    fn read(selected: &Selected) -> Vec<String> {
        crate::render::read_spans(&selected.path.to_string_lossy(), &selected.spans)
            .unwrap()
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect()
    }

    fn segment_gets(server: &Server) -> usize {
        server
            .requests()
            .iter()
            .filter(|(method, path)| method == "GET" && path.ends_with(".seg"))
            .count()
    }

    #[tokio::test]
    async fn lines_by_kind_come_from_range_reads_and_the_pruned_copy_stays() {
        let s = setup().await;
        let entry = main_entry(&s.manifest).unwrap();
        let originals: Vec<&str> = s.text.lines().collect();
        let users = [Kind::User];
        let query = LineQuery {
            ranges: Vec::new(),
            kinds: &users,
            limit: 0,
        };
        let selected = s.remote.select(&dir(), entry, &query, false).await.unwrap();
        assert_eq!(selected.spans.len(), 12);
        assert!(!selected.truncated);
        let numbers: Vec<u64> = selected.spans.iter().map(|sp| sp.line).collect();
        assert_eq!(numbers, (0..12).map(|k| 4 * k + 1).collect::<Vec<_>>());
        let texts = read(&selected);
        for (span, text) in selected.spans.iter().zip(&texts) {
            assert_eq!(text, originals[span.line as usize - 1]);
            assert_eq!((span.kind.as_str(), span.raw_type.as_str()), ("user", "user"));
        }
        // Lines, not segments: nothing was downloaded whole.
        assert!(!s.remote.paths.cache_dir().join("segments").exists());

        // The replies came along: reading them asks the storage for nothing.
        s.server.forget_requests();
        let replies = [Kind::Assistant];
        let query = LineQuery {
            ranges: Vec::new(),
            kinds: &replies,
            limit: 0,
        };
        let selected = s.remote.select(&dir(), entry, &query, false).await.unwrap();
        assert_eq!(selected.spans.len(), 12);
        assert!(read(&selected)[0].contains("\"a1\""));
        assert!(s.server.requests().is_empty(), "{:?}", s.server.requests());

        // A tool line is fetched when asked for.
        let query = LineQuery {
            ranges: vec![(3, 3)],
            kinds: &[],
            limit: 0,
        };
        let selected = s.remote.select(&dir(), entry, &query, false).await.unwrap();
        assert_eq!(read(&selected), vec![originals[2].to_string()]);
        assert_eq!(segment_gets(&s.server), 1);
    }

    #[tokio::test]
    async fn a_limit_stops_early_and_full_copies_take_whole_segments() {
        let s = setup().await;
        let entry = main_entry(&s.manifest).unwrap();
        let query = LineQuery {
            ranges: Vec::new(),
            kinds: &[],
            limit: 3,
        };
        let selected = s.remote.select(&dir(), entry, &query, true).await.unwrap();
        assert_eq!(selected.spans.iter().map(|sp| sp.line).collect::<Vec<_>>(), [1, 2, 3]);
        assert!(selected.truncated);
        // Only the first segment was looked at, and it was taken whole.
        let maps = s
            .server
            .requests()
            .iter()
            .filter(|(m, p)| m == "GET" && p.contains("/linemaps/"))
            .count();
        assert_eq!(maps, 1);
        assert!(s.remote.segment_cache(&entry.segments[0].sha256).is_file());
        assert!(!s.remote.segment_cache(&entry.segments[1].sha256).exists());
        let originals: Vec<&str> = s.text.lines().collect();
        assert_eq!(
            read(&selected),
            originals[..3].iter().map(|l| l.to_string()).collect::<Vec<_>>()
        );

        // A range that starts in a later segment skips the earlier ones.
        let query = LineQuery {
            ranges: vec![(40, 0)],
            kinds: &[],
            limit: 0,
        };
        let selected = s.remote.select(&dir(), entry, &query, false).await.unwrap();
        assert_eq!(selected.spans.len(), 9);
        assert_eq!(read(&selected)[8], originals[47]);
    }

    #[tokio::test]
    async fn whole_files_are_checked() {
        let s = setup().await;
        let entry = main_entry(&s.manifest).unwrap();
        let path = s.remote.whole_file(&dir(), entry).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), s.text);
        // A damaged segment is noticed.
        fs::remove_file(&path).unwrap();
        let seg = &entry.segments[1];
        let stored = s.server.root().join(format!(
            "{}/files/{}/{}-{}.seg",
            dir(),
            entry.path,
            seg.start,
            seg.sha256
        ));
        let mut bytes = fs::read(&stored).unwrap();
        bytes[0] ^= 1;
        fs::write(&stored, bytes).unwrap();
        let err = s.remote.whole_file(&dir(), entry).await.unwrap_err();
        assert!(err.to_string().contains("SHA-256"), "{err}");
    }

    fn manifest(id: &str, parent: Option<&str>, at: i64) -> Manifest {
        let device = id.rsplit_once('-').unwrap().0.to_string();
        Manifest {
            format: "nekorisu.manifest/1".into(),
            id: id.into(),
            parent: parent.map(str::to_owned),
            session: Default::default(),
            device,
            created_at_ms: at,
            files: Vec::new(),
            links: Default::default(),
            rewrite: false,
            encryption: "none".into(),
        }
    }

    #[test]
    fn the_newest_versions_are_those_nobody_continues() {
        let line = vec![
            manifest("a-1", None, 1),
            manifest("b-1", Some("a-1"), 2),
            manifest("a-2", Some("b-1"), 3),
        ];
        let none = HashMap::new();
        assert_eq!(heads(&line).iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a-2"]);
        assert_eq!(choose(&line, Wanted::Latest, &none).unwrap().id, "a-2");
        assert_eq!(choose(&line, Wanted::Id("b-1"), &none).unwrap().id, "b-1");
        assert!(
            choose(&line, Wanted::Id("c-1"), &none)
                .unwrap_err()
                .0
                .contains("a-1、b-1、a-2")
        );

        let fork = vec![
            manifest("a-1", None, 1),
            manifest("b-1", Some("a-1"), 2),
            manifest("a-2", Some("a-1"), 3),
        ];
        let devices = HashMap::from([(
            "b".to_string(),
            DeviceInfo {
                name: "laptop".into(),
                home: None,
            },
        )]);
        assert_eq!(
            heads(&fork).iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a-2", "b-1"]
        );
        let err = choose(&fork, Wanted::Latest, &devices).unwrap_err().0;
        assert!(
            err.contains("2 个最新版本") && err.contains("设备 laptop") && err.contains("a-2：设备 a"),
            "{err}"
        );
    }
}
