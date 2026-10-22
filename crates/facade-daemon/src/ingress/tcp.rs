//! TLS-wrapped TCP (IMAPS, SMTPS…): the entry point takes TLS off with the
//! certificate of the app's domains and passes the bytes on, in the clear, to a
//! port of the app.
//!
//! Each public port gets its own listener, started and stopped as the table
//! changes. They are bound with `SO_REUSEPORT`, so a new entry point binds them
//! next to the retiring one during a handover instead of waiting for them.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use socket2::{Domain, SockRef, Socket, TcpKeepalive, Type};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use super::firewall;
use super::{BIND_RETRY, Certs, Counted, Ingress, stopped};

/// Where a TLS port goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TcpRoute {
    pub app: String,
    /// The local port the app's container port is reachable on; `None` while the
    /// app is not running.
    pub port: Option<u16>,
    /// The app's domains, its main one first: whose certificate is shown.
    pub names: Vec<String>,
}

/// How long a client has to finish the TLS handshake, and the app to accept.
const HANDSHAKE: Duration = Duration::from_secs(10);
const CONNECT: Duration = Duration::from_secs(5);

/// The certificate for the name the client asked for (SNI) if it is one of
/// the app's domains, else the main domain's.
#[derive(Debug)]
struct AppCerts {
    certs: Arc<Certs>,
    names: Vec<String>,
}

impl ResolvesServerCert for AppCerts {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let asked = hello
            .server_name()
            .map(str::to_ascii_lowercase)
            .filter(|n| self.names.contains(n));
        asked
            .into_iter()
            .chain(self.names.iter().cloned())
            .find_map(|name| self.certs.find(&name))
            .or_else(|| Some(self.certs.fallback.clone()))
    }
}

impl Ingress {
    /// Starts listeners for the table's TLS ports that have none, and stops
    /// those of ports gone from it.
    pub fn sync_tcp(self: &Arc<Self>) {
        if self.retired.load(Ordering::Relaxed) {
            return;
        }
        let wanted: Vec<u16> = self.table.read().tcp.keys().copied().collect();
        let mut running = self.tcp.lock();
        running.retain(|port, stop| {
            let keep = wanted.contains(port);
            if !keep {
                stop.send_replace(true);
                tokio::spawn(firewall::close(*port));
            }
            keep
        });
        for port in wanted {
            running.entry(port).or_insert_with(|| {
                let (stop_tx, stop) = watch::channel(false);
                tokio::spawn(self.clone().serve_tcp(port, stop));
                stop_tx
            });
        }
    }

    /// Stops accepting on every TLS port, for good (a newer entry point took over).
    pub fn retire(&self) {
        self.retired.store(true, Ordering::Relaxed);
        self.retiring.send_replace(true);
        for (_, stop) in self.tcp.lock().drain() {
            stop.send_replace(true);
        }
    }

    async fn serve_tcp(self: Arc<Self>, port: u16, mut stop: watch::Receiver<bool>) {
        let addr = SocketAddr::from(([0u16; 8], port));
        let mut warned = false;
        let listener = loop {
            match bind_shared(port) {
                Ok(listener) => {
                    self.set_bind_error(addr, None);
                    break listener;
                }
                Err(e) => {
                    let message = format!("cannot listen on port {port} (tls): {e}");
                    if !warned {
                        warn!("{message}; retrying");
                        warned = true;
                    }
                    self.set_bind_error(addr, Some(message));
                    tokio::select! {
                        _ = tokio::time::sleep(BIND_RETRY) => {}
                        _ = stopped(&mut stop) => {
                            self.set_bind_error(addr, None);
                            return;
                        }
                    }
                }
            }
        };
        info!("serving tls on tcp port {port}");
        firewall::open(port).await;
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = stopped(&mut stop) => break,
            };
            let (tcp, peer) = match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("accept on port {port}: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let counted = Counted::new(&self.active);
            let ingress = self.clone();
            tokio::spawn(async move {
                let _counted = counted;
                if let Err(e) = ingress.tcp_connection(tcp, port).await {
                    debug!(%peer, port, "tls connection: {e:#}");
                }
            });
        }
        info!("stopped serving tls on tcp port {port}");
    }

    async fn tcp_connection(&self, tcp: TcpStream, port: u16) -> anyhow::Result<()> {
        let route = self.table.read().tcp.get(&port).cloned();
        let Some(route) = route else {
            anyhow::bail!("no app on this port any more");
        };
        let Some(target) = route.port else {
            anyhow::bail!("{} is not running", route.app);
        };
        keepalive(&tcp);
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(AppCerts {
                certs: self.certs.clone(),
                names: route.names,
            }));
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let mut client = tokio::time::timeout(HANDSHAKE, acceptor.accept(tcp))
            .await
            .map_err(|_| anyhow::anyhow!("TLS handshake timed out"))??;
        let mut app = tokio::time::timeout(CONNECT, TcpStream::connect(("127.0.0.1", target)))
            .await
            .map_err(|_| anyhow::anyhow!("{} did not accept in time", route.app))??;
        let _ = app.set_nodelay(true);
        // No idle timeout: IMAP IDLE keeps a quiet connection for half an hour.
        // Keepalives find peers that vanished.
        tokio::io::copy_bidirectional(&mut client, &mut app).await?;
        Ok(())
    }
}

/// A listener on `port` of every address (IPv6 and IPv4 when the host has
/// IPv6), that a second process may bind as well.
fn bind_shared(port: u16) -> std::io::Result<TcpListener> {
    let v6 = (|| {
        let socket = Socket::new(Domain::IPV6, Type::STREAM, None)?;
        socket.set_only_v6(false)?;
        Ok::<_, std::io::Error>((socket, SocketAddr::from(([0u16; 8], port))))
    })();
    let (socket, addr) = match v6 {
        Ok(v6) => v6,
        Err(_) => (
            Socket::new(Domain::IPV4, Type::STREAM, None)?,
            SocketAddr::from(([0, 0, 0, 0], port)),
        ),
    };
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
}

fn keepalive(tcp: &TcpStream) {
    let _ = tcp.set_nodelay(true);
    let params = TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(15))
        .with_retries(4);
    let _ = SockRef::from(tcp).set_tcp_keepalive(&params);
}

#[cfg(test)]
mod tests {
    use super::super::Table;
    use super::*;
    use facade_core::proto::CertBundle;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A free local port (bound then released).
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[tokio::test]
    async fn takes_tls_off_and_follows_the_table() {
        let dir = std::env::temp_dir().join(format!("facade-tcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ingress = Ingress::new(&dir).unwrap();

        // The app: an echo server, upper-casing.
        let app = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_port = app.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = app.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let up = buf[..n].to_ascii_uppercase();
                        s.write_all(&up).await.unwrap();
                    }
                });
            }
        });

        let generated =
            rcgen::generate_simple_self_signed(vec!["mail.example.com".into()]).unwrap();
        let public = free_port();
        let mut table = Table::default();
        table.certs.push(CertBundle {
            names: vec!["mail.example.com".into()],
            cert_pem: generated.cert.pem(),
            key_pem: generated.signing_key.serialize_pem(),
        });
        table.tcp.insert(
            public,
            TcpRoute {
                app: "valet".into(),
                port: Some(app_port),
                names: vec!["mail.example.com".into()],
            },
        );
        ingress.set_table(table.clone());

        let mut roots = rustls::RootCertStore::empty();
        roots.add(generated.cert.der().clone()).unwrap();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        ));
        let connect = || async {
            for _ in 0..50 {
                if let Ok(tcp) = TcpStream::connect(("127.0.0.1", public)).await {
                    return tcp;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("the TLS port never opened");
        };

        let name = rustls::pki_types::ServerName::try_from("mail.example.com").unwrap();
        let mut tls = connector
            .connect(name.clone(), connect().await)
            .await
            .unwrap();
        tls.write_all(b"a1 noop").await.unwrap();
        let mut buf = [0u8; 7];
        tls.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"A1 NOOP");

        // The port goes away with its route.
        table.tcp.clear();
        ingress.set_table(table);
        let mut closed = false;
        for _ in 0..50 {
            if TcpStream::connect(("127.0.0.1", public)).await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(closed, "the listener stops when its route is gone");
        // Connections already open carry on.
        tls.write_all(b"still").await.unwrap();
        let mut buf = [0u8; 5];
        tls.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"STILL");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_processes_can_share_a_port() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let port = free_port();
            let a = bind_shared(port).unwrap();
            let b = bind_shared(port).unwrap();
            assert_eq!(
                a.local_addr().unwrap().port(),
                b.local_addr().unwrap().port()
            );
        });
    }
}
