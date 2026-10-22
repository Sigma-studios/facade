//! `facaded ingress`: the entry point as its own long-lived process.
//!
//! * It listens on the node's HTTP and HTTPS ports (and the apps' TLS ports) and
//!   on a unix control socket, where the node's agent sends it the routing
//!   [`Table`] and reads back its status (addresses it cannot bind).
//! * On `SIGUSR2` (`systemctl reload facaded-ingress`), or when the agent asks
//!   after updating itself, it starts the binary now installed at its own path
//!   and hands it the listening sockets. Once the new process says it is ready,
//!   this one stops accepting, lets its open connections (WebSockets included)
//!   finish, and exits. Nothing is refused meanwhile, and if the new binary does
//!   not come up, this one simply carries on. TLS ports are not handed over:
//!   the new process binds them alongside (`SO_REUSEPORT`).
//! * Sockets are passed as `FACADE_INGRESS_FDS=http=3,https=4,control=5`; systemd
//!   socket activation (`LISTEN_FDS` with `FileDescriptorName=http|https|control`)
//!   works the same way.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::{Notify, watch};
use tracing::{info, warn};

use super::{Ingress, Table, stopped};
use crate::framing::{JsonLines, write_json};

const FDS_ENV: &str = "FACADE_INGRESS_FDS";
const READY_ENV: &str = "FACADE_INGRESS_READY_FD";
/// How long the new process has to say it is ready.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);

/// Agent → entry point.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // sent rarely
pub enum ToIngress {
    /// Replaces the routing table.
    Table { table: Table },
    /// Hand over to the binary now installed (after an agent self-update).
    Upgrade,
}

/// Entry point → agent, on connection and whenever it changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromIngress {
    Status { bind_errors: Vec<String>, pid: u32 },
}

pub struct Options {
    pub http: Option<SocketAddr>,
    pub https: Option<SocketAddr>,
    pub control: PathBuf,
    pub data_dir: PathBuf,
    /// Longest a retired process waits for its connections to end.
    pub drain: Duration,
}

/// Listening sockets this process owns, kept as file descriptors to pass on.
#[derive(Default)]
struct Sockets(Mutex<Vec<(String, OwnedFd)>>);

impl Sockets {
    fn keep(&self, name: &str, fd: RawFd) -> anyhow::Result<()> {
        // A duplicate above the range children receive sockets in, closed on exec
        // unless handed over on purpose.
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 100) };
        if dup < 0 {
            return Err(std::io::Error::last_os_error()).context("duplicating a socket");
        }
        self.0
            .lock()
            .push((name.to_string(), unsafe { OwnedFd::from_raw_fd(dup) }));
        Ok(())
    }
}

pub async fn run(opts: Options) -> anyhow::Result<()> {
    let ingress = Ingress::new(&opts.data_dir)?;
    // The TLS ports of the table last saved, until the agent sends a new one.
    ingress.sync_tcp();
    let mut inherited = take_inherited();
    let sockets = Arc::new(Sockets::default());
    let (stop_tx, stop) = watch::channel(false);
    let upgrade = Arc::new(Notify::new());

    for (name, addr, tls) in [("http", opts.http, false), ("https", opts.https, true)] {
        let Some(addr) = addr else { continue };
        let listener = inherited
            .remove(name)
            .and_then(|fd| tcp_listener(fd).ok())
            .filter(|l| l.local_addr().ok() == Some(addr));
        let (ingress, sockets, stop) = (ingress.clone(), sockets.clone(), stop.clone());
        tokio::spawn(async move {
            let listener = match listener {
                Some(l) => l,
                // Taken over from nobody, or from a process configured with
                // another address: bind it, retrying while it is busy.
                None => ingress.bind(addr, tls).await,
            };
            if let Err(e) = sockets.keep(name, listener.as_raw_fd()) {
                warn!("{e:#}: this socket will not survive the next handover");
            }
            ingress.serve(listener, tls, stop).await;
        });
    }

    let control = match inherited.remove("control").map(unix_listener) {
        Some(Ok(l)) => l,
        _ => bind_control(&opts.control)?,
    };
    sockets.keep("control", control.as_raw_fd())?;
    tokio::spawn(control_loop(
        control,
        ingress.clone(),
        upgrade.clone(),
        stop.clone(),
    ));

    signal_ready();
    info!("entry point ready (pid {})", std::process::id());

    let mut usr2 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2())?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = usr2.recv() => {}
            _ = upgrade.notified() => {}
            _ = term.recv() => return Ok(()),
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
        info!("handing the entry point over to the installed binary");
        match hand_over(&sockets).await {
            Ok(pid) => {
                info!("process {pid} took over; finishing open connections");
                stop_tx.send_replace(true);
                ingress.retire();
                break;
            }
            Err(e) => warn!("handover failed, still serving: {e:#}"),
        }
    }

    let started = Instant::now();
    while ingress.active_connections() > 0 && started.elapsed() < opts.drain {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    info!(
        "retiring with {} connection(s) still open",
        ingress.active_connections()
    );
    Ok(())
}

/// Sockets passed by a previous entry point or by systemd, by name.
fn take_inherited() -> HashMap<String, OwnedFd> {
    let mut out = HashMap::new();
    if let Ok(spec) = std::env::var(FDS_ENV) {
        for part in spec.split(',') {
            if let Some((name, fd)) = part.split_once('=')
                && let Ok(fd) = fd.parse::<RawFd>()
            {
                out.insert(name.to_string(), adopt(fd));
            }
        }
    } else if std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        == Some(std::process::id())
    {
        let count: RawFd = std::env::var("LISTEN_FDS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let names = std::env::var("LISTEN_FDNAMES").unwrap_or_default();
        let names: Vec<&str> = names.split(':').collect();
        for i in 0..count {
            let name = names.get(i as usize).copied().unwrap_or("unknown");
            out.insert(name.to_string(), adopt(3 + i));
        }
    }
    // Not for whatever this process starts later.
    for var in [FDS_ENV, "LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        unsafe { std::env::remove_var(var) };
    }
    out
}

/// Takes ownership of an inherited descriptor, closed on exec from now on.
fn adopt(fd: RawFd) -> OwnedFd {
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        OwnedFd::from_raw_fd(fd)
    }
}

fn tcp_listener(fd: OwnedFd) -> std::io::Result<TcpListener> {
    let listener = std::net::TcpListener::from(fd);
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

fn unix_listener(fd: OwnedFd) -> std::io::Result<UnixListener> {
    let listener = std::os::unix::net::UnixListener::from(fd);
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener)
}

fn bind_control(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A socket left by a process that is gone (a live one hands its socket over
    // instead, so a stale file is never in use).
    let _ = std::fs::remove_file(path);
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        // Certificates' private keys go through it.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(listener)
}

async fn control_loop(
    listener: UnixListener,
    ingress: Arc<Ingress>,
    upgrade: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    warn!("control socket: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            },
            _ = stopped(&mut stop) => return,
        };
        tokio::spawn(control_connection(
            stream,
            ingress.clone(),
            upgrade.clone(),
            stop.clone(),
        ));
    }
}

/// One agent: tables in, status out. Closed at handover, so the agent
/// reconnects to the new process.
async fn control_connection(
    stream: UnixStream,
    ingress: Arc<Ingress>,
    upgrade: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) {
    let (read, mut write) = stream.into_split();
    let mut lines = JsonLines::new(read);
    let mut status = ingress.status.subscribe();
    let pid = std::process::id();
    let send_status = |ingress: &Ingress| FromIngress::Status {
        bind_errors: ingress.bind_errors(),
        pid,
    };
    if write_json(&mut write, &send_status(&ingress))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            msg = lines.next::<ToIngress>() => match msg {
                Ok(Some(ToIngress::Table { table })) => ingress.set_table(table),
                Ok(Some(ToIngress::Upgrade)) => upgrade.notify_one(),
                Ok(None) => return,
                Err(e) => {
                    warn!("control connection: {e:#}");
                    return;
                }
            },
            changed = status.changed() => {
                if changed.is_err()
                    || write_json(&mut write, &send_status(&ingress)).await.is_err()
                {
                    return;
                }
            }
            _ = stopped(&mut stop) => return,
        }
    }
}

/// Starts the installed binary with our arguments and sockets, and waits for it
/// to report ready. Returns its pid.
async fn hand_over(sockets: &Sockets) -> anyhow::Result<u32> {
    let binary = crate::selfbin::installed_path()?;
    let (ready_read, ready_write) = pipe()?;
    let (names, sources): (Vec<String>, Vec<RawFd>) = {
        let sockets = sockets.0.lock();
        sockets
            .iter()
            .map(|(name, fd)| (name.clone(), fd.as_raw_fd()))
            .unzip()
    };
    let spec: Vec<String> = names
        .iter()
        .enumerate()
        .map(|(i, name)| format!("{name}={}", 3 + i))
        .collect();
    let ready_fd = 3 + names.len() as RawFd;
    let mut handed: Vec<RawFd> = sources;
    handed.push(ready_write.as_raw_fd());

    let mut cmd = std::process::Command::new(&binary);
    cmd.args(std::env::args_os().skip(1))
        .env(FDS_ENV, spec.join(","))
        .env(READY_ENV, ready_fd.to_string());
    // SAFETY: only async-signal-safe calls between fork and exec. Sources are
    // all >= 100 (see `Sockets::keep`), so no target overwrites a source.
    unsafe {
        cmd.pre_exec(move || {
            for (i, fd) in handed.iter().enumerate() {
                if libc::dup2(*fd, 3 + i as RawFd) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))?;
    let pid = child.id();
    drop(ready_write);

    // `ready` once it serves; end of file if it died first.
    let wait = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::from(ready_read);
        let mut buf = String::new();
        let _ = file.read_to_string(&mut buf);
        buf
    });
    match tokio::time::timeout(HANDOFF_TIMEOUT, wait).await {
        Ok(Ok(answer)) if answer.trim() == "ready" => Ok(pid),
        Ok(_) => {
            let status = child.try_wait().ok().flatten();
            bail!(
                "{} exited before it was ready{}",
                binary.display(),
                status.map(|s| format!(" ({s})")).unwrap_or_default()
            )
        }
        Err(_) => {
            let _ = child.kill();
            bail!(
                "{} was not ready within {HANDOFF_TIMEOUT:?}",
                binary.display()
            )
        }
    }
}

fn pipe() -> anyhow::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error()).context("creating a pipe");
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Tells whoever started us that we serve: the previous entry point (through
/// the pipe it gave us) and systemd (which then tracks this process).
fn signal_ready() {
    if let Some(fd) = std::env::var(READY_ENV)
        .ok()
        .and_then(|fd| fd.parse::<RawFd>().ok())
    {
        let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let _ = file.write_all(b"ready\n");
        unsafe { std::env::remove_var(READY_ENV) };
    }
    notify_systemd(&format!("MAINPID={}\nREADY=1", std::process::id()));
}

fn notify_systemd(message: &str) {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr as UnixAddr, UnixDatagram};
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let path = path.to_string_lossy().to_string();
    let addr = match path.strip_prefix('@') {
        Some(name) => UnixAddr::from_abstract_name(name.as_bytes()),
        None => UnixAddr::from_pathname(&path),
    };
    let sent = UnixDatagram::unbound()
        .and_then(|socket| addr.and_then(|addr| socket.send_to_addr(message.as_bytes(), &addr)));
    if let Err(e) = sent {
        warn!("notifying systemd: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_with_tls_ports_survives_the_control_connection() {
        let mut table = Table::default();
        table.tcp.insert(
            465,
            super::super::tcp::TcpRoute { app: "valet".into(), port: Some(32780), names: vec!["mail.example.com".into()] },
        );
        let line = serde_json::to_string(&ToIngress::Table { table: table.clone() }).unwrap();
        match serde_json::from_str::<ToIngress>(&line).unwrap() {
            ToIngress::Table { table: back } => assert_eq!(back, table),
            other => panic!("{other:?}"),
        }
    }
}
