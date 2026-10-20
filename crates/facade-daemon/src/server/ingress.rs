//! Public HTTP(S) entry point. Routes by host name to apps on any node: every
//! upstream connection is a tunnel dialed back by the app's agent, pooled by
//! hyper. WebSocket (and any HTTP/1.1 upgrade) is spliced through.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::Context;
use axum::body::Body;
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri, Version};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::RwLock;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::net::TcpListener;
use tower::{Service, ServiceExt};
use tracing::{debug, info};

use super::Server;
use super::hub::DataStream;

/// Certificates by host name, with the self-signed identity as fallback.
#[derive(Debug, Default)]
pub struct CertStore {
    certs: RwLock<HashMap<String, Arc<CertifiedKey>>>,
    fallback: RwLock<Option<Arc<CertifiedKey>>>,
}

impl CertStore {
    pub fn set_fallback(&self, key: Arc<CertifiedKey>) {
        *self.fallback.write() = Some(key);
    }

    pub fn insert(&self, domain: &str, key: Arc<CertifiedKey>) {
        self.certs.write().insert(domain.to_string(), key);
    }

    pub fn has(&self, domain: &str) -> bool {
        self.certs.read().contains_key(domain)
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        hello
            .server_name()
            .and_then(|name| self.certs.read().get(&name.to_ascii_lowercase()).cloned())
            .or_else(|| self.fallback.read().clone())
    }
}

struct Ingress {
    server: Arc<Server>,
    client: Client<TunnelConnector, Incoming>,
    api: axum::Router,
}

pub async fn run(server: Arc<Server>) -> anyhow::Result<()> {
    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(32)
        .build(TunnelConnector(server.clone()));
    let ingress = Arc::new(Ingress {
        api: super::api::router(server.clone()),
        server: server.clone(),
        client,
    });

    let http = TcpListener::bind(server.cfg.http_listen)
        .await
        .with_context(|| format!("binding {}", server.cfg.http_listen))?;
    let https = TcpListener::bind(server.cfg.https_listen)
        .await
        .with_context(|| format!("binding {}", server.cfg.https_listen))?;
    info!(
        "ingress on http://{} and https://{}",
        server.cfg.http_listen, server.cfg.https_listen
    );

    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(server.certs.clone());
    tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));

    let plain = {
        let ingress = ingress.clone();
        async move {
            loop {
                let (tcp, peer) = http.accept().await?;
                let ingress = ingress.clone();
                tokio::spawn(async move {
                    ingress.serve(TokioIo::new(tcp), peer, false).await;
                });
            }
        }
    };
    let secure = async move {
        loop {
            let (tcp, peer) = https.accept().await?;
            let ingress = ingress.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await {
                    Ok(Ok(tls)) => ingress.serve(TokioIo::new(tls), peer, true).await,
                    _ => debug!(%peer, "TLS handshake failed"),
                }
            });
        }
    };
    tokio::select! {
        r = plain => r,
        r = secure => r,
    }
}

impl Ingress {
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
        let path = req.uri().path();

        if !tls && let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
            return match self.server.challenges.read().get(token) {
                Some(key_auth) => text(StatusCode::OK, key_auth.clone()),
                None => text(StatusCode::NOT_FOUND, "unknown challenge"),
            };
        }
        if !tls && self.server.certs.has(&host) {
            let target = format!(
                "https://{host}{}",
                req.uri()
                    .path_and_query()
                    .map(|p| p.as_str())
                    .unwrap_or("/")
            );
            return Response::builder()
                .status(StatusCode::PERMANENT_REDIRECT)
                .header(header::LOCATION, target)
                .body(Body::empty())
                .expect("valid response");
        }
        if self.server.cfg.api_domain.as_deref() == Some(host.as_str()) {
            return match self.api.clone().oneshot(req.map(Body::new)).await {
                Ok(resp) => resp,
                Err(never) => match never {},
            };
        }

        let route = {
            let routes = self.server.routes.read();
            routes.domains.get(&host).and_then(|app| {
                routes
                    .nodes
                    .get(app)
                    .map(|node| (app.clone(), node.clone()))
            })
        };
        let Some((app, _node)) = route else {
            return text(
                StatusCode::NOT_FOUND,
                format!("no app is served at {host}\n"),
            );
        };
        match self.proxy(req, &app, &host, peer, tls).await {
            Ok(resp) => resp,
            Err(e) => {
                debug!(%app, "proxy error: {e:#}");
                text(
                    StatusCode::BAD_GATEWAY,
                    format!("{app} is unavailable right now\n"),
                )
            }
        }
    }

    async fn proxy(
        &self,
        mut req: Request<Incoming>,
        app: &str,
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
        *req.uri_mut() = format!("http://{app}.facade{path}").parse::<Uri>()?;
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

/// Resolves `http://<app>.facade/` to a tunnel through the app's node.
#[derive(Clone)]
struct TunnelConnector(Arc<Server>);

impl Service<Uri> for TunnelConnector {
    type Response = TunnelIo;
    type Error = anyhow::Error;
    type Future = Pin<Box<dyn Future<Output = anyhow::Result<TunnelIo>> + Send>>;

    fn poll_ready(&mut self, _: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let server = self.0.clone();
        Box::pin(async move {
            let app = uri
                .host()
                .and_then(|h| h.strip_suffix(".facade"))
                .context("bad tunnel URI")?
                .to_string();
            let node = server
                .routes
                .read()
                .nodes
                .get(&app)
                .cloned()
                .with_context(|| format!("no app named {app}"))?;
            let stream = server.hub.open_tunnel(&node, &app).await?;
            Ok(TunnelIo(TokioIo::new(stream)))
        })
    }
}

struct TunnelIo(TokioIo<DataStream>);

impl Connection for TunnelIo {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl hyper::rt::Read for TunnelIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for TunnelIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
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
}
