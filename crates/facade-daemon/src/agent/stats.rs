use facade_core::proto::NodeStats;
use sysinfo::{Disks, System};

pub struct NodeSampler {
    system: System,
    disks: Disks,
}

impl NodeSampler {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        Self {
            system,
            disks: Disks::new_with_refreshed_list(),
        }
    }

    /// CPU usage is measured since the previous call.
    pub fn sample(&mut self) -> NodeStats {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.disks.refresh(true);
        // The root filesystem is where Docker and builds live.
        let root = self
            .disks
            .list()
            .iter()
            .find(|d| d.mount_point() == std::path::Path::new("/"))
            .or_else(|| self.disks.list().first());
        let (disk_total, disk_used) = root
            .map(|d| (d.total_space(), d.total_space() - d.available_space()))
            .unwrap_or((0, 0));
        let traffic = network_traffic();
        NodeStats {
            arch: std::env::consts::ARCH.to_string(),
            cpus: self.system.cpus().len(),
            cpu_percent: self.system.global_cpu_usage(),
            load1: System::load_average().one,
            mem_used: self.system.used_memory(),
            mem_total: self.system.total_memory(),
            disk_used,
            disk_total,
            uptime: System::uptime(),
            net_rx: traffic.0,
            net_tx: traffic.1,
        }
    }
}

/// Bytes received and sent since boot by the node's network cards: the
/// interfaces backed by a device, so Docker's bridges and the containers'
/// virtual pairs (which carry the same packets again) are left out. A machine
/// whose interfaces show no device counts all but loopback and Docker's.
fn network_traffic() -> (u64, u64) {
    let Ok(table) = std::fs::read_to_string("/proc/net/dev") else {
        return (0, 0);
    };
    let rows: Vec<(String, u64, u64)> = table
        .lines()
        .skip(2)
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let fields: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            Some((name.trim().to_string(), *fields.first()?, *fields.get(8)?))
        })
        .collect();
    let physical =
        |name: &str| std::path::Path::new(&format!("/sys/class/net/{name}/device")).exists();
    let virtual_ = |name: &str| {
        name == "lo"
            || [
                "veth", "docker", "br-", "virbr", "cni", "flannel", "cali", "vxlan",
            ]
            .iter()
            .any(|p| name.starts_with(p))
    };
    let cards: Vec<_> = rows.iter().filter(|(n, ..)| physical(n)).collect();
    let counted: Vec<_> = if cards.is_empty() {
        rows.iter().filter(|(n, ..)| !virtual_(n)).collect()
    } else {
        cards
    };
    counted
        .iter()
        .fold((0, 0), |(rx, tx), (_, r, t)| (rx + r, tx + t))
}
