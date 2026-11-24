//! WebDAV storage: the few operations the archive needs and nothing else
//!: list a directory, read a whole
//! file or a byte range, write a new file. Paths are relative to the storage
//! root URL. Nothing here overwrites or deletes.
//!
//! Credentials go only into the Authorization header; error messages carry
//! neither them nor the URL.

use std::collections::HashSet;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::ops::Range;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use reqwest::{Method, StatusCode, Url};

use crate::config::{Config, StorageConfig};
use crate::paths::Paths;
use crate::secrets;

const ATTEMPTS: u32 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// A PUT gets `REQUEST_TIMEOUT` plus one second per 32 KiB of body.
const UPLOAD_BYTES_PER_SECOND: u64 = 32 * 1024;
/// Wait before the n-th retry: this many ms times n.
const BACKOFF_MS: u64 = if cfg!(test) { 10 } else { 500 };

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><propfind xmlns="DAV:"><prop><resourcetype/><getcontentlength/></prop></propfind>"#;

/// What went wrong talking to the storage, in words for the person running the command.
#[derive(Debug)]
pub enum StorageError {
    /// 401 / 403.
    Denied { status: u16 },
    /// The path is not there (GET).
    NotFound(String),
    /// Any other HTTP status we did not expect.
    Status { op: &'static str, status: u16 },
    /// Could not reach the server, or it timed out, even after retrying.
    Network(String),
    /// A path or an answer that does not make sense.
    Invalid(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Denied { status: 401 } => write!(f, "存储拒绝了用户名或密码（HTTP 401）"),
            StorageError::Denied { status } => write!(f, "存储不允许这个操作（HTTP {status}）"),
            StorageError::NotFound(path) => write!(f, "存储里没有 {path}（HTTP 404）"),
            StorageError::Status { op, status: 507 } => write!(f, "{op}失败：存储空间不足（HTTP 507）"),
            StorageError::Status { op, status } => write!(f, "{op}失败（HTTP {status}）"),
            StorageError::Network(why) => write!(f, "连不上存储：{why}"),
            StorageError::Invalid(why) => write!(f, "存储的回应或路径有问题：{why}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// What `put_new` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    Created,
    /// The name was taken (HTTP 412 on `If-None-Match: *`). For content-addressed names that is fine.
    Exists,
}

/// One child of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Everything needed to connect; the password is read from the keychain by the caller.
pub struct Spec {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub resolve: Option<IpAddr>,
}

impl fmt::Debug for Spec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Spec")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("resolve", &self.resolve)
            .finish()
    }
}

/// Checks a storage URL: http(s), a host, no user name or password inside it.
pub fn parse_url(url: &str) -> anyhow::Result<Url> {
    let parsed = Url::parse(url).map_err(|_| anyhow!("地址不对：{url}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("WebDAV 地址要以 http:// 或 https:// 开头");
    }
    if parsed.host_str().is_none() {
        bail!("地址里没有主机名");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("不要把用户名和密码写进地址：用 --username，密码用 nekorisu storage login");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("地址里不要带 ? 或 # 后面的部分");
    }
    Ok(parsed)
}

impl Spec {
    pub fn from_config(storage: &StorageConfig, password: Option<String>) -> anyhow::Result<Spec> {
        if storage.kind != "webdav" {
            bail!("存储类型 {} 还不支持（目前只有 webdav）", storage.kind);
        }
        parse_url(&storage.url)?;
        let resolve = match storage.resolve.as_deref() {
            Some(text) => Some(
                text.trim()
                    .parse::<IpAddr>()
                    .map_err(|_| anyhow!("resolve 要写成 IP 地址：{text}"))?,
            ),
            None => None,
        };
        Ok(Spec {
            url: storage.url.trim_end_matches('/').to_string(),
            username: storage.username.clone(),
            password,
            resolve,
        })
    }
}

/// Picks the storage a command should use: the named one, or the only one there is.
pub fn pick<'a>(config: &'a Config, wanted: &str) -> anyhow::Result<(&'a str, &'a StorageConfig)> {
    let names = || config.storage.keys().cloned().collect::<Vec<_>>().join("、");
    if !wanted.is_empty() {
        return config
            .storage
            .get_key_value(wanted)
            .map(|(name, storage)| (name.as_str(), storage))
            .ok_or_else(|| {
                if config.storage.is_empty() {
                    anyhow!("config.toml 里没有叫 {wanted} 的存储；先运行 nekorisu storage add {wanted} --url <地址>")
                } else {
                    anyhow!("config.toml 里没有叫 {wanted} 的存储（有：{}）", names())
                }
            });
    }
    let mut all = config.storage.iter();
    match (all.next(), all.next()) {
        (Some((name, storage)), None) => Ok((name.as_str(), storage)),
        (None, _) => bail!(
            "还没有配置存储。先运行 nekorisu storage add <名字> --url <地址> --username <用户名>，再运行 nekorisu storage login <名字>"
        ),
        _ => bail!("配置了不止一个存储（{}），请用 --storage 指定一个", names()),
    }
}

/// Builds a client for a configured storage, reading its password from the keychain
/// when it has a user name.
pub async fn connect(paths: &Paths, name: &str, storage: &StorageConfig) -> anyhow::Result<Webdav> {
    let password = if storage.username.is_some() {
        let (paths, owned) = (paths.clone(), name.to_string());
        let found = tokio::task::spawn_blocking(move || secrets::load(&paths, &owned)).await??;
        Some(found.ok_or_else(|| anyhow!("存储 {name} 的密码还没设置：先运行 nekorisu storage login {name}"))?)
    } else {
        None
    };
    Webdav::new(Spec::from_config(storage, password)?)
}

pub struct Webdav {
    client: reqwest::Client,
    /// The storage root, no trailing slash.
    base: String,
    username: Option<String>,
    password: Option<String>,
    /// Collections known to exist (made by us, or seen in a listing), root-relative.
    known_dirs: Mutex<HashSet<String>>,
}

impl fmt::Debug for Webdav {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Webdav")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Webdav {
    pub fn new(spec: Spec) -> anyhow::Result<Self> {
        let url = parse_url(&spec.url)?;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut builder = reqwest::Client::builder()
            .user_agent(format!("nekorisu/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT);
        if let Some(ip) = spec.resolve {
            let host = url.host_str().unwrap_or_default().to_string();
            let port = url
                .port_or_known_default()
                .ok_or_else(|| anyhow!("地址里没有端口，也认不出默认端口"))?;
            // A pinned address means a direct connection: a proxy would resolve the name itself.
            builder = builder.resolve(&host, SocketAddr::new(ip, port)).no_proxy();
        }
        Ok(Self {
            client: builder.build().context("建不了 HTTP 客户端")?,
            base: spec.url.trim_end_matches('/').to_string(),
            username: spec.username,
            password: spec.password,
            known_dirs: Mutex::new(HashSet::new()),
        })
    }

    /// The raw answer of a depth-0 PROPFIND on the root: what `storage test` shows. Reads only.
    pub async fn probe(&self) -> Result<u16, StorageError> {
        let headers = [
            ("Depth", "0".to_string()),
            ("Content-Type", "application/xml".to_string()),
        ];
        let response = self
            .send(propfind(), self.url_of("", true)?, &headers, Some(PROPFIND_BODY.into()))
            .await?;
        Ok(response.status().as_u16())
    }

    /// The children of a directory. A directory that does not exist is empty.
    pub async fn list(&self, dir: &str) -> Result<Vec<Entry>, StorageError> {
        let url = self.url_of(dir, true)?;
        let headers = [
            ("Depth", "1".to_string()),
            ("Content-Type", "application/xml".to_string()),
        ];
        let response = self
            .send(propfind(), url.clone(), &headers, Some(PROPFIND_BODY.into()))
            .await?;
        match response.status().as_u16() {
            207 | 200 => {}
            404 => return Ok(Vec::new()),
            status => return Err(unexpected("列目录", status)),
        }
        let text = response.text().await.map_err(network)?;
        self.remember_dir(dir);
        let own = decode(&Url::parse(&url).map(|u| u.path().to_string()).unwrap_or_default());
        let own = own.trim_end_matches('/');
        let mut entries = Vec::new();
        for raw in parse_multistatus(&text) {
            let decoded = decode(href_path(&raw.href));
            let path = decoded.trim_end_matches('/');
            if path == own {
                continue; // a depth-1 listing starts with the directory itself
            }
            let name = path.rsplit('/').next().unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            entries.push(Entry {
                name: name.to_string(),
                is_dir: raw.is_dir,
                size: raw.size,
            });
        }
        Ok(entries)
    }

    /// Size of one file, `None` if it is not there. A server that reports no size looks like
    /// `Some(0)`, so callers use this only as a sanity check.
    pub async fn stat(&self, path: &str) -> Result<Option<u64>, StorageError> {
        let headers = [
            ("Depth", "0".to_string()),
            ("Content-Type", "application/xml".to_string()),
        ];
        let response = self
            .send(
                propfind(),
                self.url_of(path, false)?,
                &headers,
                Some(PROPFIND_BODY.into()),
            )
            .await?;
        match response.status().as_u16() {
            207 | 200 => {}
            404 => return Ok(None),
            status => return Err(unexpected("查文件", status)),
        }
        let text = response.text().await.map_err(network)?;
        Ok(parse_multistatus(&text).first().map(|e| e.size))
    }

    /// The bytes of a file, or of `range` (end exclusive) when given.
    pub async fn get(&self, path: &str, range: Option<Range<u64>>) -> Result<Vec<u8>, StorageError> {
        let mut headers = Vec::new();
        if let Some(r) = &range {
            if r.end <= r.start {
                return Ok(Vec::new());
            }
            headers.push(("Range", format!("bytes={}-{}", r.start, r.end - 1)));
        }
        let response = self
            .send(Method::GET, self.url_of(path, false)?, &headers, None)
            .await?;
        let status = response.status();
        match status.as_u16() {
            200 | 206 => {}
            404 => return Err(StorageError::NotFound(path.to_string())),
            other => return Err(unexpected("读文件", other)),
        }
        let body = response.bytes().await.map_err(network)?;
        match range {
            // A server that ignores Range answers 200 with the whole file.
            Some(r) if status == StatusCode::OK => {
                let (start, end) = (r.start as usize, (r.end as usize).min(body.len()));
                Ok(body.get(start..end).unwrap_or_default().to_vec())
            }
            _ => Ok(body.to_vec()),
        }
    }

    /// Writes a file that must not exist yet: makes the parent collections, then PUTs with
    /// `If-None-Match: *`.
    pub async fn put_new(&self, path: &str, body: Vec<u8>) -> Result<Put, StorageError> {
        if let Some((dir, _)) = path.rsplit_once('/') {
            self.ensure_dir(dir).await?;
        }
        let headers = [
            ("If-None-Match", "*".to_string()),
            ("Content-Type", "application/octet-stream".to_string()),
        ];
        let response = self
            .send(Method::PUT, self.url_of(path, false)?, &headers, Some(body))
            .await?;
        match response.status().as_u16() {
            200 | 201 | 204 => Ok(Put::Created),
            412 => Ok(Put::Exists),
            status => Err(unexpected("写文件", status)),
        }
    }

    /// Makes `dir` and the collections above it, each at most once per run.
    async fn ensure_dir(&self, dir: &str) -> Result<(), StorageError> {
        if self.is_known(dir) {
            return Ok(());
        }
        let mut prefix = String::new();
        for part in dir.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if self.is_known(&prefix) {
                continue;
            }
            let mkcol = Method::from_bytes(b"MKCOL").expect("valid method");
            let response = self.send(mkcol, self.url_of(&prefix, true)?, &[], None).await?;
            match response.status().as_u16() {
                // Created; or it exists already: 405 by the RFC, 409 and redirects from other servers.
                200 | 201 | 301 | 302 | 307 | 308 | 405 | 409 => {}
                status => return Err(unexpected("建目录", status)),
            }
            self.remember_dir(&prefix);
        }
        Ok(())
    }

    fn is_known(&self, dir: &str) -> bool {
        self.known_dirs.lock().unwrap_or_else(|e| e.into_inner()).contains(dir)
    }

    /// A directory that exists implies all its ancestors do.
    fn remember_dir(&self, dir: &str) {
        let mut known = self.known_dirs.lock().unwrap_or_else(|e| e.into_inner());
        let mut prefix = String::new();
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            known.insert(prefix.clone());
        }
    }

    fn url_of(&self, path: &str, collection: bool) -> Result<String, StorageError> {
        let mut url = self.base.clone();
        if !path.is_empty() {
            check_path(path)?;
            for part in path.split('/') {
                url.push('/');
                url.push_str(&encode(part));
            }
        }
        if collection {
            url.push('/');
        }
        Ok(url)
    }

    /// One request, retried on network errors and 5xx / 429. The body is cloned per attempt.
    async fn send(
        &self,
        method: Method,
        url: String,
        headers: &[(&str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response, StorageError> {
        let timeout = match &body {
            Some(b) if method == Method::PUT => {
                REQUEST_TIMEOUT + Duration::from_secs(b.len() as u64 / UPLOAD_BYTES_PER_SECOND)
            }
            _ => REQUEST_TIMEOUT,
        };
        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(BACKOFF_MS * attempt as u64)).await;
            }
            let mut request = self.client.request(method.clone(), &url).timeout(timeout);
            if let Some(user) = &self.username {
                request = request.basic_auth(user, self.password.as_deref());
            }
            for (name, value) in headers {
                request = request.header(*name, value);
            }
            if let Some(body) = &body {
                request = request.body(body.clone());
            }
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    let transient = (status.is_server_error() && status != StatusCode::NOT_IMPLEMENTED)
                        || status == StatusCode::TOO_MANY_REQUESTS;
                    if transient && attempt + 1 < ATTEMPTS {
                        last = format!("HTTP {}", status.as_u16());
                        continue;
                    }
                    return Ok(response);
                }
                Err(err) if err.is_builder() => return Err(StorageError::Invalid(describe(err))),
                Err(err) => last = describe(err),
            }
        }
        Err(StorageError::Network(last))
    }
}

fn propfind() -> Method {
    Method::from_bytes(b"PROPFIND").expect("valid method")
}

fn unexpected(op: &'static str, status: u16) -> StorageError {
    match status {
        401 | 403 => StorageError::Denied { status },
        _ => StorageError::Status { op, status },
    }
}

fn network(err: reqwest::Error) -> StorageError {
    StorageError::Network(describe(err))
}

/// The error chain without the URL (it could carry a host the user would rather not paste around).
fn describe(err: reqwest::Error) -> String {
    let err = err.without_url();
    let mut text = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

/// Relative, no `..`, no empty parts: nothing that could point outside the storage root.
fn check_path(path: &str) -> Result<(), StorageError> {
    let bad = path.starts_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if bad {
        return Err(StorageError::Invalid(format!("不能用这个路径：{path}")));
    }
    Ok(())
}

/// Percent-encodes one path segment.
fn encode(part: &str) -> String {
    let mut out = String::with_capacity(part.len());
    for byte in part.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub(crate) fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(value) = hex_pair(bytes[i + 1], bytes[i + 2])
        {
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let digit = |c: u8| (c as char).to_digit(16);
    Some((digit(hi)? * 16 + digit(lo)?) as u8)
}

/// An href may be a path or an absolute URL; only the path matters.
fn href_path(href: &str) -> &str {
    let href = href.trim();
    let rest = match href.split_once("://") {
        Some((_, after)) => after.find('/').map_or("/", |i| &after[i..]),
        None => href,
    };
    rest.split(['?', '#']).next().unwrap_or(rest)
}

#[derive(Debug, Default, PartialEq)]
struct RawEntry {
    href: String,
    is_dir: bool,
    size: u64,
}

/// A small tolerant reader for a 207 Multi-Status body: matches element names without their
/// namespace prefix, and takes `href`, a `collection` marker and `getcontentlength` from
/// each `response`.
fn parse_multistatus(xml: &str) -> Vec<RawEntry> {
    let mut entries = Vec::new();
    let mut current: Option<RawEntry> = None;
    let mut open: Vec<String> = Vec::new();
    let mut rest = xml;
    // Text (or CDATA) belongs to the innermost open element.
    fn add_text(current: &mut Option<RawEntry>, open: &[String], text: &str) {
        if let (Some(entry), Some(inside)) = (current.as_mut(), open.last()) {
            match inside.as_str() {
                "href" => entry.href.push_str(text),
                "getcontentlength" => {
                    if let Ok(size) = text.trim().parse() {
                        entry.size = size;
                    }
                }
                _ => {}
            }
        }
    }
    while let Some(lt) = rest.find('<') {
        add_text(&mut current, &open, &unescape(&rest[..lt]));
        rest = &rest[lt..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.split_once("-->").map_or("", |(_, r)| r);
        } else if let Some(after) = rest.strip_prefix("<![CDATA[") {
            let (cdata, r) = after.split_once("]]>").unwrap_or((after, ""));
            add_text(&mut current, &open, cdata);
            rest = r;
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            rest = rest.split_once('>').map_or("", |(_, r)| r);
        } else {
            let Some(end) = rest.find('>') else { break };
            let tag = &rest[1..end];
            rest = &rest[end + 1..];
            if let Some(name) = tag.strip_prefix('/') {
                let name = local_name(name);
                if let Some(pos) = open.iter().rposition(|n| *n == name) {
                    open.truncate(pos);
                }
                if name == "response"
                    && let Some(entry) = current.take()
                {
                    entries.push(entry);
                }
            } else {
                let self_closing = tag.ends_with('/');
                let name = local_name(tag.trim_end_matches('/'));
                match name.as_str() {
                    "response" => current = Some(RawEntry::default()),
                    "collection" => {
                        if let Some(entry) = current.as_mut() {
                            entry.is_dir = true;
                        }
                    }
                    _ => {}
                }
                if !self_closing {
                    open.push(name);
                }
            }
        }
    }
    entries
}

/// `D:href foo="bar"` -> `href`.
fn local_name(tag: &str) -> String {
    let name = tag.split(char::is_whitespace).next().unwrap_or_default();
    name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase()
}

fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let replaced = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match replaced {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdav::Server;

    #[test]
    fn parses_multistatus_in_the_dialects_servers_use() {
        // dav-server / Apache style: prefix D:, absolute-path hrefs, collection marker.
        let apache = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
 <D:response><D:href>/dav/a/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
 <D:response><D:href>/dav/a/f%20x.json</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>42</D:getcontentlength></D:prop></D:propstat></D:response>
 <D:response><D:href>/dav/a/sub/</D:href><D:propstat><D:prop><D:resourcetype><D:collection></D:collection></D:resourcetype><D:getcontentlength/></D:prop></D:propstat></D:response>
</D:multistatus>"#;
        let entries = parse_multistatus(apache);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_dir && entries[2].is_dir && !entries[1].is_dir);
        assert_eq!((entries[1].href.as_str(), entries[1].size), ("/dav/a/f%20x.json", 42));

        // No prefix at all, default namespace, absolute URLs, entities, upper-case tags, CDATA.
        let plain = r#"<multistatus xmlns="DAV:">
<response><href>http://h.example:8080/dav/a/x&amp;y.seg</href><propstat><prop><getcontentlength> 7 </getcontentlength></prop></propstat></response>
<RESPONSE><HREF><![CDATA[/dav/a/c d.json]]></HREF><propstat><prop><resourcetype/></prop></propstat></RESPONSE>
</multistatus>"#;
        let entries = parse_multistatus(plain);
        assert_eq!(entries.len(), 2);
        assert_eq!(href_path(&entries[0].href), "/dav/a/x&y.seg");
        assert_eq!(entries[0].size, 7);
        assert_eq!(entries[1].href, "/dav/a/c d.json");
        assert!(parse_multistatus("not xml at all").is_empty());
        assert!(parse_multistatus("<a><b").is_empty());
    }

    #[test]
    fn encodes_and_decodes_path_parts() {
        assert_eq!(encode("a b/é.json"), "a%20b%2F%C3%A9.json");
        assert_eq!(decode("a%20b%2F%C3%A9.json%zz%4"), "a b/é.json%zz%4");
        assert_eq!(decode("%E5%90%8D"), "名");
        assert_eq!(href_path("https://h.example/dav/x?y#z"), "/dav/x");
        assert_eq!(href_path("https://h.example"), "/");
        assert_eq!(unescape("a&amp;b&#65;&#x42;&bogus;&"), "a&bAB&bogus;&");
    }

    #[test]
    fn refuses_paths_that_could_leave_the_root() {
        let client = Webdav::new(Spec {
            url: "http://127.0.0.1:9/dav".into(),
            username: None,
            password: None,
            resolve: None,
        })
        .unwrap();
        assert!(client.url_of("", false).is_ok(), "the root itself is fine");
        for bad in ["/abs", "a//b", "a/../b", "..", "a/./b", "a\\b", "a/\n"] {
            assert!(client.url_of(bad, false).is_err(), "{bad:?}");
        }
        assert_eq!(
            client.url_of("nekorisu/v1/a b.json", false).unwrap(),
            "http://127.0.0.1:9/dav/nekorisu/v1/a%20b.json"
        );
    }

    #[test]
    fn rejects_urls_with_credentials_and_bad_resolve() {
        assert!(parse_url("https://me:secret@dav.example/").is_err());
        assert!(parse_url("ftp://dav.example/").is_err());
        assert!(parse_url("https://dav.example/dav?x=1").is_err());
        let storage = |resolve: &str| StorageConfig {
            kind: "webdav".into(),
            url: "https://dav.example/dav/".into(),
            username: None,
            resolve: Some(resolve.into()),
        };
        assert!(Spec::from_config(&storage("not-an-ip"), None).is_err());
        let spec = Spec::from_config(&storage("203.0.113.9"), None).unwrap();
        assert_eq!(spec.url, "https://dav.example/dav");
        assert_eq!(spec.resolve, Some("203.0.113.9".parse().unwrap()));
        let debug = format!(
            "{:?}",
            Spec {
                password: Some("hunter2".into()),
                ..spec
            }
        );
        assert!(!debug.contains("hunter2"));
    }

    #[test]
    fn picks_the_only_storage_or_asks() {
        let parse = |text: &str| -> Config { toml::from_str(text).unwrap() };
        let none = parse("");
        assert!(pick(&none, "").unwrap_err().to_string().contains("storage add"));
        let one = parse("[storage.a]\nkind='webdav'\nurl='http://x'\n");
        assert_eq!(pick(&one, "").unwrap().0, "a");
        assert_eq!(pick(&one, "a").unwrap().0, "a");
        assert!(pick(&one, "b").unwrap_err().to_string().contains("有：a"));
        let two = parse("[storage.a]\nkind='webdav'\nurl='http://x'\n[storage.b]\nkind='webdav'\nurl='http://y'\n");
        assert!(pick(&two, "").unwrap_err().to_string().contains("a、b"));
        assert_eq!(pick(&two, "b").unwrap().1.url, "http://y");
    }

    #[tokio::test]
    async fn put_list_get_range_and_nested_collections() {
        let server = Server::start(None).await;
        let dav = Webdav::new(server.spec()).unwrap();

        let path = "nekorisu/v1/sessions/codex/abc/files/sessions/2026/11/01/a b.jsonl/0-deadbeef.seg";
        assert_eq!(dav.put_new(path, b"0123456789".to_vec()).await.unwrap(), Put::Created);
        assert_eq!(
            std::fs::read(server.root().join(path)).unwrap(),
            b"0123456789",
            "nested collections were made and the file landed byte for byte"
        );
        assert_eq!(dav.get(path, None).await.unwrap(), b"0123456789");
        assert_eq!(dav.get(path, Some(3..7)).await.unwrap(), b"3456");
        assert_eq!(dav.get(path, Some(8..100)).await.unwrap(), b"89");
        assert!(dav.get(path, Some(4..4)).await.unwrap().is_empty());
        assert_eq!(dav.stat(path).await.unwrap(), Some(10));
        assert_eq!(dav.stat("nekorisu/v1/nothing").await.unwrap(), None);
        assert!(matches!(
            dav.get("nekorisu/nope", None).await,
            Err(StorageError::NotFound(_))
        ));

        let dir = "nekorisu/v1/sessions/codex/abc/files/sessions/2026/11/01";
        let listing = dav.list(dir).await.unwrap();
        assert_eq!(
            listing,
            [Entry {
                name: "a b.jsonl".into(),
                is_dir: true,
                size: 0
            }]
        );
        let files = dav.list(&format!("{dir}/a b.jsonl")).await.unwrap();
        assert_eq!(
            files,
            [Entry {
                name: "0-deadbeef.seg".into(),
                is_dir: false,
                size: 10
            }]
        );
        assert!(dav.list("nekorisu/v1/not/there").await.unwrap().is_empty());
        // A directory is listed without itself.
        let top = dav.list("nekorisu/v1").await.unwrap();
        assert_eq!(top.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["sessions"]);
    }

    #[tokio::test]
    async fn put_new_does_not_overwrite() {
        let server = Server::start(None).await;
        let dav = Webdav::new(server.spec()).unwrap();
        assert_eq!(
            dav.put_new("nekorisu/v1/x.json", b"first".to_vec()).await.unwrap(),
            Put::Created
        );
        assert_eq!(
            dav.put_new("nekorisu/v1/x.json", b"second".to_vec()).await.unwrap(),
            Put::Exists
        );
        assert_eq!(dav.get("nekorisu/v1/x.json", None).await.unwrap(), b"first");
        // A second client starts with an empty directory cache: MKCOL on existing collections is fine.
        let other = Webdav::new(server.spec()).unwrap();
        assert_eq!(
            other.put_new("nekorisu/v1/y.json", b"y".to_vec()).await.unwrap(),
            Put::Created
        );
    }

    #[tokio::test]
    async fn auth_is_required_and_errors_do_not_leak_credentials() {
        let server = Server::start(Some(("me", "synthetic-secret"))).await;

        let mut anonymous = server.spec();
        anonymous.username = None;
        anonymous.password = None;
        let dav = Webdav::new(anonymous).unwrap();
        assert_eq!(dav.probe().await.unwrap(), 401);
        let err = dav.list("nekorisu").await.unwrap_err();
        assert!(matches!(err, StorageError::Denied { status: 401 }), "{err:?}");

        let mut wrong = server.spec();
        wrong.password = Some("synthetic-wrong".into());
        let dav = Webdav::new(wrong).unwrap();
        let err = dav.put_new("nekorisu/a.json", b"x".to_vec()).await.unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(matches!(err, StorageError::Denied { status: 401 }));
        assert!(
            !shown.contains("synthetic-wrong") && !shown.contains("synthetic-secret") && !shown.contains("127.0.0.1")
        );
        assert!(
            !server.root().join("nekorisu").exists(),
            "nothing was created without credentials"
        );

        let dav = Webdav::new(server.spec()).unwrap();
        assert_eq!(dav.probe().await.unwrap(), 207);
        assert_eq!(
            dav.put_new("nekorisu/a.json", b"x".to_vec()).await.unwrap(),
            Put::Created
        );
        assert!(server.root().join("nekorisu/a.json").is_file());
    }

    #[tokio::test]
    async fn retries_server_errors_then_gives_up() {
        let server = Server::start(None).await;
        let dav = Webdav::new(server.spec()).unwrap();
        server.fail_next(2, 503);
        assert_eq!(
            dav.put_new("nekorisu/v1/r.json", b"r".to_vec()).await.unwrap(),
            Put::Created
        );

        server.fail_next(10, 503);
        let err = dav.get("nekorisu/v1/r.json", None).await.unwrap_err();
        assert!(matches!(err, StorageError::Status { status: 503, .. }), "{err:?}");
    }

    #[tokio::test]
    async fn base_path_is_part_of_every_url() {
        let server = Server::start_at("/dav/inner", None).await;
        let dav = Webdav::new(server.spec()).unwrap();
        dav.put_new("nekorisu/v1/a.json", b"a".to_vec()).await.unwrap();
        assert_eq!(std::fs::read(server.root().join("nekorisu/v1/a.json")).unwrap(), b"a");
        let names: Vec<_> = dav
            .list("nekorisu/v1")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["a.json"]);
        assert_eq!(dav.probe().await.unwrap(), 207);
    }

    /// `resolve`: the URL names a host that does not exist; the pinned address reaches the
    /// loopback server, and the Host header still carries the URL's host.
    #[tokio::test]
    async fn resolve_pins_the_address_and_keeps_the_host_header() {
        let server = Server::start_at("/dav", None).await;
        let host = format!("some.invalid-name:{}", server.port());
        let mut spec = server.spec();
        spec.url = format!("http://{host}/dav");
        spec.resolve = Some("127.0.0.1".parse().unwrap());
        let dav = Webdav::new(spec).unwrap();
        assert_eq!(dav.probe().await.unwrap(), 207);
        assert_eq!(
            dav.put_new("nekorisu/v1/pinned.json", b"p".to_vec()).await.unwrap(),
            Put::Created
        );
        assert_eq!(
            std::fs::read(server.root().join("nekorisu/v1/pinned.json")).unwrap(),
            b"p"
        );
        let hosts = server.seen_hosts();
        assert!(!hosts.is_empty() && hosts.iter().all(|h| *h == host), "{hosts:?}");

        // Without the pin the same URL cannot be reached: the name does not resolve (or a proxy
        // in this machine's environment answers instead, with an error).
        let mut unpinned = server.spec();
        unpinned.url = format!("http://{host}/dav");
        unpinned.resolve = None;
        match Webdav::new(unpinned).unwrap().probe().await {
            Err(StorageError::Network(_)) => {}
            other => assert_ne!(other.unwrap(), 207, "the unpinned client reached the server"),
        }
    }

    /// Runs the ignored test below in a child process with proxy variables pointing at a closed
    /// port: changing the environment of this process would race with the other tests.
    #[test]
    fn resolve_ignores_proxy_variables() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::tests::child_pinned_client_with_dead_proxy",
                "--ignored",
                "--nocapture",
            ])
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("http_proxy", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("https_proxy", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .env("all_proxy", "http://127.0.0.1:1")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && text.contains("pinned client worked through dead proxy variables"),
            "{text}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("unpinned client is stopped by the dead proxy"),
            "the proxy variables had no effect, so the test proves nothing:\n{text}"
        );
    }

    #[tokio::test]
    #[ignore = "run by resolve_ignores_proxy_variables in a child process with proxy variables set"]
    async fn child_pinned_client_with_dead_proxy() {
        let server = Server::start_at("/dav", None).await;
        let pinned = Webdav::new(Spec {
            url: format!("http://some.invalid-name:{}/dav", server.port()),
            username: None,
            password: None,
            resolve: Some("127.0.0.1".parse().unwrap()),
        })
        .unwrap();
        assert_eq!(pinned.probe().await.unwrap(), 207);
        println!("pinned client worked through dead proxy variables");

        // The same variables do stop a client that is not pinned: it goes to the dead proxy.
        let unpinned = Webdav::new(Spec {
            url: format!("http://127.0.0.1:{}/dav", server.port()),
            username: None,
            password: None,
            resolve: None,
        })
        .unwrap();
        if unpinned.probe().await.is_err() {
            println!("unpinned client is stopped by the dead proxy");
        }
    }
}
