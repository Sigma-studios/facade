//! The vulnerability check: what a node runs ([`Inventory`]), and what the
//! server found in it ([`Finding`]).

use serde::{Deserialize, Serialize};

/// What is installed on a node, for the server to match against vulnerability
/// data. It never leaves the facade server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    /// Of everything below: unchanged inventories are not sent again.
    pub hash: String,
    /// `dpkg` or `nix`.
    pub kind: String,
    pub packages: Vec<Package>,
    /// The running kernel (`uname -r`).
    #[serde(default)]
    pub kernel: Option<String>,
}

/// One installed package.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// The source package it was built from, where the ecosystem has them
    /// (Ubuntu's `linux-image-7.0.0-42-generic` comes from `linux`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_version: Option<String>,
}

impl Inventory {
    /// The hash of a package list and kernel, stable whatever their order.
    pub fn hash_of(kind: &str, packages: &[Package], kernel: Option<&str>) -> String {
        let mut lines: Vec<String> = packages
            .iter()
            .map(|p| {
                format!(
                    "{}\t{}\t{}\t{}",
                    p.name,
                    p.version,
                    p.source.as_deref().unwrap_or(""),
                    p.source_version.as_deref().unwrap_or("")
                )
            })
            .collect();
        lines.sort();
        // FNV-1a, twice with different seeds: no crypto needed, only change detection.
        let mut text = format!("{kind}\n{}\n", kernel.unwrap_or(""));
        text.push_str(&lines.join("\n"));
        let fnv = |seed: u64| {
            text.bytes().fold(seed, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        };
        format!(
            "{:016x}{:016x}",
            fnv(0xcbf2_9ce4_8422_2325),
            fnv(0x6c62_272e_07bb_0142)
        )
    }
}

/// How bad a vulnerability is: the distribution's priority, else CVSS.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum VulnSeverity {
    Critical,
    High,
    Medium,
    Low,
    /// Negligible, or not triaged yet.
    #[default]
    Unknown,
}

impl VulnSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            VulnSeverity::Critical => "critical",
            VulnSeverity::High => "high",
            VulnSeverity::Medium => "medium",
            VulnSeverity::Low => "low",
            VulnSeverity::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" => VulnSeverity::Critical,
            "high" | "important" => VulnSeverity::High,
            "medium" | "moderate" => VulnSeverity::Medium,
            "low" => VulnSeverity::Low,
            _ => VulnSeverity::Unknown,
        }
    }

    /// At least as bad as `min`.
    pub fn at_least(self, min: VulnSeverity) -> bool {
        self <= min
    }
}

/// Where a finding stands on its node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    /// The distribution has not released a fix.
    NoFix,
    /// A fixed version is available: an update is waiting.
    FixAvailable,
    /// The fixed kernel is installed but not running.
    FixedAfterReboot,
}

impl FindingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingStatus::NoFix => "no_fix",
            FindingStatus::FixAvailable => "fix_available",
            FindingStatus::FixedAfterReboot => "fixed_after_reboot",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "no_fix" => Some(FindingStatus::NoFix),
            "fix_available" => Some(FindingStatus::FixAvailable),
            "fixed_after_reboot" => Some(FindingStatus::FixedAfterReboot),
            _ => None,
        }
    }

    /// `no fix released`, `fix available`, `fixed after reboot`.
    pub fn label(self) -> &'static str {
        match self {
            FindingStatus::NoFix => "no fix released",
            FindingStatus::FixAvailable => "fix available",
            FindingStatus::FixedAfterReboot => "fixed after reboot",
        }
    }
}

/// A vulnerability affecting what a node runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub node: String,
    /// `CVE-2026-98432`.
    pub cve: String,
    /// The (source) package: `linux`, `openssl`.
    pub package: String,
    /// The version installed (or running, for a kernel).
    pub installed: String,
    /// The first version with the fix, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed: Option<String>,
    pub severity: VulnSeverity,
    pub status: FindingStatus,
    /// One line on what it is.
    #[serde(default)]
    pub title: String,
    /// The distribution's page about it.
    #[serde(default)]
    pub url: String,
    /// When it was first found on this node.
    pub first_seen: u64,
    /// Silenced until its status changes, by whom and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<Ack>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub by: String,
    pub at: u64,
    #[serde(default)]
    pub note: String,
}

/// A node's findings, counted, for its header and the node list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VulnSummary {
    pub critical: u32,
    pub high: u32,
    pub other: u32,
    /// Critical and high ones without a fix released (and not acknowledged).
    #[serde(default)]
    pub unfixed: u32,
    /// When the vulnerability data the findings come from was published.
    #[serde(default)]
    pub data_as_of: Option<u64>,
    /// Why there are no findings to trust: no inventory from this node, data
    /// not downloaded or too old, a system with no data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown: Option<String>,
}

impl VulnSummary {
    /// `1 critical, no fix`, `2 high`, or `None` when there is none to flag.
    pub fn badge(&self) -> Option<String> {
        let mut parts = vec![];
        if self.critical > 0 {
            parts.push(format!("{} critical", self.critical));
        }
        if self.high > 0 {
            parts.push(format!("{} high", self.high));
        }
        if parts.is_empty() {
            return None;
        }
        let mut badge = parts.join(", ");
        if self.unfixed > 0 && self.unfixed == self.critical + self.high {
            badge.push_str(", no fix");
        } else if self.unfixed > 0 {
            badge.push_str(&format!(" ({} without a fix)", self.unfixed));
        }
        Some(badge)
    }
}

/// `GET /api/nodes/{node}/vulns`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeVulns {
    pub node: String,
    pub summary: VulnSummary,
    /// Worst first.
    pub findings: Vec<Finding>,
    /// The data the node is matched against: `Ubuntu 26.04 LTS (OVAL)`.
    #[serde(default)]
    pub source: Option<String>,
}

/// `POST /api/vulns/{cve}/ack`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AckRequest {
    /// Only this node's finding; every node's otherwise.
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub note: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_ignore_order() {
        let a = Package {
            name: "a".into(),
            version: "1".into(),
            ..Default::default()
        };
        let b = Package {
            name: "b".into(),
            version: "2".into(),
            ..Default::default()
        };
        let h1 = Inventory::hash_of("dpkg", &[a.clone(), b.clone()], Some("7.0.0-42-generic"));
        let h2 = Inventory::hash_of("dpkg", &[b, a.clone()], Some("7.0.0-42-generic"));
        assert_eq!(h1, h2);
        assert_ne!(
            h1,
            Inventory::hash_of("dpkg", &[a], Some("7.0.0-42-generic"))
        );
        assert_eq!(h1.len(), 32);
    }

    #[test]
    fn badges() {
        let mut s = VulnSummary {
            critical: 1,
            unfixed: 1,
            ..Default::default()
        };
        assert_eq!(s.badge().as_deref(), Some("1 critical, no fix"));
        s.high = 2;
        assert_eq!(
            s.badge().as_deref(),
            Some("1 critical, 2 high (1 without a fix)")
        );
        assert_eq!(VulnSummary::default().badge(), None);
        assert!(VulnSeverity::Critical.at_least(VulnSeverity::High));
        assert!(!VulnSeverity::Medium.at_least(VulnSeverity::High));
    }
}
