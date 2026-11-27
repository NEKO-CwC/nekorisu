//! The trusted source: where it is (`source.json`, set only on this device), pulling the config
//! it signs, keeping the last good one, and telling it this device's state. Only core talks to it.
//!
//! On disk, under `~/.nekorisu/`:
//!   source.json                   name, address and public keys of the source
//!   remote/current                the serial of the config in use
//!   remote/<serial>/signed.json   that config as signed, checked again whenever it is read
//!   remote/<serial>/resources/    what it refers to, by SHA-256
//!   remote/status.json            when it last checked, and what went wrong
//!   remote/paths.json             what each side said about path maps last time (`pathmap::Memory`)
//!   remote/values.json            the same for the other values (`settings`)
//!   remote/requests.json          what this device did with each force and exec request
//!   exec.json                     the "accept everything" switch; only ever set on this device

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use nekorisu::sign::{CONTEXT_CONFIG, Envelope, PublicKey, sha256_hex};
use serde::{Deserialize, Deserializer, Serialize};
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request};

use crate::config::PathMap;
use crate::pathmap::SourceRule;
use crate::paths::Paths;
use crate::proto::cloud::cloud_client::CloudClient;
use crate::proto::cloud::get_config_response::Result as ConfigResult;
use crate::proto::cloud::{
    ConfigFailure, ConfigFailureReason, GetConfigRequest, GetResourceRequest, LocalPathMap, LocalValue,
    ReportStateRequest, RequestState, RequestStatus,
};
use crate::remote::write_atomic;
use crate::secrets::{self, Kind};
use crate::timefmt::now_ms;

/// The protocol major version (`nekorisu-protocol`).
const PROTOCOL: &str = "1";
/// The config format major version this client reads.
const SCHEMA_MAJOR: &str = "1";
/// Between checks when the config does not say.
pub const DEFAULT_POLL: Duration = Duration::from_secs(3600);
const MAX_RESOURCE: u64 = 16 << 20;
/// Older configs kept besides the one in use.
const KEEP_OLDER: usize = 1;

/// The trusted source. Only ever set on this device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    /// https://host[:port]; http only to this machine.
    pub url: String,
    /// Base64 Ed25519 public keys; a config signed by any of them is taken.
    pub keys: Vec<String>,
}

impl Source {
    pub fn load(paths: &Paths) -> anyhow::Result<Option<Source>> {
        match fs::read(source_file(paths)) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).with_context(|| format!("{} 读不懂", source_file(paths).display()))?,
            )),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Writes source.json. A different source (address or keys) starts over: what the old one
    /// sent, and its serial numbers, mean nothing to the new one.
    pub fn save(&self, paths: &Paths) -> anyhow::Result<bool> {
        self.check()?;
        let before = Source::load(paths).ok().flatten();
        let changed = before.as_ref().is_none_or(|b| b.url != self.url || b.keys != self.keys);
        if changed {
            match fs::remove_dir_all(remote_dir(paths)) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            // Running scripts was allowed for the old source, not this one.
            Exec::off(paths, "换了来源")?;
        }
        write_atomic(&source_file(paths), &serde_json::to_vec_pretty(self)?)?;
        Ok(changed)
    }

    pub fn check(&self) -> anyhow::Result<()> {
        crate::config::check_storage_name(&self.name).map_err(|_| {
            anyhow!(
                "来源的名字只能用字母、数字、点、下划线和短横线，并且以字母或数字开头：{}",
                self.name
            )
        })?;
        let url = reqwest::Url::parse(&self.url).with_context(|| format!("{} 不是一个地址", self.url))?;
        let host = url.host_str().unwrap_or_default();
        match url.scheme() {
            "https" => {}
            "http" if is_loopback(host) => {}
            "http" => bail!("来源要用 https；http 只能连这台电脑自己（localhost）"),
            other => bail!("来源的地址要以 https:// 开头，不是 {other}://"),
        }
        if url.path() != "/" || url.query().is_some() {
            bail!("来源的地址只写协议、主机和端口，比如 https://nekorisu.example.com:8443");
        }
        if self.keys.is_empty() {
            bail!("至少要一把公钥（--key）");
        }
        self.public_keys().map(|_| ())
    }

    pub fn public_keys(&self) -> anyhow::Result<Vec<PublicKey>> {
        self.keys
            .iter()
            .map(|k| PublicKey::from_base64(k).map_err(|e| anyhow!("公钥 {k}：{e}")))
            .collect()
    }

    pub fn key_ids(&self) -> Vec<String> {
        self.public_keys()
            .map(|keys| keys.into_iter().map(|k| k.key_id).collect())
            .unwrap_or_default()
    }

    /// The token from `nekorisu source login`, if any. Reads the keychain on macOS.
    pub async fn token(&self, paths: &Paths) -> anyhow::Result<Option<String>> {
        let (paths, name) = (paths.clone(), self.name.clone());
        tokio::task::spawn_blocking(move || secrets::load_kind(Kind::Source, &paths, &name)).await?
    }
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

pub fn source_file(paths: &Paths) -> PathBuf {
    paths.home.join("source.json")
}

pub fn remote_dir(paths: &Paths) -> PathBuf {
    paths.home.join("remote")
}

pub fn paths_memory_file(paths: &Paths) -> PathBuf {
    remote_dir(paths).join("paths.json")
}

pub fn values_memory_file(paths: &Paths) -> PathBuf {
    remote_dir(paths).join("values.json")
}

/// config.toml with the trusted source's values merged in, for code outside core
/// (`nekorisu update`, the updater). Reads what core remembered and changes nothing.
pub fn effective_config(paths: &Paths) -> anyhow::Result<crate::config::Config> {
    let mut config = crate::config::load(paths)?;
    let table = crate::config::load_table(paths)?;
    let theirs = Source::load(paths)
        .ok()
        .flatten()
        .and_then(|s| load_held(paths, &s))
        .map(|h| h.config.values())
        .unwrap_or_default();
    let memory: crate::pathmap::Memory = fs::read(values_memory_file(paths))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let (applied, _) = crate::settings::merge(
        &crate::settings::local_values(&table),
        &crate::settings::locked(&table),
        &theirs,
        &memory,
    );
    crate::settings::apply(&mut config, &applied);
    Ok(config)
}

// ---- the config ----

/// A resource a config refers to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ResourceRef {
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub executable: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DigestRefs {
    pub excerpt: Option<ResourceRef>,
    pub summary: Option<ResourceRef>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PathsSection {
    pub map: Vec<SourceRule>,
}

/// The parts of a signed config (`Config` in cloud.proto) this client reads; the rest is
/// ignored, as are fields it does not know.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Pushed {
    pub schema: String,
    #[serde(deserialize_with = "number_or_text")]
    pub serial: u64,
    pub issued_at: String,
    pub min_client_version: String,
    pub poll_interval_secs: u32,
    pub digest: DigestRefs,
    pub kind_rules: Option<ResourceRef>,
    pub paths: PathsSection,
    pub local: PushedLocal,
    pub update: PushedUpdate,
    pub force: Vec<ForceRequest>,
    pub requests: Vec<ExecRequest>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PushedLocal {
    pub copy_form: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PushedUpdate {
    pub channel: String,
    pub urls: Vec<String>,
}

/// Asks to change keys, locked ones included; applies only when the user accepts it here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ForceRequest {
    pub id: String,
    pub changes: Vec<ForceChange>,
    pub devices: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ForceChange {
    pub key: String,
    pub value_json: String,
}

/// About the "accept everything" switch: allow (only a request) or revoke (at once).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ExecRequest {
    pub id: String,
    #[serde(deserialize_with = "exec_action")]
    pub action: ExecAction,
    pub devices: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecAction {
    #[default]
    Unknown,
    Allow,
    Revoke,
}

/// Enum names, as the proto3 JSON mapping writes them, or their numbers.
fn exec_action<'de, D: Deserializer<'de>>(d: D) -> Result<ExecAction, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) if s == "EXEC_ACTION_ALLOW" => ExecAction::Allow,
        serde_json::Value::String(s) if s == "EXEC_ACTION_REVOKE" => ExecAction::Revoke,
        serde_json::Value::Number(n) if n.as_u64() == Some(1) => ExecAction::Allow,
        serde_json::Value::Number(n) if n.as_u64() == Some(2) => ExecAction::Revoke,
        _ => ExecAction::Unknown,
    })
}

fn for_device(devices: &[String], device: &str) -> bool {
    devices.is_empty() || devices.iter().any(|d| d == device)
}

/// The JSON mapping of proto3 writes 64-bit numbers as text; plain numbers are read too.
fn number_or_text<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Number(u64),
        Text(String),
    }
    match Either::deserialize(d)? {
        Either::Number(n) => Ok(n),
        Either::Text(t) => t.parse().map_err(serde::de::Error::custom),
    }
}

impl Pushed {
    fn resources(&self) -> Vec<&ResourceRef> {
        [
            self.digest.excerpt.as_ref(),
            self.digest.summary.as_ref(),
            self.kind_rules.as_ref(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The poll interval it asks for, or the default.
    pub fn poll(&self) -> Duration {
        match self.poll_interval_secs {
            0 => DEFAULT_POLL,
            secs => Duration::from_secs(u64::from(secs.clamp(60, 7 * 86_400))),
        }
    }

    /// The values it pushes for keys this device can also set (`settings`).
    pub fn values(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        crate::settings::source_values(&self.local.copy_form, &self.update.channel, &self.update.urls)
    }

    /// Its force requests meant for `device`.
    pub fn force_for(&self, device: &str) -> Vec<&ForceRequest> {
        self.force.iter().filter(|f| for_device(&f.devices, device)).collect()
    }

    /// Its exec requests meant for `device`.
    pub fn exec_for(&self, device: &str) -> Vec<&ExecRequest> {
        self.requests
            .iter()
            .filter(|r| for_device(&r.devices, device))
            .collect()
    }

    /// Set when this client is older than the source asks for.
    pub fn too_old(&self) -> Option<String> {
        let want = semver::Version::parse(&self.min_client_version).ok()?;
        let have = semver::Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
        (have < want).then(|| self.min_client_version.clone())
    }
}

/// A config as signed, the way it is kept on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Signed {
    etag: String,
    key_id: String,
    /// Base64.
    payload: String,
    /// Base64.
    signature: String,
    received_ms: i64,
}

impl Signed {
    fn open(&self, keys: &[PublicKey]) -> anyhow::Result<Pushed> {
        let envelope = Envelope {
            payload: self.payload.clone(),
            key_id: self.key_id.clone(),
            signature: self.signature.clone(),
        };
        let payload = envelope.open(CONTEXT_CONFIG, keys).map_err(|e| anyhow!("{e}"))?;
        serde_json::from_slice(&payload).context("配置不是合法的 JSON")
    }
}

/// The config in use.
#[derive(Clone, Debug)]
pub struct Held {
    pub serial: u64,
    pub etag: String,
    pub config: Pushed,
}

impl Held {
    /// The source's path rules (this device picks its own in `pathmap::merge`).
    pub fn path_rules(&self) -> &[SourceRule] {
        &self.config.paths.map
    }
}

/// Reads the config in use and checks its signature again; `None` when there is none, or it
/// no longer checks out (then it is as if there were none, and the next pull starts over).
pub fn load_held(paths: &Paths, source: &Source) -> Option<Held> {
    let dir = remote_dir(paths);
    let serial: u64 = fs::read_to_string(dir.join("current")).ok()?.trim().parse().ok()?;
    let bytes = fs::read(dir.join(serial.to_string()).join("signed.json")).ok()?;
    let signed: Signed = serde_json::from_slice(&bytes).ok()?;
    let keys = source.public_keys().ok()?;
    match signed.open(&keys) {
        Ok(config) if config.serial == serial => Some(Held {
            serial,
            etag: signed.etag,
            config,
        }),
        Ok(_) | Err(_) => {
            eprintln!("{} 保存的第 {serial} 版配置验不过签名，不用它", now_ms());
            None
        }
    }
}

/// What the last check found; kept so it can be shown after a restart.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub checked_ms: i64,
    /// Empty when the last check worked.
    pub error: String,
    /// The newest config this device refused, and why (cloud.proto `ConfigFailureReason`).
    pub refused: Option<(u64, i32)>,
}

impl Status {
    pub fn load(paths: &Paths) -> Status {
        fs::read(remote_dir(paths).join("status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self, paths: &Paths) {
        if let Err(err) = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|b| write_atomic(&remote_dir(paths).join("status.json"), &b))
        {
            eprintln!("{} 记不下来源的状态：{err}", now_ms());
        }
    }
}

// ---- requests and the "accept everything" switch ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Accepted,
    Rejected,
}

/// What this device did with each request, by ID. An ID is handled once: a request taken out of
/// the config and put back is not asked again, and nothing it applied is undone.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Handled {
    pub requests: std::collections::BTreeMap<String, HandledRequest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandledRequest {
    pub answer: Answer,
    pub at_ms: i64,
}

impl Handled {
    fn file(paths: &Paths) -> PathBuf {
        remote_dir(paths).join("requests.json")
    }

    pub fn load(paths: &Paths) -> Handled {
        fs::read(Handled::file(paths))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn state(&self, id: &str) -> Option<Answer> {
        self.requests.get(id).map(|r| r.answer)
    }

    pub fn record(paths: &Paths, id: &str, answer: Answer) -> anyhow::Result<()> {
        let mut handled = Handled::load(paths);
        handled.requests.insert(
            id.to_string(),
            HandledRequest {
                answer,
                at_ms: now_ms(),
            },
        );
        write_atomic(&Handled::file(paths), &serde_json::to_vec_pretty(&handled)?)?;
        Ok(())
    }
}

/// The "accept everything" switch: whether scripts the source sends may run here. Off unless the
/// user turns it on at this device (`nekorisu source allow-exec`); the source can ask, and can
/// turn it off, never on. Nothing runs scripts yet; the switch is kept and reported.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Exec {
    pub allowed: bool,
    pub changed_ms: i64,
    /// Why it is the way it is, for `source status`.
    pub why: String,
}

impl Exec {
    fn file(paths: &Paths) -> PathBuf {
        paths.home.join("exec.json")
    }

    pub fn load(paths: &Paths) -> Exec {
        fs::read(Exec::file(paths))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(paths: &Paths, allowed: bool, why: &str) -> anyhow::Result<()> {
        let exec = Exec {
            allowed,
            changed_ms: now_ms(),
            why: why.to_string(),
        };
        write_atomic(&Exec::file(paths), &serde_json::to_vec_pretty(&exec)?)?;
        Ok(())
    }

    pub fn on(paths: &Paths, why: &str) -> anyhow::Result<()> {
        Exec::save(paths, true, why)
    }

    /// Off; does nothing when it already is.
    pub fn off(paths: &Paths, why: &str) -> anyhow::Result<()> {
        if Exec::load(paths).allowed {
            Exec::save(paths, false, why)?;
        }
        Ok(())
    }
}

// ---- talking to the source ----

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A newer config is in use now.
    Updated(u64),
    /// The source has nothing newer.
    Unchanged,
}

/// Why a pull failed, with the reason the source is told (`ReportState.last_failure`) when the
/// config itself was refused.
#[derive(Debug)]
struct Refused {
    serial: u64,
    reason: ConfigFailureReason,
    message: String,
}

enum Failure {
    /// Could not ask: no connection, no token, ...
    Ask(anyhow::Error),
    Refused(Refused),
}

impl From<anyhow::Error> for Failure {
    fn from(err: anyhow::Error) -> Self {
        Failure::Ask(err)
    }
}

pub struct Device<'a> {
    pub id: &'a str,
}

async fn connect(url: &str) -> anyhow::Result<CloudClient<Channel>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut endpoint = Endpoint::from_shared(url.to_string())
        .with_context(|| format!("{url} 不是一个地址"))?
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60));
    if url.starts_with("https://") {
        endpoint = endpoint
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .context("TLS 设置不了")?;
    }
    let channel = endpoint
        .connect()
        .await
        .map_err(|e| anyhow!("连不上来源 {url}：{}", error_chain(&e)))?;
    Ok(CloudClient::new(channel))
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(next) = source {
        text.push('：');
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

fn request<T>(message: T, device: &Device, token: Option<&str>) -> anyhow::Result<Request<T>> {
    let mut r = Request::new(message);
    let meta = r.metadata_mut();
    meta.insert("nekorisu-protocol", MetadataValue::from_static(PROTOCOL));
    meta.insert(
        "nekorisu-version",
        MetadataValue::from_static(env!("CARGO_PKG_VERSION")),
    );
    meta.insert(
        "nekorisu-device",
        MetadataValue::try_from(device.id).context("设备 ID 放不进请求头")?,
    );
    if let Some(token) = token {
        meta.insert(
            "authorization",
            MetadataValue::try_from(format!("Bearer {token}")).context("令牌里有不能放进请求头的字符")?,
        );
    }
    Ok(r)
}

fn status_error(what: &str, status: tonic::Status) -> anyhow::Error {
    match status.code() {
        Code::Unauthenticated => anyhow!(
            "来源不认这个令牌，或者要令牌：运行 nekorisu source login（{}）",
            status.message()
        ),
        Code::FailedPrecondition => anyhow!("来源不支持这个版本的协议：{}", status.message()),
        Code::NotFound if what == "GetConfig" => anyhow!("来源还没有发布配置"),
        Code::Unavailable => anyhow!("连不上来源：{}", status.message()),
        _ => anyhow!("{what} 失败（{:?}）：{}", status.code(), status.message()),
    }
}

/// Asks the source for a newer config and, if there is one, checks it and puts it in use.
/// Records the result in status.json. Telling the source this device's state is `report`.
pub async fn pull(paths: &Paths, source: &Source, device: &Device<'_>, token: Option<&str>) -> anyhow::Result<Outcome> {
    let mut status = Status::load(paths);
    status.checked_ms = now_ms();
    let result = pull_inner(paths, source, device, token).await;
    let outcome = match result {
        Ok(outcome) => {
            status.error.clear();
            if let Outcome::Updated(_) = outcome {
                status.refused = None;
            }
            Ok(outcome)
        }
        Err(Failure::Ask(err)) => {
            status.error = format!("{err:#}");
            Err(err)
        }
        Err(Failure::Refused(refused)) => {
            status.error = refused.message.clone();
            status.refused = Some((refused.serial, refused.reason as i32));
            Err(anyhow!(refused.message))
        }
    };
    status.save(paths);
    outcome
}

async fn pull_inner(
    paths: &Paths,
    source: &Source,
    device: &Device<'_>,
    token: Option<&str>,
) -> Result<Outcome, Failure> {
    let keys = source.public_keys()?;
    let held = load_held(paths, source);
    let mut client = connect(&source.url).await?;
    let reply = client
        .get_config(request(
            GetConfigRequest {
                etag: held.as_ref().map(|h| h.etag.clone()).unwrap_or_default(),
            },
            device,
            token,
        )?)
        .await
        .map_err(|s| status_error("GetConfig", s))?
        .into_inner();
    let signed = match reply.result {
        Some(ConfigResult::NotModified(_)) if held.is_some() => return Ok(Outcome::Unchanged),
        Some(ConfigResult::Config(c)) => c,
        Some(ConfigResult::NotModified(_)) | None => {
            return Err(anyhow!("来源说没有新配置，可这台设备还没有任何配置").into());
        }
    };
    let kept = Signed {
        etag: reply.etag,
        key_id: signed.key_id,
        payload: B64.encode(&signed.payload),
        signature: B64.encode(&signed.signature),
        received_ms: now_ms(),
    };
    let refuse = |serial: u64, reason: ConfigFailureReason, message: String| {
        Failure::Refused(Refused {
            serial,
            reason,
            message,
        })
    };
    // Which serial it claims, for the report, before it is trusted.
    let claimed = serde_json::from_slice::<Pushed>(&signed.payload)
        .map(|p| p.serial)
        .unwrap_or(0);
    let config = kept.open(&keys).map_err(|err| {
        refuse(
            claimed,
            ConfigFailureReason::BadSignature,
            format!("来源发来的配置验不过签名，没有用它：{err:#}"),
        )
    })?;
    let major = config.schema.split('.').next().unwrap_or_default();
    if major != SCHEMA_MAJOR {
        return Err(refuse(
            config.serial,
            ConfigFailureReason::UnsupportedSchema,
            format!("来源发来的配置格式是 {}，这个版本只认 {SCHEMA_MAJOR}.x", config.schema),
        ));
    }
    if let Some(held) = &held
        && config.serial <= held.serial
    {
        return Err(refuse(
            config.serial,
            ConfigFailureReason::Other,
            format!(
                "来源发来的是第 {} 版配置，不比在用的第 {} 版新，没有用它",
                config.serial, held.serial
            ),
        ));
    }

    // Every resource, checked, before anything changes.
    let dir = remote_dir(paths);
    let staging = dir.join(format!(".incoming-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(staging.join("resources")).map_err(anyhow::Error::from)?;
    let wanted: BTreeSet<String> = config
        .resources()
        .iter()
        .map(|r| r.sha256.to_ascii_lowercase())
        .collect();
    for sha in &wanted {
        let target = staging.join("resources").join(sha);
        let reused = held
            .as_ref()
            .map(|h| dir.join(h.serial.to_string()).join("resources").join(sha))
            .filter(|old| fs::read(old).is_ok_and(|b| sha256_hex(&b) == *sha));
        let bytes = match reused {
            Some(old) => fs::read(old).map_err(anyhow::Error::from)?,
            None => fetch_resource(&mut client, sha, device, token)
                .await
                .map_err(|(reason, err)| refuse(config.serial, reason, format!("{err:#}")))?,
        };
        fs::write(&target, &bytes).map_err(anyhow::Error::from)?;
    }
    write_atomic(
        &staging.join("signed.json"),
        &serde_json::to_vec_pretty(&kept).map_err(anyhow::Error::from)?,
    )
    .map_err(anyhow::Error::from)?;

    // In place: the folder, then the pointer.
    let final_dir = dir.join(config.serial.to_string());
    let _ = fs::remove_dir_all(&final_dir);
    fs::rename(&staging, &final_dir).map_err(anyhow::Error::from)?;
    write_atomic(&dir.join("current"), config.serial.to_string().as_bytes()).map_err(anyhow::Error::from)?;
    prune(&dir, config.serial);
    if let Some(min) = config.too_old() {
        eprintln!(
            "{} 来源要求客户端至少是 {min}，这台是 {}：运行 nekorisu update",
            now_ms(),
            env!("CARGO_PKG_VERSION")
        );
    }
    Ok(Outcome::Updated(config.serial))
}

async fn fetch_resource(
    client: &mut CloudClient<Channel>,
    sha: &str,
    device: &Device<'_>,
    token: Option<&str>,
) -> Result<Vec<u8>, (ConfigFailureReason, anyhow::Error)> {
    let unavailable = |err: anyhow::Error| (ConfigFailureReason::ResourceUnavailable, err);
    let req = request(
        GetResourceRequest {
            sha256: sha.to_string(),
        },
        device,
        token,
    )
    .map_err(unavailable)?;
    let mut stream = client
        .get_resource(req)
        .await
        .map_err(|s| unavailable(status_error("GetResource", s)))?
        .into_inner();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream
        .message()
        .await
        .map_err(|s| unavailable(status_error("GetResource", s)))?
    {
        if bytes.len() as u64 + chunk.data.len() as u64 > MAX_RESOURCE {
            return Err(unavailable(anyhow!("资源 {sha} 超过 {MAX_RESOURCE} 字节")));
        }
        bytes.extend_from_slice(&chunk.data);
    }
    if sha256_hex(&bytes) != sha {
        return Err((
            ConfigFailureReason::ResourceHashMismatch,
            anyhow!("资源 {sha} 的内容和 SHA-256 对不上"),
        ));
    }
    Ok(bytes)
}

/// Keeps the config in use and the one before it.
fn prune(dir: &Path, current: u64) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut older: Vec<u64> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u64>().ok())
        .filter(|s| *s < current)
        .collect();
    older.sort_unstable_by(|a, b| b.cmp(a));
    for serial in older.into_iter().skip(KEEP_OLDER) {
        let _ = fs::remove_dir_all(dir.join(serial.to_string()));
    }
}

/// What this device tells its source about itself, besides what it reads from disk.
#[derive(Clone, Debug, Default)]
pub struct DeviceReport {
    /// Its own `[[paths.map]]`.
    pub paths: Vec<PathMap>,
    /// Its own values: key, JSON text, locked.
    pub values: Vec<(String, String, bool)>,
}

/// Tells the source what config this device runs, its own values and path rules, what it did
/// with requests, whether it runs scripts, and the newest config it refused.
pub async fn report(
    paths: &Paths,
    source: &Source,
    device: &Device<'_>,
    token: &str,
    own: &DeviceReport,
) -> anyhow::Result<()> {
    let held = load_held(paths, source);
    let status = Status::load(paths);
    let handled = Handled::load(paths);
    let mut requests = Vec::new();
    if let Some(held) = &held {
        let ids = held
            .config
            .force_for(device.id)
            .into_iter()
            .map(|f| f.id.clone())
            .chain(held.config.exec_for(device.id).into_iter().map(|r| r.id.clone()));
        for id in ids {
            let state = match handled.state(&id) {
                Some(Answer::Accepted) => RequestState::Accepted,
                Some(Answer::Rejected) => RequestState::Rejected,
                None => RequestState::Pending,
            };
            requests.push(RequestStatus {
                id,
                state: state as i32,
            });
        }
    }
    let message = ReportStateRequest {
        applied_serial: held.as_ref().map_or(0, |h| h.serial),
        applied_etag: held.as_ref().map(|h| h.etag.clone()).unwrap_or_default(),
        local_values: own
            .values
            .iter()
            .map(|(key, value_json, locked)| LocalValue {
                key: key.clone(),
                value_json: value_json.clone(),
                locked: *locked,
            })
            .collect(),
        exec_allowed: Exec::load(paths).allowed,
        requests,
        last_failure: status.refused.map(|(serial, reason)| ConfigFailure { serial, reason }),
        paths: own
            .paths
            .iter()
            .map(|m| LocalPathMap {
                from: m.from.clone(),
                to: m.to.clone(),
                locked: m.lock,
            })
            .collect(),
    };
    let mut client = connect(&source.url).await?;
    client
        .report_state(request(message, device, Some(token))?)
        .await
        .map_err(|s| status_error("ReportState", s))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testsource::{TestSource, config};

    const TOKEN: &str = "synthetic-device-token";
    const DEVICE: Device<'static> = Device { id: "dev-test" };

    fn home() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            home: dir.path().to_path_buf(),
        };
        (dir, paths)
    }

    async fn tell(paths: &Paths, source: &Source, paths_map: &[PathMap]) {
        let own = DeviceReport {
            paths: paths_map.to_vec(),
            values: vec![("local.copy_form".into(), "\"full\"".into(), true)],
        };
        report(paths, source, &DEVICE, TOKEN, &own).await.unwrap();
    }

    fn local(from: &str, to: &str, lock: bool) -> PathMap {
        PathMap {
            from: from.into(),
            to: to.into(),
            lock,
        }
    }

    #[test]
    fn a_source_is_checked_before_it_is_kept() {
        let (_dir, paths) = home();
        let key = SecretKey::generate().public().to_base64();
        let good = Source {
            name: "home".into(),
            url: "https://nekorisu.example:8443".into(),
            keys: vec![key.clone()],
        };
        assert!(good.save(&paths).unwrap(), "new");
        assert_eq!(Source::load(&paths).unwrap(), Some(good.clone()));
        assert!(!good.save(&paths).unwrap(), "the same again");

        let bad = |url: &str, keys: Vec<String>| Source {
            name: "home".into(),
            url: url.into(),
            keys,
        };
        assert!(
            bad("http://nekorisu.example", vec![key.clone()]).check().is_err(),
            "http elsewhere"
        );
        assert!(
            bad("http://127.0.0.1:7070", vec![key.clone()]).check().is_ok(),
            "http to this machine"
        );
        assert!(bad("https://x.example/path", vec![key.clone()]).check().is_err());
        assert!(bad("https://x.example", vec![]).check().is_err());
        assert!(bad("https://x.example", vec!["not a key".into()]).check().is_err());

        // Another source starts over: what the old one sent is gone.
        fs::create_dir_all(remote_dir(&paths).join("7")).unwrap();
        let other = Source {
            keys: vec![SecretKey::generate().public().to_base64()],
            ..good
        };
        assert!(other.save(&paths).unwrap());
        assert!(!remote_dir(&paths).exists());
    }

    use nekorisu::sign::SecretKey;

    #[tokio::test]
    async fn pulls_checks_keeps_and_reports() {
        let src = TestSource::start(Some(TOKEN)).await;
        let (_dir, paths) = home();
        let source = src.source("home");
        source.save(&paths).unwrap();
        let mine = [local(r"D:\work", "/Users/me/w", true)];

        // Nothing published yet; then no token.
        assert!(pull(&paths, &source, &DEVICE, Some(TOKEN)).await.is_err());
        let recipe = br#"{"runner":"excerpt"}"#;
        let sha = src.add_resource(recipe, recipe);
        let mut doc = config(
            2,
            json!([{ "from": "/home/me/src", "to": "/Users/me/src", "devices": ["dev-test"] }]),
        );
        doc["digest"] = json!({ "excerpt": { "name": "excerpt", "version": "1", "sha256": sha, "executable": false } });
        doc["poll_interval_secs"] = json!(600);
        src.publish(&doc);
        let err = pull(&paths, &source, &DEVICE, None).await.unwrap_err();
        assert!(format!("{err:#}").contains("source login"), "{err:#}");
        assert!(Status::load(&paths).error.contains("source login"));

        // With the token: in use, its resource fetched and checked, the state reported.
        assert_eq!(
            pull(&paths, &source, &DEVICE, Some(TOKEN)).await.unwrap(),
            Outcome::Updated(2)
        );
        let held = load_held(&paths, &source).unwrap();
        assert_eq!(held.serial, 2);
        assert_eq!(held.config.poll(), Duration::from_secs(600));
        assert_eq!(held.path_rules()[0].to, "/Users/me/src");
        assert_eq!(held.path_rules()[0].devices, vec!["dev-test".to_string()]);
        let kept = remote_dir(&paths).join("2/resources").join(&sha);
        assert_eq!(fs::read(kept).unwrap(), recipe);
        assert!(Status::load(&paths).error.is_empty());
        tell(&paths, &source, &mine).await;
        let reports = src.reports();
        let (device, report) = reports.last().unwrap();
        assert_eq!(device, "dev-test");
        assert_eq!(report.applied_serial, 2);
        assert_eq!(report.paths.len(), 1);
        assert_eq!(
            (report.paths[0].from.as_str(), report.paths[0].locked),
            (r"D:\work", true)
        );
        assert!(report.last_failure.is_none());
        assert_eq!(report.local_values[0].key, "local.copy_form");
        assert!(report.local_values[0].locked && !report.exec_allowed);

        // The same again: not modified.
        assert_eq!(
            pull(&paths, &source, &DEVICE, Some(TOKEN)).await.unwrap(),
            Outcome::Unchanged
        );

        // Newer ones replace it; only the one before stays on disk.
        for serial in [4, 6] {
            src.publish(&config(serial, json!([])));
            assert_eq!(
                pull(&paths, &source, &DEVICE, Some(TOKEN)).await.unwrap(),
                Outcome::Updated(serial)
            );
        }
        assert!(!remote_dir(&paths).join("2").exists());
        assert!(remote_dir(&paths).join("4").exists() && remote_dir(&paths).join("6").exists());
        assert!(load_held(&paths, &source).unwrap().path_rules().is_empty());
    }

    #[tokio::test]
    async fn refuses_what_does_not_check_out_and_keeps_the_last_good_one() {
        let src = TestSource::start(Some(TOKEN)).await;
        let (_dir, paths) = home();
        let source = src.source("home");
        source.save(&paths).unwrap();
        src.publish(&config(5, json!([{ "from": "/a", "to": "/b", "devices": [] }])));
        pull(&paths, &source, &DEVICE, Some(TOKEN)).await.unwrap();

        let still_5 = |why: &str| {
            let held = load_held(&paths, &source).unwrap();
            assert_eq!(held.serial, 5, "{why}");
            assert_eq!(held.path_rules().len(), 1, "{why}");
        };
        let refused = async |reason: ConfigFailureReason| {
            tell(&paths, &source, &[]).await;
            let report = src.reports().last().unwrap().1.clone();
            assert_eq!(report.last_failure.map(|f| f.reason), Some(reason as i32));
            assert_eq!(report.applied_serial, 5);
        };

        // Signed by a key the device does not know.
        src.publish_signed_by(&config(9, json!([])), &SecretKey::generate());
        let err = pull(&paths, &source, &DEVICE, Some(TOKEN)).await.unwrap_err();
        assert!(format!("{err:#}").contains("签名"), "{err:#}");
        still_5("bad signature");
        refused(ConfigFailureReason::BadSignature).await;

        // Not newer.
        src.publish(&config(5, json!([])));
        assert!(pull(&paths, &source, &DEVICE, Some(TOKEN)).await.is_err());
        still_5("same serial");
        src.publish(&config(3, json!([])));
        assert!(pull(&paths, &source, &DEVICE, Some(TOKEN)).await.is_err());
        still_5("older serial");

        // A format it does not know.
        let mut doc = config(7, json!([]));
        doc["schema"] = json!("2.0");
        src.publish(&doc);
        assert!(pull(&paths, &source, &DEVICE, Some(TOKEN)).await.is_err());
        still_5("schema");
        refused(ConfigFailureReason::UnsupportedSchema).await;

        // A resource whose bytes do not match, and one that is not there.
        let sha = src.add_resource(b"the real recipe", b"something else");
        let mut doc = config(8, json!([]));
        doc["kind_rules"] = json!({ "name": "kinds", "version": "1", "sha256": sha, "executable": false });
        src.publish(&doc);
        assert!(pull(&paths, &source, &DEVICE, Some(TOKEN)).await.is_err());
        still_5("resource hash");
        refused(ConfigFailureReason::ResourceHashMismatch).await;
        let mut doc = config(8, json!([]));
        doc["kind_rules"] = json!({ "name": "kinds", "version": "1", "sha256": "ab".repeat(32), "executable": false });
        src.publish(&doc);
        assert!(pull(&paths, &source, &DEVICE, Some(TOKEN)).await.is_err());
        refused(ConfigFailureReason::ResourceUnavailable).await;

        // Unreachable: the last good one stays in use.
        let gone = Source {
            url: "http://127.0.0.1:1".into(),
            ..source.clone()
        };
        assert!(pull(&paths, &gone, &DEVICE, Some(TOKEN)).await.is_err());
        still_5("unreachable");

        // Tampered on disk: not used.
        let signed = remote_dir(&paths).join("5/signed.json");
        let mut kept: serde_json::Value = serde_json::from_slice(&fs::read(&signed).unwrap()).unwrap();
        kept["payload"] = json!(B64.encode(serde_json::to_vec(&config(5, json!([]))).unwrap()));
        fs::write(&signed, serde_json::to_vec(&kept).unwrap()).unwrap();
        assert!(load_held(&paths, &source).is_none());
    }

    #[test]
    fn reads_serials_as_numbers_or_text_and_ignores_unknown_fields() {
        let doc: Pushed = serde_json::from_value(json!({ "schema": "1.0", "serial": "12", "storages": [1] })).unwrap();
        assert_eq!(doc.serial, 12);
        let doc: Pushed = serde_json::from_value(json!({ "serial": 3, "min_client_version": "999.0.0" })).unwrap();
        assert_eq!(doc.serial, 3);
        assert_eq!(doc.too_old().as_deref(), Some("999.0.0"));
        assert_eq!(doc.poll(), DEFAULT_POLL);
    }
}
