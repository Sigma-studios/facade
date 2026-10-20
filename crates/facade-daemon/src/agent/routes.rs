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
use tokio::sync::{Notify, watch};
use tracing::{debug, info};

use super::Agent;
use crate::framing::{JsonLines, write_json};
use crate::ingress::process::{FromIngress, ToIngress};
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
        table: watch::Sender<Table>,
        connected: AtomicBool,
        /// From the entry point's last status.
        bind_errors: Mutex<Vec<String>>,
        upgrade: Notify,
    },
}

impl Link {
    pub fn new(mode: IngressMode, data_dir: &std::path::Path) -> anyhow::Result<Self> {
        Ok(match mode {
            IngressMode::Embedded { http, https } => Link::Embedded {
                ingress: Ingress::new(data_dir)?,
                http,
                https,
            },
            IngressMode::External { control } => Link::External {
                control,
                table: watch::channel(Table::default()).0,
                connected: AtomicBool::new(false),
                bind_errors: Default::default(),
                upgrade: Notify::new(),
            },
        })
    }

    fn publish(&self, new: Table) {
        match self {
            Link::Embedded { ingress, .. } => ingress.set_table(new),
            Link::External { table, .. } => {
                table.send_if_modified(|current| {
                    let changed = *current != new;
                    *current = new;
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
        let workloads: Vec<_> = self.workloads.lock().values().cloned().collect();
        let mut routes = BTreeMap::new();
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
            if let Some(main) = w.spec.domains.first() {
                for domain in &w.spec.redirects {
                    routes.insert(domain.clone(), Route::Redirect { to: main.clone() });
                }
            }
        }
        self.link.publish(Table {
            routes,
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
            upgrade,
        } = &self.link
        else {
            return;
        };
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
            let result: anyhow::Result<()> = async {
                let current = tables.borrow_and_update().clone();
                write_json(&mut write, &ToIngress::Table { table: current }).await?;
                loop {
                    tokio::select! {
                        changed = tables.changed() => {
                            changed?;
                            let current = tables.borrow_and_update().clone();
                            write_json(&mut write, &ToIngress::Table { table: current }).await?;
                        }
                        _ = upgrade.notified() => {
                            write_json(&mut write, &ToIngress::Upgrade).await?;
                        }
                        msg = lines.next::<FromIngress>() => match msg? {
                            Some(FromIngress::Status { bind_errors: errors, pid }) => {
                                debug!(pid, ?errors, "entry point status");
                                *bind_errors.lock() = errors;
                            }
                            None => return Ok(()),
                        },
                    }
                }
            }
            .await;
            connected.store(false, Ordering::Relaxed);
            match result {
                Ok(()) => info!("the entry point closed its connection (handover?), reconnecting"),
                Err(e) => info!("entry point connection: {e:#}"),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}
