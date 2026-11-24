//! What a release publishes and how the client checks it
//!
//! `<channel>.json` holds two envelopes: the signing-key certificate (signed by
//! a root key built into the client) and the channel manifest (signed by the
//! signing key the certificate names). Losing the signing key costs a new
//! certificate, not a new client.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::sign::{CONTEXT_RELEASE_CERT, CONTEXT_RELEASE_MANIFEST, Envelope, Error, PublicKey};

pub const MANIFEST_SCHEMA: u32 = 1;

/// Root-signed statement: "this key may sign releases until `not_after_ms`".
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SigningCert {
    pub purpose: String,
    pub key: String,
    /// Higher serials replace lower ones; a client never goes back to a lower one.
    pub serial: u64,
    pub not_before_ms: i64,
    pub not_after_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub schema: u32,
    pub channel: String,
    pub version: String,
    pub released_at_ms: i64,
    /// Keyed by Rust target triple, e.g. `aarch64-apple-darwin`.
    pub assets: BTreeMap<String, Asset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
}

/// The file published as `<channel>.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedRelease {
    pub cert: Envelope,
    pub manifest: Envelope,
}

#[derive(Debug)]
pub struct Verified {
    pub manifest: Manifest,
    pub cert_serial: u64,
}

/// Checks the chain root key → signing-key certificate → manifest.
///
/// `min_cert_serial` is the highest certificate serial this device has already
/// accepted; anything lower is refused, which is how a leaked signing key gets
/// retired.
pub fn verify(bytes: &[u8], roots: &[PublicKey], now_ms: i64, min_cert_serial: u64) -> Result<Verified, Error> {
    if roots.is_empty() {
        return Err(Error::from("这个程序里没有内置的根公钥，没法验证更新"));
    }
    let signed: SignedRelease =
        serde_json::from_slice(bytes).map_err(|e| Error::from(format!("版本说明的格式不对：{e}")))?;

    let cert_bytes = signed.cert.open(CONTEXT_RELEASE_CERT, roots)?;
    let cert: SigningCert =
        serde_json::from_slice(&cert_bytes).map_err(|e| Error::from(format!("签名钥证书的格式不对：{e}")))?;
    if cert.purpose != "release" {
        return Err(Error::from("这张证书不是用来签发版本的"));
    }
    if now_ms < cert.not_before_ms || now_ms > cert.not_after_ms {
        return Err(Error::from("签名钥证书不在有效期内（也可能是这台机器的时间不对）"));
    }
    if cert.serial < min_cert_serial {
        return Err(Error::from(format!(
            "签名钥证书的序号 {} 比这台设备已经认过的 {} 旧，可能是已经作废的钥匙",
            cert.serial, min_cert_serial
        )));
    }
    let signing_key = PublicKey::from_base64(&cert.key)?;

    let manifest_bytes = signed.manifest.open(CONTEXT_RELEASE_MANIFEST, &[signing_key])?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|e| Error::from(format!("版本说明的格式不对：{e}")))?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(Error::from(format!("不认识版本说明的格式版本 {}", manifest.schema)));
    }
    Ok(Verified {
        manifest,
        cert_serial: cert.serial,
    })
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        crate::sign::error(message)
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        crate::sign::error(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::SecretKey;

    const DAY: i64 = 86_400_000;

    fn cert(root: &SecretKey, signer: &SecretKey, serial: u64, not_after_ms: i64) -> Envelope {
        let cert = SigningCert {
            purpose: "release".into(),
            key: signer.public().to_base64(),
            serial,
            not_before_ms: 0,
            not_after_ms,
        };
        root.sign(CONTEXT_RELEASE_CERT, &serde_json::to_vec(&cert).unwrap())
    }

    fn manifest(signer: &SecretKey, version: &str) -> Envelope {
        let manifest = Manifest {
            schema: MANIFEST_SCHEMA,
            channel: "stable".into(),
            version: version.into(),
            released_at_ms: 1,
            assets: BTreeMap::new(),
            notes_url: None,
        };
        signer.sign(CONTEXT_RELEASE_MANIFEST, &serde_json::to_vec(&manifest).unwrap())
    }

    fn file(cert: Envelope, manifest: Envelope) -> Vec<u8> {
        serde_json::to_vec(&SignedRelease { cert, manifest }).unwrap()
    }

    #[test]
    fn accepts_a_valid_chain() {
        let root = SecretKey::generate();
        let signer = SecretKey::generate();
        let bytes = file(cert(&root, &signer, 3, 10 * DAY), manifest(&signer, "0.2.0"));
        let verified = verify(&bytes, &[root.public()], DAY, 3).unwrap();
        assert_eq!(verified.manifest.version, "0.2.0");
        assert_eq!(verified.cert_serial, 3);
    }

    #[test]
    fn refuses_expired_or_retired_certs() {
        let root = SecretKey::generate();
        let signer = SecretKey::generate();
        let bytes = file(cert(&root, &signer, 1, DAY), manifest(&signer, "0.2.0"));
        assert!(verify(&bytes, &[root.public()], 2 * DAY, 0).is_err(), "expired");
        assert!(verify(&bytes, &[root.public()], 0, 2).is_err(), "retired serial");
    }

    #[test]
    fn refuses_a_manifest_signed_by_another_key() {
        let root = SecretKey::generate();
        let signer = SecretKey::generate();
        let attacker = SecretKey::generate();
        let bytes = file(cert(&root, &signer, 1, DAY), manifest(&attacker, "9.9.9"));
        assert!(verify(&bytes, &[root.public()], 0, 0).is_err());
    }

    #[test]
    fn refuses_a_cert_not_signed_by_a_root() {
        let root = SecretKey::generate();
        let fake_root = SecretKey::generate();
        let signer = SecretKey::generate();
        let bytes = file(cert(&fake_root, &signer, 1, DAY), manifest(&signer, "0.2.0"));
        assert!(verify(&bytes, &[root.public()], 0, 0).is_err());
        assert!(verify(&bytes, &[], 0, 0).is_err(), "no roots built in");
    }
}
