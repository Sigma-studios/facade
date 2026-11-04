//! What agents measure beyond CPU, memory and network: an app's CPU throttling,
//! disk reads and writes, processes and health checks; a node's pressure (Linux
//! PSI), disk reads and writes and time waiting on disk.
//!
//! Agents send cumulative counters, as they do for network bytes; the server
//! keeps the last ones of each 5-minute bucket and turns what they grew by into
//! rates when history is read. A figure the agent cannot read (cgroup v1, an
//! older kernel, a file missing) is `None`: nothing is shown rather than zeros.

use serde::{Deserialize, Serialize};

/// CFS bandwidth counters of a container with a CPU limit (cgroup `cpu.stat`):
/// enforcement periods (100 ms each) it ran in, those it was held back in, and
/// for how long in all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuThrottle {
    pub periods: u64,
    pub throttled: u64,
    pub throttled_usec: u64,
}

impl CpuThrottle {
    /// The share, in percent, of the periods between two readings in which the
    /// app was held back; `None` when the counters went back (a new container).
    /// An app that did not run at all was not held back.
    pub fn share_between(a: &Self, b: &Self) -> Option<f64> {
        if b.periods < a.periods || b.throttled < a.throttled {
            return None;
        }
        let periods = b.periods - a.periods;
        Some(if periods == 0 {
            0.0
        } else {
            ((b.throttled - a.throttled) as f64 * 100.0 / periods as f64).min(100.0)
        })
    }
}

/// Bytes read from and written to block devices: by a container since it
/// started (cgroup `io.stat`), by a node's root disk since boot (`/proc/diskstats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskIo {
    pub read: u64,
    pub written: u64,
}

/// One run of a container's health check (Docker keeps the last five).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthCheck {
    /// When it started, in milliseconds since the epoch.
    pub at_ms: u64,
    pub ok: bool,
    /// How long it took, in milliseconds.
    pub ms: u64,
    /// The end of what it printed, for failures.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
}

/// Failures in a row at the end of `checks` (oldest first).
pub fn failing_streak(checks: &[HealthCheck]) -> usize {
    checks.iter().rev().take_while(|c| !c.ok).count()
}

/// What an app's container reports beyond CPU, memory and network.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppResources {
    /// Only for an app with a CPU limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle: Option<CpuThrottle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_io: Option<DiskIo>,
    /// Processes and threads running now, and how many it may run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids_limit: Option<u64>,
    /// Docker's last results of its health check, oldest first; empty without one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub health: Vec<HealthCheck>,
}

impl AppResources {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether it runs close to its limit of processes (over 80 %).
    pub fn pids_warning(&self) -> Option<(u64, u64)> {
        let (now, limit) = (self.pids?, self.pids_limit?);
        (limit > 0 && now * 5 > limit * 4).then_some((now, limit))
    }
}

/// One line of a `/proc/pressure/*` file: the share of time (percent) some
/// tasks waited for the resource, over the last 10 and 60 seconds, and the
/// total time they did since boot, in microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Psi {
    pub avg10: f64,
    pub avg60: f64,
    pub total: u64,
}

/// A node's pressure stall information ("some" lines); a resource the kernel
/// does not report is `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Pressure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Psi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Psi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io: Option<Psi>,
}

/// How strained a node is, from its pressure over the last minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strain {
    Low,
    Moderate,
    High,
}

impl std::fmt::Display for Strain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Strain::Low => "low",
            Strain::Moderate => "moderate",
            Strain::High => "high",
        })
    }
}

/// Shares of the last minute (percent) at which tasks waiting make a node's
/// strain moderate, then high: CPU, memory, disk. Waiting for CPU is normal on
/// a busy machine; waiting for memory (reclaim, swap) or disk hurts sooner.
pub const STRAIN_THRESHOLDS: [(f64, f64); 3] = [(25.0, 60.0), (5.0, 20.0), (10.0, 30.0)];

/// How the strain is judged, for a tooltip.
pub const STRAIN_HELP: &str = "Share of the last minute some tasks waited (Linux pressure stall information). Moderate from 25 % for CPU, 5 % for memory, 10 % for disk; high from 60 %, 20 %, 30 %.";

impl Pressure {
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.memory.is_none() && self.io.is_none()
    }

    /// The worst of its resources' strain; `None` without any.
    pub fn strain(&self) -> Option<Strain> {
        [self.cpu, self.memory, self.io]
            .iter()
            .zip(STRAIN_THRESHOLDS)
            .filter_map(|(psi, (moderate, high))| {
                let avg = psi.as_ref()?.avg60;
                Some(if avg >= high {
                    Strain::High
                } else if avg >= moderate {
                    Strain::Moderate
                } else {
                    Strain::Low
                })
            })
            .max()
    }
}

/// CPU time since boot, in clock ticks: all of it, and what was idle waiting
/// on disk (`/proc/stat`), for kernels without pressure information.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTicks {
    pub total: u64,
    pub iowait: u64,
}

/// What a node reports beyond CPU, memory, disk space and network.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<Pressure>,
    /// Its root disk's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_io: Option<DiskIo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ticks: Option<CpuTicks>,
}

impl NodeResources {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The figures of a [`crate::api::UsagePoint`] that come from these counters,
/// over the point's period; `None` where not known.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourcePoint {
    /// Percent of the CPU limit's periods the app was held back in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttled: Option<f64>,
    /// Bytes per second read from and written to disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_write: Option<f64>,
    /// Percent of the time some tasks waited for CPU, memory, disk (nodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure_cpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure_mem: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure_io: Option<f64>,
    /// Percent of CPU time idle waiting on disk (nodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_wait: Option<f64>,
}

/// What a counter grew by per second; `None` if it went back.
fn per_sec(a: u64, b: u64, secs: u64) -> Option<f64> {
    (b >= a && secs > 0).then(|| (b - a) as f64 / secs as f64)
}

impl ResourcePoint {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn disk(&mut self, a: Option<&DiskIo>, b: Option<&DiskIo>, secs: u64) {
        if let (Some(a), Some(b)) = (a, b) {
            self.disk_read = per_sec(a.read, b.read, secs);
            self.disk_write = per_sec(a.written, b.written, secs);
        }
    }

    /// An app's figures between readings `a` and `b` (with their times).
    pub fn app_between(a: (u64, &AppResources), b: (u64, &AppResources)) -> Self {
        let ((t0, a), (t1, b)) = (a, b);
        let mut point = Self::default();
        if t1 <= t0 {
            return point;
        }
        if let (Some(x), Some(y)) = (&a.throttle, &b.throttle) {
            point.throttled = CpuThrottle::share_between(x, y);
        }
        point.disk(a.disk_io.as_ref(), b.disk_io.as_ref(), t1 - t0);
        point
    }

    /// A node's figures between readings `a` and `b` (with their times).
    pub fn node_between(a: (u64, &NodeResources), b: (u64, &NodeResources)) -> Self {
        let ((t0, a), (t1, b)) = (a, b);
        let mut point = Self::default();
        if t1 <= t0 {
            return point;
        }
        let secs = t1 - t0;
        point.disk(a.disk_io.as_ref(), b.disk_io.as_ref(), secs);
        if let (Some(x), Some(y)) = (&a.pressure, &b.pressure) {
            // Microseconds stalled per second of the period, as a percentage.
            let share = |x: Option<Psi>, y: Option<Psi>| {
                per_sec(x?.total, y?.total, secs).map(|us| (us / 10_000.0).min(100.0))
            };
            point.pressure_cpu = share(x.cpu, y.cpu);
            point.pressure_mem = share(x.memory, y.memory);
            point.pressure_io = share(x.io, y.io);
        }
        if let (Some(x), Some(y)) = (&a.cpu_ticks, &b.cpu_ticks)
            && y.total > x.total
            && y.iowait >= x.iowait
        {
            point.io_wait = Some((y.iowait - x.iowait) as f64 * 100.0 / (y.total - x.total) as f64);
        }
        point
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttling_is_a_share_of_periods() {
        let a = CpuThrottle {
            periods: 100,
            throttled: 10,
            throttled_usec: 0,
        };
        let b = CpuThrottle {
            periods: 300,
            throttled: 60,
            throttled_usec: 0,
        };
        assert_eq!(CpuThrottle::share_between(&a, &b), Some(25.0));
        assert_eq!(CpuThrottle::share_between(&b, &b), Some(0.0));
        // A new container: counters start again.
        assert_eq!(CpuThrottle::share_between(&b, &a), None);
    }

    #[test]
    fn node_figures_come_from_totals() {
        let psi = |total| Psi {
            avg10: 0.0,
            avg60: 0.0,
            total,
        };
        let a = NodeResources {
            pressure: Some(Pressure {
                cpu: Some(psi(1_000_000)),
                memory: Some(psi(0)),
                io: None,
            }),
            disk_io: Some(DiskIo {
                read: 1000,
                written: 0,
            }),
            cpu_ticks: Some(CpuTicks {
                total: 1000,
                iowait: 10,
            }),
        };
        let b = NodeResources {
            pressure: Some(Pressure {
                cpu: Some(psi(4_000_000)),
                memory: Some(psi(150_000)),
                io: Some(psi(5)),
            }),
            disk_io: Some(DiskIo {
                read: 11_000,
                written: 5_000,
            }),
            cpu_ticks: Some(CpuTicks {
                total: 2000,
                iowait: 60,
            }),
        };
        let p = ResourcePoint::node_between((100, &a), (110, &b));
        // 3 s stalled in 10 s.
        assert_eq!(p.pressure_cpu, Some(30.0));
        assert_eq!(p.pressure_mem, Some(1.5));
        assert_eq!(p.pressure_io, None);
        assert_eq!((p.disk_read, p.disk_write), (Some(1000.0), Some(500.0)));
        assert_eq!(p.io_wait, Some(5.0));
        // A reboot: nothing compares.
        let back = ResourcePoint::node_between((110, &b), (120, &a));
        assert_eq!(
            (back.pressure_cpu, back.disk_read, back.io_wait),
            (None, None, None)
        );
    }

    #[test]
    fn strain_is_the_worst_resource() {
        let psi = |avg60| {
            Some(Psi {
                avg10: 0.0,
                avg60,
                total: 0,
            })
        };
        let p = |cpu, memory, io| Pressure { cpu, memory, io };
        assert_eq!(p(None, None, None).strain(), None);
        assert_eq!(p(psi(20.0), psi(1.0), psi(2.0)).strain(), Some(Strain::Low));
        assert_eq!(
            p(psi(30.0), psi(1.0), None).strain(),
            Some(Strain::Moderate)
        );
        assert_eq!(
            p(psi(0.0), psi(25.0), psi(0.0)).strain(),
            Some(Strain::High)
        );
    }

    #[test]
    fn pids_warn_past_80_percent() {
        let r = |pids, limit| AppResources {
            pids: Some(pids),
            pids_limit: Some(limit),
            ..Default::default()
        };
        assert_eq!(r(3276, 4096).pids_warning(), None);
        assert_eq!(r(3300, 4096).pids_warning(), Some((3300, 4096)));
        assert_eq!(AppResources::default().pids_warning(), None);
    }
}
