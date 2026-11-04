//! History of what [`facade_core::resources`] measures: the last counters of
//! each usage bucket of an app or node (rates come from one bucket to the next,
//! like network bytes), volume sizes per hour, and the last health checks of
//! each app.

use std::collections::HashMap;

use facade_core::api::{UsagePoint, VolumeSizes};
use facade_core::proto::VolumeUsage;
use facade_core::resources::{
    AppResources, CpuThrottle, CpuTicks, DiskIo, HealthCheck, NodeResources, Pressure, Psi,
    ResourcePoint,
};
use rusqlite::{Connection, params};

use super::{Store, USAGE_BUCKET, UsageOf};

/// Counters are NULL where the agent did not know them, so that a report
/// without them leaves what an earlier one in the bucket said.
pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS app_resources (
    app TEXT NOT NULL,
    at INTEGER NOT NULL,
    thr_periods INTEGER,
    thr_count INTEGER,
    thr_usec INTEGER,
    io_read INTEGER,
    io_write INTEGER,
    PRIMARY KEY (app, at)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS node_resources (
    node TEXT NOT NULL,
    at INTEGER NOT NULL,
    psi_cpu INTEGER,
    psi_mem INTEGER,
    psi_io INTEGER,
    io_read INTEGER,
    io_write INTEGER,
    cpu_ticks INTEGER,
    iowait INTEGER,
    PRIMARY KEY (node, at)
) WITHOUT ROWID;
-- What each volume took, measured by its node's storage report, per hour.
CREATE TABLE IF NOT EXISTS volume_sizes (
    app TEXT NOT NULL,
    volume TEXT NOT NULL,
    at INTEGER NOT NULL,
    size INTEGER NOT NULL,
    PRIMARY KEY (app, volume, at)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS health_checks (
    app TEXT NOT NULL,
    at_ms INTEGER NOT NULL,
    ok INTEGER NOT NULL,
    ms INTEGER NOT NULL,
    output TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (app, at_ms)
) WITHOUT ROWID;
"#;

/// Seconds of volume sizes stored as one row.
const VOLUME_BUCKET: u64 = 3600;
/// Health checks kept per app.
pub const HEALTH_KEEP: usize = 60;

const APP_COLUMNS: [&str; 5] = [
    "thr_periods",
    "thr_count",
    "thr_usec",
    "io_read",
    "io_write",
];
const NODE_COLUMNS: [&str; 7] = [
    "psi_cpu",
    "psi_mem",
    "psi_io",
    "io_read",
    "io_write",
    "cpu_ticks",
    "iowait",
];

fn table(of: UsageOf) -> (&'static str, &'static str, &'static [&'static str]) {
    match of {
        UsageOf::App => ("app_resources", "app", &APP_COLUMNS),
        UsageOf::Node => ("node_resources", "node", &NODE_COLUMNS),
    }
}

type Row = Vec<Option<i64>>;

fn app_row(r: &AppResources) -> Row {
    let t = r.throttle.as_ref();
    let d = r.disk_io.as_ref();
    [
        t.map(|t| t.periods),
        t.map(|t| t.throttled),
        t.map(|t| t.throttled_usec),
        d.map(|d| d.read),
        d.map(|d| d.written),
    ]
    .map(|v| v.map(|v| v as i64))
    .to_vec()
}

fn node_row(r: &NodeResources) -> Row {
    let p = r.pressure.as_ref();
    let d = r.disk_io.as_ref();
    let c = r.cpu_ticks.as_ref();
    [
        p.and_then(|p| p.cpu).map(|p| p.total),
        p.and_then(|p| p.memory).map(|p| p.total),
        p.and_then(|p| p.io).map(|p| p.total),
        d.map(|d| d.read),
        d.map(|d| d.written),
        c.map(|c| c.total),
        c.map(|c| c.iowait),
    ]
    .map(|v| v.map(|v| v as i64))
    .to_vec()
}

fn get(row: &Row, i: usize) -> Option<u64> {
    row.get(i).copied().flatten().map(|v| v as u64)
}

fn app_resources(row: &Row) -> AppResources {
    let throttle = (|| {
        Some(CpuThrottle {
            periods: get(row, 0)?,
            throttled: get(row, 1)?,
            throttled_usec: get(row, 2)?,
        })
    })();
    AppResources {
        throttle,
        disk_io: disk(row, 3),
        ..Default::default()
    }
}

fn node_resources(row: &Row) -> NodeResources {
    let psi = |i| {
        get(row, i).map(|total| Psi {
            total,
            ..Default::default()
        })
    };
    let pressure = Pressure {
        cpu: psi(0),
        memory: psi(1),
        io: psi(2),
    };
    NodeResources {
        pressure: (!pressure.is_empty()).then_some(pressure),
        disk_io: disk(row, 3),
        cpu_ticks: (|| {
            Some(CpuTicks {
                total: get(row, 5)?,
                iowait: get(row, 6)?,
            })
        })(),
    }
}

fn disk(row: &Row, i: usize) -> Option<DiskIo> {
    Some(DiskIo {
        read: get(row, i)?,
        written: get(row, i + 1)?,
    })
}

impl Store {
    /// Keeps the counters of one report of apps (`name`, counters) or of a node.
    pub fn record_resources(
        &self,
        at: u64,
        apps: &[(String, AppResources)],
        node: (&str, &NodeResources),
    ) -> anyhow::Result<()> {
        let bucket = at - at % USAGE_BUCKET;
        let mut conn = self.0.lock();
        let tx = conn.transaction()?;
        let rows = apps
            .iter()
            .map(|(name, r)| (UsageOf::App, name.as_str(), app_row(r)))
            .chain([(UsageOf::Node, node.0, node_row(node.1))]);
        for (of, name, row) in rows {
            if row.iter().all(Option::is_none) {
                continue;
            }
            let (table, key, columns) = table(of);
            let places: Vec<String> = (3..3 + columns.len()).map(|i| format!("?{i}")).collect();
            let set: Vec<String> = columns
                .iter()
                .map(|c| format!("{c} = COALESCE(excluded.{c}, {c})"))
                .collect();
            let mut stmt = tx.prepare_cached(&format!(
                "INSERT INTO {table} ({key}, at, {}) VALUES (?1, ?2, {})
                 ON CONFLICT ({key}, at) DO UPDATE SET {}",
                columns.join(", "),
                places.join(", "),
                set.join(", ")
            ))?;
            let mut values: Vec<Box<dyn rusqlite::ToSql>> =
                vec![Box::new(name.to_string()), Box::new(bucket as i64)];
            values.extend(
                row.into_iter()
                    .map(|v| Box::new(v) as Box<dyn rusqlite::ToSql>),
            );
            stmt.execute(rusqlite::params_from_iter(values.iter()))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Volume sizes from a node's storage report.
    pub fn record_volume_sizes(&self, at: u64, volumes: &[VolumeUsage]) -> anyhow::Result<()> {
        let bucket = at - at % VOLUME_BUCKET;
        let mut conn = self.0.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO volume_sizes (app, volume, at, size) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (app, volume, at) DO UPDATE SET size = excluded.size",
            )?;
            for v in volumes {
                if let Some(size) = v.size {
                    stmt.execute(params![v.app, v.volume, bucket as i64, size as i64])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The sizes of the app's volumes since `since`, by volume.
    pub fn volume_sizes(&self, app: &str, since: u64) -> anyhow::Result<Vec<VolumeSizes>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT volume, at, size FROM volume_sizes WHERE app = ?1 AND at >= ?2
             ORDER BY volume, at",
        )?;
        let mut out: Vec<VolumeSizes> = Vec::new();
        let rows = stmt.query_map(params![app, since as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        for row in rows {
            let (volume, at, size) = row?;
            match out.last_mut() {
                Some(v) if v.volume == volume => v.points.push((at, size)),
                _ => out.push(VolumeSizes {
                    volume,
                    points: vec![(at, size)],
                }),
            }
        }
        Ok(out)
    }

    /// Adds health checks of an app (those already kept are skipped), keeping
    /// the last [`HEALTH_KEEP`].
    pub fn record_health(&self, app: &str, checks: &[HealthCheck]) -> anyhow::Result<()> {
        let mut conn = self.0.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO health_checks (app, at_ms, ok, ms, output)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for c in checks {
                stmt.execute(params![app, c.at_ms as i64, c.ok, c.ms as i64, c.output])?;
            }
            tx.execute(
                "DELETE FROM health_checks WHERE app = ?1 AND at_ms < (
                    SELECT MIN(at_ms) FROM (SELECT at_ms FROM health_checks WHERE app = ?1
                                            ORDER BY at_ms DESC LIMIT ?2))",
                params![app, HEALTH_KEEP as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The app's last health checks, oldest first.
    pub fn health_checks(&self, app: &str) -> anyhow::Result<Vec<HealthCheck>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT at_ms, ok, ms, output FROM health_checks WHERE app = ?1
             ORDER BY at_ms DESC LIMIT ?2",
        )?;
        let mut checks = stmt
            .query_map(params![app, HEALTH_KEEP as i64], |r| {
                Ok(HealthCheck {
                    at_ms: r.get::<_, i64>(0)? as u64,
                    ok: r.get(1)?,
                    ms: r.get::<_, i64>(2)? as u64,
                    output: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        checks.reverse();
        Ok(checks)
    }
}

/// Fills in the resource figures of `rows`, periods of `step` seconds read by
/// [`Store::usage`]: what the counters grew by since the period before, if
/// that one is adjacent.
pub(super) fn fill(
    conn: &Connection,
    of: UsageOf,
    name: &str,
    since: u64,
    step: u64,
    rows: &mut [UsagePoint],
) -> anyhow::Result<()> {
    let (table, key, columns) = table(of);
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT at / ?3 * ?3, {} FROM {table} WHERE {key} = ?1 AND at >= ?2 ORDER BY at",
        columns.join(", ")
    ))?;
    // Each period's last counters: each column's last known value.
    let mut last: HashMap<u64, Row> = HashMap::new();
    let buckets = stmt.query_map(params![name, since as i64, step as i64], |r| {
        let values = (1..=columns.len())
            .map(|i| r.get::<_, Option<i64>>(i))
            .collect::<Result<Row, _>>()?;
        Ok((r.get::<_, i64>(0)? as u64, values))
    })?;
    for bucket in buckets {
        let (period, values) = bucket?;
        let row = last
            .entry(period)
            .or_insert_with(|| vec![None; columns.len()]);
        for (kept, new) in row.iter_mut().zip(values) {
            if new.is_some() {
                *kept = new;
            }
        }
    }
    for i in 1..rows.len() {
        let (t0, t1) = (rows[i - 1].at, rows[i].at);
        if t1 - t0 > step * 2 {
            continue;
        }
        let (Some(a), Some(b)) = (last.get(&t0), last.get(&t1)) else {
            continue;
        };
        rows[i].resources = match of {
            UsageOf::App => {
                ResourcePoint::app_between((t0, &app_resources(a)), (t1, &app_resources(b)))
            }
            UsageOf::Node => {
                ResourcePoint::node_between((t0, &node_resources(a)), (t1, &node_resources(b)))
            }
        };
    }
    Ok(())
}

/// Drops counters and volume sizes older than `before`, as usage goes.
pub(super) fn prune(conn: &Connection, before: u64) -> anyhow::Result<()> {
    for table in ["app_resources", "node_resources", "volume_sizes"] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE at < ?1"),
            [before as i64],
        )?;
    }
    Ok(())
}

pub(super) fn delete_app(conn: &Connection, app: &str) -> anyhow::Result<()> {
    for table in ["app_resources", "health_checks"] {
        conn.execute(&format!("DELETE FROM {table} WHERE app = ?1"), [app])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::server::store::UsageSample;

    fn sample(name: &str) -> UsageSample {
        UsageSample {
            name: name.into(),
            cpu: 1.0,
            mem: 1,
            disk: 1,
            net_rx: 0,
            net_tx: 0,
            traffic: None,
        }
    }

    #[test]
    fn app_rates_come_from_counters() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let app = |periods, throttled, read, written| AppResources {
            throttle: Some(CpuThrottle {
                periods,
                throttled,
                throttled_usec: 0,
            }),
            disk_io: Some(DiskIo { read, written }),
            ..Default::default()
        };
        let node = NodeResources::default();
        let record = |at: u64, r: AppResources| {
            store
                .record_usage(UsageOf::App, at, &[sample("a")])
                .unwrap();
            store
                .record_resources(at, &[("a".into(), r)], ("n", &node))
                .unwrap();
        };
        record(600, app(100, 0, 0, 0));
        // A report without counters (an older agent) leaves the bucket's.
        record(650, AppResources::default());
        // 3000 periods in 300 s, 750 throttled; 300 MB written.
        record(900, app(3100, 750, 3000, 300_000_000));
        // A new container: counters start again.
        record(1200, app(10, 0, 0, 0));
        record(1500, app(3010, 0, 0, 0));
        let points = store.usage(UsageOf::App, "a", 0, USAGE_BUCKET).unwrap();
        let figures: Vec<_> = points
            .iter()
            .map(|p| (p.resources.throttled, p.resources.disk_write))
            .collect();
        assert_eq!(
            figures,
            vec![
                (None, None),
                (Some(25.0), Some(1_000_000.0)),
                (None, None),
                (Some(0.0), Some(0.0)),
            ]
        );
        assert_eq!(points[1].resources.disk_read, Some(10.0));
    }

    #[test]
    fn node_pressure_comes_from_totals() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let node = |psi_mem: Option<u64>, read| NodeResources {
            pressure: psi_mem.map(|total| Pressure {
                memory: Some(Psi {
                    total,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            disk_io: Some(DiskIo { read, written: 0 }),
            cpu_ticks: None,
        };
        for (at, r) in [
            (600, node(Some(0), 0)),
            (900, node(Some(30_000_000), 3_000)),
        ] {
            store
                .record_usage(UsageOf::Node, at, &[sample("n")])
                .unwrap();
            store.record_resources(at, &[], ("n", &r)).unwrap();
        }
        let points = store.usage(UsageOf::Node, "n", 0, USAGE_BUCKET).unwrap();
        // 30 s stalled in 300 s.
        assert_eq!(points[1].resources.pressure_mem, Some(10.0));
        assert_eq!(points[1].resources.pressure_cpu, None);
        assert_eq!(points[1].resources.disk_read, Some(10.0));
        store.prune_usage(900).unwrap();
        let points = store.usage(UsageOf::Node, "n", 0, USAGE_BUCKET).unwrap();
        assert!(points[0].resources.is_empty());
    }

    #[test]
    fn volume_sizes_by_hour() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let volume = |size| VolumeUsage {
            name: "facade-a_data".into(),
            app: "a".into(),
            volume: "data".into(),
            size,
            limit: None,
            in_use: true,
        };
        store
            .record_volume_sizes(3600, &[volume(Some(10))])
            .unwrap();
        // Later in the same hour: the last measure.
        store
            .record_volume_sizes(4500, &[volume(Some(20))])
            .unwrap();
        store
            .record_volume_sizes(7200, &[volume(Some(30))])
            .unwrap();
        // Not measured: nothing.
        store.record_volume_sizes(10800, &[volume(None)]).unwrap();
        let sizes = store.volume_sizes("a", 0).unwrap();
        assert_eq!(sizes.len(), 1);
        assert_eq!(sizes[0].points, vec![(3600, 20), (7200, 30)]);
        assert!(store.volume_sizes("b", 0).unwrap().is_empty());
    }

    #[test]
    fn health_checks_keep_the_last_ones() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let check = |at_ms: u64| HealthCheck {
            at_ms,
            ok: at_ms.is_multiple_of(2),
            ms: 5,
            output: String::new(),
        };
        let batch = |from: u64| (from..from + 5).map(check).collect::<Vec<_>>();
        for from in (0..100).step_by(3) {
            // Docker's log overlaps from one report to the next.
            store.record_health("a", &batch(from)).unwrap();
        }
        let kept = store.health_checks("a").unwrap();
        assert_eq!(kept.len(), HEALTH_KEEP);
        assert_eq!(kept.last().unwrap().at_ms, 103);
        assert_eq!(kept[0].at_ms, 103 + 1 - HEALTH_KEEP as u64);
        store.delete_app("a").unwrap();
        assert!(store.health_checks("a").unwrap().is_empty());
    }
}
