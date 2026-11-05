//! Upstream kernel vulnerabilities, for systems that run kernels close to
//! upstream (NixOS ships the stable releases as they are): OSV's `Linux`
//! records (from the kernel CNA), with the first fixed release on each stable
//! branch. About 60 MB, refreshed daily, and only downloaded when a node needs it.
//!
//! Only the running kernel is matched: a distribution's other packages need
//! data that knows its patches.

use std::io::{Read, Seek};

use anyhow::Context;
use facade_core::vulns::{FindingStatus, Inventory, VulnSeverity};
use serde::Deserialize;

use super::oval::Match;

pub struct KernelDb {
    vulns: Vec<KernelVuln>,
    /// The newest record's modification time.
    pub generated: Option<u64>,
}

struct KernelVuln {
    cve: String,
    title: String,
    severity: VulnSeverity,
    ranges: Vec<Range>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Range {
    introduced: Version,
    /// Fixed from this release; `None`: not fixed on this branch.
    fixed: Option<Version>,
    /// Or affected up to this one included.
    last_affected: Option<Version>,
}

/// `6.12.44` (anything after the third number ignored: `-hardened1`, `-rc2`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u32, u32, u32);

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let core: String = s
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let mut parts = core
            .split('.')
            .filter(|p| !p.is_empty())
            .map(|p| p.parse::<u32>());
        let a = parts.next()?.ok()?;
        let b = parts.next().transpose().ok()?.unwrap_or(0);
        let c = parts.next().transpose().ok()?.unwrap_or(0);
        Some(Version(a, b, c))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

impl Range {
    fn affects(&self, v: Version) -> bool {
        v >= self.introduced
            && match (self.fixed, self.last_affected) {
                (Some(f), _) => v < f,
                (None, Some(l)) => v <= l,
                (None, None) => true,
            }
    }
}

// ---------------------------------------------------------------- OSV records

#[derive(Deserialize)]
struct Record {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    details: String,
    #[serde(default)]
    modified: String,
    #[serde(default)]
    withdrawn: Option<String>,
    #[serde(default)]
    severity: Vec<Severity>,
    #[serde(default)]
    affected: Vec<Affected>,
}

#[derive(Deserialize)]
struct Severity {
    #[serde(rename = "type")]
    kind: String,
    score: String,
}

#[derive(Deserialize)]
struct Affected {
    #[serde(default)]
    package: Option<OsvPackage>,
    #[serde(default)]
    ranges: Vec<OsvRange>,
}

#[derive(Deserialize)]
struct OsvPackage {
    ecosystem: String,
}

#[derive(Deserialize)]
struct OsvRange {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    events: Vec<serde_json::Map<String, serde_json::Value>>,
}

impl KernelDb {
    /// Reads OSV's `Linux/all.zip`.
    pub fn from_zip(input: impl Read + Seek) -> anyhow::Result<Self> {
        let mut zip = zip::ZipArchive::new(input).context("reading the OSV archive")?;
        let mut vulns = Vec::new();
        let mut generated = None;
        let mut buf = String::new();
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            if !entry.name().ends_with(".json") {
                continue;
            }
            buf.clear();
            entry.read_to_string(&mut buf)?;
            let Ok(record) = serde_json::from_str::<Record>(&buf) else {
                continue;
            };
            if let Some(m) = super::oval::parse_time(&record.modified) {
                generated = generated.max(Some(m));
            }
            if let Some(v) = KernelVuln::from_record(record) {
                vulns.push(v);
            }
        }
        anyhow::ensure!(
            !vulns.is_empty(),
            "no kernel vulnerability in the OSV archive"
        );
        Ok(Self { vulns, generated })
    }

    pub fn len(&self) -> usize {
        self.vulns.len()
    }

    /// What affects the running kernel. The current system's kernel (what the
    /// next boot runs), when the inventory names it, tells a fix waiting for a
    /// reboot from one waiting for an update.
    pub fn matches(&self, inventory: &Inventory, current_kernel: Option<&str>) -> Vec<Match> {
        let Some(running) = inventory.kernel.as_deref() else {
            return vec![];
        };
        let Some(v) = Version::parse(running) else {
            return vec![];
        };
        let next = current_kernel.and_then(Version::parse).filter(|n| *n != v);
        let mut out = Vec::new();
        for vuln in &self.vulns {
            let Some(range) = vuln.ranges.iter().find(|r| r.affects(v)) else {
                continue;
            };
            let fixed_by_next = next.is_some_and(|n| !vuln.ranges.iter().any(|r| r.affects(n)));
            let (fixed, status) = match (range.fixed, fixed_by_next) {
                (_, true) => (next.map(|n| n.to_string()), FindingStatus::FixedAfterReboot),
                (Some(f), false) => (Some(f.to_string()), FindingStatus::FixAvailable),
                (None, false) => (None, FindingStatus::NoFix),
            };
            out.push(Match {
                cve: vuln.cve.clone(),
                package: "linux".into(),
                installed: running.to_string(),
                fixed,
                severity: vuln.severity,
                status,
                title: vuln.title.clone(),
                url: format!("https://osv.dev/vulnerability/{}", vuln.cve),
            });
        }
        out.sort_by(|a, b| (a.severity, &a.cve).cmp(&(b.severity, &b.cve)));
        out
    }
}

impl KernelVuln {
    fn from_record(r: Record) -> Option<Self> {
        if r.withdrawn.is_some() {
            return None;
        }
        let mut ranges = Vec::new();
        for a in &r.affected {
            if a.package.as_ref().is_none_or(|p| p.ecosystem != "Linux") {
                continue;
            }
            for range in a.ranges.iter().filter(|r| r.kind == "ECOSYSTEM") {
                ranges.extend(ranges_of(&range.events));
            }
        }
        if ranges.is_empty() {
            return None;
        }
        let cve = std::iter::once(&r.id)
            .chain(r.aliases.iter())
            .find(|id| id.starts_with("CVE-"))
            .cloned()
            .unwrap_or(r.id.clone());
        let title = if r.summary.is_empty() {
            r.details
                .lines()
                .find(|l| !l.trim().is_empty() && !l.starts_with("In the Linux kernel"))
                .unwrap_or("")
                .trim()
                .to_string()
        } else {
            r.summary.clone()
        };
        let severity = r
            .severity
            .iter()
            .filter(|s| s.kind == "CVSS_V3")
            .find_map(|s| cvss3_score(&s.score))
            .map(severity_of)
            .unwrap_or(VulnSeverity::Unknown);
        Some(Self {
            cve,
            title,
            severity,
            ranges,
        })
    }
}

/// OSV events in order: `introduced` opens a range, `fixed` or
/// `last_affected` closes it.
fn ranges_of(events: &[serde_json::Map<String, serde_json::Value>]) -> Vec<Range> {
    let version = |v: &serde_json::Value| v.as_str().and_then(Version::parse);
    let mut out = Vec::new();
    let mut open: Option<Version> = None;
    for e in events {
        if let Some(v) = e.get("introduced") {
            if let Some(start) = open.take() {
                out.push(Range {
                    introduced: start,
                    fixed: None,
                    last_affected: None,
                });
            }
            open = Some(if v.as_str() == Some("0") {
                Version::default()
            } else {
                match version(v) {
                    Some(v) => v,
                    None => continue,
                }
            });
        } else if let Some(v) = e.get("fixed") {
            if let (Some(start), Some(end)) = (open.take(), version(v)) {
                out.push(Range {
                    introduced: start,
                    fixed: Some(end),
                    last_affected: None,
                });
            }
        } else if let Some(v) = e.get("last_affected")
            && let (Some(start), Some(end)) = (open.take(), version(v))
        {
            out.push(Range {
                introduced: start,
                fixed: None,
                last_affected: Some(end),
            });
        }
    }
    if let Some(start) = open {
        out.push(Range {
            introduced: start,
            fixed: None,
            last_affected: None,
        });
    }
    out
}

fn severity_of(score: f64) -> VulnSeverity {
    match score {
        s if s >= 9.0 => VulnSeverity::Critical,
        s if s >= 7.0 => VulnSeverity::High,
        s if s >= 4.0 => VulnSeverity::Medium,
        s if s > 0.0 => VulnSeverity::Low,
        _ => VulnSeverity::Unknown,
    }
}

/// The base score of a CVSS 3.x vector (`CVSS:3.1/AV:N/AC:L/…`).
fn cvss3_score(vector: &str) -> Option<f64> {
    let mut m = std::collections::HashMap::new();
    for part in vector.split('/').skip(1) {
        let (k, v) = part.split_once(':')?;
        m.insert(k, v);
    }
    let changed = *m.get("S")? == "C";
    let av = match *m.get("AV")? {
        "N" => 0.85,
        "A" => 0.62,
        "L" => 0.55,
        "P" => 0.2,
        _ => return None,
    };
    let ac = match *m.get("AC")? {
        "L" => 0.77,
        "H" => 0.44,
        _ => return None,
    };
    let pr = match (*m.get("PR")?, changed) {
        ("N", _) => 0.85,
        ("L", false) => 0.62,
        ("L", true) => 0.68,
        ("H", false) => 0.27,
        ("H", true) => 0.5,
        _ => return None,
    };
    let ui = match *m.get("UI")? {
        "N" => 0.85,
        "R" => 0.62,
        _ => return None,
    };
    let cia = |k: &str| -> Option<f64> {
        Some(match *m.get(k)? {
            "H" => 0.56,
            "L" => 0.22,
            "N" => 0.0,
            _ => return None,
        })
    };
    let (c, i, a) = (cia("C")?, cia("I")?, cia("A")?);
    let iss = 1.0 - (1.0 - c) * (1.0 - i) * (1.0 - a);
    let impact = if changed {
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15)
    } else {
        6.42 * iss
    };
    if impact <= 0.0 {
        return Some(0.0);
    }
    let exploitability = 8.22 * av * ac * pr * ui;
    let base = if changed {
        (1.08 * (impact + exploitability)).min(10.0)
    } else {
        (impact + exploitability).min(10.0)
    };
    Some(round_up(base))
}

/// CVSS 3.1's rounding up to one decimal.
fn round_up(x: f64) -> f64 {
    let i = (x * 100_000.0).round() as i64;
    if i % 10_000 == 0 {
        i as f64 / 100_000.0
    } else {
        ((i / 10_000) + 1) as f64 / 10.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use facade_core::vulns::Package;

    #[test]
    fn scores_cvss3_vectors() {
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"),
            Some(9.8)
        );
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H"),
            Some(7.8)
        );
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H"),
            Some(5.5)
        );
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N"),
            Some(6.1)
        );
        assert_eq!(
            cvss3_score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N"),
            Some(0.0)
        );
        assert_eq!(cvss3_score("nonsense"), None);
    }

    /// The record for the TCP use-after-free, as OSV has it (trimmed).
    const RECORD: &str = r#"{
        "id": "CVE-2026-98432",
        "modified": "2026-11-04T17:27:21.973156910Z",
        "summary": "ipv4: free inet_opt and ireq_opt after an RCU grace period",
        "severity": [{"type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"}],
        "affected": [
            {"ranges": [{"type": "GIT", "repo": "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git",
                         "events": [{"introduced": "079096f103faca2dd87342cca6f23d4b34da8871"}, {"fixed": "b3b027ef0cd322bfa839b9aaf5b27551eea54cf6"}]}]},
            {"package": {"name": "Kernel", "ecosystem": "Linux"},
             "ranges": [
                {"type": "ECOSYSTEM", "events": [{"introduced": "4.4.0"}, {"fixed": "5.10.272"}]},
                {"type": "ECOSYSTEM", "events": [{"introduced": "5.11.0"}, {"fixed": "5.15.223"}]},
                {"type": "ECOSYSTEM", "events": [{"introduced": "5.16.0"}, {"fixed": "6.1.190"}]},
                {"type": "ECOSYSTEM", "events": [{"introduced": "6.2.0"}, {"fixed": "6.6.159"}]},
                {"type": "ECOSYSTEM", "events": [{"introduced": "6.7.0"}, {"fixed": "6.12.113"}]},
                {"type": "ECOSYSTEM", "events": [{"introduced": "6.13.0"}, {"fixed": "6.18.56"}]},
                {"type": "ECOSYSTEM", "events": [{"introduced": "6.19.0"}, {"fixed": "7.2.10"}]}
             ]}
        ]
    }"#;

    fn db() -> KernelDb {
        let record: Record = serde_json::from_str(RECORD).unwrap();
        KernelDb {
            vulns: vec![KernelVuln::from_record(record).unwrap()],
            generated: None,
        }
    }

    fn node(running: &str) -> Inventory {
        Inventory {
            kind: "nix".into(),
            kernel: Some(running.into()),
            packages: vec![Package {
                name: "linux".into(),
                version: running.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn matches_the_running_kernel_by_stable_branch() {
        let db = db();
        let m = db.matches(&node("6.12.44"), None);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].cve, "CVE-2026-98432");
        assert_eq!(m[0].severity, VulnSeverity::Critical);
        assert_eq!(m[0].status, FindingStatus::FixAvailable);
        assert_eq!(m[0].fixed.as_deref(), Some("6.12.113"));
        assert!(db.matches(&node("6.12.113"), None).is_empty());
        assert!(db.matches(&node("7.3.1"), None).is_empty());
        assert!(db.matches(&node("4.3.9"), None).is_empty());
        // 7.0 is not a stable branch: the fix is in 7.2.10.
        assert_eq!(
            db.matches(&node("7.0.0"), None)[0].fixed.as_deref(),
            Some("7.2.10")
        );
        // The fixed kernel is the current system's: a reboot fixes it.
        let after = db.matches(&node("6.12.44"), Some("6.12.115"));
        assert_eq!(after[0].status, FindingStatus::FixedAfterReboot);
        assert_eq!(after[0].fixed.as_deref(), Some("6.12.115"));
        assert_eq!(
            Version::parse("6.12.44-hardened1"),
            Some(Version(6, 12, 44))
        );
        assert_eq!(Version::parse("7.3"), Some(Version(7, 3, 0)));
    }

    #[test]
    fn open_ranges_have_no_fix() {
        let events: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_str(
            r#"[{"introduced": "0"}, {"fixed": "5.0.0"}, {"introduced": "6.1.0"}]"#,
        )
        .unwrap();
        let r = ranges_of(&events);
        assert_eq!(r.len(), 2);
        assert!(r[0].affects(Version(4, 0, 0)));
        assert!(!r[0].affects(Version(5, 0, 0)));
        assert!(r[1].affects(Version(9, 9, 9)));
        assert_eq!(r[1].fixed, None);
    }

    /// `FACADE_OSV_LINUX=…/all.zip cargo test -- --ignored`: the whole archive reads.
    #[test]
    #[ignore]
    fn reads_the_whole_archive() {
        let path = std::env::var("FACADE_OSV_LINUX").expect("FACADE_OSV_LINUX");
        let started = std::time::Instant::now();
        let db = KernelDb::from_zip(std::fs::File::open(path).unwrap()).unwrap();
        eprintln!(
            "{} kernel vulnerabilities in {:?}",
            db.len(),
            started.elapsed()
        );
        let m = db.matches(&node("6.12.44"), None);
        let count = |s| m.iter().filter(|f| f.severity == s).count();
        eprintln!(
            "6.12.44: {} ({} critical, {} high), no fix: {}",
            m.len(),
            count(VulnSeverity::Critical),
            count(VulnSeverity::High),
            m.iter()
                .filter(|f| f.status == FindingStatus::NoFix)
                .count()
        );
        assert!(m.iter().any(|f| f.cve == "CVE-2026-98432"));
    }
}
