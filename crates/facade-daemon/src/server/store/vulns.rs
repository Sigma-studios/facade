//! The vulnerability check's tables: each node's last inventory, what was
//! found in it (with when each finding was first seen), and acknowledgements.

use facade_core::vulns::{Ack, Finding, FindingStatus, Inventory, VulnSeverity};
use rusqlite::{OptionalExtension, params};

use super::Store;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS inventories (
    node TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    at INTEGER NOT NULL,
    body TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS vuln_findings (
    node TEXT NOT NULL,
    cve TEXT NOT NULL,
    package TEXT NOT NULL,
    installed TEXT NOT NULL,
    fixed TEXT,
    severity TEXT NOT NULL,
    status TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    url TEXT NOT NULL DEFAULT '',
    first_seen INTEGER NOT NULL,
    PRIMARY KEY (node, cve, package)
);
-- An acknowledgement holds while the finding keeps the status it was given for.
CREATE TABLE IF NOT EXISTS vuln_acks (
    node TEXT NOT NULL,
    cve TEXT NOT NULL,
    status TEXT NOT NULL,
    by TEXT NOT NULL,
    at INTEGER NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (node, cve)
);
";

/// What changed on a node between two matches.
#[derive(Debug, Default)]
pub struct FindingChanges {
    pub appeared: Vec<Finding>,
    /// No longer found: fixed (or the package is gone).
    pub gone: Vec<Finding>,
    /// Same CVE and package, another status: `(before, now)`.
    pub changed: Vec<(Finding, Finding)>,
    /// Nothing was recorded for the node before.
    pub first: bool,
}

impl Store {
    pub fn inventory(&self, node: &str) -> anyhow::Result<Option<(Inventory, u64)>> {
        let row: Option<(String, u64)> = self
            .0
            .lock()
            .query_row(
                "SELECT body, at FROM inventories WHERE node = ?1",
                [node],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((body, at)) => Some((serde_json::from_str(&body)?, at)),
            None => None,
        })
    }

    pub fn inventory_hash(&self, node: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .query_row(
                "SELECT hash FROM inventories WHERE node = ?1",
                [node],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn put_inventory(&self, node: &str, inv: &Inventory, at: u64) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT OR REPLACE INTO inventories (node, hash, at, body) VALUES (?1, ?2, ?3, ?4)",
            params![node, inv.hash, at, serde_json::to_string(inv)?],
        )?;
        Ok(())
    }

    /// Marks an unchanged inventory as checked now.
    pub fn touch_inventory(&self, node: &str, at: u64) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE inventories SET at = ?2 WHERE node = ?1",
            params![node, at],
        )?;
        Ok(())
    }

    /// Every finding (of one node, or all), acknowledgements included, worst first.
    pub fn findings(&self, node: Option<&str>) -> anyhow::Result<Vec<Finding>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(
            "SELECT f.node, f.cve, f.package, f.installed, f.fixed, f.severity, f.status,
                    f.title, f.url, f.first_seen, a.by, a.at, a.note, a.status
             FROM vuln_findings f
             LEFT JOIN vuln_acks a ON a.node = f.node AND a.cve = f.cve
             WHERE ?1 IS NULL OR f.node = ?1",
        )?;
        let rows = stmt.query_map([node], |r| {
            let status: String = r.get(6)?;
            let status = FindingStatus::parse(&status).unwrap_or(FindingStatus::NoFix);
            let acked_status: Option<String> = r.get(13)?;
            let ack = match (r.get::<_, Option<String>>(10)?, acked_status) {
                (Some(by), Some(s)) if s == status.as_str() => Some(Ack {
                    by,
                    at: r.get(11)?,
                    note: r.get(12)?,
                }),
                _ => None,
            };
            Ok(Finding {
                node: r.get(0)?,
                cve: r.get(1)?,
                package: r.get(2)?,
                installed: r.get(3)?,
                fixed: r.get(4)?,
                severity: VulnSeverity::parse(&r.get::<_, String>(5)?),
                status,
                title: r.get(7)?,
                url: r.get(8)?,
                first_seen: r.get(9)?,
                ack,
            })
        })?;
        let mut out = rows.collect::<Result<Vec<_>, _>>()?;
        out.sort_by(|a, b| {
            (a.severity, a.status, &a.node, &a.cve, &a.package)
                .cmp(&(b.severity, b.status, &b.node, &b.cve, &b.package))
        });
        Ok(out)
    }

    /// Replaces a node's findings with `now` (their `first_seen` kept where
    /// already found), and says what changed.
    pub fn replace_findings(
        &self,
        node: &str,
        now: Vec<Finding>,
        at: u64,
    ) -> anyhow::Result<FindingChanges> {
        let before = self.findings(Some(node))?;
        let first = before.is_empty() && self.inventory_matched(node)?.is_none();
        let key = |f: &Finding| (f.cve.clone(), f.package.clone());
        let old: std::collections::HashMap<_, _> = before.iter().map(|f| (key(f), f)).collect();
        let mut changes = FindingChanges {
            first,
            ..Default::default()
        };
        let mut conn = self.0.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM vuln_findings WHERE node = ?1", [node])?;
        let mut kept = std::collections::HashSet::new();
        for mut f in now {
            match old.get(&key(&f)) {
                Some(prev) => {
                    f.first_seen = prev.first_seen;
                    if prev.status != f.status {
                        changes.changed.push(((*prev).clone(), f.clone()));
                    }
                    kept.insert(key(&f));
                }
                None => {
                    f.first_seen = at;
                    changes.appeared.push(f.clone());
                }
            }
            tx.execute(
                "INSERT OR REPLACE INTO vuln_findings
                 (node, cve, package, installed, fixed, severity, status, title, url, first_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    node,
                    f.cve,
                    f.package,
                    f.installed,
                    f.fixed,
                    f.severity.as_str(),
                    f.status.as_str(),
                    f.title,
                    f.url,
                    f.first_seen
                ],
            )?;
        }
        changes.gone = before
            .into_iter()
            .filter(|f| !kept.contains(&key(f)))
            .collect();
        // Acknowledgements of findings gone, or whose status moved, lapse.
        tx.execute(
            "DELETE FROM vuln_acks WHERE node = ?1 AND NOT EXISTS (
                SELECT 1 FROM vuln_findings f
                WHERE f.node = vuln_acks.node AND f.cve = vuln_acks.cve
                  AND f.status = vuln_acks.status)",
            [node],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)",
            params![format!("vulns:matched:{node}"), at.to_string()],
        )?;
        tx.commit()?;
        Ok(changes)
    }

    /// When a node was last matched, if ever.
    pub fn inventory_matched(&self, node: &str) -> anyhow::Result<Option<u64>> {
        Ok(self
            .kv_get(&format!("vulns:matched:{node}"))?
            .and_then(|v| v.parse().ok()))
    }

    /// Acknowledges a CVE's findings on a node, at their current status.
    /// Returns how many findings it covers.
    pub fn ack_finding(
        &self,
        node: &str,
        cve: &str,
        by: &str,
        note: &str,
        at: u64,
    ) -> anyhow::Result<usize> {
        let conn = self.0.lock();
        // The worst status among its packages: what the acknowledgement is for.
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM vuln_findings WHERE node = ?1 AND cve = ?2
                 ORDER BY CASE status WHEN 'no_fix' THEN 0 WHEN 'fix_available' THEN 1 ELSE 2 END
                 LIMIT 1",
                params![node, cve],
                |r| r.get(0),
            )
            .optional()?;
        let Some(status) = status else {
            return Ok(0);
        };
        conn.execute(
            "INSERT OR REPLACE INTO vuln_acks (node, cve, status, by, at, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![node, cve, status, by, at, note],
        )?;
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM vuln_findings WHERE node = ?1 AND cve = ?2 AND status = ?3",
            params![node, cve, status],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    pub fn unack_finding(&self, node: &str, cve: &str) -> anyhow::Result<bool> {
        Ok(self.0.lock().execute(
            "DELETE FROM vuln_acks WHERE node = ?1 AND cve = ?2",
            params![node, cve],
        )? > 0)
    }

    /// Forgets a removed node's inventory and findings.
    pub fn forget_vulns(&self, node: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        for table in ["inventories", "vuln_findings", "vuln_acks"] {
            conn.execute(&format!("DELETE FROM {table} WHERE node = ?1"), [node])?;
        }
        conn.execute(
            "DELETE FROM kv WHERE key = ?1",
            [format!("vulns:matched:{node}")],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(cve: &str, status: FindingStatus) -> Finding {
        Finding {
            node: "main".into(),
            cve: cve.into(),
            package: "linux".into(),
            installed: "7.0.0-42-generic".into(),
            fixed: None,
            severity: VulnSeverity::Critical,
            status,
            title: String::new(),
            url: String::new(),
            first_seen: 0,
            ack: None,
        }
    }

    #[test]
    fn findings_keep_their_first_sighting_and_acks_lapse_with_their_status() {
        let dir = std::env::temp_dir().join(format!("facade-vulns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();

        let c = store
            .replace_findings("main", vec![finding("CVE-1", FindingStatus::NoFix)], 100)
            .unwrap();
        assert!(c.first);
        assert_eq!(c.appeared.len(), 1);
        assert_eq!(
            store
                .ack_finding("main", "CVE-1", "admin", "mptcp off", 110)
                .unwrap(),
            1
        );
        assert!(store.findings(Some("main")).unwrap()[0].ack.is_some());

        // Same status: kept, first seen kept, still acknowledged.
        let c = store
            .replace_findings("main", vec![finding("CVE-1", FindingStatus::NoFix)], 200)
            .unwrap();
        assert!(!c.first && c.appeared.is_empty() && c.changed.is_empty());
        let f = &store.findings(Some("main")).unwrap()[0];
        assert_eq!(f.first_seen, 100);
        assert_eq!(f.ack.as_ref().unwrap().note, "mptcp off");

        // A fix comes out: the acknowledgement lapses.
        let c = store
            .replace_findings(
                "main",
                vec![finding("CVE-1", FindingStatus::FixAvailable)],
                300,
            )
            .unwrap();
        assert_eq!(c.changed.len(), 1);
        assert!(store.findings(Some("main")).unwrap()[0].ack.is_none());

        // Fixed.
        let c = store.replace_findings("main", vec![], 400).unwrap();
        assert_eq!(c.gone.len(), 1);
        assert!(store.findings(None).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
