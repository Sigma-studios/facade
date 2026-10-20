//! The node's public HTTP(S) entry point. Routes by host name to the apps running
//! on this node, straight to their local port; WebSocket (and any HTTP/1.1
//! upgrade) is spliced through.
//!
//! Certificates and ACME HTTP-01 answers come from the server. They are kept on
//! disk too, so apps stay reachable over HTTPS while the server is away.
//!
//! An address that is taken (another web server on 80/443) is retried every few
//! seconds and reported to the server, so facade takes over as soon as the other
//! program stops.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use facade_core::proto::{CertBundle, ProxyRoute, Workload};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri, Version};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::RwLock;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::net::TcpListener;
use tower::ServiceExt;
use tracing::{debug, info, warn};

use super::Agent;
use crate::tls;

/// Where the entry point listens. `None` disables a listener.
#[derive(Debug, Clone, Copy)]
pub struct IngressOptions {
    pub http: Option<SocketAddr>,
    pub https: Option<SocketAddr>,
}

/// Certificates by name; `*.example.com` covers one level of subdomains.
#[derive(Debug)]
pub struct Certs {
    by_name: RwLock<HashMap<String, Arc<CertifiedKey>>>,
    fallback: Arc<CertifiedKey>,
    file: PathBuf,
}

impl Certs {
    pub fn load(dir: &std::path::Path) -> anyhow::Result<Self> {
        let generated = rcgen::generate_simple_self_signed(vec!["facade".to_string()])?;
        let fallback = tls::certified_key(
            generated.cert.pem().as_bytes(),
            generated.signing_key.serialize_pem().as_bytes(),
        )?;
        let certs = Self {
            by_name: Default::default(),
            fallback,
            file: dir.join("certificates.json"),
        };
        if let Ok(raw) = std::fs::read(&certs.file)
            && let Ok(bundles) = serde_json::from_slice::<Vec<CertBundle>>(&raw)
        {
            certs.install(&bundles);
        }
        Ok(certs)
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
                Err(e) => warn!(names = ?bundle.names, "bad certificate from server: {e:#}"),
            }
        }
        *self.by_name.write() = by_name;
    }

    /// Replaces every certificate, and remembers them for the next start.
    pub fn replace(&self, bundles: &[CertBundle]) {
        self.install(bundles);
        match serde_json::to_vec(bundles) {
            Ok(json) => {
                if let Err(e) = tls::write_private(&self.file, &json) {
                    warn!("saving certificates: {e}");
                }
            }
            Err(e) => warn!("saving certificates: {e}"),
        }
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

/// What the server pushes besides workloads.
#[derive(Default)]
pub struct Routing {
    pub challenges: RwLock<HashMap<String, String>>,
    pub proxies: RwLock<Vec<ProxyRoute>>,
}

pub async fn run(agent: Arc<Agent>, options: IngressOptions) {
    let client: Client<HttpConnector, Incoming> = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .build(HttpConnector::new());
    let ingress = Arc::new(Ingress { agent, client });
    for (addr, tls) in [(options.http, false), (options.https, true)] {
        if let Some(addr) = addr {
            tokio::spawn(ingress.clone().listen(addr, tls));
        }
    }
}

const BIND_RETRY: Duration = Duration::from_secs(5);

struct Ingress {
    agent: Arc<Agent>,
    client: Client<HttpConnector, Incoming>,
}

enum Target {
    Port(u16),
    Static(PathBuf),
}

impl Ingress {
    /// Binds `addr`, retrying while another program holds it.
    async fn listen(self: Arc<Self>, addr: SocketAddr, tls: bool) {
        let scheme = if tls { "https" } else { "http" };
        let mut warned = false;
        let listener = loop {
            match TcpListener::bind(addr).await {
                Ok(listener) => break listener,
                Err(e) => {
                    let message = format!("cannot listen on {addr} ({scheme}): {e}");
                    if !warned {
                        warn!(
                            "{message}; apps are not reachable over {scheme} until it is free, retrying"
                        );
                        warned = true;
                    }
                    self.agent.ingress_errors.lock().insert(addr, message);
                    tokio::time::sleep(BIND_RETRY).await;
                }
            }
        };
        self.agent.ingress_errors.lock().remove(&addr);
        info!("serving {scheme}://{addr}");
        self.accept(listener, tls).await;
    }

    async fn accept(self: Arc<Self>, listener: TcpListener, tls: bool) {
        let acceptor = tls.then(|| {
            let mut config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(self.agent.certs.clone());
            config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            tokio_rustls::TlsAcceptor::from(Arc::new(config))
        });
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let ingress = self.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match acceptor {
                    None => ingress.serve(TokioIo::new(tcp), peer, false).await,
                    Some(acceptor) => {
                        match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp))
                            .await
                        {
                            Ok(Ok(stream)) => ingress.serve(TokioIo::new(stream), peer, true).await,
                            _ => debug!(%peer, "TLS handshake failed"),
                        }
                    }
                }
            });
        }
    }

    async fn serve<I>(self: Arc<Self>, io: I, peer: SocketAddr, tls: bool)
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
        let routing = &self.agent.routing;

        if !tls
            && let Some(token) = req
                .uri()
                .path()
                .strip_prefix("/.well-known/acme-challenge/")
        {
            return match routing.challenges.read().get(token) {
                Some(key_auth) => text(StatusCode::OK, key_auth.clone()),
                None => text(StatusCode::NOT_FOUND, "unknown challenge"),
            };
        }
        if let Some(main) = self.redirect_for(&host) {
            let scheme = if tls || self.agent.certs.find(&main).is_some() {
                "https"
            } else {
                "http"
            };
            return redirect(format!(
                "{scheme}://{main}{}",
                req.uri()
                    .path_and_query()
                    .map(|p| p.as_str())
                    .unwrap_or("/")
            ));
        }
        if !tls && self.agent.certs.find(&host).is_some() {
            let target = format!(
                "https://{host}{}",
                req.uri()
                    .path_and_query()
                    .map(|p| p.as_str())
                    .unwrap_or("/")
            );
            return redirect(target);
        }

        let proxy = routing
            .proxies
            .read()
            .iter()
            .find(|p| p.domain == host)
            .map(|p| p.port);
        let (name, target) = if let Some(port) = proxy {
            ("facade".to_string(), Ok(Target::Port(port)))
        } else {
            let Some(workload) = self.workload_for(&host) else {
                return text(
                    StatusCode::NOT_FOUND,
                    format!("no app is served at {host}\n"),
                );
            };
            let target = self.target(&workload).await;
            (workload.spec.name, target)
        };
        let result = match target {
            Ok(Target::Static(root)) => serve_static(req, root).await,
            Ok(Target::Port(port)) => self.proxy(req, port, &host, peer, tls).await,
            Err(e) => Err(e),
        };
        result.unwrap_or_else(|e| {
            debug!(app = %name, "proxy error: {e:#}");
            text(
                StatusCode::BAD_GATEWAY,
                format!("{name} is unavailable right now\n"),
            )
        })
    }

    /// The main domain `host` redirects to, if it is one of an app's redirects.
    fn redirect_for(&self, host: &str) -> Option<String> {
        self.agent
            .workloads
            .lock()
            .values()
            .find(|w| w.spec.enabled && w.spec.redirects.iter().any(|d| d == host))
            .and_then(|w| w.spec.domains.first().cloned())
    }

    fn workload_for(&self, host: &str) -> Option<Workload> {
        self.agent
            .workloads
            .lock()
            .values()
            .find(|w| w.spec.enabled && w.spec.domains.iter().any(|d| d == host))
            .cloned()
    }

    async fn target(&self, workload: &Workload) -> anyhow::Result<Target> {
        if workload.spec.is_static() {
            return Ok(Target::Static(self.agent.runtime.static_root(workload)?));
        }
        Ok(Target::Port(
            self.agent.runtime.http_target(workload).await?.port(),
        ))
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
            tokio::spawn(async move {
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
    fn wildcards_cover_one_level() {
        let dir = std::env::temp_dir().join(format!("facade-certs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let certs = Certs::load(&dir).unwrap();
        let generated = rcgen::generate_simple_self_signed(vec!["*.example.com".into()]).unwrap();
        certs.replace(&[CertBundle {
            names: vec!["example.com".into(), "*.example.com".into()],
            cert_pem: generated.cert.pem(),
            key_pem: generated.signing_key.serialize_pem(),
        }]);
        assert!(certs.find("example.com").is_some());
        assert!(certs.find("app.example.com").is_some());
        assert!(certs.find("a.b.example.com").is_none());
        assert!(certs.find("other.com").is_none());
        // Reloaded from disk after a restart.
        assert!(Certs::load(&dir).unwrap().find("app.example.com").is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
