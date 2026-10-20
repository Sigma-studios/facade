//! What is listening on this machine, from `/proc`: TCP sockets in LISTEN state and
//! unconnected UDP sockets, with the process holding each one and, when it is a
//! facade app, which app.
//!
//! Only the host's network namespace is visible here, which is exactly what can
//! conflict with ports an app wants to publish or bind on the host network.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};

use facade_core::Protocol;
use facade_core::proto::Listener;

/// Every listener, sorted by protocol and port. `containers` maps container ids
/// to apps, `published` maps published host ports to apps (Docker's own proxy
/// process holds those).
pub fn scan(
    containers: &HashMap<String, String>,
    published: &HashMap<(Protocol, u16), String>,
) -> Vec<Listener> {
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
    let owners = socket_owners();
    let me = std::process::id();
    let mut out: Vec<Listener> = sockets
        .into_iter()
        .map(|(protocol, address, port, inode)| {
            let pid = owners.get(&inode).copied();
            let process = pid.and_then(|p| {
                std::fs::read_to_string(format!("/proc/{p}/comm"))
                    .ok()
                    .map(|s| s.trim().to_string())
            });
            let app = if pid == Some(me) {
                Some("facade".to_string())
            } else if let Some(app) = pid.and_then(|p| container_of(p, containers)) {
                Some(app)
            } else if matches!(process.as_deref(), Some("docker-proxy" | "rootlessport")) {
                published.get(&(protocol, port)).cloned()
            } else {
                None
            };
            Listener {
                protocol,
                address,
                port,
                process,
                pid,
                app,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (a.protocol as u8, a.port, &a.address).cmp(&(b.protocol as u8, b.port, &b.address))
    });
    out.dedup();
    out
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

/// The app whose container runs `pid`, from the container id in its cgroup path.
fn container_of(pid: u32, containers: &HashMap<String, String>) -> Option<String> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    containers
        .iter()
        .find(|(id, _)| cgroup.contains(id.as_str()))
        .map(|(_, app)| app.clone())
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
        let found = scan(&HashMap::new(), &HashMap::new());
        let mine = found
            .iter()
            .find(|l| l.port == port && l.protocol == Protocol::Tcp)
            .expect("listed");
        assert_eq!(mine.address, "127.0.0.1");
        assert_eq!(mine.app.as_deref(), Some("facade"));
    }
}
