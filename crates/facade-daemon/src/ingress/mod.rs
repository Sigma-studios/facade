//! A node's public HTTP(S) entry point. Routes by host name, straight to the
//! apps running on the node; WebSocket (and any HTTP/1.1 upgrade) is spliced
//! through.
//!
//! It also takes TLS off raw TCP ports for apps that ask (see [`tcp`]), and
//! opens those ports in ufw while it serves them (see [`firewall`]).
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

mod firewall;
pub mod process;
pub mod tcp;
pub mod traffic;

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use facade_core::proto::{CertBundle, Traffic};
use http_body_util::BodyExt;
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

use self::traffic::{AppTraffic, Open};
use crate::tls;

/// Everything the entry point needs to know, replaced as a whole by the agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Table {
    /// Host name → where it goes.
    pub routes: BTreeMap<String, Route>,
    pub certs: Vec<CertBundle>,
    /// Public TCP port → the app TLS-wrapped connections on it go to.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        with = "port_keys"
    )]
    pub tcp: BTreeMap<u16, tcp::TcpRoute>,
    /// ACME HTTP-01 token → key authorization.
    pub challenges: BTreeMap<String, String>,
}

/// Port-keyed maps as JSON objects with string keys, read back by hand: the
/// table travels inside an internally tagged enum, whose buffering cannot turn
/// `"465"` back into a number.
mod port_keys {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<V: Serialize, S: Serializer>(
        map: &BTreeMap<u16, V>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let by_name: BTreeMap<String, &V> = map.iter().map(|(k, v)| (k.to_string(), v)).collect();
        by_name.serialize(s)
    }

    pub fn deserialize<'de, V: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<u16, V>, D::Error> {
        let by_name = BTreeMap::<String, V>::deserialize(d)?;
        by_name
            .into_iter()
            .map(|(k, v)| {
                k.parse()
                    .map(|k| (k, v))
                    .map_err(|_| serde::de::Error::custom(format!("bad port {k}")))
            })
            .collect()
    }
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

impl Route {
    /// The app answering at this host, if one does.
    fn app(&self) -> Option<&str> {
        match self {
            Route::Port { app, .. } | Route::Static { app, .. } | Route::Unavailable { app } => {
                Some(app)
            }
            Route::Redirect { .. } => None,
        }
    }
}

/// The table in use, with the traffic counters of the apps it routes to: one
/// lock gives a request both its route and where to count it.
struct Routing {
    table: Table,
    apps: HashMap<String, Arc<AppTraffic>>,
}

/// The apps a table routes to, by host name or TLS port.
fn routed_apps(table: &Table) -> impl Iterator<Item = &str> {
    table
        .routes
        .values()
        .filter_map(Route::app)
        .chain(table.tcp.values().map(|t| t.app.as_str()))
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
    routing: RwLock<Routing>,
    certs: Arc<Certs>,
    client: Client<HttpConnector, Incoming>,
    /// Where the table is kept between runs.
    file: PathBuf,
    /// Addresses that cannot be bound yet, with why.
    bind_errors: Mutex<BTreeMap<SocketAddr, String>>,
    /// Connections being served (WebSockets included), so a retiring process
    /// knows when it is done.
    active: Arc<AtomicUsize>,
    /// Requests in flight and connections open to each local port, so the agent
    /// knows when a container traffic moved away from is done with.
    upstreams: Mutex<HashMap<u16, Arc<AtomicUsize>>>,
    /// Told whenever `bind_errors` changes.
    pub status: watch::Sender<()>,
    /// The listener of each TLS port, by the sender that stops it.
    tcp: Mutex<HashMap<u16, watch::Sender<bool>>>,
    /// Set once a newer entry point took over: no TLS port starts any more.
    retired: AtomicBool,
    /// Turns true then too: open connections finish what they are doing and
    /// close, so clients reconnect to the new process. Kept alive (a page
    /// polling every few seconds keeps it so), a connection would otherwise
    /// stay here, with routes that no longer change, until the drain ends.
    retiring: watch::Sender<bool>,
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
        let apps = routed_apps(&table)
            .map(|app| (app.to_string(), AppTraffic::new()))
            .collect();
        Ok(Arc::new(Self {
            routing: RwLock::new(Routing { table, apps }),
            certs,
            client,
            file,
            bind_errors: Default::default(),
            active: Default::default(),
            upstreams: Default::default(),
            status: watch::channel(()).0,
            tcp: Default::default(),
            retired: AtomicBool::new(false),
            retiring: watch::channel(false).0,
        }))
    }

    /// Replaces the table (and saves it) if it changed.
    pub fn set_table(self: &Arc<Self>, table: Table) {
        {
            let current = &self.routing.read().table;
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
        // Counters of ports nothing goes to any more, once idle.
        let routed: std::collections::HashSet<u16> = table
            .routes
            .values()
            .filter_map(|r| match r {
                Route::Port { port, .. } => Some(*port),
                _ => None,
            })
            .chain(table.tcp.values().filter_map(|t| t.port))
            .collect();
        self.upstreams
            .lock()
            .retain(|port, open| routed.contains(port) || open.load(Ordering::Relaxed) > 0);
        {
            let mut routing = self.routing.write();
            // Apps keep their counters while routed to (a deploy moving one to
            // another port included), and while connections to them last.
            let mut apps: HashMap<String, Arc<AppTraffic>> = routed_apps(&table)
                .map(|app| {
                    let counters = routing.apps.get(app).cloned();
                    (app.to_string(), counters.unwrap_or_else(AppTraffic::new))
                })
                .collect();
            for (app, counters) in routing.apps.drain() {
                if counters.open() > 0 {
                    apps.entry(app).or_insert(counters);
                }
            }
            *routing = Routing { table, apps };
        }
        self.sync_tcp();
    }

    /// The counters of every app routed to, as they are now.
    pub fn traffic(&self) -> BTreeMap<String, Traffic> {
        self.routing
            .read()
            .apps
            .iter()
            .map(|(app, counters)| (app.clone(), counters.snapshot()))
            .collect()
    }

    /// Where to count traffic for `app`.
    fn app_traffic(&self, app: &str) -> Option<Arc<AppTraffic>> {
        self.routing.read().apps.get(app).cloned()
    }

    /// Counts a request or connection to a local port while alive. Taken as soon
    /// as the route is looked up, so a request routed by the previous table is
    /// counted by the time the agent asks about its port.
    fn upstream(&self, port: u16) -> Counted {
        let counter = self.upstreams.lock().entry(port).or_default().clone();
        Counted::new(&counter)
    }

    /// Requests in flight and connections open to these local ports.
    pub fn open_connections(&self, ports: &[u16]) -> usize {
        let upstreams = self.upstreams.lock();
        ports
            .iter()
            .filter_map(|p| upstreams.get(p))
            .map(|open| open.load(Ordering::Relaxed))
            .sum()
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
        let mut retiring = self.retiring.subscribe();
        // The app the connection's first request went to counts it as open
        // until it closes (or turns into a WebSocket, which counts on its own).
        let open = Arc::new(OnceLock::<Open>::new());
        let service = hyper::service::service_fn(move |req| {
            let ingress = self.clone();
            let open = open.clone();
            async move { Ok::<_, Infallible>(ingress.handle(req, peer, tls, &open).await) }
        });
        let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
        let conn = builder.serve_connection_with_upgrades(io, service);
        tokio::pin!(conn);
        tokio::select! {
            _ = conn.as_mut() => return,
            _ = retiring.wait_for(|r| *r) => {}
        }
        // HTTP/2: GOAWAY; HTTP/1.1: no keep-alive after the current response.
        // Upgraded connections (WebSockets) carry on until they end.
        conn.as_mut().graceful_shutdown();
        let _ = conn.await;
    }

    async fn handle(
        &self,
        req: Request<Incoming>,
        peer: SocketAddr,
        tls: bool,
        open: &OnceLock<Open>,
    ) -> Response<Body> {
        let started = Instant::now();
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
            return match self.routing.read().table.challenges.get(token) {
                Some(key_auth) => text(StatusCode::OK, key_auth.clone()),
                None => text(StatusCode::NOT_FOUND, "unknown challenge"),
            };
        }
        let (route, counters) = {
            let routing = self.routing.read();
            let route = routing.table.routes.get(&host).cloned();
            let counters = route
                .as_ref()
                .and_then(Route::app)
                .and_then(|app| routing.apps.get(app).cloned());
            (route, counters)
        };
        let upstream = match &route {
            Some(Route::Port { port, .. }) => Some(self.upstream(*port)),
            _ => None,
        };
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
        if let Some(counters) = &counters {
            open.get_or_init(|| Open::new(counters.clone()));
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
                .proxy(req, port, upstream, counters.clone(), &host, peer, tls)
                .await
                .map_err(|e| (app, e)),
        };
        match result {
            Ok(response) => {
                if let Some(counters) = &counters {
                    counters.record(response.status().as_u16(), Some(started.elapsed()));
                }
                response
            }
            Err((app, e)) => {
                debug!(%app, "proxy error: {e:#}");
                if let Some(counters) = &counters {
                    counters.record(StatusCode::BAD_GATEWAY.as_u16(), None);
                }
                text(
                    StatusCode::BAD_GATEWAY,
                    format!("{app} is unavailable right now\n"),
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn proxy(
        &self,
        mut req: Request<Incoming>,
        port: u16,
        // Counts the request until its response is sent, or the connection it
        // turns into (an upgrade) until it ends.
        upstream: Option<Counted>,
        // Where the connection an upgrade turns into counts as open.
        counters: Option<Arc<AppTraffic>>,
        host: &str,
        peer: SocketAddr,
        tls: bool,
    ) -> anyhow::Result<Response<Body>> {
        let upgrade = is_upgrade(req.headers());
        let websocket = upgrade
            && req
                .headers()
                .get(header::UPGRADE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
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
            let app_side = hyper::upgrade::on(&mut resp);
            // The HTTP connection ends here; the spliced one counts on its own.
            let counted = (
                Counted::new(&self.active),
                upstream,
                counters.map(Open::new),
            );
            tokio::spawn(async move {
                let _counted = counted;
                match tokio::try_join!(downstream, app_side) {
                    Ok((down, up)) if websocket => {
                        splice_websocket(TokioIo::new(down), TokioIo::new(up)).await
                    }
                    Ok((down, up)) => {
                        let mut down = TokioIo::new(down);
                        let mut up = TokioIo::new(up);
                        let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
                    }
                    Err(e) => debug!("upgrade failed: {e}"),
                }
            });
            return Ok(resp.map(Body::new));
        }
        strip_hop_by_hop(resp.headers_mut());
        // The body streams after this returns: counted until it is sent (or the
        // client goes).
        Ok(resp.map(|body| {
            Body::new(body.map_frame(move |frame| {
                let _ = &upstream;
                frame
            }))
        }))
    }
}

const BIND_RETRY: Duration = Duration::from_secs(5);

/// How long a WebSocket's other side has to finish once one side closed.
const WEBSOCKET_LINGER: Duration = Duration::from_secs(2);

/// Passes bytes both ways until either side closes, then gives the other a
/// moment to send what it still has (a close frame) and closes it too. A
/// WebSocket has no use for a half-closed connection, and apps that never close
/// theirs once the client left would otherwise keep it open here, and the
/// container they run in from counting as drained after a deploy.
async fn splice_websocket<A, B>(a: A, b: B)
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);
    let a_to_b = async {
        let _ = tokio::io::copy(&mut a_read, &mut b_write).await;
        let _ = b_write.shutdown().await;
    };
    let b_to_a = async {
        let _ = tokio::io::copy(&mut b_read, &mut a_write).await;
        let _ = a_write.shutdown().await;
    };
    tokio::pin!(a_to_b, b_to_a);
    tokio::select! {
        _ = &mut a_to_b => {
            let _ = tokio::time::timeout(WEBSOCKET_LINGER, &mut b_to_a).await;
        }
        _ = &mut b_to_a => {
            let _ = tokio::time::timeout(WEBSOCKET_LINGER, &mut a_to_b).await;
        }
    }
}

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
    let cache = cache_control(req.uri().path());
    let mut response = service.oneshot(req).await?;
    // A missing file may exist after the next deploy.
    let cache = if response.status().is_success() || response.status() == StatusCode::NOT_MODIFIED {
        cache
    } else {
        "no-cache"
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    // The same URL is answered with brotli, gzip or plain bytes.
    headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    Ok(response.map(Body::new))
}

/// Files whose name carries a content hash (`app-3f2a9c1d.js`, Vite's
/// `index-DiwrgTda.js`) never change: browsers keep them. Anything else is
/// revalidated on every use (a cheap 304 while it is unchanged), so a deploy is
/// seen at once.
fn cache_control(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or_default();
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let hashed = stem.split(['-', '.', '_']).skip(1).any(|part| {
        let hex = part.len() >= 8
            && part.chars().all(|c| c.is_ascii_hexdigit())
            && part.chars().any(|c| c.is_ascii_digit());
        let rollup = part.len() == 8
            && part.chars().all(|c| c.is_ascii_alphanumeric())
            && part.chars().skip(1).any(|c| c.is_ascii_uppercase())
            && part.chars().any(|c| c.is_ascii_lowercase());
        hex || rollup
    });
    if hashed {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
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

    #[test]
    fn caches_hashed_files_only() {
        for path in [
            "/prisme-web-d283876f31793b05.js",
            "/assets/index-DiwrgTda.js",
            "/static/js/main.8c3a2f1e.chunk.js",
            "/app_bg-0123abcd.wasm",
        ] {
            assert_eq!(
                cache_control(path),
                "public, max-age=31536000, immutable",
                "{path}"
            );
        }
        for path in [
            "/",
            "/index.html",
            "/prisme-web_bg.wasm",
            "/worker.js",
            "/jquery-3.7.1.min.js",
            "/bootstrap.bundle.js",
            "/fonts/Inter-Regular.ttf",
            "/deadbeef.js",
            "/logo-Facebook.png",
        ] {
            assert_eq!(cache_control(path), "no-cache", "{path}");
        }
    }

    fn host_of(h: &str) -> Option<String> {
        request_host(&Request::builder().header("host", h).body(()).unwrap())
    }

    #[tokio::test]
    async fn a_retiring_entry_point_closes_kept_alive_connections() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = std::env::temp_dir().join(format!("facade-retire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ingress = Ingress::new(&dir).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_stop_tx, stop) = watch::channel(false);
        tokio::spawn(ingress.clone().serve(listener, false, stop));

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let ask = b"GET / HTTP/1.1\r\nHost: nowhere.example\r\n\r\n";
        client.write_all(ask).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = client.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 404"));
        assert_eq!(ingress.active_connections(), 1, "kept alive");

        ingress.retire();
        let closed = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
        assert!(
            matches!(closed, Ok(Ok(0))),
            "the connection closes: {closed:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ingress.active_connections(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn upgrades_are_spliced_through_and_outlive_retiring() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // The app: answers the upgrade (checking the proxy kept its headers),
        // then echoes whatever comes, as a WebSocket server would frames.
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_port = app.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = app.accept().await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                sock.read_exact(&mut byte).await.unwrap();
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
            assert!(head.contains("upgrade: websocket"), "{head}");
            assert!(head.contains("connection: upgrade"), "{head}");
            assert!(head.contains("host: ws.example.com"), "{head}");
            sock.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\n\
                  Upgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .await
            .unwrap();
            let mut buf = [0u8; 1024];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => sock.write_all(&buf[..n]).await.unwrap(),
                }
            }
        });

        let dir = std::env::temp_dir().join(format!("facade-upgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ingress = Ingress::new(&dir).unwrap();
        let mut table = Table::default();
        table.routes.insert(
            "ws.example.com".into(),
            Route::Port {
                app: "ws".into(),
                port: app_port,
            },
        );
        ingress.set_table(table);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_stop_tx, stop) = watch::channel(false);
        tokio::spawn(ingress.clone().serve(listener, false, stop));

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                b"GET /signal HTTP/1.1\r\nHost: ws.example.com\r\n\
                  Upgrade: websocket\r\nConnection: Upgrade\r\n\
                  Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .await
            .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        assert!(
            head.to_ascii_lowercase().contains("upgrade: websocket"),
            "{head}"
        );

        async fn echo(client: &mut tokio::net::TcpStream, msg: &[u8]) {
            client.write_all(msg).await.unwrap();
            let mut got = vec![0u8; msg.len()];
            tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut got))
                .await
                .expect("echoed in time")
                .unwrap();
            assert_eq!(got, msg);
        }
        echo(&mut client, b"hello").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            ingress.active_connections(),
            1,
            "the spliced connection counts"
        );
        assert_eq!(
            ingress.open_connections(&[app_port]),
            1,
            "as a connection to the app's port"
        );
        let traffic = &ingress.traffic()["ws"];
        assert_eq!(
            (traffic.requests, traffic.open),
            (1, 1),
            "its upgrade as a request, then the WebSocket as one connection"
        );

        // The app was replaced: its route goes elsewhere, the WebSocket stays
        // with the old container until it ends, and the agent sees it.
        let mut moved = Table::default();
        moved.routes.insert(
            "ws.example.com".into(),
            Route::Port {
                app: "ws".into(),
                port: 1,
            },
        );
        ingress.set_table(moved);
        echo(&mut client, b"after the switch").await;
        assert_eq!(ingress.open_connections(&[app_port]), 1);

        // A newer entry point took over: the WebSocket carries on.
        ingress.retire();
        tokio::time::sleep(Duration::from_millis(100)).await;
        echo(&mut client, b"still here").await;
        assert_eq!(ingress.active_connections(), 1);

        drop(client);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(ingress.active_connections(), 0, "counted until it ends");
        assert_eq!(ingress.open_connections(&[app_port]), 0);
        assert_eq!(ingress.traffic()["ws"].open, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_websocket_ends_with_its_client_even_if_the_app_keeps_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // The app answers the upgrade, then never reads nor closes again.
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_port = app.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = app.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            sock.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\n\
                  Upgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(sock);
        });
        let dir = std::env::temp_dir().join(format!("facade-ws-linger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ingress = Ingress::new(&dir).unwrap();
        let mut table = Table::default();
        table.routes.insert(
            "ws.example.com".into(),
            Route::Port {
                app: "ws".into(),
                port: app_port,
            },
        );
        ingress.set_table(table);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_stop_tx, stop) = watch::channel(false);
        tokio::spawn(ingress.clone().serve(listener, false, stop));

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                b"GET / HTTP/1.1\r\nHost: ws.example.com\r\n\
                  Upgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = client.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 101"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ingress.open_connections(&[app_port]), 1);
        drop(client);
        tokio::time::sleep(WEBSOCKET_LINGER + Duration::from_millis(300)).await;
        assert_eq!(ingress.open_connections(&[app_port]), 0);
        assert_eq!(ingress.active_connections(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn requests_count_until_their_response_is_sent() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // The app: sends its headers at once, then the body a moment later.
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_port = app.local_addr().unwrap().port();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (mut sock, _) = app.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n")
                .await
                .unwrap();
            let _ = released.await;
            sock.write_all(b"done").await.unwrap();
            let _ = sock.read(&mut buf).await;
        });

        let dir = std::env::temp_dir().join(format!("facade-count-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ingress = Ingress::new(&dir).unwrap();
        let mut table = Table::default();
        table.routes.insert(
            "app.example.com".into(),
            Route::Port {
                app: "app".into(),
                port: app_port,
            },
        );
        ingress.set_table(table);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_stop_tx, stop) = watch::channel(false);
        tokio::spawn(ingress.clone().serve(listener, false, stop));

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: app.example.com\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = client.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
        assert_eq!(
            ingress.open_connections(&[app_port]),
            1,
            "its body is still coming"
        );
        release.send(()).unwrap();
        let mut got = String::from_utf8_lossy(&buf[..n]).to_string();
        while !got.ends_with("done") {
            let n = client.read(&mut buf).await.unwrap();
            got.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            ingress.open_connections(&[app_port]),
            0,
            "sent: the client connection kept alive does not count"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sends a GET on a kept-alive connection and reads the answer: its status.
    async fn ask(client: &mut tokio::net::TcpStream, host: &str, path: &str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        client
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut got = Vec::new();
        let mut byte = [0u8; 1];
        while !got.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut byte).await.unwrap();
            got.push(byte[0]);
        }
        let head = String::from_utf8(got).unwrap().to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map_or(0, |n| n.trim().parse().unwrap());
        let mut body = vec![0u8; length];
        client.read_exact(&mut body).await.unwrap();
        head[9..12].parse().unwrap()
    }

    #[tokio::test]
    async fn traffic_is_counted_per_app() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // The app: answers by path, keeping its connections alive.
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_port = app.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = app.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        head.clear();
                        while !head.ends_with(b"\r\n\r\n") {
                            if sock.read_exact(&mut byte).await.is_err() {
                                return;
                            }
                            head.push(byte[0]);
                        }
                        let head = String::from_utf8_lossy(&head).to_string();
                        let status = match head.split(' ').nth(1) {
                            Some("/missing") => "404 Not Found",
                            Some("/boom") => "500 Internal Server Error",
                            _ => "200 OK",
                        };
                        let answer = format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\n\r\nok");
                        sock.write_all(answer.as_bytes()).await.unwrap();
                    }
                });
            }
        });
        let dir = std::env::temp_dir().join(format!("facade-traffic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ingress = Ingress::new(&dir).unwrap();
        let mut table = Table::default();
        table.routes.insert(
            "app.example.com".into(),
            Route::Port {
                app: "app".into(),
                port: app_port,
            },
        );
        table.routes.insert(
            "down.example.com".into(),
            Route::Unavailable { app: "down".into() },
        );
        table.routes.insert(
            "www.example.com".into(),
            Route::Redirect {
                to: "app.example.com".into(),
            },
        );
        ingress.set_table(table.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_stop_tx, stop) = watch::channel(false);
        tokio::spawn(ingress.clone().serve(listener, false, stop));

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert_eq!(ask(&mut client, "app.example.com", "/").await, 200);
        assert_eq!(ask(&mut client, "app.example.com", "/missing").await, 404);
        assert_eq!(ask(&mut client, "app.example.com", "/boom").await, 500);
        // Answered by the entry point: neither app's.
        assert_eq!(ask(&mut client, "www.example.com", "/").await, 308);
        assert_eq!(ask(&mut client, "nowhere.example.com", "/").await, 404);
        let mut other = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert_eq!(ask(&mut other, "down.example.com", "/").await, 502);

        let traffic = ingress.traffic();
        assert_eq!(
            traffic.keys().collect::<Vec<_>>(),
            ["app", "down"],
            "only apps are counted"
        );
        let app = &traffic["app"];
        assert_eq!(
            (app.requests, app.client_errors, app.server_errors),
            (3, 1, 1)
        );
        assert_eq!(app.latency.iter().sum::<u64>(), 3);
        assert_eq!(app.open, 1, "the kept-alive connection");
        let down = &traffic["down"];
        assert_eq!((down.requests, down.server_errors), (1, 1));
        assert_eq!(
            down.latency.iter().sum::<u64>(),
            0,
            "an app that is down is not fast"
        );
        assert_eq!(down.open, 1);

        // Counters stay with an app across tables, and go with it once idle.
        drop(other);
        tokio::time::sleep(Duration::from_millis(50)).await;
        table.routes.remove("down.example.com");
        ingress.set_table(table);
        let traffic = ingress.traffic();
        assert_eq!(traffic.keys().collect::<Vec<_>>(), ["app"]);
        assert_eq!(traffic["app"].requests, 3);
        drop(client);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ingress.traffic()["app"].open, 0);
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(again.routing.read().table, table);
        assert!(again.certs.find("app.example.com").is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
