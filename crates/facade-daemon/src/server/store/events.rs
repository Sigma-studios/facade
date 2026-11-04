//! The Activity table: one row per entry, a crash loop or a burst of disk
//! errors folded into one row with a count.

use facade_core::events::{
    Category, EventDetails, EventKind, EventView, EventsPage, Scope, Severity,
};
use rusqlite::types::ToSql;
use rusqlite::{Connection, OptionalExtension, params};

use super::Store;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    first_at INTEGER NOT NULL,
    count INTEGER NOT NULL DEFAULT 1,
    scope TEXT NOT NULL,
    app TEXT,
    node TEXT,
    kind TEXT NOT NULL,
    category TEXT NOT NULL,
    severity TEXT NOT NULL,
    problem INTEGER NOT NULL DEFAULT 0,
    summary TEXT NOT NULL,
    actor TEXT,
    details TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS events_at ON events(at, id);
CREATE INDEX IF NOT EXISTS events_app ON events(app, at);
CREATE INDEX IF NOT EXISTS events_node ON events(node, scope, at);
"#;

pub(super) fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// How long the same thing happening again folds into its entry.
const FOLD_WITHIN: u64 = 15 * 60;

/// An entry to store.
#[derive(Debug, Clone, Default)]
pub struct EventRecord {
    pub at: u64,
    pub scope: Scope,
    pub app: Option<String>,
    pub node: Option<String>,
    pub kind: Option<EventKind>,
    pub severity: Severity,
    pub problem: bool,
    pub summary: String,
    pub actor: Option<String>,
    pub details: EventDetails,
    /// The same entry again within a few minutes (same app or node, kind and
    /// summary, nothing else in between) adds to its count instead.
    pub fold: bool,
}

/// What to list. `apps`: those the caller may see (`None`: everything).
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    pub apps: Option<Vec<String>>,
    pub app: Option<String>,
    pub node: Option<String>,
    pub category: Option<Category>,
    pub kind: Option<EventKind>,
    pub problems: bool,
    pub before: Option<i64>,
    pub limit: u32,
}

const COLUMNS: &str =
    "id, at, first_at, count, scope, app, node, kind, severity, problem, summary, actor, details";

fn event_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventView> {
    let details: String = r.get(12)?;
    let kind: String = r.get(7)?;
    let scope: String = r.get(4)?;
    let severity: String = r.get(8)?;
    Ok(EventView {
        id: r.get(0)?,
        at: r.get(1)?,
        first_at: r.get(2)?,
        count: r.get(3)?,
        scope: Scope::parse(&scope),
        app: r.get(5)?,
        node: r.get(6)?,
        kind: EventKind::parse(&kind),
        severity: Severity::parse(&severity),
        problem: r.get(9)?,
        summary: r.get(10)?,
        actor: r.get(11)?,
        details: serde_json::from_str(&details).unwrap_or_default(),
        has_logs: false,
    })
}

/// Adds a query argument, returning its placeholder.
fn arg(v: Box<dyn ToSql>, args: &mut Vec<Box<dyn ToSql>>) -> String {
    args.push(v);
    format!("?{}", args.len())
}

impl Store {
    /// Stores an entry (or folds it into the last one), returning its id.
    pub fn insert_event(&self, e: &EventRecord) -> anyhow::Result<i64> {
        let kind = e.kind.unwrap_or(EventKind::Other);
        let details = serde_json::to_string(&e.details)?;
        let conn = self.0.lock();
        if e.fold {
            let last: Option<(i64, u64, String, String, String)> = conn
                .query_row(
                    "SELECT id, at, kind, summary, details FROM events
                     WHERE app IS ?1 AND node IS ?2 AND scope = ?3
                     ORDER BY at DESC, id DESC LIMIT 1",
                    params![e.app, e.node, e.scope.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            if let Some((id, at, k, summary, before)) = last
                && k == kind.as_str()
                && summary == e.summary
                && e.at.saturating_sub(at) <= FOLD_WITHIN
            {
                // The latest occurrence's details are the ones worth reading,
                // with the last log lines taken (snapshots are not taken at
                // every turn of a crash loop).
                let mut latest = e.details.clone();
                if latest.logs.is_empty() {
                    latest.logs = serde_json::from_str::<EventDetails>(&before)
                        .map(|d| d.logs)
                        .unwrap_or_default();
                }
                conn.execute(
                    "UPDATE events SET count = count + 1, at = MAX(at, ?2), details = ?3 WHERE id = ?1",
                    params![id, e.at, serde_json::to_string(&latest)?],
                )?;
                return Ok(id);
            }
        }
        conn.execute(
            "INSERT INTO events (at, first_at, scope, app, node, kind, category, severity, problem, summary, actor, details)
             VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                e.at,
                e.scope.as_str(),
                e.app,
                e.node,
                kind.as_str(),
                kind.category().as_str(),
                e.severity.as_str(),
                e.problem,
                e.summary,
                e.actor,
                details
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Entries matching `q`, newest first, without their log lines.
    pub fn query_events(&self, q: &EventQuery) -> anyhow::Result<EventsPage> {
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(apps) = &q.apps {
            if apps.is_empty() {
                return Ok(EventsPage::default());
            }
            let marks: Vec<String> = apps
                .iter()
                .map(|a| arg(Box::new(a.clone()), &mut args))
                .collect();
            clauses.push(format!("app IN ({})", marks.join(", ")));
        }
        if let Some(app) = &q.app {
            let m = arg(Box::new(app.clone()), &mut args);
            clauses.push(format!("app = {m}"));
        }
        if let Some(node) = &q.node {
            let m = arg(Box::new(node.clone()), &mut args);
            clauses.push(format!("scope = 'node' AND node = {m}"));
        }
        if let Some(category) = q.category {
            let m = arg(Box::new(category.as_str()), &mut args);
            clauses.push(format!("category = {m}"));
        }
        if let Some(kind) = q.kind {
            let m = arg(Box::new(kind.as_str()), &mut args);
            clauses.push(format!("kind = {m}"));
        }
        if q.problems {
            clauses.push("problem = 1".into());
        }
        let conn = self.0.lock();
        if let Some(before) = q.before {
            let Some(at) = conn
                .query_row("SELECT at FROM events WHERE id = ?1", [before], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()?
            else {
                return Ok(EventsPage::default());
            };
            let a = arg(Box::new(at), &mut args);
            let i = arg(Box::new(before), &mut args);
            clauses.push(format!("(at < {a} OR (at = {a} AND id < {i}))"));
        }
        let limit = q.limit.clamp(1, 500);
        let filter = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let sql = format!(
            "SELECT {COLUMNS} FROM events {filter} ORDER BY at DESC, id DESC LIMIT {}",
            limit + 1
        );
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let mut events: Vec<EventView> = stmt
            .query_map(refs.as_slice(), event_from_row)?
            .collect::<Result<_, _>>()?;
        let more = events.len() > limit as usize;
        events.truncate(limit as usize);
        for e in &mut events {
            e.has_logs = !e.details.logs.is_empty();
            e.details.logs.clear();
        }
        Ok(EventsPage { events, more })
    }

    /// One entry, log lines included.
    pub fn get_event(&self, id: i64) -> anyhow::Result<Option<EventView>> {
        let conn = self.0.lock();
        let mut e = conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM events WHERE id = ?1"),
                [id],
                event_from_row,
            )
            .optional()?;
        if let Some(e) = &mut e {
            e.has_logs = !e.details.logs.is_empty();
        }
        Ok(e)
    }

    /// The latest entry of `kind` on `node` since `since`.
    pub fn last_node_event(
        &self,
        node: &str,
        kind: EventKind,
        since: u64,
    ) -> anyhow::Result<Option<EventView>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM events WHERE node = ?1 AND kind = ?2 AND at >= ?3
                     ORDER BY at DESC, id DESC LIMIT 1"
                ),
                params![node, kind.as_str(), since],
                event_from_row,
            )
            .optional()?)
    }

    /// The latest entry about an alert (by its key): its id and kind.
    pub fn last_alert_event(&self, key: &str) -> anyhow::Result<Option<(i64, EventKind)>> {
        let conn = self.0.lock();
        Ok(conn
            .query_row(
                "SELECT id, kind FROM events
                 WHERE category = 'alerts' AND json_extract(details, '$.alert') = ?1
                 ORDER BY at DESC, id DESC LIMIT 1",
                [key],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        EventKind::parse(&r.get::<_, String>(1)?),
                    ))
                },
            )
            .optional()?)
    }

    /// How many times each app had each of `kinds` in `[from, to)`, a folded
    /// entry counting for its occurrences: `(app, kind, count)`.
    pub fn app_event_counts(
        &self,
        kinds: &[EventKind],
        from: u64,
        to: u64,
    ) -> anyhow::Result<Vec<(String, EventKind, u64)>> {
        let list: Vec<String> = kinds.iter().map(|k| format!("'{}'", k.as_str())).collect();
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT app, kind, SUM(count) FROM events
             WHERE app IS NOT NULL AND at >= ?1 AND at < ?2 AND kind IN ({})
             GROUP BY app, kind",
            list.join(", ")
        ))?;
        let rows = stmt
            .query_map(params![from, to], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    EventKind::parse(&r.get::<_, String>(1)?),
                    r.get::<_, i64>(2)? as u64,
                ))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Drops entries older than `before`.
    pub fn prune_events(&self, before: u64) -> anyhow::Result<usize> {
        Ok(self
            .0
            .lock()
            .execute("DELETE FROM events WHERE at < ?1", [before])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("facade-events-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir.join("facade.db")).unwrap(), dir)
    }

    fn crash(at: u64, app: &str) -> EventRecord {
        EventRecord {
            at,
            scope: Scope::App,
            app: Some(app.into()),
            node: Some("main".into()),
            kind: Some(EventKind::Crash),
            severity: Severity::Error,
            problem: true,
            summary: "Crashed: exit code 1".into(),
            details: EventDetails {
                logs: vec![format!("line at {at}")],
                ..Default::default()
            },
            fold: true,
            ..Default::default()
        }
    }

    #[test]
    fn counts_per_app_and_kind_with_folded_ones() {
        let (store, dir) = store();
        store.insert_event(&crash(1000, "blog")).unwrap();
        store.insert_event(&crash(1060, "blog")).unwrap();
        store.insert_event(&crash(1070, "api")).unwrap();
        // Outside the period.
        store.insert_event(&crash(5000, "blog")).unwrap();
        let mut restart = crash(1200, "blog");
        restart.kind = Some(EventKind::Restart);
        restart.fold = false;
        store.insert_event(&restart).unwrap();
        let mut counts = store
            .app_event_counts(&[EventKind::Crash, EventKind::Restart], 0, 2000)
            .unwrap();
        counts.sort_by(|a, b| (&a.0, a.1.as_str()).cmp(&(&b.0, b.1.as_str())));
        assert_eq!(
            counts,
            [
                ("api".to_string(), EventKind::Crash, 1),
                ("blog".to_string(), EventKind::Crash, 2),
                ("blog".to_string(), EventKind::Restart, 1),
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn crash_loops_fold_and_pages_follow() {
        let (store, dir) = store();
        let first = store.insert_event(&crash(1000, "blog")).unwrap();
        assert_eq!(store.insert_event(&crash(1060, "blog")).unwrap(), first);
        // Another app in between does not break the fold for this one.
        store.insert_event(&crash(1070, "api")).unwrap();
        assert_eq!(store.insert_event(&crash(1100, "blog")).unwrap(), first);
        let e = store.get_event(first).unwrap().unwrap();
        assert_eq!((e.count, e.first_at, e.at), (3, 1000, 1100));
        assert_eq!(e.details.logs, vec!["line at 1100".to_string()]);
        // A turn without a snapshot keeps the last one.
        let mut bare = crash(1110, "blog");
        bare.details.logs.clear();
        bare.details.exit_code = Some(2);
        assert_eq!(store.insert_event(&bare).unwrap(), first);
        let e = store.get_event(first).unwrap().unwrap();
        assert_eq!(e.details.logs, vec!["line at 1100".to_string()]);
        assert_eq!(e.details.exit_code, Some(2));
        // Long after: a new entry.
        let later = store
            .insert_event(&crash(1110 + FOLD_WITHIN + 1, "blog"))
            .unwrap();
        assert_ne!(later, first);
        // Something else in between: a new entry too.
        store
            .insert_event(&EventRecord {
                at: 5000,
                app: Some("blog".into()),
                node: Some("main".into()),
                kind: Some(EventKind::Restart),
                summary: "Restarted by admin".into(),
                ..Default::default()
            })
            .unwrap();
        let after = store.insert_event(&crash(5010, "blog")).unwrap();
        assert_ne!(after, later);

        let page = store
            .query_events(&EventQuery {
                app: Some("blog".into()),
                limit: 2,
                ..Default::default()
            })
            .unwrap();
        assert!(page.more);
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[0].id, after);
        assert!(page.events[0].has_logs && page.events[0].details.logs.is_empty());
        let next = store
            .query_events(&EventQuery {
                app: Some("blog".into()),
                before: Some(page.events[1].id),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert!(!next.more);
        assert_eq!(next.events.len(), 2);
        assert_eq!(next.events[1].id, first);

        let problems = store
            .query_events(&EventQuery {
                problems: true,
                category: Some(Category::Restarts),
                apps: Some(vec!["api".into()]),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(problems.events.len(), 1);
        assert_eq!(problems.events[0].app.as_deref(), Some("api"));
        // A member seeing no app sees nothing.
        let none = store
            .query_events(&EventQuery {
                apps: Some(vec![]),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert!(none.events.is_empty());
        let alert = |kind, at| EventRecord {
            at,
            kind: Some(kind),
            summary: "x".into(),
            details: EventDetails {
                alert: Some("app:blog:down".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(store.last_alert_event("app:blog:down").unwrap(), None);
        let fired = store
            .insert_event(&alert(EventKind::AlertFired, 6000))
            .unwrap();
        assert_eq!(
            store.last_alert_event("app:blog:down").unwrap(),
            Some((fired, EventKind::AlertFired))
        );
        let resolved = store
            .insert_event(&alert(EventKind::AlertResolved, 6100))
            .unwrap();
        assert_eq!(
            store.last_alert_event("app:blog:down").unwrap(),
            Some((resolved, EventKind::AlertResolved))
        );
        assert_eq!(store.prune_events(1111).unwrap(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
