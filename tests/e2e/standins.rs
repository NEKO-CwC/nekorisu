//! What the devices talk to, in this process: a WebDAV storage on a temp directory and a
//! trusted source that serves configs signed with a test key. Loopback only, synthetic data.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use dav_server::DavHandler;
use dav_server::fakels::FakeLs;
use dav_server::localfs::LocalFs;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use nekorisu::sign::{CONTEXT_CONFIG, SecretKey, sha256_hex};
use tokio::net::TcpListener;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

pub mod proto {
    #![allow(clippy::all, clippy::pedantic, dead_code)]
    tonic::include_proto!("nekorisu.cloud.v1");
}

use proto::cloud_server::{Cloud, CloudServer};
use proto::get_config_response::Result as ConfigResult;
use proto::{
    GetConfigRequest, GetConfigResponse, GetResourceRequest, NotModified, ReportStateRequest, ReportStateResponse,
    ResourceChunk, SignedConfig,
};

// ---- storage ----

pub struct Dav {
    _dir: tempfile::TempDir,
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for Dav {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Dav({})", self.port)
    }
}

impl Dav {
    pub async fn start() -> Dav {
        let dir = tempfile::tempdir().unwrap();
        let dav = DavHandler::builder()
            .filesystem(LocalFs::new(dir.path(), true, false, false))
            .locksystem(FakeLs::new())
            .build_handler();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let dav = dav.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let dav = dav.clone();
                        async move { Ok::<_, Infallible>(dav.handle(request).await) }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Dav { _dir: dir, port, task }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Dav {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ---- trusted source ----

#[derive(Default)]
struct Data {
    /// payload, key id, signature, etag
    config: Option<(Vec<u8>, String, Vec<u8>, String)>,
    resources: BTreeMap<String, Vec<u8>>,
    reports: Vec<(String, ReportStateRequest)>,
}

#[derive(Clone)]
struct Service {
    data: Arc<Mutex<Data>>,
    token: String,
}

impl Service {
    fn check(&self, meta: &tonic::metadata::MetadataMap) -> Result<(), Status> {
        let given = meta
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if given == Some(self.token.as_str()) {
            Ok(())
        } else {
            Err(Status::unauthenticated("a token is required"))
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
        let chunk = ResourceChunk {
            total_size: bytes.len() as u64,
            data: bytes,
        };
        Ok(Response::new(Box::pin(tokio_stream::iter(vec![Ok(chunk)]))))
    }

    async fn report_state(
        &self,
        request: Request<ReportStateRequest>,
    ) -> Result<Response<ReportStateResponse>, Status> {
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

pub struct Source {
    pub url: String,
    pub token: String,
    key: SecretKey,
    data: Arc<Mutex<Data>>,
    serial: u64,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Source({})", self.url)
    }
}

impl Source {
    pub async fn start(token: &str) -> Source {
        let data = Arc::new(Mutex::new(Data::default()));
        let service = Service {
            data: data.clone(),
            token: token.to_string(),
        };
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(CloudServer::new(service))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await;
        });
        Source {
            url: format!("http://127.0.0.1:{port}"),
            token: token.to_string(),
            key: SecretKey::generate(),
            data,
            serial: 0,
            task,
        }
    }

    pub fn public_key(&self) -> String {
        self.key.public().to_base64()
    }

    /// Signs and serves the next config; `body` holds the fields besides schema and serial.
    pub fn publish(&mut self, mut body: serde_json::Value) -> u64 {
        self.serial += 2;
        body["schema"] = serde_json::json!("1.0");
        body["serial"] = serde_json::json!(self.serial);
        body["issued_at"] = serde_json::json!("2026-11-27T00:00:00Z");
        let payload = serde_json::to_vec(&body).unwrap();
        let envelope = self.key.sign(CONTEXT_CONFIG, &payload);
        let etag = sha256_hex(&payload);
        self.data.lock().unwrap().config =
            Some((payload, envelope.key_id, B64.decode(envelope.signature).unwrap(), etag));
        self.serial
    }

    /// The newest report from the device with this ID.
    pub fn report_from(&self, device_id: &str) -> Option<ReportStateRequest> {
        self.data
            .lock()
            .unwrap()
            .reports
            .iter()
            .rev()
            .find(|(d, _)| d == device_id)
            .map(|(_, r)| r.clone())
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ---- a real server ----

/// A running nekorisu-server for tests, given by environment variables (`tests/e2e/server.sh`
/// starts one): its panel address, its device protocol address, the device token and the
/// administrator password. Only ever a test instance: scenarios replace its path maps.
pub struct RealServer {
    pub http: String,
    pub grpc: String,
    pub token: String,
    pub public_key: String,
    cookie: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for RealServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RealServer({})", self.http)
    }
}

pub fn real_server_configured() -> bool {
    std::env::var_os("NEKORISU_E2E_SERVER").is_some()
}

impl RealServer {
    pub async fn connect() -> anyhow::Result<RealServer> {
        let var = |name: &str| std::env::var(name).map_err(|_| anyhow::anyhow!("{name} is not set"));
        let http = var("NEKORISU_E2E_SERVER")?.trim_end_matches('/').to_string();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder().no_proxy().build()?;
        let login = client
            .post(format!("{http}/api/auth/login"))
            .header("content-type", "application/json")
            .body(serde_json::json!({ "password": var("NEKORISU_E2E_SERVER_PASSWORD")? }).to_string())
            .send()
            .await?;
        anyhow::ensure!(login.status().is_success(), "panel login: {}", login.status());
        let cookie = login
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .ok_or_else(|| anyhow::anyhow!("panel login gave no cookie"))?
            .to_string();
        let mut server = RealServer {
            http,
            grpc: var("NEKORISU_E2E_SERVER_GRPC")?,
            token: var("NEKORISU_E2E_SERVER_TOKEN")?,
            public_key: String::new(),
            cookie,
            client,
        };
        let config = server.get("/api/config").await?;
        server.public_key = config["signing"]["public_key"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no public key on the config page"))?
            .to_string();
        Ok(server)
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let reply = self
            .client
            .get(format!("{}{path}", self.http))
            .header("cookie", &self.cookie)
            .send()
            .await?;
        let status = reply.status();
        let body = reply.bytes().await?;
        anyhow::ensure!(
            status.is_success(),
            "GET {path}: {status} {}",
            String::from_utf8_lossy(&body)
        );
        Ok(serde_json::from_slice(&body)?)
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let reply = self
            .client
            .post(format!("{}{path}", self.http))
            .header("cookie", &self.cookie)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await?;
        let status = reply.status();
        let body = reply.bytes().await?;
        anyhow::ensure!(
            status.is_success(),
            "POST {path}: {status} {}",
            String::from_utf8_lossy(&body)
        );
        Ok(serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
    }

    /// No path maps and no force requests from the panel: every scenario starts the same.
    pub async fn reset(&self) -> anyhow::Result<()> {
        self.post("/api/paths", serde_json::json!({ "items": [] })).await?;
        let paths = self.get("/api/paths").await?;
        for f in paths["force"].as_array().into_iter().flatten() {
            if let Some(id) = f["id"].as_str() {
                self.post(&format!("/api/paths/force/{id}/withdraw"), serde_json::json!({}))
                    .await?;
            }
        }
        Ok(())
    }
}
