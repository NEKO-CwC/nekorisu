//! An in-process WebDAV server for the storage and archive tests: loopback only,
//! files on a temp directory, synthetic data. It can require Basic auth, serve
//! under a path prefix, answer the next few requests with an error, and it
//! remembers what it was asked.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use dav_server::DavHandler;
use dav_server::body::Body;
use dav_server::fakels::FakeLs;
use dav_server::localfs::LocalFs;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::storage::Spec;

#[derive(Default)]
struct State {
    fail_left: AtomicU32,
    fail_status: AtomicU16,
    hosts: Mutex<Vec<String>>,
    requests: Mutex<Vec<(String, String)>>,
}

pub struct Server {
    dir: tempfile::TempDir,
    port: u16,
    prefix: String,
    auth: Option<(String, String)>,
    state: Arc<State>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Serves at the root of its URL.
    pub async fn start(auth: Option<(&str, &str)>) -> Server {
        Self::start_at("", auth).await
    }

    /// Serves under `prefix` (e.g. "/dav"), like a storage whose URL has a path.
    pub async fn start_at(prefix: &str, auth: Option<(&str, &str)>) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = DavHandler::builder()
            .filesystem(LocalFs::new(dir.path(), true, false, false))
            .locksystem(FakeLs::new());
        if !prefix.is_empty() {
            builder = builder.strip_prefix(prefix);
        }
        let dav = builder.build_handler();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(State::default());
        let auth_owned = auth.map(|(u, p)| (u.to_string(), p.to_string()));
        let task = tokio::spawn({
            let (state, auth) = (state.clone(), auth_owned.clone());
            let served = Arc::new((dir.path().to_path_buf(), prefix.to_string()));
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (dav, state, auth, served) = (dav.clone(), state.clone(), auth.clone(), served.clone());
                    tokio::spawn(async move {
                        let service = service_fn(move |request| {
                            let (dav, state, auth, served) = (dav.clone(), state.clone(), auth.clone(), served.clone());
                            async move { Ok::<_, Infallible>(respond(&dav, &state, auth.as_ref(), &served, request).await) }
                        });
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        });
        Server {
            dir,
            port,
            prefix: prefix.to_string(),
            auth: auth_owned,
            state,
            task,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The directory the files are really in.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// A connection spec for this server, pinned to loopback (so tests never depend on
    /// proxy variables) and with the credentials the server wants.
    pub fn spec(&self) -> Spec {
        Spec {
            url: format!("http://127.0.0.1:{}{}", self.port, self.prefix),
            username: self.auth.as_ref().map(|(u, _)| u.clone()),
            password: self.auth.as_ref().map(|(_, p)| p.clone()),
            resolve: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        }
    }

    /// Answer the next `count` requests with `status` and no work done.
    pub fn fail_next(&self, count: u32, status: u16) {
        self.state.fail_status.store(status, Ordering::SeqCst);
        self.state.fail_left.store(count, Ordering::SeqCst);
    }

    /// The Host header of every request so far.
    pub fn seen_hosts(&self) -> Vec<String> {
        self.state.hosts.lock().unwrap().clone()
    }

    /// (method, decoded path inside the served root) of every request that got past authentication.
    pub fn requests(&self) -> Vec<(String, String)> {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|(method, path)| {
                let path = crate::storage::decode(path);
                let inside = path.strip_prefix(&self.prefix).unwrap_or(&path);
                (method.clone(), inside.trim_start_matches('/').to_string())
            })
            .collect()
    }

    /// Paths written with PUT so far, in order.
    pub fn puts(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter(|(method, _)| method == "PUT")
            .map(|(_, path)| path)
            .collect()
    }

    pub fn forget_requests(&self) {
        self.state.requests.lock().unwrap().clear();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn respond(
    dav: &DavHandler,
    state: &State,
    auth: Option<&(String, String)>,
    (root, prefix): &(PathBuf, String),
    request: Request<hyper::body::Incoming>,
) -> Response<Body> {
    if let Some(host) = request.headers().get("host").and_then(|h| h.to_str().ok()) {
        state.hosts.lock().unwrap().push(host.to_string());
    }
    if let Some((user, pass)) = auth {
        use base64::Engine;
        let want = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
        );
        let got = request.headers().get("authorization").and_then(|h| h.to_str().ok());
        if got != Some(want.as_str()) {
            return Response::builder()
                .status(401)
                .header("WWW-Authenticate", "Basic realm=\"test\"")
                .body(Body::empty())
                .unwrap();
        }
    }
    let left = state.fail_left.load(Ordering::SeqCst);
    if left > 0 {
        state.fail_left.store(left - 1, Ordering::SeqCst);
        let status = state.fail_status.load(Ordering::SeqCst);
        return Response::builder().status(status).body(Body::empty()).unwrap();
    }
    state
        .requests
        .lock()
        .unwrap()
        .push((request.method().to_string(), request.uri().path().to_string()));
    // MKCOL on a directory that exists: real servers (and dav-server elsewhere) answer 405, but on
    // Windows dav-server's LocalFs reads the OS error code as a Unix one and answers 500.
    if request.method().as_str() == "MKCOL" {
        let path = crate::storage::decode(request.uri().path());
        let inside = path.strip_prefix(prefix.as_str()).unwrap_or(&path).trim_matches('/');
        if root.join(inside).is_dir() {
            return Response::builder().status(405).body(Body::empty()).unwrap();
        }
    }
    dav.handle(request).await
}

/// Not a test: serves WebDAV on loopback (under `/dav`, user `me` when a password is given) so a
/// hand-run smoke test can point the real `nekorisu` binary at it. Writes `<port> <directory>`
/// to the file named by NEKORISU_TEST_DAV_INFO and serves for NEKORISU_TEST_DAV_SECS seconds:
///
/// `NEKORISU_TEST_DAV_INFO=/tmp/info cargo test --bin nekorisu serve_for_manual_smoke -- --ignored`
#[tokio::test]
#[ignore = "a hand-run helper, not a test"]
async fn serve_for_manual_smoke() {
    let password = std::env::var("NEKORISU_TEST_DAV_PASSWORD").ok();
    let server = Server::start_at("/dav", password.as_deref().map(|p| ("me", p))).await;
    let info = std::env::var("NEKORISU_TEST_DAV_INFO").expect("NEKORISU_TEST_DAV_INFO");
    std::fs::write(info, format!("{} {}", server.port(), server.root().display())).unwrap();
    let secs = std::env::var("NEKORISU_TEST_DAV_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
}
