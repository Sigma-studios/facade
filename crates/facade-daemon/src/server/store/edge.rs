//! Tables of what is seen from outside: bytes served and visitor sketches per
//! day, failed requests per 10 minutes, each app's state minute by minute, and
//! where each domain's outside check stands.

use facade_core::insights::ErrorRow;
use rusqlite::{OptionalExtension, params};

use super::Store;
use crate::hll;

pub const SCHEMA: &str = "
-- Bytes through the entry point (as on the wire) and a HyperLogLog sketch of
-- the visitors, per app and UTC day.
CREATE TABLE IF NOT EXISTS edge_daily (
    app TEXT NOT NULL,
    day INTEGER NOT NULL,
    bytes_in INTEGER NOT NULL DEFAULT 0,
    bytes_out INTEGER NOT NULL DEFAULT 0,
    visitors BLOB,
    PRIMARY KEY (app, day)
);
-- The same bytes per node, which apps leave when they move or go.
CREATE TABLE IF NOT EXISTS edge_node_daily (
    node TEXT NOT NULL,
    day INTEGER NOT NULL,
    bytes_in INTEGER NOT NULL DEFAULT 0,
    bytes_out INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (node, day)
);
-- Failed requests per app, status, method and normalised path, per
-- `ERROR_BUCKET` seconds.
CREATE TABLE IF NOT EXISTS edge_errors (
    app TEXT NOT NULL,
    at INTEGER NOT NULL,
    status INTEGER NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    count INTEGER NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    PRIMARY KEY (app, at, status, method, path)
);
-- Each app's state, one byte per minute of a UTC day (see `Uptime`).
CREATE TABLE IF NOT EXISTS uptime_minutes (
    app TEXT NOT NULL,
    day INTEGER NOT NULL,
    minutes BLOB NOT NULL,
    PRIMARY KEY (app, day)
);
-- Where each domain's outside check stands, kept across restarts.
CREATE TABLE IF NOT EXISTS domain_reach (
    domain TEXT PRIMARY KEY,
    last_ok INTEGER,
    down_since INTEGER
);
";

/// Failed requests are added up per this many seconds.
pub const ERROR_BUCKET: u64 = 600;
/// Days of bytes and visitors kept: this calendar month and the one before.
pub const DAILY_KEEP_DAYS: u64 = 62;
pub const ERRORS_KEEP: u64 = 8 * 86_400;
pub const UPTIME_KEEP_DAYS: u64 = 31;

/// A domain, when its check last answered, and since when it has not.
pub type ReachMark = (String, Option<u64>, Option<u64>);

/// One group of failed requests, as added to a bucket.
pub struct ErrorAdd<'a> {
    pub status: u16,
    pub method: &'a str,
    pub path: &'a str,
    pub count: u64,
    pub first_seen: u64,
    pub last_seen: u64,
}

impl Store {
    pub fn add_bytes(
        &self,
        app: &str,
        node: &str,
        day: u64,
        received: u64,
        sent: u64,
    ) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "INSERT INTO edge_daily (app, day, bytes_in, bytes_out) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (app, day) DO UPDATE SET
                bytes_in = bytes_in + excluded.bytes_in, bytes_out = bytes_out + excluded.bytes_out",
            params![app, day as i64, received as i64, sent as i64],
        )?;
        conn.execute(
            "INSERT INTO edge_node_daily (node, day, bytes_in, bytes_out) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (node, day) DO UPDATE SET
                bytes_in = bytes_in + excluded.bytes_in, bytes_out = bytes_out + excluded.bytes_out",
            params![node, day as i64, received as i64, sent as i64],
        )?;
        Ok(())
    }

    /// Bytes `(day, received, sent)` of an app (or, `node`, a node) from `from` on.
    pub fn bytes_by_day(
        &self,
        name: &str,
        node: bool,
        from: u64,
    ) -> anyhow::Result<Vec<(u64, u64, u64)>> {
        let (table, key) = if node {
            ("edge_node_daily", "node")
        } else {
            ("edge_daily", "app")
        };
        let conn = self.0.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT day, bytes_in, bytes_out FROM {table} WHERE {key} = ?1 AND day >= ?2 ORDER BY day"
        ))?;
        let rows = stmt
            .query_map(params![name, from as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                ))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Merges a day's visitor sketch into the one kept.
    pub fn merge_visitors(&self, app: &str, day: u64, registers: &[u8]) -> anyhow::Result<()> {
        let conn = self.0.lock();
        let kept: Option<Option<Vec<u8>>> = conn
            .query_row(
                "SELECT visitors FROM edge_daily WHERE app = ?1 AND day = ?2",
                params![app, day as i64],
                |r| r.get(0),
            )
            .optional()?;
        let mut merged = registers.to_vec();
        if let Some(kept) = kept.flatten().filter(|k| k.len() == merged.len()) {
            hll::merge(&mut merged, &kept);
            if merged == kept {
                return Ok(());
            }
        }
        conn.execute(
            "INSERT INTO edge_daily (app, day, visitors) VALUES (?1, ?2, ?3)
             ON CONFLICT (app, day) DO UPDATE SET visitors = excluded.visitors",
            params![app, day as i64, merged],
        )?;
        Ok(())
    }

    /// Visitor sketches `(day, registers)` of an app from `from` on.
    pub fn visitor_sketches(&self, app: &str, from: u64) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT day, visitors FROM edge_daily
             WHERE app = ?1 AND day >= ?2 AND visitors IS NOT NULL ORDER BY day",
        )?;
        let rows = stmt
            .query_map(params![app, from as i64], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Adds failed requests to an app's bucket starting at `at`.
    pub fn add_errors(&self, app: &str, at: u64, adds: &[ErrorAdd]) -> anyhow::Result<()> {
        if adds.is_empty() {
            return Ok(());
        }
        let mut conn = self.0.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO edge_errors (app, at, status, method, path, count, first_seen, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (app, at, status, method, path) DO UPDATE SET
                    count = count + excluded.count,
                    first_seen = min(first_seen, excluded.first_seen),
                    last_seen = max(last_seen, excluded.last_seen)",
            )?;
            for a in adds {
                stmt.execute(params![
                    app,
                    at as i64,
                    a.status,
                    a.method,
                    a.path,
                    a.count as i64,
                    a.first_seen as i64,
                    a.last_seen as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// An app's failed requests from the bucket holding `since` on, added up,
    /// most frequent first: at most `limit` of them, and how many groups there
    /// were in all.
    pub fn errors_since(
        &self,
        app: &str,
        since: u64,
        limit: usize,
    ) -> anyhow::Result<(Vec<ErrorRow>, usize)> {
        let from = since / ERROR_BUCKET * ERROR_BUCKET;
        let conn = self.0.lock();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM edge_errors WHERE app = ?1 AND at >= ?2
             GROUP BY status, method, path)",
            params![app, from as i64],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare_cached(
            "SELECT status, method, path, SUM(count) AS n, MIN(first_seen), MAX(last_seen)
             FROM edge_errors WHERE app = ?1 AND at >= ?2
             GROUP BY status, method, path ORDER BY n DESC, MAX(last_seen) DESC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![app, from as i64, limit as i64], |r| {
                Ok(ErrorRow {
                    status: r.get(0)?,
                    method: r.get(1)?,
                    path: r.get(2)?,
                    count: r.get::<_, i64>(3)? as u64,
                    first_seen: r.get::<_, i64>(4)? as u64,
                    last_seen: r.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((rows, total as usize))
    }

    pub fn uptime_day(&self, app: &str, day: u64) -> anyhow::Result<Option<Vec<u8>>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                "SELECT minutes FROM uptime_minutes WHERE app = ?1 AND day = ?2",
                params![app, day as i64],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn put_uptime_day(&self, app: &str, day: u64, minutes: &[u8]) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT INTO uptime_minutes (app, day, minutes) VALUES (?1, ?2, ?3)
             ON CONFLICT (app, day) DO UPDATE SET minutes = excluded.minutes",
            params![app, day as i64, minutes],
        )?;
        Ok(())
    }

    /// An app's minutes `(day, minutes)` from `from` on.
    pub fn uptime_days(&self, app: &str, from: u64) -> anyhow::Result<Vec<(u64, Vec<u8>)>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT day, minutes FROM uptime_minutes WHERE app = ?1 AND day >= ?2 ORDER BY day",
        )?;
        let rows = stmt
            .query_map(params![app, from as i64], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Every domain's `(domain, last_ok, down_since)`.
    pub fn reach_marks(&self) -> anyhow::Result<Vec<ReachMark>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare("SELECT domain, last_ok, down_since FROM domain_reach")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                    r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                ))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn put_reach_mark(
        &self,
        domain: &str,
        last_ok: Option<u64>,
        down_since: Option<u64>,
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT INTO domain_reach (domain, last_ok, down_since) VALUES (?1, ?2, ?3)
             ON CONFLICT (domain) DO UPDATE SET last_ok = excluded.last_ok,
                down_since = excluded.down_since",
            params![
                domain,
                last_ok.map(|v| v as i64),
                down_since.map(|v| v as i64)
            ],
        )?;
        Ok(())
    }

    pub fn delete_reach_mark(&self, domain: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM domain_reach WHERE domain = ?1", [domain])?;
        Ok(())
    }

    /// Drops what is older than each table keeps.
    pub fn prune_edge(&self, now: u64) -> anyhow::Result<()> {
        let today = facade_core::insights::day_of(now);
        let conn = self.0.lock();
        let daily = today.saturating_sub(DAILY_KEEP_DAYS) as i64;
        conn.execute("DELETE FROM edge_daily WHERE day < ?1", [daily])?;
        conn.execute("DELETE FROM edge_node_daily WHERE day < ?1", [daily])?;
        conn.execute(
            "DELETE FROM edge_errors WHERE at < ?1",
            [now.saturating_sub(ERRORS_KEEP) as i64],
        )?;
        conn.execute(
            "DELETE FROM uptime_minutes WHERE day < ?1",
            [today.saturating_sub(UPTIME_KEEP_DAYS) as i64],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn bytes_visitors_and_errors_add_up() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store.add_bytes("a", "main", 10, 100, 1000).unwrap();
        store.add_bytes("a", "main", 10, 1, 10).unwrap();
        store.add_bytes("a", "eu", 11, 5, 50).unwrap();
        assert_eq!(
            store.bytes_by_day("a", false, 0).unwrap(),
            [(10, 101, 1010), (11, 5, 50)]
        );
        assert_eq!(
            store.bytes_by_day("main", true, 0).unwrap(),
            [(10, 101, 1010)]
        );

        let mut a = vec![0u8; hll::REGISTERS];
        a[0] = 3;
        let mut b = vec![0u8; hll::REGISTERS];
        b[0] = 1;
        b[1] = 2;
        store.merge_visitors("a", 10, &a).unwrap();
        store.merge_visitors("a", 10, &b).unwrap();
        let sketches = store.visitor_sketches("a", 0).unwrap();
        assert_eq!(sketches.len(), 1);
        assert_eq!(&sketches[0].1[..3], &[3, 2, 0]);

        let add = |path, count, at| ErrorAdd {
            status: 404,
            method: "GET",
            path,
            count,
            first_seen: at,
            last_seen: at,
        };
        store
            .add_errors("a", 600, &[add("/x", 2, 650), add("/y", 1, 700)])
            .unwrap();
        store.add_errors("a", 1200, &[add("/x", 5, 1300)]).unwrap();
        let (rows, total) = store.errors_since("a", 0, 10).unwrap();
        assert_eq!(total, 2);
        assert_eq!(
            rows.iter()
                .map(|r| (r.path.as_str(), r.count, r.first_seen, r.last_seen))
                .collect::<Vec<_>>(),
            [("/x", 7, 650, 1300), ("/y", 1, 700, 700)]
        );
        // From the bucket holding the start on.
        let (rows, _) = store.errors_since("a", 1250, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 5);
        let (rows, total) = store.errors_since("a", 0, 1).unwrap();
        assert_eq!((rows.len(), total), (1, 2));

        store.prune_edge(86_400 * 100).unwrap();
        assert!(store.bytes_by_day("a", false, 0).unwrap().is_empty());
        assert_eq!(store.errors_since("a", 0, 10).unwrap().1, 0);
    }
}
