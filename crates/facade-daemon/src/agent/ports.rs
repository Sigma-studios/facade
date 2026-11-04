//! What is listening on this machine, from `/proc`: TCP sockets in LISTEN state and
//! unconnected UDP sockets, with the process holding each one and, when it is a
//! facade app, which app; for the rest, what the program is, when it is known
//! (the node's entry point, another container, a system service).
//!
//! Only the host's network namespace is visible here, which is exactly what can
//! conflict with ports an app wants to publish or bind on the host network.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr};

use facade_core::Protocol;
use facade_core::proto::Listener;

/// Who the sockets may belong to, from Docker and the agent.
#[derive(Debug, Default)]
pub struct Owners {
    /// Container id → app, for this node's facade containers.
    pub containers: HashMap<String, String>,
    /// Published host port → app (Docker's own proxy process holds those).
    pub published: HashMap<(Protocol, u16), String>,
    /// Container id → name, for the containers facade does not manage.
    pub other_containers: HashMap<String, String>,
    /// Published host port → container name, for those containers.
    pub other_published: HashMap<(Protocol, u16), String>,
    /// TLS ports the entry point serves → the app it passes them to.
    pub tls_ports: HashMap<u16, String>,
    /// Ports this process serves HTTP(S) on, when the entry point runs in it.
    pub entry_ports: HashSet<u16>,
}

/// Every listener, sorted by protocol and port.
pub fn scan(owners: &Owners) -> Vec<Listener> {
    let mut sockets: Vec<(Protocol, String, u16, u64)> = Vec::new();
    for (file, protocol, v6) in [
        ("/proc/net/tcp", Protocol::Tcp, false),
        ("/proc/net/tcp6", Protocol::Tcp, true),
        ("/proc/net/udp", Protocol::Udp, false),
        ("/proc/net/udp6", Protocol::Udp, true),
    ] {
        if let Ok(table) = std::fs::read_to_string(file) {
            sockets.extend(
                parse_table(&table, protocol, v6)
                    .into_iter()
                    .map(|(addr, port, inode)| (protocol, addr, port, inode)),
            );
        }
    }
    let inodes = socket_owners();
    let me = std::process::id();
    let mut out: Vec<Listener> = sockets
        .into_iter()
        .map(|(protocol, address, port, inode)| {
            let pid = inodes.get(&inode).copied();
            let process = pid.and_then(|p| {
                std::fs::read_to_string(format!("/proc/{p}/comm"))
                    .ok()
                    .map(|s| s.trim().to_string())
            });
            let (app, description) = identify(owners, me, pid, process.as_deref(), protocol, port);
            Listener {
                protocol,
                address,
                port,
                process,
                pid,
                app,
                description,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (a.protocol as u8, a.port, &a.address).cmp(&(b.protocol as u8, b.port, &b.address))
    });
    out.dedup();
    out
}

/// The app a socket belongs to (`facade` for facade itself), and what holds it
/// in words.
fn identify(
    owners: &Owners,
    me: u32,
    pid: Option<u32>,
    process: Option<&str>,
    protocol: Protocol,
    port: u16,
) -> (Option<String>, Option<String>) {
    let tls_app = (protocol == Protocol::Tcp)
        .then(|| owners.tls_ports.get(&port))
        .flatten();
    let entry_point = |http: bool| match tls_app {
        Some(app) => (
            Some(app.clone()),
            Some(format!("façade entry point: TLS for {app}")),
        ),
        None => (
            Some("facade".to_string()),
            http.then(|| "façade entry point".to_string()),
        ),
    };
    let Some(pid) = pid else {
        return (None, None);
    };
    if pid == me {
        return entry_point(owners.entry_ports.contains(&port));
    }
    if process.is_some_and(|p| p.starts_with("facaded")) {
        // The entry point as its own process (`facaded ingress`): a pid of its
        // own, which changes with every handover.
        if is_ingress(pid) {
            return entry_point(true);
        }
        return (Some("facade".into()), None);
    }
    if let Ok(cgroup) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
        if let Some(app) = container_of(&cgroup, &owners.containers) {
            return (Some(app), None);
        }
        if let Some(name) = container_of(&cgroup, &owners.other_containers) {
            return (None, Some(format!("container {name}")));
        }
    }
    let Some(process) = process else {
        return (None, None);
    };
    if matches!(process, "docker-proxy" | "rootlessport") {
        if let Some(app) = owners.published.get(&(protocol, port)) {
            return (Some(app.clone()), None);
        }
        return (
            None,
            Some(match owners.other_published.get(&(protocol, port)) {
                Some(name) => format!("container {name}"),
                None => "a Docker container's published port".into(),
            }),
        );
    }
    (
        None,
        system_service(process, protocol, port).map(str::to_string),
    )
}

/// Whether `pid` runs `facaded ingress`.
fn is_ingress(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
        cmdline
            .split(|b| *b == 0)
            .skip(1)
            .take(4)
            .any(|arg| arg == b"ingress")
    })
}

/// What the usual programs of a Linux server are, by process name (as the
/// kernel truncates it, to 15 characters).
fn system_service(process: &str, protocol: Protocol, port: u16) -> Option<&'static str> {
    Some(match (process, protocol, port) {
        // Socket activation: systemd holds the port until a connection comes.
        ("systemd", Protocol::Tcp, 22) => "SSH (system)",
        ("systemd", _, 111) => "RPC port mapper (system)",
        ("systemd", ..) => "a system service, started by systemd on demand",
        ("sshd" | "sshd-session", ..) => "SSH (system)",
        ("systemd-resolve", ..) => "DNS stub (systemd-resolved)",
        ("systemd-network", Protocol::Udp, 68 | 546) => "DHCP client (systemd-networkd)",
        ("systemd-network", ..) => "network setup (systemd-networkd)",
        ("systemd-timesyn", ..) => "time sync (systemd-timesyncd)",
        ("dhclient" | "dhcpcd" | "udhcpc", ..) => "DHCP client (system)",
        ("NetworkManager", ..) => "network setup (NetworkManager)",
        ("chronyd", ..) => "time sync (chrony)",
        ("ntpd", ..) => "time sync (ntpd)",
        ("rpcbind", ..) => "RPC port mapper (system)",
        ("avahi-daemon", ..) => "mDNS (Avahi)",
        ("cupsd", ..) => "printing (CUPS)",
        ("dnsmasq", ..) => "DNS and DHCP (dnsmasq)",
        ("named", ..) => "DNS server (BIND)",
        ("unbound", ..) => "DNS resolver (Unbound)",
        ("dockerd", ..) => "Docker",
        ("containerd", ..) => "containerd (Docker)",
        ("tailscaled", ..) => "Tailscale",
        ("master", Protocol::Tcp, _) => "mail server (Postfix)",
        ("exim4" | "exim", ..) => "mail server (Exim)",
        ("nginx", ..) => "nginx",
        ("apache2" | "httpd", ..) => "Apache",
        ("caddy", ..) => "Caddy",
        ("haproxy", ..) => "HAProxy",
        ("postgres", ..) => "PostgreSQL",
        ("mysqld" | "mariadbd", ..) => "MySQL",
        ("redis-server", ..) => "Redis",
        ("smbd" | "nmbd", ..) => "file sharing (Samba)",
        ("cockpit-ws", ..) => "Cockpit",
        ("snmpd", ..) => "SNMP",
        _ => return None,
    })
}

/// `(address, port, inode)` of listening entries of one `/proc/net` table.
fn parse_table(table: &str, protocol: Protocol, v6: bool) -> Vec<(String, u16, u64)> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (local, remote, state, inode) = (
                fields.get(1)?,
                fields.get(2)?,
                fields.get(3)?,
                fields.get(9)?,
            );
            let listening = match protocol {
                Protocol::Tcp => *state == "0A",
                // Unconnected UDP sockets have no remote port.
                Protocol::Udp => remote.ends_with(":0000"),
            };
            if !listening {
                return None;
            }
            let (addr, port) = local.split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            Some((decode_addr(addr, v6)?, port, inode.parse().ok()?))
        })
        .collect()
}

/// `/proc/net` addresses are 32-bit words in host byte order.
fn decode_addr(hex: &str, v6: bool) -> Option<String> {
    let words: Vec<u32> = (0..hex.len() / 8)
        .map(|i| u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).ok())
        .collect::<Option<_>>()?;
    if v6 {
        let mut bytes = [0u8; 16];
        for (i, w) in words.iter().enumerate().take(4) {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
        }
        Some(Ipv6Addr::from(bytes).to_string())
    } else {
        Some(Ipv4Addr::from(words.first()?.to_ne_bytes()).to_string())
    }
}

/// Socket inode → pid, from every process's open files.
fn socket_owners() -> HashMap<u64, u32> {
    let mut owners = HashMap::new();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return owners;
    };
    for proc in procs.flatten() {
        let Ok(pid) = proc.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(proc.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(target) = std::fs::read_link(fd.path())
                && let Some(inode) = target
                    .to_str()
                    .and_then(|t| t.strip_prefix("socket:["))
                    .and_then(|t| t.strip_suffix(']'))
                    .and_then(|t| t.parse().ok())
            {
                owners.entry(inode).or_insert(pid);
            }
        }
    }
    owners
}

/// What `containers` says of the container a process runs in, from the
/// container id in its cgroup path.
fn container_of(cgroup: &str, containers: &HashMap<String, String>) -> Option<String> {
    containers
        .iter()
        .find(|(id, _)| cgroup.contains(id.as_str()))
        .map(|(_, v)| v.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_net() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:2382 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 0000000000000000 100 0 0 10 0
   1: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000     0        0 4243 1 0000000000000000 100 0 0 10 0
";
        assert_eq!(
            parse_table(tcp, Protocol::Tcp, false),
            [("0.0.0.0".to_string(), 9090, 4242)]
        );
        let udp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  1: 00000000:0D96 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 77 2 0000000000000000 0
";
        assert_eq!(
            parse_table(udp, Protocol::Udp, false),
            [("0.0.0.0".to_string(), 3478, 77)]
        );
        let tcp6 = "  sl  local_address                         remote_address                        st
   0: 00000000000000000000000000000000:01BB 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 99 1
";
        assert_eq!(
            parse_table(tcp6, Protocol::Tcp, true),
            [("::".to_string(), 443, 99)]
        );
    }

    #[test]
    fn sees_its_own_socket() {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let found = scan(&Owners::default());
        let mine = found
            .iter()
            .find(|l| l.port == port && l.protocol == Protocol::Tcp)
            .expect("listed");
        assert_eq!(mine.address, "127.0.0.1");
        assert_eq!(mine.app.as_deref(), Some("facade"));
        assert_eq!(mine.description, None);
    }

    #[test]
    fn names_what_holds_a_port() {
        let me = std::process::id();
        let owners = Owners {
            tls_ports: HashMap::from([(993, "valet".to_string())]),
            entry_ports: HashSet::from([443]),
            published: HashMap::from([((Protocol::Tcp, 8080), "api".to_string())]),
            other_published: HashMap::from([((Protocol::Tcp, 3001), "grafana".to_string())]),
            ..Default::default()
        };
        let id = |pid, process, protocol, port| identify(&owners, me, pid, process, protocol, port);
        assert_eq!(
            id(Some(me), Some("facaded"), Protocol::Tcp, 993),
            (
                Some("valet".into()),
                Some("façade entry point: TLS for valet".into())
            )
        );
        assert_eq!(
            id(Some(me), Some("facaded"), Protocol::Tcp, 443),
            (Some("facade".into()), Some("façade entry point".into()))
        );
        assert_eq!(
            id(Some(me), Some("facaded"), Protocol::Tcp, 7701),
            (Some("facade".into()), None)
        );
        // Pids that are not this test's: /proc has nothing to say about them.
        let other = u32::MAX - 1;
        assert_eq!(
            id(Some(other), Some("docker-proxy"), Protocol::Tcp, 8080),
            (Some("api".into()), None)
        );
        assert_eq!(
            id(Some(other), Some("docker-proxy"), Protocol::Tcp, 3001),
            (None, Some("container grafana".into()))
        );
        assert_eq!(
            id(Some(1), Some("systemd"), Protocol::Tcp, 22).1.as_deref(),
            Some("SSH (system)")
        );
        assert_eq!(
            id(Some(other), Some("systemd-resolve"), Protocol::Udp, 53)
                .1
                .as_deref(),
            Some("DNS stub (systemd-resolved)")
        );
        assert_eq!(
            id(Some(other), Some("systemd-network"), Protocol::Udp, 68)
                .1
                .as_deref(),
            Some("DHCP client (systemd-networkd)")
        );
        assert_eq!(
            id(Some(other), Some("mystery"), Protocol::Tcp, 9),
            (None, None)
        );
    }
}
