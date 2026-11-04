//! What the node's entry point serves: the agent turns its workloads, the
//! certificates and the ACME answers from the server into a routing table, and
//! hands it to the entry point, in this process or in `facaded ingress`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::net::UnixStream;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tracing::{debug, info};

use super::Agent;
use crate::framing::{JsonLines, write_json};
use crate::ingress::process::{FromIngress, ToIngress};
use crate::ingress::tcp::TcpRoute;
use crate::ingress::{Ingress, Route, Table};

/// The table is rebuilt this often anyway, so a container restarted by Docker
/// (on a new local port) is found again quickly.
const REFRESH_EVERY: Duration = Duration::from_secs(5);

/// Where the node's entry point runs.
#[derive(Debug, Clone)]
pub enum IngressMode {
    /// In this process, listening on these addresses (`None` disables one).
    Embedded {
        http: Option<SocketAddr>,
        https: Option<SocketAddr>,
    },
    /// In `facaded ingress`, reached through this control socket.
    External { control: PathBuf },
}

pub enum Link {
    Embedded {
        ingress: Arc<Ingress>,
        http: Option<SocketAddr>,
        https: Option<SocketAddr>,
    },
    External {
        control: PathBuf,
        /// `None` until the first table: an agent just started knows no app
        /// yet, and must not empty the entry point's routes.
        table: watch::Sender<Option<Table>>,
        connected: AtomicBool,
        /// From the entry point's last status.
        bind_errors: Mutex<Vec<String>>,
        /// Whether it counts traffic (older ones do not know the question).
        counts_traffic: AtomicBool,
        upgrade: Notify,
        /// Questions about open connections, sent after the latest table: an
        /// answer also says the entry point uses that table.
        queries: mpsc::UnboundedSender<Query>,
        answers: tokio::sync::Mutex<mpsc::UnboundedReceiver<Query>>,
    },
}

/// A question for the entry point, and where its answer goes.
pub enum Query {
    /// How many connections are open to these local ports.
    Connections(Vec<u16>, oneshot::Sender<usize>),
    /// The apps' traffic counters.
    Traffic(oneshot::Sender<AppsTraffic>),
}

type AppsTraffic = BTreeMap<String, facade_core::proto::Traffic>;

/// An answer to wait for, in the order asked.
enum Waiting {
    Connections(oneshot::Sender<usize>),
    Traffic(oneshot::Sender<AppsTraffic>),
}

/// How long the entry point has to answer a question.
const ANSWER_WAIT: Duration = Duration::from_secs(5);

impl Link {
    pub fn new(mode: IngressMode, data_dir: &std::path::Path) -> anyhow::Result<Self> {
        Ok(match mode {
            IngressMode::Embedded { http, https } => Link::Embedded {
                ingress: Ingress::new(data_dir)?,
                http,
                https,
            },
            IngressMode::External { control } => {
                let (queries, answers) = mpsc::unbounded_channel();
                Link::External {
                    control,
                    table: watch::channel(None).0,
                    connected: AtomicBool::new(false),
                    bind_errors: Default::default(),
                    counts_traffic: AtomicBool::new(false),
                    upgrade: Notify::new(),
                    queries,
                    answers: tokio::sync::Mutex::new(answers),
                }
            }
        })
    }

    /// Requests and connections the entry point has open to these local ports,
    /// once it uses the table last published; `None` if it does not answer (not
    /// running, or older than this question).
    pub async fn open_connections(&self, ports: &[u16]) -> Option<usize> {
        match self {
            Link::Embedded { ingress, .. } => Some(ingress.open_connections(ports)),
            Link::External {
                queries, connected, ..
            } => {
                if !connected.load(Ordering::Relaxed) {
                    return None;
                }
                let (tx, rx) = oneshot::channel();
                queries.send(Query::Connections(ports.to_vec(), tx)).ok()?;
                tokio::time::timeout(ANSWER_WAIT, rx).await.ok()?.ok()
            }
        }
    }

    /// The traffic counters of the apps the entry point routes to; `None` if it
    /// cannot tell (not running, or older than counting).
    pub async fn traffic(&self) -> Option<AppsTraffic> {
        match self {
            Link::Embedded { ingress, .. } => Some(ingress.traffic()),
            Link::External {
                queries,
                connected,
                counts_traffic,
                ..
            } => {
                if !connected.load(Ordering::Relaxed) || !counts_traffic.load(Ordering::Relaxed) {
                    return None;
                }
                let (tx, rx) = oneshot::channel();
                queries.send(Query::Traffic(tx)).ok()?;
                tokio::time::timeout(ANSWER_WAIT, rx).await.ok()?.ok()
            }
        }
    }

    fn publish(&self, new: Table) {
        match self {
            Link::Embedded { ingress, .. } => ingress.set_table(new),
            Link::External { table, .. } => {
                table.send_if_modified(|current| {
                    let changed = current.as_ref() != Some(&new);
                    *current = Some(new);
                    changed
                });
            }
        }
    }

    /// Problems to report to the server.
    pub fn errors(&self) -> Vec<String> {
        match self {
            Link::Embedded { ingress, .. } => ingress.bind_errors(),
            Link::External {
                connected,
                bind_errors,
                control,
                ..
            } => {
                if connected.load(Ordering::Relaxed) {
                    bind_errors.lock().clone()
                } else {
                    vec![format!(
                        "the entry point (facaded-ingress) is not running: nothing answers at {}",
                        control.display()
                    )]
                }
            }
        }
    }

    /// The ports this process serves HTTP(S) on, when the entry point runs in it.
    pub fn embedded_ports(&self) -> std::collections::HashSet<u16> {
        match self {
            Link::Embedded { http, https, .. } => [http, https]
                .into_iter()
                .flatten()
                .map(|a| a.port())
                .collect(),
            Link::External { .. } => Default::default(),
        }
    }

    /// Asks an external entry point to hand over to the binary now installed.
    pub fn request_upgrade(&self) {
        if let Link::External { upgrade, .. } = self {
            upgrade.notify_one();
        }
    }
}

impl Agent {
    /// Starts the entry point (embedded) or the connection to it (external), and
    /// the periodic table refresh.
    pub(super) fn start_routing(self: &Arc<Self>) {
        match &self.link {
            Link::Embedded {
                ingress,
                http,
                https,
            } => {
                for (addr, tls) in [(*http, false), (*https, true)] {
                    if let Some(addr) = addr {
                        tokio::spawn(ingress.clone().listen(addr, tls));
                    }
                }
                ingress.sync_tcp();
            }
            Link::External { .. } => {
                tokio::spawn(self.clone().ingress_connection_loop());
            }
        }
        let agent = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REFRESH_EVERY);
            loop {
                tick.tick().await;
                agent.refresh_routes().await;
            }
        });
    }

    /// Rebuilds the routing table from the current state and publishes it.
    pub(super) async fn refresh_routes(&self) {
        // Until the server has said which apps run here, an empty table would
        // take every route down (right after the agent restarts).
        if !self.synced.load(Ordering::Relaxed) {
            return;
        }
        let _routing = self.routing.lock().await;
        let workloads: Vec<_> = self.workloads.lock().values().cloned().collect();
        let mut routes = BTreeMap::new();
        let mut tcp = BTreeMap::new();
        for proxy in self.proxies.lock().iter() {
            routes.insert(
                proxy.domain.clone(),
                Route::Port {
                    app: "facade".into(),
                    port: proxy.port,
                },
            );
        }
        for w in workloads.iter().filter(|w| w.spec.enabled) {
            let app = w.spec.name.clone();
            let target = if w.spec.is_static() {
                match self.runtime.static_root(w) {
                    Ok(root) => Route::Static {
                        app: app.clone(),
                        root,
                    },
                    Err(_) => Route::Unavailable { app: app.clone() },
                }
            } else {
                match self.runtime.http_target(w).await {
                    Ok(addr) => Route::Port {
                        app: app.clone(),
                        port: addr.port(),
                    },
                    Err(_) => Route::Unavailable { app: app.clone() },
                }
            };
            for domain in &w.spec.domains {
                routes.insert(domain.clone(), target.clone());
            }
            for t in &w.spec.tls_ports {
                let port = self
                    .runtime
                    .loopback_target(w, t.container)
                    .await
                    .ok()
                    .map(|a| a.port());
                tcp.insert(
                    t.public,
                    TcpRoute {
                        app: app.clone(),
                        port,
                        names: w.spec.domains.clone(),
                    },
                );
            }
            if let Some(main) = w.spec.domains.first() {
                for domain in &w.spec.redirects {
                    routes.insert(domain.clone(), Route::Redirect { to: main.clone() });
                }
            }
        }
        self.link.publish(Table {
            routes,
            tcp,
            certs: self.certs.lock().clone(),
            challenges: self.challenges.lock().clone(),
        });
    }

    /// Keeps a connection to `facaded ingress`: tables out, status in.
    async fn ingress_connection_loop(self: Arc<Self>) {
        let Link::External {
            control,
            table,
            connected,
            bind_errors,
            counts_traffic,
            upgrade,
            answers,
            ..
        } = &self.link
        else {
            return;
        };
        let mut questions = answers.lock().await;
        let mut logged = false;
        loop {
            let stream = match UnixStream::connect(control).await {
                Ok(stream) => stream,
                Err(e) => {
                    if !logged {
                        info!("waiting for the entry point at {}: {e}", control.display());
                        logged = true;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            logged = false;
            connected.store(true, Ordering::Relaxed);
            let (read, mut write) = stream.into_split();
            let mut lines = JsonLines::new(read);
            let mut tables = table.subscribe();
            // Answered in the order asked.
            let mut waiting = std::collections::VecDeque::<Waiting>::new();
            let result: anyhow::Result<()> = async {
                let current = tables.borrow_and_update().clone();
                if let Some(table) = current {
                    write_json(&mut write, &ToIngress::Table { table }).await?;
                }
                loop {
                    tokio::select! {
                        changed = tables.changed() => {
                            changed?;
                            let current = tables.borrow_and_update().clone();
                            if let Some(table) = current {
                                write_json(&mut write, &ToIngress::Table { table }).await?;
                            }
                        }
                        _ = upgrade.notified() => {
                            write_json(&mut write, &ToIngress::Upgrade).await?;
                        }
                        Some(query) = questions.recv() => {
                            // Asked after a table was published: that table first.
                            if tables.has_changed()? {
                                let current = tables.borrow_and_update().clone();
                                if let Some(table) = current {
                                    write_json(&mut write, &ToIngress::Table { table }).await?;
                                }
                            }
                            let (question, answer) = match query {
                                Query::Connections(ports, answer) => {
                                    (ToIngress::Connections { ports }, Waiting::Connections(answer))
                                }
                                Query::Traffic(answer) => (ToIngress::Traffic, Waiting::Traffic(answer)),
                            };
                            write_json(&mut write, &question).await?;
                            waiting.push_back(answer);
                        }
                        msg = lines.next::<FromIngress>() => match msg? {
                            Some(FromIngress::Status { bind_errors: errors, pid, traffic }) => {
                                debug!(pid, ?errors, "entry point status");
                                *bind_errors.lock() = errors;
                                counts_traffic.store(traffic, Ordering::Relaxed);
                            }
                            Some(FromIngress::Connections { open }) => {
                                if let Some(Waiting::Connections(answer)) = waiting.pop_front() {
                                    let _ = answer.send(open);
                                }
                            }
                            Some(FromIngress::Traffic { apps }) => {
                                if let Some(Waiting::Traffic(answer)) = waiting.pop_front() {
                                    let _ = answer.send(apps);
                                }
                            }
                            None => return Ok(()),
                        },
                    }
                }
            }
            .await;
            connected.store(false, Ordering::Relaxed);
            counts_traffic.store(false, Ordering::Relaxed);
            match result {
                Ok(()) => info!("the entry point closed its connection (handover?), reconnecting"),
                Err(e) => info!("entry point connection: {e:#}"),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}
