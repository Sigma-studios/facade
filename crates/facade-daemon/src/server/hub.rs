//! Connections from agents: authentication, request/response over the control
//! connection, and matching dialed-back data connections to waiting tunnels.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use facade_core::proto::{AgentEvent, AgentMsg, AgentRequest, Hello, PROTOCOL_VERSION, ServerMsg};
use parking_lot::{Mutex, RwLock};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::{Server, hash_secret};
use crate::framing::{JsonLines, read_line_unbuffered, write_json};
use crate::tls;

pub type DataStream = tokio_rustls::server::TlsStream<TcpStream>;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PING_EVERY: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(70);

#[derive(Default)]
pub struct Hub {
    nodes: RwLock<HashMap<String, Arc<NodeConn>>>,
    tunnels: Mutex<HashMap<Uuid, oneshot::Sender<DataStream>>>,
    next_id: AtomicU64,
}

/// An agent's answer: an error, or data for requests that return some.
type Answer = Result<Option<serde_json::Value>, String>;

pub struct NodeConn {
    tx: mpsc::Sender<ServerMsg>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Answer>>>,
}

impl Hub {
    pub fn is_connected(&self, node: &str) -> bool {
        self.nodes.read().contains_key(node)
    }

    pub fn connection(&self, node: &str) -> Option<Arc<NodeConn>> {
        self.nodes.read().get(node).cloned()
    }

    /// Sends a request and waits for the agent's acknowledgement.
    pub async fn request(&self, node: &str, req: AgentRequest) -> anyhow::Result<()> {
        self.ask(node, req).await.map(drop)
    }

    /// Sends a request whose answer carries data, and decodes it.
    pub async fn query<T: serde::de::DeserializeOwned>(
        &self,
        node: &str,
        req: AgentRequest,
    ) -> anyhow::Result<T> {
        let data = self
            .ask(node, req)
            .await?
            .ok_or_else(|| anyhow!("node {node} sent no answer (is its agent up to date?)"))?;
        Ok(serde_json::from_value(data)?)
    }

    async fn ask(
        &self,
        node: &str,
        req: AgentRequest,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let conn = self
            .connection(node)
            .ok_or_else(|| anyhow!("node {node} is offline"))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        conn.pending.lock().insert(id, tx);
        if conn.tx.send(ServerMsg::Request { id, req }).await.is_err() {
            conn.pending.lock().remove(&id);
            bail!("node {node} disconnected");
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(data))) => Ok(data),
            Ok(Ok(Err(error))) => Err(anyhow!(error)),
            Ok(Err(_)) => Err(anyhow!("node {node} disconnected")),
            Err(_) => {
                conn.pending.lock().remove(&id);
                Err(anyhow!("node {node} did not answer"))
            }
        }
    }

    /// Asks the agent to dial back a data connection and returns it.
    async fn dial_back(
        &self,
        node: &str,
        make: impl FnOnce(Uuid) -> AgentRequest,
    ) -> anyhow::Result<DataStream> {
        let tunnel = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        self.tunnels.lock().insert(tunnel, tx);
        let result = async {
            self.request(node, make(tunnel)).await?;
            tokio::time::timeout(Duration::from_secs(10), rx)
                .await
                .context("agent did not dial back")?
                .context("tunnel dropped")
        }
        .await;
        self.tunnels.lock().remove(&tunnel);
        result
    }

    /// A stream of the volume as a gzipped tar.
    pub async fn open_volume_export(
        &self,
        node: &str,
        app: &str,
        volume: &str,
    ) -> anyhow::Result<DataStream> {
        let (app, volume) = (app.to_string(), volume.to_string());
        self.dial_back(node, |tunnel| AgentRequest::VolumeExport {
            tunnel,
            app,
            volume,
        })
        .await
    }

    /// Replaces the volume's contents with the archive at `path`, waiting for the
    /// agent to finish.
    pub async fn import_volume(
        &self,
        node: &str,
        app: &str,
        volume: &str,
        path: &std::path::Path,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        let (a, v) = (app.to_string(), volume.to_string());
        let mut stream = self
            .dial_back(node, |tunnel| AgentRequest::VolumeImport {
                tunnel,
                app: a,
                volume: v,
            })
            .await?;
        let mut file = tokio::fs::File::open(path).await?;
        let size = file.metadata().await?.len();
        stream.write_all(format!("{size}\n").as_bytes()).await?;
        tokio::io::copy(&mut file, &mut stream).await?;
        stream.flush().await?;
        let answer = read_line_unbuffered(&mut stream)
            .await
            .context("the agent hung up while restoring")?;
        match answer.split_once(' ') {
            _ if answer == "ok" => Ok(()),
            Some(("error", message)) => bail!("{message}"),
            _ => bail!("unexpected answer from agent: {answer}"),
        }
    }

    pub async fn open_logs(
        &self,
        node: &str,
        app: &str,
        tail: u32,
        follow: bool,
    ) -> anyhow::Result<DataStream> {
        let app = app.to_string();
        self.dial_back(node, |tunnel| AgentRequest::Logs {
            tunnel,
            app,
            tail,
            follow,
        })
        .await
    }
}

pub async fn listen(server: Arc<Server>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(server.cfg.node_listen)
        .await
        .with_context(|| format!("binding node listener {}", server.cfg.node_listen))?;
    info!("node listener on {}", server.cfg.node_listen);
    let acceptor =
        tokio_rustls::TlsAcceptor::from(tls::single_cert_server(server.identity.key.clone()));
    loop {
        let (tcp, peer) = listener.accept().await?;
        let server = server.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Err(e) = accept(server, acceptor, tcp, peer).await {
                debug!(%peer, "node connection: {e:#}");
            }
        });
    }
}

async fn accept(
    server: Arc<Server>,
    acceptor: tokio_rustls::TlsAcceptor,
    tcp: TcpStream,
    peer: SocketAddr,
) -> anyhow::Result<()> {
    tcp.set_nodelay(true)?;
    let mut stream = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp))
        .await
        .context("TLS handshake timed out")??;
    let line = tokio::time::timeout(Duration::from_secs(10), read_line_unbuffered(&mut stream))
        .await
        .context("no hello")??;
    let hello: Hello = serde_json::from_str(&line).context("malformed hello")?;

    let (node, secret) = match &hello {
        Hello::Control { node, secret, .. }
        | Hello::Data { node, secret, .. }
        | Hello::Fetch { node, secret, .. } => (node, secret),
    };
    let authorized = server
        .store
        .node_secret_hash(node)?
        .is_some_and(|expected| expected == hash_secret(secret));
    if !authorized {
        warn!(%peer, %node, "rejected agent with unknown node or bad secret");
        let _ = write_json(
            &mut stream,
            &ServerMsg::Rejected {
                reason: "unknown node or invalid secret (re-issue a join token with `facade node token`)".into(),
            },
        )
        .await;
        return Ok(());
    }

    match hello {
        Hello::Fetch { node, sha256, .. } => send_artifact(&server, &node, &sha256, stream).await,
        Hello::Data { tunnel, .. } => {
            if let Some(waiter) = server.hub.tunnels.lock().remove(&tunnel) {
                let _ = waiter.send(stream);
            }
            Ok(())
        }
        Hello::Control {
            node,
            protocol,
            agent_version,
            hostname,
            ..
        } => {
            if protocol != PROTOCOL_VERSION {
                let reason = format!(
                    "protocol {protocol} not supported, server speaks {PROTOCOL_VERSION}: upgrade facaded on both sides"
                );
                let _ = write_json(&mut stream, &ServerMsg::Rejected { reason }).await;
                return Ok(());
            }
            control(server, stream, peer, node, hostname, agent_version).await
        }
    }
}

async fn control(
    server: Arc<Server>,
    stream: DataStream,
    peer: SocketAddr,
    node: String,
    hostname: String,
    agent_version: String,
) -> anyhow::Result<()> {
    let (read, mut write) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(256);
    let conn = Arc::new(NodeConn {
        tx: tx.clone(),
        pending: Default::default(),
    });

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_json(&mut write, &msg).await.is_err() {
                break;
            }
        }
    });
    let pinger = tokio::spawn({
        let tx = tx.clone();
        async move {
            let mut tick = tokio::time::interval(PING_EVERY);
            loop {
                tick.tick().await;
                if tx.send(ServerMsg::Ping).await.is_err() {
                    break;
                }
            }
        }
    });

    server.hub.nodes.write().insert(node.clone(), conn.clone());
    server
        .store
        .node_connected(&node, &hostname, &peer.ip().to_string(), &agent_version)?;
    server
        .live
        .write()
        .nodes
        .entry(node.clone())
        .or_default()
        .offline_since = None;
    info!(%node, %peer, %hostname, "node connected");

    // Full desired state first, so a reconnecting agent converges.
    {
        let server = server.clone();
        let node = node.clone();
        tokio::spawn(async move {
            let result = match server.node_workloads(&node) {
                Ok(workloads) => {
                    server
                        .hub
                        .request(&node, AgentRequest::Sync { workloads })
                        .await
                }
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                warn!(%node, "sync failed: {e:#}");
            }
            if let Err(e) = super::acme::push_ingress(&server, &node).await {
                warn!(%node, "sending certificates failed: {e:#}");
            }
            // Its address may have changed: DNS records follow.
            server.domains_wake.notify_one();
        });
    }

    let mut lines = JsonLines::new(read);
    let result: anyhow::Result<()> = async {
        loop {
            let msg = tokio::time::timeout(READ_TIMEOUT, lines.next::<AgentMsg>())
                .await
                .context("agent went silent")??;
            let Some(msg) = msg else { return Ok(()) };
            match msg {
                AgentMsg::Response { id, error, data } => {
                    if let Some(waiter) = conn.pending.lock().remove(&id) {
                        let _ = waiter.send(match error {
                            Some(e) => Err(e),
                            None => Ok(data),
                        });
                    }
                }
                AgentMsg::Pong => server.store.touch_node(&node)?,
                AgentMsg::Event { event } => on_event(&server, &node, event),
            }
        }
    }
    .await;

    writer.abort();
    pinger.abort();
    {
        let mut nodes = server.hub.nodes.write();
        if nodes.get(&node).is_some_and(|c| Arc::ptr_eq(c, &conn)) {
            nodes.remove(&node);
            server
                .live
                .write()
                .nodes
                .entry(node.clone())
                .or_default()
                .offline_since = Some(facade_core::now());
            let apps: Vec<String> = server
                .routes
                .read()
                .nodes
                .iter()
                .filter(|(_, n)| **n == node)
                .map(|(a, _)| a.clone())
                .collect();
            server
                .store
                .fail_running_deployments(&apps, "node disconnected during deployment")?;
        }
    }
    match &result {
        Ok(()) => info!(%node, "node disconnected"),
        Err(e) => warn!(%node, "node disconnected: {e:#}"),
    }
    result
}

/// Streams an uploaded executable to an agent that runs an app using it.
async fn send_artifact(
    server: &Server,
    node: &str,
    sha256: &str,
    mut stream: DataStream,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let allowed = super::artifacts::used_by_node(server, node)?
        .iter()
        .any(|s| s == sha256);
    let file = match super::artifacts::path(server, sha256) {
        Some(path) if allowed => tokio::fs::File::open(path).await.ok(),
        _ => None,
    };
    let Some(mut file) = file else {
        stream
            .write_all(b"error the executable is not on the server (upload it again)\n")
            .await?;
        return Ok(());
    };
    let size = file.metadata().await?.len();
    stream.write_all(format!("ok {size}\n").as_bytes()).await?;
    tokio::io::copy(&mut file, &mut stream).await?;
    stream.shutdown().await?;
    Ok(())
}

fn on_event(server: &Server, node: &str, event: AgentEvent) {
    let result = match event {
        AgentEvent::Report(report) => {
            server.record_report(node, report);
            Ok(())
        }
        AgentEvent::AppState {
            app,
            state,
            message,
        } => {
            server.set_app_state(&app, state, message);
            Ok(())
        }
        AgentEvent::DeployLog { deployment, line } => {
            server.store.append_deploy_log(deployment, &line)
        }
        AgentEvent::DeployDone { deployment, error } => {
            server.store.finish_deployment(deployment, error.as_deref())
        }
    };
    if let Err(e) = result {
        warn!(%node, "handling agent event: {e:#}");
    }
}
