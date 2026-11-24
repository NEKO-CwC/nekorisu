//! Ed25519 signatures in a small JSON envelope.
//!
//! The signature covers `context || 0x00 || payload`, where the context names
//! what is being signed. A signature made for one purpose (say, a signing-key
//! certificate) can therefore never be replayed as another (a release
//! manifest or a pushed config).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Root key signs a signing-key certificate.
pub const CONTEXT_RELEASE_CERT: &str = "nekorisu/release-cert/v1";
/// Signing key signs a channel's release manifest.
pub const CONTEXT_RELEASE_MANIFEST: &str = "nekorisu/release-manifest/v1";
/// A trusted source signs the config it pushes.
pub const CONTEXT_CONFIG: &str = "nekorisu/config/v1";

const SECRET_KIND: &str = "nekorisu-secret-key";

#[derive(Debug)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err(message: impl Into<String>) -> Error {
    Error(message.into())
}

pub(crate) fn error(message: impl Into<String>) -> Error {
    err(message)
}

/// The first 8 bytes of SHA-256 over the raw public key, in hex.
pub fn key_id(key: &VerifyingKey) -> String {
    let digest = Sha256::digest(key.as_bytes());
    hex::encode(&digest[..8])
}

#[derive(Clone, Debug)]
pub struct PublicKey {
    pub key_id: String,
    pub key: VerifyingKey,
}

impl PublicKey {
    pub fn from_base64(text: &str) -> Result<Self, Error> {
        let bytes = B64
            .decode(text.trim())
            .map_err(|e| err(format!("公钥不是合法的 base64：{e}")))?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| err("公钥长度不对，应该是 32 字节"))?;
        let key = VerifyingKey::from_bytes(&bytes).map_err(|e| err(format!("公钥无效：{e}")))?;
        Ok(Self {
            key_id: key_id(&key),
            key,
        })
    }

    pub fn to_base64(&self) -> String {
        B64.encode(self.key.as_bytes())
    }

    /// One key per line: base64, optionally followed by `# comment`. Blank lines
    /// and lines starting with `#` are skipped.
    pub fn parse_list(text: &str) -> Result<Vec<Self>, Error> {
        text.lines()
            .map(|line| line.split('#').next().unwrap_or("").trim())
            .filter(|line| !line.is_empty())
            .map(Self::from_base64)
            .collect()
    }
}

/// The root public keys built into this binary (keys/root.pub, or the file
/// `NEKORISU_ROOT_PUB` named at build time). Empty until
/// the maintainers generate them; with no keys, self-update stays off.
pub fn embedded_root_keys() -> Vec<PublicKey> {
    PublicKey::parse_list(include_str!(env!("NEKORISU_ROOT_PUB_FILE"))).unwrap_or_default()
}

/// A secret key as stored on disk: in a password manager (root key) or in a
/// CI secret (signing key). Never written anywhere else.
#[derive(Serialize, Deserialize)]
pub struct SecretKeyFile {
    pub kind: String,
    pub key_id: String,
    pub secret: String,
}

pub struct SecretKey {
    key: SigningKey,
}

impl SecretKey {
    pub fn generate() -> Self {
        Self {
            key: SigningKey::generate(&mut rand_core::OsRng),
        }
    }

    pub fn public(&self) -> PublicKey {
        let key = self.key.verifying_key();
        PublicKey {
            key_id: key_id(&key),
            key,
        }
    }

    pub fn to_file_json(&self) -> String {
        let file = SecretKeyFile {
            kind: SECRET_KIND.to_string(),
            key_id: self.public().key_id,
            secret: B64.encode(self.key.to_bytes()),
        };
        serde_json::to_string_pretty(&file).expect("serializable")
    }

    pub fn from_file_json(text: &str) -> Result<Self, Error> {
        let file: SecretKeyFile =
            serde_json::from_str(text.trim()).map_err(|e| err(format!("私钥文件格式不对：{e}")))?;
        if file.kind != SECRET_KIND {
            return Err(err("这不是 nekorisu 的私钥文件"));
        }
        let bytes = B64
            .decode(&file.secret)
            .map_err(|e| err(format!("私钥不是合法的 base64：{e}")))?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| err("私钥长度不对"))?;
        let key = Self {
            key: SigningKey::from_bytes(&bytes),
        };
        if key.public().key_id != file.key_id {
            return Err(err("私钥文件里的 key_id 和私钥对不上"));
        }
        Ok(key)
    }

    pub fn sign(&self, context: &str, payload: &[u8]) -> Envelope {
        let signature = self.key.sign(&signed_bytes(context, payload));
        Envelope {
            payload: B64.encode(payload),
            key_id: self.public().key_id,
            signature: B64.encode(signature.to_bytes()),
        }
    }
}

/// A signed payload. The payload is kept as the exact bytes that were signed,
/// so no JSON canonicalization is ever needed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Envelope {
    pub payload: String,
    pub key_id: String,
    pub signature: String,
}

impl Envelope {
    /// Checks the signature with whichever of `keys` has the envelope's key ID
    /// and returns the payload bytes.
    pub fn open(&self, context: &str, keys: &[PublicKey]) -> Result<Vec<u8>, Error> {
        let key = keys
            .iter()
            .find(|k| k.key_id == self.key_id)
            .ok_or_else(|| err(format!("签名用的钥匙 {} 不在信任的公钥里", self.key_id)))?;
        let payload = B64
            .decode(&self.payload)
            .map_err(|e| err(format!("内容不是合法的 base64：{e}")))?;
        let signature = B64
            .decode(&self.signature)
            .map_err(|e| err(format!("签名不是合法的 base64：{e}")))?;
        let signature = Signature::from_slice(&signature).map_err(|e| err(format!("签名格式不对：{e}")))?;
        key.key
            .verify_strict(&signed_bytes(context, &payload), &signature)
            .map_err(|_| err("签名验证不通过"))?;
        Ok(payload)
    }
}

fn signed_bytes(context: &str, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(context.len() + 1 + payload.len());
    bytes.extend_from_slice(context.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(payload);
    bytes
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = SecretKey::generate();
        let envelope = key.sign(CONTEXT_CONFIG, b"hello");
        assert_eq!(envelope.open(CONTEXT_CONFIG, &[key.public()]).unwrap(), b"hello");
    }

    #[test]
    fn context_separates_purposes() {
        let key = SecretKey::generate();
        let envelope = key.sign(CONTEXT_RELEASE_CERT, b"payload");
        assert!(envelope.open(CONTEXT_RELEASE_MANIFEST, &[key.public()]).is_err());
    }

    #[test]
    fn rejects_tampering_and_unknown_keys() {
        let key = SecretKey::generate();
        let other = SecretKey::generate();
        let mut envelope = key.sign(CONTEXT_CONFIG, b"payload");
        assert!(envelope.open(CONTEXT_CONFIG, &[other.public()]).is_err());
        envelope.payload = B64.encode(b"payloaD");
        assert!(envelope.open(CONTEXT_CONFIG, &[key.public()]).is_err());
    }

    #[test]
    fn secret_file_round_trip() {
        let key = SecretKey::generate();
        let back = SecretKey::from_file_json(&key.to_file_json()).unwrap();
        assert_eq!(back.public().key_id, key.public().key_id);
    }

    #[test]
    fn parses_key_list_with_comments() {
        let a = SecretKey::generate().public();
        let b = SecretKey::generate().public();
        let text = format!(
            "# root keys\n{}  # primary\n\n{} # backup\n",
            a.to_base64(),
            b.to_base64()
        );
        let keys = PublicKey::parse_list(&text).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[1].key_id, b.key_id);
    }
}
