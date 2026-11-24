//! Maintainers' release tool (docs/release.md). Not shipped to users: the
//! release workflow packages only the `nekorisu` binary.
//!
//!   keygen    make a key pair (root keys go to a password manager, the signing key to a CI secret)
//!   certify   sign a signing-key certificate with a root key
//!   manifest  hash the release archives and sign `<channel>.json`
//!   verify    check `<channel>.json` the way the client will

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use nekorisu::release::{self, Asset, MANIFEST_SCHEMA, Manifest, SignedRelease, SigningCert};
use nekorisu::sign::{self, CONTEXT_RELEASE_CERT, CONTEXT_RELEASE_MANIFEST, Envelope, PublicKey, SecretKey};

const DAY_MS: i64 = 86_400_000;
const ASSET_PREFIX: &str = "nekorisu-";
const ASSET_SUFFIX: &str = ".tar.gz";

#[derive(Parser)]
#[command(name = "nekorisu-release", about = "nekorisu 维护者的发版工具（docs/release.md）")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 生成一对钥匙：私钥写进文件（只有你能读），公钥打印出来
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// 用根钥给签名钥签一张证书
    Certify {
        /// 根钥的私钥文件
        #[arg(long)]
        root_key: PathBuf,
        /// 签名钥的公钥（base64，keygen 打印的那个）
        #[arg(long)]
        signing_pub: String,
        /// 证书序号：每换一次签名钥加一，客户端只认不比自己见过的小的
        #[arg(long)]
        serial: u64,
        /// 有效期，天
        #[arg(long, default_value_t = 365)]
        days: i64,
        #[arg(long)]
        out: PathBuf,
    },
    /// 给发布目录里的压缩包算 SHA-256，生成并签名 <渠道>.json，顺便写 SHA256SUMS
    Manifest {
        #[arg(long)]
        channel: String,
        #[arg(long)]
        version: String,
        /// 压缩包的下载地址前缀，比如 https://github.com/NEKO-CwC/nekorisu/releases/download/v0.1.0
        #[arg(long)]
        base_url: String,
        /// 放压缩包的目录（nekorisu-<平台>.tar.gz）
        #[arg(long)]
        dist: PathBuf,
        /// 签名钥证书（certify 的输出）
        #[arg(long)]
        cert: PathBuf,
        /// 从哪个环境变量读签名钥的私钥文件内容（CI 里用）
        #[arg(long, conflicts_with = "key")]
        key_env: Option<String>,
        /// 签名钥的私钥文件
        #[arg(long)]
        key: Option<PathBuf>,
        #[arg(long)]
        notes_url: Option<String>,
    },
    /// 用内置的根公钥（或 --roots 指定的）验证 <渠道>.json
    Verify {
        file: PathBuf,
        #[arg(long)]
        roots: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Keygen { out } => keygen(&out),
        Command::Certify {
            root_key,
            signing_pub,
            serial,
            days,
            out,
        } => certify(&root_key, &signing_pub, serial, days, &out),
        Command::Manifest {
            channel,
            version,
            base_url,
            dist,
            cert,
            key_env,
            key,
            notes_url,
        } => {
            let secret = read_secret(key_env.as_deref(), key.as_deref())?;
            manifest(&channel, &version, &base_url, &dist, &cert, &secret, notes_url)
        }
        Command::Verify { file, roots } => verify(&file, roots.as_deref()),
    }
}

fn keygen(out: &Path) -> anyhow::Result<()> {
    let key = SecretKey::generate();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(out)
        .with_context(|| format!("建不了 {}（已经存在的话不会覆盖）", out.display()))?;
    file.write_all(key.to_file_json().as_bytes())?;
    let public = key.public();
    println!(
        "私钥：{}（只有你能读；存进密码管理器或 CI secret 后删掉这个文件）",
        out.display()
    );
    println!("key_id：{}", public.key_id);
    println!("公钥：{}", public.to_base64());
    Ok(())
}

fn certify(root_key: &Path, signing_pub: &str, serial: u64, days: i64, out: &Path) -> anyhow::Result<()> {
    let root = SecretKey::from_file_json(&fs::read_to_string(root_key)?)?;
    let signing = PublicKey::from_base64(signing_pub)?;
    let now = now_ms();
    let cert = SigningCert {
        purpose: "release".into(),
        key: signing.to_base64(),
        serial,
        not_before_ms: now - DAY_MS, // tolerate devices whose clock runs a little behind
        not_after_ms: now + days * DAY_MS,
    };
    let envelope = root.sign(CONTEXT_RELEASE_CERT, &serde_json::to_vec(&cert)?);
    fs::write(out, serde_json::to_string_pretty(&envelope)? + "\n")?;
    println!(
        "证书：{}（根钥 {}，签名钥 {}，序号 {}，{} 天有效）",
        out.display(),
        root.public().key_id,
        signing.key_id,
        serial,
        days
    );
    Ok(())
}

fn manifest(
    channel: &str,
    version: &str,
    base_url: &str,
    dist: &Path,
    cert: &Path,
    secret: &SecretKey,
    notes_url: Option<String>,
) -> anyhow::Result<()> {
    semver::Version::parse(version).with_context(|| format!("{version} 不是合法的版本号"))?;
    let cert: Envelope = serde_json::from_str(&fs::read_to_string(cert)?).context("证书文件格式不对")?;

    let mut assets = BTreeMap::new();
    let mut sums = String::new();
    let mut names: Vec<String> = fs::read_dir(dist)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(ASSET_PREFIX) && n.ends_with(ASSET_SUFFIX))
        .collect();
    names.sort();
    for name in &names {
        let bytes = fs::read(dist.join(name))?;
        let target = &name[ASSET_PREFIX.len()..name.len() - ASSET_SUFFIX.len()];
        let sha256 = sign::sha256_hex(&bytes);
        sums.push_str(&format!("{sha256}  {name}\n"));
        assets.insert(
            target.to_string(),
            Asset {
                url: format!("{}/{name}", base_url.trim_end_matches('/')),
                sha256,
                size: bytes.len() as u64,
            },
        );
    }
    if assets.is_empty() {
        bail!("{} 里没有 {ASSET_PREFIX}<平台>{ASSET_SUFFIX}", dist.display());
    }
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA,
        channel: channel.to_string(),
        version: version.to_string(),
        released_at_ms: now_ms(),
        assets,
        notes_url,
    };
    let signed = SignedRelease {
        cert,
        manifest: secret.sign(CONTEXT_RELEASE_MANIFEST, &serde_json::to_vec(&manifest)?),
    };
    let out = dist.join(format!("{channel}.json"));
    fs::write(&out, serde_json::to_string_pretty(&signed)? + "\n")?;
    fs::write(dist.join("SHA256SUMS"), sums)?;
    println!(
        "已写 {}：{} {}，{} 个平台",
        out.display(),
        channel,
        version,
        names.len()
    );
    Ok(())
}

fn verify(file: &Path, roots: Option<&Path>) -> anyhow::Result<()> {
    let roots = match roots {
        Some(path) => PublicKey::parse_list(&fs::read_to_string(path)?)?,
        None => sign::embedded_root_keys(),
    };
    let verified = release::verify(&fs::read(file)?, &roots, now_ms(), 0)?;
    let m = &verified.manifest;
    println!(
        "验证通过：{} {}，证书序号 {}",
        m.channel, m.version, verified.cert_serial
    );
    for (target, asset) in &m.assets {
        println!("  {target}  {} 字节  {}", asset.size, asset.sha256);
    }
    Ok(())
}

fn read_secret(env: Option<&str>, path: Option<&Path>) -> anyhow::Result<SecretKey> {
    let text = match (env, path) {
        (Some(var), _) => std::env::var(var).with_context(|| format!("环境变量 {var} 没有设置"))?,
        (None, Some(path)) => fs::read_to_string(path)?,
        (None, None) => bail!("要用 --key 或 --key-env 指定签名钥"),
    };
    Ok(SecretKey::from_file_json(&text)?)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
