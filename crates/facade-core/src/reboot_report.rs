//! How a reboot went, measured by the node's agent once its apps are back: when
//! the shutdown began, how long each app took to stop (and whether it had to be
//! killed), how long the machine was down, and when each app ran and answered
//! its health check again. It rides on the reboot's Activity entry.
//!
//! Times are milliseconds since the epoch. What the node could not tell (its
//! journal keeps nothing of the previous boot, an app never came back) is
//! `None`.

use serde::{Deserialize, Serialize};

/// Two boot times closer than this (seconds) are the same boot: the kernel's
/// boot time moves by a second or so when the clock is adjusted. A machine
/// rebooted sooner after starting than this reads as not rebooted.
pub const SAME_BOOT_SECS: u64 = 30;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RebootReport {
    /// When Docker was told to stop (the shutdown reaching it), from the journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_at: Option<u64>,
    /// When Docker had stopped every container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_stopped_at: Option<u64>,
    /// The last thing the system logged before going down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_at: Option<u64>,
    /// When the system started again: its first process.
    pub boot_at: u64,
    /// When Docker was running again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_up_at: Option<u64>,
    /// When the report was made: an app not back by then was not back.
    pub at: u64,
    /// Docker's journal had a shutdown: no line means the machine went down
    /// under it (a crash, a power cut), and the apps with it.
    #[serde(default)]
    pub clean: bool,
    #[serde(default)]
    pub apps: Vec<RebootApp>,
}

/// One container that ran before the reboot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RebootApp {
    /// The container's name without `facade-`: the app, or a stack's service.
    pub name: String,
    /// The app it belongs to, for links.
    pub app: String,
    /// When it stopped for the shutdown (`None`: it never got to stop).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<u64>,
    /// It did not exit within its stop timeout and was killed.
    #[serde(default)]
    pub killed: bool,
    /// Seconds.
    pub stop_timeout: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    /// It has a health check.
    #[serde(default)]
    pub health: bool,
    /// Its first passing health check after starting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthy_at: Option<u64>,
}

impl RebootApp {
    /// Back: running, and healthy if it has a health check.
    pub fn ready_at(&self) -> Option<u64> {
        if self.health {
            self.healthy_at
        } else {
            self.started_at
        }
    }

    /// How long it took to stop, from the shutdown's start.
    pub fn stop_took(&self, report: &RebootReport) -> Option<u64> {
        Some(self.stopped_at?.saturating_sub(report.shutdown_at?))
    }

    /// From stopping (or the shutdown's start, or the last log line, when it
    /// never got to stop) until back.
    pub fn down(&self, report: &RebootReport) -> Option<u64> {
        let from = self.stopped_at.or(report.shutdown_at).or(report.off_at)?;
        Some(self.ready_at()?.saturating_sub(from))
    }

    /// After the system started, when it was back.
    pub fn back_after_boot(&self, report: &RebootReport) -> Option<u64> {
        Some(self.ready_at()?.saturating_sub(report.boot_at))
    }
}

impl RebootReport {
    /// How long the machine was off: from its last log line to its start.
    pub fn off_for(&self) -> Option<u64> {
        Some(self.boot_at.saturating_sub(self.off_at?))
    }

    /// The containers that are not back.
    pub fn not_back(&self) -> Vec<&RebootApp> {
        self.apps
            .iter()
            .filter(|a| a.ready_at().is_none())
            .collect()
    }

    pub fn killed(&self) -> Vec<&RebootApp> {
        self.apps.iter().filter(|a| a.killed).collect()
    }

    /// The app down the longest, and for how long.
    pub fn longest_down(&self) -> Option<(&RebootApp, u64)> {
        self.apps
            .iter()
            .filter_map(|a| Some((a, a.down(self)?)))
            .max_by_key(|(_, d)| *d)
    }

    /// The app slowest to stop, and how long it took.
    pub fn slowest_stop(&self) -> Option<(&RebootApp, u64)> {
        self.apps
            .iter()
            .filter_map(|a| Some((a, a.stop_took(self)?)))
            .max_by_key(|(_, d)| *d)
    }

    /// Something went wrong: an app not back, killed, or no clean shutdown.
    pub fn troubled(&self) -> bool {
        !self.clean || !self.not_back().is_empty() || !self.killed().is_empty()
    }

    /// One line, for the Activity entry: `Rebooted: 12 apps back, down 1m 12s at
    /// most (relay); off 41 s`.
    pub fn summary(&self) -> String {
        let mut parts = vec![];
        if !self.clean {
            parts.push("not shut down cleanly (a crash or power cut?)".to_string());
        }
        let n = self.apps.len();
        let not_back = self.not_back();
        if n == 0 {
            parts.push("no app ran there".into());
        } else if not_back.is_empty() {
            let back = if n == 1 {
                format!("{} back", self.apps[0].name)
            } else {
                format!("all {n} apps back")
            };
            parts.push(match self.longest_down() {
                Some((_, d)) if n == 1 => format!("{back}, down {}", duration(d)),
                Some((a, d)) => format!("{back}, down {} at most ({})", duration(d), a.name),
                None => back,
            });
        } else {
            parts.push(format!("not back: {}", names(&not_back)));
        }
        let killed = self.killed();
        if !killed.is_empty() {
            parts.push(format!("killed at shutdown: {}", names(&killed)));
        }
        if let Some(off) = self.off_for() {
            parts.push(format!("off {}", duration(off)));
        }
        format!("Rebooted: {}", parts.join("; "))
    }

    /// What happened, step by step, for the CLI and Discord.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![];
        if let Some(at) = self.shutdown_at {
            let mut line = format!("Shutdown began at {}", clock(at));
            if let Some((a, took)) = self.slowest_stop() {
                line += &format!(
                    "; apps stopped within {} (slowest: {})",
                    duration(took),
                    a.name
                );
            }
            lines.push(line);
        } else if !self.clean {
            lines.push(
                "No shutdown in Docker's journal: the machine went down under the apps".into(),
            );
        }
        for a in self.killed() {
            lines.push(format!(
                "{} did not stop within its {} s stop timeout and was killed",
                a.name, a.stop_timeout
            ));
        }
        match (self.off_at, self.off_for()) {
            (Some(off), Some(d)) => lines.push(format!(
                "Off from {} to {} ({})",
                clock(off),
                clock(self.boot_at),
                duration(d)
            )),
            _ => lines.push(format!("Started again at {}", clock(self.boot_at))),
        }
        if let Some(up) = self.docker_up_at {
            lines.push(format!(
                "Docker running {} after the start",
                duration(up.saturating_sub(self.boot_at))
            ));
        }
        for a in &self.apps {
            let back = match (a.back_after_boot(self), a.started_at) {
                (Some(t), _) if a.health => format!("healthy {} after the start", duration(t)),
                (Some(t), _) => format!("running {} after the start", duration(t)),
                (None, Some(s)) => format!(
                    "running {} after the start, never healthy",
                    duration(s.saturating_sub(self.boot_at))
                ),
                (None, None) => "not running again".into(),
            };
            let down = a
                .down(self)
                .map(|d| format!(", down {}", duration(d)))
                .unwrap_or_default();
            lines.push(format!("{}: {back}{down}", a.name));
        }
        lines
    }
}

fn names(apps: &[&RebootApp]) -> String {
    apps.iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `850 ms`, `4.2 s`, `42 s`, `1m 12s`, `1h 03m`.
pub fn duration(ms: u64) -> String {
    match ms {
        0..1000 => format!("{ms} ms"),
        1000..10_000 => format!("{:.1} s", ms as f64 / 1000.0),
        10_000..60_000 => format!("{} s", ms / 1000),
        60_000..3_600_000 => format!("{}m {:02}s", ms / 60_000, ms / 1000 % 60),
        _ => format!("{}h {:02}m", ms / 3_600_000, ms / 60_000 % 60),
    }
}

/// `04:00:05 UTC`.
fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!(
        "{:02}:{:02}:{:02} UTC",
        s % 86400 / 3600,
        s % 3600 / 60,
        s % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> RebootReport {
        let t0 = 1_000_000_000;
        RebootReport {
            shutdown_at: Some(t0),
            docker_stopped_at: Some(t0 + 10_500),
            off_at: Some(t0 + 12_000),
            boot_at: t0 + 53_000,
            docker_up_at: Some(t0 + 59_000),
            at: t0 + 120_000,
            clean: true,
            apps: vec![
                RebootApp {
                    name: "pg".into(),
                    app: "pg".into(),
                    stopped_at: Some(t0 + 300),
                    stop_timeout: 60,
                    started_at: Some(t0 + 60_000),
                    ..Default::default()
                },
                RebootApp {
                    name: "relay".into(),
                    app: "relay".into(),
                    stopped_at: Some(t0 + 10_000),
                    killed: true,
                    stop_timeout: 10,
                    started_at: Some(t0 + 61_000),
                    health: true,
                    healthy_at: Some(t0 + 72_000),
                },
            ],
        }
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(duration(850), "850 ms");
        assert_eq!(duration(4_230), "4.2 s");
        assert_eq!(duration(42_000), "42 s");
        assert_eq!(duration(72_000), "1m 12s");
        assert_eq!(duration(3_780_000), "1h 03m");
    }

    #[test]
    fn measures_each_app() {
        let r = report();
        let relay = &r.apps[1];
        assert_eq!(relay.stop_took(&r), Some(10_000));
        assert_eq!(relay.ready_at(), Some(r.boot_at + 19_000));
        assert_eq!(relay.down(&r), Some(62_000));
        assert_eq!(r.off_for(), Some(41_000));
        assert_eq!(
            r.longest_down().map(|(a, _)| a.name.as_str()),
            Some("relay")
        );
        assert!(r.troubled());
        assert_eq!(
            r.summary(),
            "Rebooted: all 2 apps back, down 1m 02s at most (relay); killed at shutdown: relay; off 41 s"
        );
        let lines = r.lines();
        assert!(lines[0].contains("apps stopped within 10 s (slowest: relay)"));
        assert!(
            lines
                .iter()
                .any(|l| l == "relay: healthy 19 s after the start, down 1m 02s")
        );
    }

    #[test]
    fn an_app_not_back_or_never_healthy() {
        let mut r = report();
        r.apps[1].healthy_at = None;
        r.apps[1].killed = false;
        assert_eq!(r.not_back().len(), 1);
        assert_eq!(r.summary(), "Rebooted: not back: relay; off 41 s");
        assert!(r.lines().iter().any(|l| l.contains("never healthy")));
        r.apps.truncate(1);
        assert!(!r.troubled());
        assert_eq!(r.summary(), "Rebooted: pg back, down 59 s; off 41 s");
    }

    #[test]
    fn a_crash_has_no_shutdown() {
        let mut r = report();
        r.clean = false;
        r.shutdown_at = None;
        r.docker_stopped_at = None;
        for a in &mut r.apps {
            a.stopped_at = None;
            a.killed = false;
        }
        assert!(r.summary().starts_with("Rebooted: not shut down cleanly"));
        // Down from the last log line.
        assert_eq!(r.apps[0].down(&r), Some(48_000));
    }
}
