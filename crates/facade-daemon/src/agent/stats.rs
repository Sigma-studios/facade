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
        }
    }
}
