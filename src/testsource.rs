//! A stand-in trusted source for tests: serves configs signed with a test key over plain HTTP/2
//! on a loopback port, and keeps what devices report. Never part of a release build.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use nekorisu::sign::{CONTEXT_CONFIG, SecretKey, sha256_hex};
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::cloud::Source;
use crate::proto::cloud::cloud_server::{Cloud, CloudServer};
use crate::proto::cloud::get_config_response::Result as ConfigResult;
use crate::proto::cloud::{
    GetConfigRequest, GetConfigResponse, GetResourceRequest, NotModified, ReportStateRequest, ReportStateResponse,
    ResourceChunk, SignedConfig,
};

#[derive(Default)]
struct Data {
    /// payload, key id, signature, etag
    config: Option<(Vec<u8>, String, Vec<u8>, String)>,
    resources: HashMap<String, Vec<u8>>,
    reports: Vec<(String, ReportStateRequest)>,
}

#[derive(Clone)]
struct Service {
    data: Arc<Mutex<Data>>,
    token: Option<String>,
}

impl Service {
    fn check(&self, request: &tonic::metadata::MetadataMap) -> Result<(), Status> {
        let given = request
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match (&self.token, given) {
            (None, _) => Ok(()),
            (Some(want), Some(given)) if want == given => Ok(()),
            _ => Err(Status::unauthenticated("a token is required")),
        }
    }
}

type Chunks = Pin<Box<dyn Stream<Item = Result<ResourceChunk, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl Cloud for Service {
    async fn get_config(&self, request: Request<GetConfigRequest>) -> Result<Response<GetConfigResponse>, Status> {
        self.check(request.metadata())?;
        let data = self.data.lock().unwrap();
        let (payload, key_id, signature, etag) = data
            .config
            .clone()
            .ok_or_else(|| Status::not_found("no config has been published yet"))?;
        let result = if request.get_ref().etag == etag {
            ConfigResult::NotModified(NotModified {})
        } else {
            ConfigResult::Config(SignedConfig {
                payload,
                key_id,
                signature,
            })
        };
        Ok(Response::new(GetConfigResponse {
            result: Some(result),
            etag,
        }))
    }

    type GetResourceStream = Chunks;

    async fn get_resource(&self, request: Request<GetResourceRequest>) -> Result<Response<Chunks>, Status> {
        self.check(request.metadata())?;
        let bytes = self
            .data
            .lock()
            .unwrap()
            .resources
            .get(&request.get_ref().sha256)
            .cloned()
            .ok_or_else(|| Status::not_found("no such resource"))?;
        let total = bytes.len() as u64;
        // Two chunks, to exercise putting them together.
        let half = bytes.len() / 2;
        let chunks = vec![
            Ok(ResourceChunk {
                total_size: total,
                data: bytes[..half].to_vec(),
            }),
            Ok(ResourceChunk {
                total_size: 0,
                data: bytes[half..].to_vec(),
            }),
        ];
        Ok(Response::new(Box::pin(tokio_stream::iter(chunks))))
    }

    async fn report_state(
        &self,
        request: Request<ReportStateRequest>,
    ) -> Result<Response<ReportStateResponse>, Status> {
        if self.token.is_none() {
            return Err(Status::unauthenticated("reporting needs a token"));
        }
        self.check(request.metadata())?;
        let device = request
            .metadata()
            .get("nekorisu-device")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        self.data.lock().unwrap().reports.push((device, request.into_inner()));
        Ok(Response::new(ReportStateResponse {}))
    }
}

pub struct TestSource {
    pub url: String,
    pub key: SecretKey,
    data: Arc<Mutex<Data>>,
}

impl TestSource {
    /// `token`: what devices must present; `None` takes anyone (and refuses reports).
    pub async fn start(token: Option<&str>) -> TestSource {
        let data = Arc::new(Mutex::new(Data::default()));
        let service = Service {
            data: data.clone(),
            token: token.map(str::to_owned),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(CloudServer::new(service))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        TestSource {
            url: format!("http://127.0.0.1:{}", addr.port()),
            key: SecretKey::generate(),
            data,
        }
    }

    pub fn source(&self, name: &str) -> Source {
        Source {
            name: name.to_string(),
            url: self.url.clone(),
            keys: vec![self.key.public().to_base64()],
        }
    }

    /// Serves `doc` signed with this source's key.
    pub fn publish(&self, doc: &serde_json::Value) {
        self.publish_signed_by(doc, &self.key);
    }

    pub fn publish_signed_by(&self, doc: &serde_json::Value, key: &SecretKey) {
        let payload = serde_json::to_vec(doc).unwrap();
        let envelope = key.sign(CONTEXT_CONFIG, &payload);
        let etag = sha256_hex(&payload);
        self.data.lock().unwrap().config =
            Some((payload, envelope.key_id, B64.decode(envelope.signature).unwrap(), etag));
    }

    /// Serves `bytes` under the SHA-256 of `claimed` (normally the same bytes).
    pub fn add_resource(&self, claimed: &[u8], bytes: &[u8]) -> String {
        let sha = sha256_hex(claimed);
        self.data.lock().unwrap().resources.insert(sha.clone(), bytes.to_vec());
        sha
    }

    pub fn reports(&self) -> Vec<(String, ReportStateRequest)> {
        self.data.lock().unwrap().reports.clone()
    }
}

/// A config document as a source would send it.
pub fn config(serial: u64, paths: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schema": "1.0",
        "serial": serial,
        "issued_at": "2026-11-27T00:00:00Z",
        "min_client_version": "",
        "features": [],
        "poll_interval_secs": 0,
        "update": { "channel": "", "urls": [] },
        "digest": {},
        "local": { "copy_form": "" },
        "force": [],
        "requests": [],
        "paths": { "map": paths },
    })
}
