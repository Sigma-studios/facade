//! What the agent measures beyond `docker stats`' CPU, memory and network:
//! containers' CPU throttling, disk reads and writes, processes and health
//! checks; the node's pressure (PSI), root disk activity and iowait.
//!
//! Containers are read from their cgroup (v2) when the agent can see it, else
//! from what `docker stats` said (cgroup v1, or a cgroup out of the agent's
//! sight); a figure neither has is left out, never sent as zero.

use std::path::{Path, PathBuf};

use bollard::models::{ContainerStatsResponse, Health};
use facade_core::resources::{
    AppResources, CpuThrottle, CpuTicks, DiskIo, HealthCheck, NodeResources, Pressure, Psi,
};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// `/proc/diskstats` counts 512-byte sectors, whatever the device's own size.
const SECTOR: u64 = 512;
/// What is kept of a failed health check's output.
const HEALTH_OUTPUT: usize = 300;

// ---------------------------------------------------------------- node

pub fn node_resources() -> NodeResources {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    NodeResources {
        pressure: pressure(),
        disk_io: read("/proc/diskstats").and_then(|stats| {
            let mountinfo = read("/proc/self/mountinfo").unwrap_or_default();
            root_disk_io(&stats, &mountinfo, |name| {
                Path::new("/sys/block").join(name).exists()
            })
        }),
        cpu_ticks: read("/proc/stat").and_then(|s| parse_cpu_ticks(&s)),
    }
}

/// `None` on kernels without PSI, or with it turned off (`psi=0`: reading the
/// files fails).
fn pressure() -> Option<Pressure> {
    let read = |name: &str| {
        std::fs::read_to_string(format!("/proc/pressure/{name}"))
            .ok()
            .and_then(|s| parse_psi(&s))
    };
    let p = Pressure {
        cpu: read("cpu"),
        memory: read("memory"),
        io: read("io"),
    };
    (!p.is_empty()).then_some(p)
}

/// The "some" line of a `/proc/pressure/*` file.
pub fn parse_psi(text: &str) -> Option<Psi> {
    let line = text.lines().find_map(|l| l.strip_prefix("some "))?;
    let mut psi = Psi::default();
    let (mut avg10, mut avg60, mut total) = (false, false, false);
    for field in line.split_whitespace() {
        match field.split_once('=') {
            Some(("avg10", v)) => (psi.avg10, avg10) = (v.parse().ok()?, true),
            Some(("avg60", v)) => (psi.avg60, avg60) = (v.parse().ok()?, true),
            Some(("total", v)) => (psi.total, total) = (v.parse().ok()?, true),
            _ => {}
        }
    }
    (avg10 && avg60 && total).then_some(psi)
}

/// The machine's CPU time from the first line of `/proc/stat`: user, nice,
/// system, idle, iowait, irq, softirq, steal (guest time is already in user).
pub fn parse_cpu_ticks(text: &str) -> Option<CpuTicks> {
    let line = text.lines().find_map(|l| l.strip_prefix("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    (fields.len() >= 5).then(|| CpuTicks {
        total: fields.iter().take(8).sum(),
        iowait: fields[4],
    })
}

/// One block device of `/proc/diskstats`.
#[derive(Debug, PartialEq)]
pub struct DiskStat {
    pub major: u32,
    pub minor: u32,
    pub name: String,
    pub io: DiskIo,
}

pub fn parse_diskstats(text: &str) -> Vec<DiskStat> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // major minor name reads merged sectors_read ms writes merged sectors_written …
            if f.len() < 10 {
                return None;
            }
            Some(DiskStat {
                major: f[0].parse().ok()?,
                minor: f[1].parse().ok()?,
                name: f[2].to_string(),
                io: DiskIo {
                    read: f[5].parse::<u64>().ok()? * SECTOR,
                    written: f[9].parse::<u64>().ok()? * SECTOR,
                },
            })
        })
        .collect()
}

/// The device (`major:minor`) mounted at `mount`, from a mount table; the last
/// mount there wins, as it hides the ones before.
pub fn mount_device(mountinfo: &str, mount: &str) -> Option<(u32, u32)> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (major, minor) = f.get(2)?.split_once(':')?;
            (*f.get(4)? == mount).then(|| Some((major.parse().ok()?, minor.parse().ok()?)))?
        })
        .next_back()
}

/// The root disk's reads and writes: the device mounted at `/`, else at
/// Docker's data root (an agent in a container sees an overlay at `/`, a
/// btrfs or ZFS root has no device of its own), else all the machine's
/// physical disks (`whole_disk`: listed in `/sys/block`) added up. Stacked
/// devices (LVM, RAID) are left out of that sum: their disks count already.
pub fn root_disk_io(
    diskstats: &str,
    mountinfo: &str,
    whole_disk: impl Fn(&str) -> bool,
) -> Option<DiskIo> {
    let devices = parse_diskstats(diskstats);
    for mount in ["/", "/var/lib/docker"] {
        if let Some((major, minor)) = mount_device(mountinfo, mount).filter(|d| d.0 != 0)
            && let Some(d) = devices
                .iter()
                .find(|d| d.major == major && d.minor == minor)
        {
            return Some(d.io);
        }
    }
    let virtual_ = ["loop", "ram", "zram", "dm-", "md", "nbd", "sr", "fd"];
    let disks: Vec<&DiskStat> = devices
        .iter()
        .filter(|d| whole_disk(&d.name) && !virtual_.iter().any(|v| d.name.starts_with(v)))
        .collect();
    (!disks.is_empty()).then(|| DiskIo {
        read: disks.iter().map(|d| d.io.read).sum(),
        written: disks.iter().map(|d| d.io.written).sum(),
    })
}

// ---------------------------------------------------------------- containers

/// An app container's figures: `pid` its main process (to find its cgroup),
/// `cpu_limited` whether it has a CPU limit (throttling means nothing
/// otherwise), `stats` what Docker said, `health` its health check state.
pub fn app_resources(
    pid: Option<i64>,
    cpu_limited: bool,
    stats: Option<&ContainerStatsResponse>,
    health: Option<&Health>,
) -> AppResources {
    let cgroup = pid.filter(|p| *p > 0).and_then(container_cgroup);
    let file = |name: &str| {
        cgroup
            .as_ref()
            .and_then(|dir| std::fs::read_to_string(dir.join(name)).ok())
    };
    let mut r = AppResources::default();
    if cpu_limited {
        r.throttle = file("cpu.stat")
            .and_then(|s| parse_cpu_stat(&s))
            .or_else(|| stats.and_then(docker_throttle));
    }
    r.disk_io = file("io.stat")
        .map(|s| parse_io_stat(&s))
        .or_else(|| stats.and_then(docker_disk_io));
    let docker_pids = stats.and_then(|s| s.pids_stats.as_ref());
    r.pids = file("pids.current")
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| docker_pids.and_then(|p| p.current));
    r.pids_limit = match file("pids.max") {
        // "max": no limit.
        Some(s) => s.trim().parse().ok(),
        // Docker says 0 or a huge number for none.
        None => docker_pids
            .and_then(|p| p.limit)
            .filter(|l| *l > 0 && *l < u64::MAX / 2),
    };
    r.health = health.map(health_checks).unwrap_or_default();
    r
}

/// The cgroup v2 directory of process `pid`, as the agent sees it: paths in
/// `/proc/<pid>/cgroup` are relative to the reader's cgroup namespace, which
/// is where the agent's `/sys/fs/cgroup` is rooted.
fn container_cgroup(pid: i64) -> Option<PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let dir = Path::new(CGROUP_ROOT).join(unified_path(&text)?.trim_start_matches('/'));
    dir.join("cgroup.controllers").exists().then_some(dir)
}

/// The cgroup v2 path in a `/proc/<pid>/cgroup` file (its `0::` line); `None`
/// on a cgroup v1 machine, or for a process in another cgroup namespace (`/..`).
pub fn unified_path(text: &str) -> Option<&str> {
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    (!path.starts_with("/..")).then_some(path)
}

/// `cpu.stat`: cgroup v2 says `throttled_usec`, v1 `throttled_time` in
/// nanoseconds.
pub fn parse_cpu_stat(text: &str) -> Option<CpuThrottle> {
    let value = |key: &str| {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once(' ')?;
            (k == key).then(|| v.trim().parse::<u64>().ok())?
        })
    };
    Some(CpuThrottle {
        periods: value("nr_periods")?,
        throttled: value("nr_throttled")?,
        throttled_usec: value("throttled_usec")
            .or_else(|| value("throttled_time").map(|ns| ns / 1000))?,
    })
}

/// `io.stat`: one line per device (`8:0 rbytes=… wbytes=… rios=…`), added up.
/// A container that has not touched a disk yet has an empty file: zeros.
pub fn parse_io_stat(text: &str) -> DiskIo {
    let mut io = DiskIo::default();
    for field in text.split_whitespace() {
        match field.split_once('=') {
            Some(("rbytes", v)) => io.read += v.parse::<u64>().unwrap_or(0),
            Some(("wbytes", v)) => io.written += v.parse::<u64>().unwrap_or(0),
            _ => {}
        }
    }
    io
}

fn docker_throttle(s: &ContainerStatsResponse) -> Option<CpuThrottle> {
    let t = s.cpu_stats.as_ref()?.throttling_data.as_ref()?;
    Some(CpuThrottle {
        periods: t.periods?,
        throttled: t.throttled_periods?,
        throttled_usec: t.throttled_time? / 1000,
    })
}

/// Docker's blkio figures: per device and operation (`read`, `write`; cgroup v1
/// adds `Total`, `Sync`… and capitalises).
fn docker_disk_io(s: &ContainerStatsResponse) -> Option<DiskIo> {
    let entries = s
        .blkio_stats
        .as_ref()?
        .io_service_bytes_recursive
        .as_ref()
        .filter(|e| !e.is_empty())?;
    let mut io = DiskIo::default();
    for e in entries {
        let value = e.value.unwrap_or(0);
        match e.op.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("read") => io.read += value,
            Some("write") => io.written += value,
            _ => {}
        }
    }
    Some(io)
}

fn health_checks(health: &Health) -> Vec<HealthCheck> {
    health
        .log
        .iter()
        .flatten()
        .filter_map(|r| {
            let start = millis_since_epoch(r.start.as_deref()?)?;
            let end = r
                .end
                .as_deref()
                .and_then(millis_since_epoch)
                .unwrap_or(start);
            let ok = r.exit_code == Some(0);
            let output = if ok {
                String::new()
            } else {
                tail(r.output.as_deref().unwrap_or("").trim(), HEALTH_OUTPUT)
            };
            Some(HealthCheck {
                at_ms: start,
                ok,
                ms: end.saturating_sub(start),
                output,
            })
        })
        .collect()
}

/// The last `max` bytes of `s`, cut at a character boundary.
fn tail(s: &str, max: usize) -> String {
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// An RFC 3339 time as Docker writes it (`2026-11-04T20:01:02.123456789Z`, or
/// with an offset), in milliseconds since the epoch.
pub fn millis_since_epoch(s: &str) -> Option<u64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    // The offset: `Z`, or `+hh:mm` / `-hh:mm` after the seconds.
    let (clock, offset) = match time.find(['Z', 'z', '+', '-']) {
        Some(i) => (&time[..i], &time[i..]),
        None => (time, "Z"),
    };
    let offset_secs = match offset.as_bytes().first() {
        Some(b'+' | b'-') => {
            let (h, m) = offset[1..].split_once(':')?;
            let secs = h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60;
            if offset.starts_with('-') { -secs } else { secs }
        }
        _ => 0,
    };
    let mut c = clock.splitn(3, ':');
    let (hour, minute) = (
        c.next()?.parse::<i64>().ok()?,
        c.next()?.parse::<i64>().ok()?,
    );
    let rest = c.next()?;
    let (secs, fraction) = rest.split_once('.').unwrap_or((rest, ""));
    let secs: i64 = secs.parse().ok()?;
    // Milliseconds: the first three digits of the fraction.
    let millis: i64 = format!("{fraction:0<3}")[..3].parse().ok()?;
    // Days since the epoch of a proleptic Gregorian date (Howard Hinnant's).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total = (days * 86400 + hour * 3600 + minute * 60 + secs - offset_secs) * 1000 + millis;
    u64::try_from(total).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Samples from an Ubuntu 24.04 VPS (kernel 6.8) and Docker Desktop.
    const PSI_CPU: &str = "some avg10=1.53 avg60=0.87 avg300=0.42 total=180455711\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
    const PSI_OLD_CPU: &str = "some avg10=0.00 avg60=0.12 avg300=0.08 total=9136112\n";

    #[test]
    fn reads_pressure() {
        assert_eq!(
            parse_psi(PSI_CPU),
            Some(Psi {
                avg10: 1.53,
                avg60: 0.87,
                total: 180_455_711
            })
        );
        // Kernels before 5.13 have no "full" line for CPU.
        assert_eq!(parse_psi(PSI_OLD_CPU).unwrap().total, 9_136_112);
        assert_eq!(parse_psi(""), None);
        assert_eq!(
            parse_psi("full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"),
            None
        );
    }

    #[test]
    fn reads_cpu_ticks() {
        let stat = "cpu  4705 356 584 3699176 2343 0 9 0 0 0\ncpu0 1393 280 181 924316 568 0 6 0 0 0\nintr 1\n";
        assert_eq!(
            parse_cpu_ticks(stat),
            Some(CpuTicks {
                total: 4705 + 356 + 584 + 3_699_176 + 2343 + 9,
                iowait: 2343
            })
        );
        assert_eq!(parse_cpu_ticks("intr 1\n"), None);
    }

    const DISKSTATS: &str = "   7       0 loop0 51 0 2122 13 0 0 0 0 0 40 13 0 0 0 0 0 0
 259       0 nvme0n1 412398 30219 33264418 151262 2398743 1954427 92733896 2165840 0 1571344 2353215 0 0 0 0 73811 36112
 259       1 nvme0n1p1 411934 30219 33224834 151105 2398743 1954427 92733896 2165840 0 1571232 2316945 0 0 0 0 0 0
 253       0 dm-0 1000 0 8000 10 2000 0 16000 20 0 30 30 0 0 0 0 0 0
";
    const MOUNTINFO: &str =
        "24 1 259:1 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p1 rw,discard,errors=remount-ro
25 24 0:22 / /sys rw,nosuid,nodev,noexec,relatime shared:2 - sysfs sysfs rw
";
    // An agent in a container (docker:dind): an overlay at `/`, Docker's data on a disk.
    const MOUNTINFO_DIND: &str = "1060 940 0:183 / / rw,relatime master:400 - overlay overlay rw,lowerdir=/var/lib/docker/overlay2/l/X
1102 1060 254:1 /docker/volumes/fz/_data /var/lib/docker rw,relatime master:1 - ext4 /dev/vda1 rw,discard
";

    #[test]
    fn finds_the_root_disk() {
        let parsed = parse_diskstats(DISKSTATS);
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[2].name, "nvme0n1p1");
        assert_eq!(parsed[2].io.read, 33_224_834 * 512);
        assert_eq!(parsed[2].io.written, 92_733_896 * 512);
        assert_eq!(mount_device(MOUNTINFO, "/"), Some((259, 1)));

        let whole = |name: &str| !name.contains('p') || name.starts_with("loop");
        // `/`'s partition.
        let io = root_disk_io(DISKSTATS, MOUNTINFO, whole).unwrap();
        assert_eq!(io.read, 33_224_834 * 512);
        // In a container: Docker's data root, here a device diskstats does
        // not list, so the whole disks (not loop, not dm) added up.
        let io = root_disk_io(DISKSTATS, MOUNTINFO_DIND, whole).unwrap();
        assert_eq!(io.read, 33_264_418 * 512);
        assert_eq!(root_disk_io("", MOUNTINFO, whole), None);
    }

    #[test]
    fn reads_container_cgroups() {
        // Docker with the systemd driver; a v1 machine has no `0::` line, a
        // hybrid one an empty unified hierarchy.
        assert_eq!(
            unified_path("0::/system.slice/docker-3f2a.scope\n"),
            Some("/system.slice/docker-3f2a.scope")
        );
        assert_eq!(
            unified_path("12:pids:/docker/3f2a\n11:cpu,cpuacct:/docker/3f2a\n"),
            None
        );
        assert_eq!(unified_path("0::/../other\n"), None);

        let v2 = "usage_usec 81283311\nuser_usec 72141095\nsystem_usec 9142216\nnr_periods 4211\nnr_throttled 1170\nthrottled_usec 61540622\nnr_bursts 0\nburst_usec 0\n";
        assert_eq!(
            parse_cpu_stat(v2),
            Some(CpuThrottle {
                periods: 4211,
                throttled: 1170,
                throttled_usec: 61_540_622
            })
        );
        let v1 = "nr_periods 10\nnr_throttled 2\nthrottled_time 5000000\n";
        assert_eq!(parse_cpu_stat(v1).unwrap().throttled_usec, 5000);
        // No CPU controller: only usage.
        assert_eq!(
            parse_cpu_stat("usage_usec 1\nuser_usec 1\nsystem_usec 0\n"),
            None
        );

        let io = "254:0 rbytes=1146880 wbytes=104857600 rios=31 wios=809 dbytes=0 dios=0\n8:0 rbytes=4096 wbytes=0 rios=1 wios=0 dbytes=0 dios=0\n";
        assert_eq!(
            parse_io_stat(io),
            DiskIo {
                read: 1_146_880 + 4096,
                written: 104_857_600
            }
        );
        assert_eq!(parse_io_stat(""), DiskIo::default());
    }

    #[test]
    fn reads_docker_times() {
        assert_eq!(millis_since_epoch("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(
            millis_since_epoch("2026-11-04T20:01:02.123456789Z"),
            Some(1_793_822_462_123)
        );
        assert_eq!(
            millis_since_epoch("2026-11-04T21:01:02.123456789+01:00"),
            Some(1_793_822_462_123)
        );
        assert_eq!(
            millis_since_epoch("2026-11-04T20:01:02Z"),
            Some(1_793_822_462_000)
        );
        assert_eq!(millis_since_epoch("yesterday"), None);
    }

    #[test]
    fn keeps_the_end_of_outputs() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 3), "ab");
        assert_eq!(tail("aé", 1), "");
    }
}
