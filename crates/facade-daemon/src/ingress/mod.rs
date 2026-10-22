//! A node's public HTTP(S) entry point. Routes by host name, straight to the
//! apps running on the node; WebSocket (and any HTTP/1.1 upgrade) is spliced
//! through.
//!
//! It also takes TLS off raw TCP ports for apps that ask (see [`tcp`]).
//!
//! What to serve comes as a [`Table`] from the node's agent: routes, TLS ports,
//! certificates and ACME HTTP-01 answers. The table is kept on disk, so apps stay reachable
//! while the agent (or the server) is away or restarting.
//!
//! The entry point runs either inside the agent (`embedded`, handy in
//! development) or as its own process, `facaded ingress` (see [`process`]): then
//! updating facade's control plane or agent never touches traffic, and the
//! entry point updates itself by handing its listening sockets to the new
//! binary while the old one finishes its open connections.

pub mod process;
pub mod tcp;

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use facade_core::proto::CertBundle;
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri, Version};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::{Mutex, RwLock};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower::ServiceExt;
use tracing::{debug, info, warn};

use crate::tls;

/// Everything the entry point needs to know, replaced as a whole by the agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Table {
    /// Host name → where it goes.
    pub routes: BTreeMap<String, Route>,
    pub certs: Vec<CertBundle>,
    /// Public TCP port → the app TLS-wrapped connections on it go to.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tcp: BTreeMap<u16, tcp::TcpRoute>,
    /// ACME HTTP-01 token → key authorization.
    pub challenges: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Route {
    /// An app (or facade's own API) listening on this local port.
    Port { app: String, port: u16 },
    /// Files of a static site.
    Static { app: String, root: PathBuf },
    /// A permanent redirect to another host, path and query kept.
    Redirect { to: String },
    /// An app that cannot be reached right now (not started, or failing).
    Unavailable { app: String },
}

/// Certificates by name; `*.example.com` covers one level of subdomains.
#[derive(Debug)]
pub struct Certs {
    by_name: RwLock<HashMap<String, Arc<CertifiedKey>>>,
    fallback: Arc<CertifiedKey>,
}

impl Certs {
    pub fn new() -> anyhow::Result<Self> {
        let generated = rcgen::generate_simple_self_signed(vec!["facade".to_string()])?;
        let fallback = tls::certified_key(
            generated.cert.pem().as_bytes(),
            generated.signing_key.serialize_pem().as_bytes(),
        )?;
        Ok(Self {
            by_name: Default::default(),
            fallback,
        })
    }

    fn install(&self, bundles: &[CertBundle]) {
        let mut by_name = HashMap::new();
        for bundle in bundles {
            match tls::certified_key(bundle.cert_pem.as_bytes(), bundle.key_pem.as_bytes()) {
                Ok(key) => {
                    for name in &bundle.names {
                        by_name.insert(name.to_ascii_lowercase(), key.clone());
                    }
                }
                Err(e) => warn!(names = ?bundle.names, "bad certificate: {e:#}"),
            }
        }
        *self.by_name.write() = by_name;
    }

    pub fn find(&self, host: &str) -> Option<Arc<CertifiedKey>> {
        let by_name = self.by_name.read();
        by_name.get(host).cloned().or_else(|| {
            let (_, parent) = host.split_once('.')?;
            by_name.get(&format!("*.{parent}")).cloned()
        })
    }
}

impl ResolvesServerCert for Certs {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        hello
            .server_name()
            .and_then(|name| self.find(&name.to_ascii_lowercase()))
            .or_else(|| Some(self.fallback.clone()))
    }
}

/// The entry point's state, shared by its listeners.
pub struct Ingress {
    table: RwLock<Table>,
    certs: Arc<Certs>,
    client: Client<HttpConnector, Incoming>,
    /// Where the table is kept between runs.
    file: PathBuf,
    /// Addresses that cannot be bound yet, with why.
    bind_errors: Mutex<BTreeMap<SocketAddr, String>>,
    /// Connections being served (WebSockets included), so a retiring process
    /// knows when it is done.
    active: Arc<AtomicUsize>,
    /// Told whenever `bind_errors` changes.
    pub status: watch::Sender<()>,
    /// The listener of each TLS port, by the sender that stops it.
    tcp: Mutex<HashMap<u16, watch::Sender<bool>>>,
    /// Set once a newer entry point took over: no TLS port starts any more.
    retired: AtomicBool,
}

impl Ingress {
    /// Loads the last table saved in `dir`.
    pub fn new(dir: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        tls::create_private_dir(dir)?;
        let file = dir.join("ingress-table.json");
        let table: Table = std::fs::read(&file)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        let certs = Arc::new(Certs::new()?);
        certs.install(&table.certs);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(60))
            .build(HttpConnector::new());
        Ok(Arc::new(Self {
            table: RwLock::new(table),
            certs,
            client,
            file,
            bind_errors: Default::default(),
            active: Default::default(),
            status: watch::channel(()).0,
            tcp: Default::default(),
            retired: AtomicBool::new(false),
        }))
    }

    /// Replaces the table (and saves it) if it changed.
    pub fn set_table(self: &Arc<Self>, table: Table) {
        {
            let current = self.table.read();
            if *current == table {
                return;
            }
            if current.certs != table.certs {
                self.certs.install(&table.certs);
            }
        }
        match serde_json::to_vec(&table) {
            Ok(json) => {
                if let Err(e) = tls::write_private(&self.file, &json) {
                    warn!("saving the routing table: {e}");
                }
            }
            Err(e) => warn!("saving the routing table: {e}"),
        }
        *self.table.write() = table;
        self.sync_tcp();
    }

    pub fn bind_errors(&self) -> Vec<String> {
        self.bind_errors.lock().values().cloned().collect()
    }

    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    fn set_bind_error(&self, addr: SocketAddr, error: Option<String>) {
        let changed = {
            let mut errors = self.bind_errors.lock();
            match error {
                Some(e) => errors.insert(addr, e.clone()).as_ref() != Some(&e),
                None => errors.remove(&addr).is_some(),
            }
        };
        if changed {
            self.status.send_replace(());
        }
    }

    /// Binds `addr`, retrying while another program holds it.
    pub async fn bind(self: &Arc<Self>, addr: SocketAddr, tls: bool) -> TcpListener {
        let scheme = if tls { "https" } else { "http" };
        let mut warned = false;
        loop {
            match TcpListener::bind(addr).await {
                Ok(listener) => {
                    self.set_bind_error(addr, None);
                    return listener;
                }
                Err(e) => {
                    let message = format!("cannot listen on {addr} ({scheme}): {e}");
                    if !warned {
                        warn!(
                            "{message}; apps are not reachable over {scheme} until it is free, retrying"
                        );
                        warned = true;
                    }
                    self.set_bind_error(addr, Some(message));
                    tokio::time::sleep(BIND_RETRY).await;
                }
            }
        }
    }

    /// Binds (with retries) and serves `addr` until the process ends.
    pub async fn listen(self: Arc<Self>, addr: SocketAddr, tls: bool) {
        let listener = self.bind(addr, tls).await;
        let (_keep, stop) = watch::channel(false);
        self.serve(listener, tls, stop).await;
    }

    /// Accepts connections until `stop` turns true; connections already accepted
    /// carry on.
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        tls: bool,
        mut stop: watch::Receiver<bool>,
    ) {
        let addr = listener.local_addr().ok();
        info!(
            "serving {}://{}",
            if tls { "https" } else { "http" },
            addr.map(|a| a.to_string()).unwrap_or_default()
        );
        let acceptor = tls.then(|| {
            let mut config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(self.certs.clone());
            config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            tokio_rustls::TlsAcceptor::from(Arc::new(config))
        });
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = stopped(&mut stop) => return,
            };
            let (tcp, peer) = match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let ingress = self.clone();
            let acceptor = acceptor.clone();
            let counted = Counted::new(&ingress.active);
            tokio::spawn(async move {
                let _counted = counted;
                match acceptor {
                    None => ingress.connection(TokioIo::new(tcp), peer, false).await,
                    Some(acceptor) => {
                        match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp))
                            .await
                        {
                            Ok(Ok(stream)) => {
                                ingress.connection(TokioIo::new(stream), peer, true).await
                            }
                            _ => debug!(%peer, "TLS handshake failed"),
                        }
                    }
                }
            });
        }
    }

    async fn connection<I>(self: Arc<Self>, io: I, peer: SocketAddr, tls: bool)
    where
        I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let service = hyper::service::service_fn(move |req| {
            let ingress = self.clone();
            async move { Ok::<_, Infallible>(ingress.handle(req, peer, tls).await) }
        });
        let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
            .serve_connection_with_upgrades(io, service)
            .await;
    }

    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr, tls: bool) -> Response<Body> {
        let host = request_host(&req).unwrap_or_default();
        let path = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "/".into());

        if !tls
            && let Some(token) = req
                .uri()
                .path()
                .strip_prefix("/.well-known/acme-challenge/")
        {
            return match self.table.read().challenges.get(token) {
                Some(key_auth) => text(StatusCode::OK, key_auth.clone()),
                None => text(StatusCode::NOT_FOUND, "unknown challenge"),
            };
        }
        let route = self.table.read().routes.get(&host).cloned();
        if let Some(Route::Redirect { to }) = &route {
            let scheme = if tls || self.certs.find(to).is_some() {
                "https"
            } else {
                "http"
            };
            return redirect(format!("{scheme}://{to}{path}"));
        }
        if !tls && self.certs.find(&host).is_some() {
            return redirect(format!("https://{host}{path}"));
        }
        let result = match route {
            None => {
                return text(
                    StatusCode::NOT_FOUND,
                    format!("no app is served at {host}\n"),
                );
            }
            Some(Route::Redirect { .. }) => unreachable!("answered above"),
            Some(Route::Unavailable { app }) => Err((app, anyhow::anyhow!("not running"))),
            Some(Route::Static { app, root }) => {
                serve_static(req, root).await.map_err(|e| (app, e))
            }
            Some(Route::Port { app, port }) => self
                .proxy(req, port, &host, peer, tls)
                .await
                .map_err(|e| (app, e)),
        };
        result.unwrap_or_else(|(app, e)| {
            debug!(%app, "proxy error: {e:#}");
            text(
                StatusCode::BAD_GATEWAY,
                format!("{app} is unavailable right now\n"),
            )
        })
    }

    async fn proxy(
        &self,
        mut req: Request<Incoming>,
        port: u16,
        host: &str,
        peer: SocketAddr,
        tls: bool,
    ) -> anyhow::Result<Response<Body>> {
        let upgrade = is_upgrade(req.headers());
        let downstream_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));

        let path = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "/".into());
        *req.uri_mut() = format!("http://127.0.0.1:{port}{path}").parse::<Uri>()?;
        // HTTP/2 from the client still goes to the app as HTTP/1.1.
        *req.version_mut() = Version::HTTP_11;

        let headers = req.headers_mut();
        if !upgrade {
            strip_hop_by_hop(headers);
        }
        headers.insert(header::HOST, HeaderValue::from_str(host)?);
        let client_ip = peer.ip().to_string();
        let forwarded_for = match headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            Some(prior) => format!("{prior}, {client_ip}"),
            None => client_ip.clone(),
        };
        headers.insert("x-forwarded-for", HeaderValue::from_str(&forwarded_for)?);
        headers.insert("x-real-ip", HeaderValue::from_str(&client_ip)?);
        headers.insert(
            "x-forwarded-proto",
            HeaderValue::from_static(if tls { "https" } else { "http" }),
        );
        headers.insert("x-forwarded-host", HeaderValue::from_str(host)?);

        let mut resp = self.client.request(req).await?;

        if resp.status() == StatusCode::SWITCHING_PROTOCOLS
            && let Some(downstream) = downstream_upgrade
        {
            let upstream = hyper::upgrade::on(&mut resp);
            // The HTTP connection ends here; the spliced one counts on its own.
            let counted = Counted::new(&self.active);
            tokio::spawn(async move {
                let _counted = counted;
                match tokio::try_join!(downstream, upstream) {
                    Ok((down, up)) => {
                        let mut down = TokioIo::new(down);
                        let mut up = TokioIo::new(up);
                        let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
                    }
                    Err(e) => debug!("upgrade failed: {e}"),
                }
            });
        } else {
            strip_hop_by_hop(resp.headers_mut());
        }
        Ok(resp.map(Body::new))
    }
}

const BIND_RETRY: Duration = Duration::from_secs(5);

/// Resolves once `stop` turns true (or its sender is gone).
pub async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

/// Counts itself in `counter` while alive.
struct Counted(Arc<AtomicUsize>);

impl Counted {
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter.clone())
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn serve_static(req: Request<Incoming>, root: PathBuf) -> anyhow::Result<Response<Body>> {
    use tower_http::services::{ServeDir, ServeFile};
    use tower_http::set_status::SetStatus;
    let not_found = SetStatus::new(ServeFile::new(root.join("404.html")), StatusCode::NOT_FOUND);
    let service = ServeDir::new(&root)
        .append_index_html_on_directories(true)
        .precompressed_gzip()
        .precompressed_br()
        .not_found_service(not_found);
    let response = service.oneshot(req).await?;
    Ok(response.map(Body::new))
}

fn redirect(location: String) -> Response<Body> {
    Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .expect("valid response")
}

fn text(status: StatusCode, body: impl Into<String>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body.into()))
        .expect("valid response")
}

/// Lowercased host without port, from the Host header or the HTTP/2 authority.
fn request_host<B>(req: &Request<B>) -> Option<String> {
    let raw = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()))?;
    let host = match raw.strip_prefix('[') {
        Some(v6) => format!("[{}]", v6.split(']').next().unwrap_or_default()),
        None => raw.split(':').next().unwrap_or_default().to_string(),
    };
    Some(host.to_ascii_lowercase())
}

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
        && headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
            })
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
    headers.remove("proxy-connection");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_of(h: &str) -> Option<String> {
        request_host(&Request::builder().header("host", h).body(()).unwrap())
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host_of("App.Example.com:8443").as_deref(),
            Some("app.example.com")
        );
        assert_eq!(host_of("example.com").as_deref(), Some("example.com"));
        assert_eq!(host_of("[::1]:80").as_deref(), Some("[::1]"));
    }

    #[test]
    fn wildcards_cover_one_level_and_tables_persist() {
        let dir = std::env::temp_dir().join(format!("facade-ingress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ingress = Ingress::new(&dir).unwrap();
        let generated = rcgen::generate_simple_self_signed(vec!["*.example.com".into()]).unwrap();
        let mut table = Table::default();
        table.certs.push(CertBundle {
            names: vec!["example.com".into(), "*.example.com".into()],
            cert_pem: generated.cert.pem(),
            key_pem: generated.signing_key.serialize_pem(),
        });
        table.routes.insert(
            "app.example.com".into(),
            Route::Port {
                app: "app".into(),
                port: 8080,
            },
        );
        ingress.set_table(table.clone());
        assert!(ingress.certs.find("example.com").is_some());
        assert!(ingress.certs.find("app.example.com").is_some());
        assert!(ingress.certs.find("a.b.example.com").is_none());
        assert!(ingress.certs.find("other.com").is_none());
        // Reloaded from disk after a restart.
        let again = Ingress::new(&dir).unwrap();
        assert_eq!(*again.table.read(), table);
        assert!(again.certs.find("app.example.com").is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
