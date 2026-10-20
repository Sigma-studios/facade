//! SQLite persistence. Queries are tiny, so a single mutex-guarded connection is
//! plenty and keeps the API synchronous.

use std::path::Path;

use anyhow::Context;
use facade_core::AppSpec;
use facade_core::api::{DeploymentStatus, DeploymentView};
use facade_core::proto::CredentialKind;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

pub struct Store(Mutex<Connection>);

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'admin',
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS nodes (
    name TEXT PRIMARY KEY,
    secret_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    hostname TEXT,
    address TEXT,
    agent_version TEXT,
    last_seen INTEGER
);
CREATE TABLE IF NOT EXISTS apps (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS env (
    app TEXT NOT NULL REFERENCES apps(name) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value BLOB NOT NULL,
    sensitive INTEGER NOT NULL,
    PRIMARY KEY (app, key)
);
CREATE TABLE IF NOT EXISTS deployments (
    id TEXT PRIMARY KEY,
    app TEXT NOT NULL,
    revision INTEGER NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    error TEXT,
    log TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS deployments_app ON deployments(app, started_at);
CREATE TABLE IF NOT EXISTS certs (
    domain TEXT PRIMARY KEY,
    cert_pem TEXT NOT NULL,
    key_pem BLOB NOT NULL,
    not_before INTEGER NOT NULL,
    not_after INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS dns_records (
    domain TEXT PRIMARY KEY,
    zone TEXT NOT NULL,
    ip TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS app_access (
    app TEXT NOT NULL REFERENCES apps(name) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (app, user_id)
);
"#;

/// Registry and git logins, each belonging to a user (`owner_id`; 0 for the
/// admins, who share theirs).
const CREDENTIALS: &str = r#"
CREATE TABLE IF NOT EXISTS credentials (
    kind TEXT NOT NULL,
    host TEXT NOT NULL,
    owner_id INTEGER NOT NULL DEFAULT 0,
    username TEXT NOT NULL,
    secret BLOB NOT NULL,
    PRIMARY KEY (kind, host, owner_id)
);
"#;

/// The owner id of logins shared by every admin.
pub const ADMINS: i64 = 0;

/// `ALTER TABLE … ADD COLUMN`, once.
fn add_column(conn: &Connection, table: &str, column: &str, kind: &str) -> anyhow::Result<()> {
    let exists = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .flatten()
        .any(|name| name == column);
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
    }
    Ok(())
}

pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub role: String,
    /// Shared by every app the user owns; `None` is no limit.
    pub memory_cap_mb: Option<u64>,
    pub cpu_cap: Option<f64>,
    pub must_change_password: bool,
}

impl UserRow {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

const USER_COLUMNS: &str =
    "u.id, u.username, u.password_hash, u.role, u.memory_cap_mb, u.cpu_cap, u.must_change_password";

fn user_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        id: r.get(0)?,
        username: r.get(1)?,
        password_hash: r.get(2)?,
        role: r.get(3)?,
        memory_cap_mb: r.get(4)?,
        cpu_cap: r.get(5)?,
        must_change_password: r.get(6)?,
    })
}

pub struct NodeRow {
    pub name: String,
    pub hostname: Option<String>,
    pub address: Option<String>,
    /// Set by hand; otherwise `address` (or the server's own) is used.
    pub public_ip: Option<String>,
    pub agent_version: Option<String>,
    pub last_seen: Option<u64>,
}

pub struct AppRow {
    pub spec: AppSpec,
    pub revision: u64,
    /// The user whose resources and logins the app uses; `None` for the admins.
    pub owner: Option<i64>,
}

pub struct EnvRow {
    pub key: String,
    pub sealed: Vec<u8>,
    pub sensitive: bool,
}

pub struct CredentialRow {
    pub owner_id: i64,
    pub kind: CredentialKind,
    pub host: String,
    pub username: String,
    pub sealed_secret: Vec<u8>,
}

pub struct CertRow {
    pub domain: String,
    pub cert_pem: String,
    pub sealed_key: Vec<u8>,
    pub not_before: u64,
    pub not_after: u64,
}

fn status_str(s: DeploymentStatus) -> &'static str {
    match s {
        DeploymentStatus::Running => "running",
        DeploymentStatus::Succeeded => "succeeded",
        DeploymentStatus::Failed => "failed",
    }
}

fn parse_status(s: &str) -> DeploymentStatus {
    match s {
        "running" => DeploymentStatus::Running,
        "succeeded" => DeploymentStatus::Succeeded,
        _ => DeploymentStatus::Failed,
    }
}

impl Store {
    /// The database of a running server, read only and as it is (no schema
    /// changes), for tools like `facaded upgrade-check`.
    pub fn open_read_only(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        Ok(Self(Mutex::new(conn)))
    }

    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        add_column(&conn, "nodes", "public_ip", "TEXT")?;
        add_column(&conn, "deployments", "git_commit", "TEXT")?;
        add_column(
            &conn,
            "users",
            "must_change_password",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column(&conn, "users", "memory_cap_mb", "INTEGER")?;
        add_column(&conn, "users", "cpu_cap", "REAL")?;
        add_column(&conn, "apps", "owner_id", "INTEGER")?;
        migrate_credentials(&conn)?;
        Ok(Self(Mutex::new(conn)))
    }

    // ---- users & sessions

    pub fn user_count(&self) -> anyhow::Result<i64> {
        Ok(self
            .0
            .lock()
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
    }

    /// `generated`: the password was made up by facade and must be changed.
    pub fn create_user(
        &self,
        username: &str,
        hash: &str,
        role: &str,
        generated: bool,
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT INTO users (username, password_hash, role, created_at, must_change_password)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![username, hash, role, facade_core::now(), generated],
        )?;
        Ok(())
    }

    pub fn flag_password_change(&self, user_id: i64) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE users SET must_change_password = 1 WHERE id = ?1",
            [user_id],
        )?;
        Ok(())
    }

    pub fn user_by_name(&self, username: &str) -> anyhow::Result<Option<UserRow>> {
        Ok(self
            .0
            .lock()
            .query_row(
                &format!("SELECT {USER_COLUMNS} FROM users u WHERE u.username = ?1"),
                [username],
                user_from_row,
            )
            .optional()?)
    }

    pub fn list_users(&self) -> anyhow::Result<Vec<UserRow>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {USER_COLUMNS} FROM users u ORDER BY u.username"
        ))?;
        let users = stmt
            .query_map([], user_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(users)
    }

    pub fn user_by_id(&self, id: i64) -> anyhow::Result<Option<UserRow>> {
        Ok(self
            .0
            .lock()
            .query_row(
                &format!("SELECT {USER_COLUMNS} FROM users u WHERE u.id = ?1"),
                [id],
                user_from_row,
            )
            .optional()?)
    }

    pub fn set_user_role(&self, id: i64, role: &str) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE users SET role = ?2 WHERE id = ?1",
            params![id, role],
        )?;
        Ok(())
    }

    pub fn set_user_caps(
        &self,
        id: i64,
        memory_cap_mb: Option<u64>,
        cpu_cap: Option<f64>,
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE users SET memory_cap_mb = ?2, cpu_cap = ?3 WHERE id = ?1",
            params![id, memory_cap_mb, cpu_cap],
        )?;
        Ok(())
    }

    /// Deletes a user, their sessions, grants and logins; their apps go back to
    /// the admins.
    pub fn delete_user(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute("UPDATE apps SET owner_id = NULL WHERE owner_id = ?1", [id])?;
        conn.execute("DELETE FROM credentials WHERE owner_id = ?1", [id])?;
        conn.execute("DELETE FROM users WHERE id = ?1", [id])?;
        Ok(())
    }

    /// A new password chosen by an admin: valid once, then it must be changed.
    pub fn reset_password(&self, user_id: i64, hash: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE users SET password_hash = ?1, must_change_password = 1 WHERE id = ?2",
            params![hash, user_id],
        )?;
        conn.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id])?;
        Ok(())
    }

    pub fn set_password(&self, user_id: i64, hash: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "UPDATE users SET password_hash = ?1, must_change_password = 0 WHERE id = ?2",
            params![hash, user_id],
        )?;
        conn.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id])?;
        Ok(())
    }

    pub fn create_session(
        &self,
        token_hash: &str,
        user_id: i64,
        expires_at: u64,
    ) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute(
            "DELETE FROM sessions WHERE expires_at < ?1",
            [facade_core::now()],
        )?;
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES (?1, ?2, ?3)",
            params![token_hash, user_id, expires_at],
        )?;
        Ok(())
    }

    pub fn session_user(&self, token_hash: &str) -> anyhow::Result<Option<UserRow>> {
        Ok(self
            .0
            .lock()
            .query_row(
                &format!(
                    "SELECT {USER_COLUMNS} FROM sessions s
                     JOIN users u ON u.id = s.user_id
                     WHERE s.token_hash = ?1 AND s.expires_at > ?2"
                ),
                params![token_hash, facade_core::now()],
                user_from_row,
            )
            .optional()?)
    }

    pub fn delete_session(&self, token_hash: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
        Ok(())
    }

    // ---- nodes

    pub fn upsert_node_secret(&self, name: &str, secret_hash: &str) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT INTO nodes (name, secret_hash, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET secret_hash = excluded.secret_hash",
            params![name, secret_hash, facade_core::now()],
        )?;
        Ok(())
    }

    pub fn node_exists(&self, name: &str) -> anyhow::Result<bool> {
        Ok(self.node_secret_hash(name)?.is_some())
    }

    pub fn node_secret_hash(&self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .query_row(
                "SELECT secret_hash FROM nodes WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn list_nodes(&self) -> anyhow::Result<Vec<NodeRow>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(
            "SELECT name, hostname, address, agent_version, last_seen, public_ip FROM nodes ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(NodeRow {
                    name: r.get(0)?,
                    hostname: r.get(1)?,
                    address: r.get(2)?,
                    agent_version: r.get(3)?,
                    last_seen: r.get(4)?,
                    public_ip: r.get(5)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn node_connected(
        &self,
        name: &str,
        hostname: &str,
        address: &str,
        agent_version: &str,
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE nodes SET hostname = ?2, address = ?3, agent_version = ?4, last_seen = ?5
             WHERE name = ?1",
            params![name, hostname, address, agent_version, facade_core::now()],
        )?;
        Ok(())
    }

    pub fn touch_node(&self, name: &str) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE nodes SET last_seen = ?2 WHERE name = ?1",
            params![name, facade_core::now()],
        )?;
        Ok(())
    }

    pub fn set_node_ip(&self, name: &str, ip: Option<&str>) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE nodes SET public_ip = ?2 WHERE name = ?1",
            params![name, ip],
        )?;
        Ok(())
    }

    pub fn delete_node(&self, name: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM nodes WHERE name = ?1", [name])?;
        Ok(())
    }

    // ---- apps

    fn app_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, u64, Option<i64>)> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    }

    fn parse_app((spec, revision, owner): (String, u64, Option<i64>)) -> anyhow::Result<AppRow> {
        Ok(AppRow {
            spec: serde_json::from_str(&spec)?,
            revision,
            owner,
        })
    }

    pub fn list_apps(&self) -> anyhow::Result<Vec<AppRow>> {
        let raw: Vec<(String, u64, Option<i64>)> = {
            let conn = self.0.lock();
            let mut stmt =
                conn.prepare("SELECT spec, revision, owner_id FROM apps ORDER BY name")?;
            stmt.query_map([], Self::app_from_row)?
                .collect::<Result<_, _>>()?
        };
        raw.into_iter().map(Self::parse_app).collect()
    }

    pub fn get_app(&self, name: &str) -> anyhow::Result<Option<AppRow>> {
        let raw = self
            .0
            .lock()
            .query_row(
                "SELECT spec, revision, owner_id FROM apps WHERE name = ?1",
                [name],
                Self::app_from_row,
            )
            .optional()?;
        raw.map(Self::parse_app).transpose()
    }

    /// Inserts or replaces the spec; the revision is kept.
    pub fn put_app(&self, spec: &AppSpec) -> anyhow::Result<()> {
        let now = facade_core::now();
        self.0.lock().execute(
            "INSERT INTO apps (name, spec, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, updated_at = excluded.updated_at",
            params![spec.name, serde_json::to_string(spec)?, now],
        )?;
        Ok(())
    }

    pub fn set_app_owner(&self, name: &str, owner: Option<i64>) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE apps SET owner_id = ?2 WHERE name = ?1",
            params![name, owner],
        )?;
        Ok(())
    }

    /// Users given access to an app besides its owner.
    pub fn app_access(&self, app: &str) -> anyhow::Result<Vec<i64>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare("SELECT user_id FROM app_access WHERE app = ?1")?;
        let ids = stmt
            .query_map([app], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    pub fn grant_access(&self, app: &str, user_id: i64) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT OR IGNORE INTO app_access (app, user_id) VALUES (?1, ?2)",
            params![app, user_id],
        )?;
        Ok(())
    }

    pub fn revoke_access(&self, app: &str, user_id: i64) -> anyhow::Result<bool> {
        Ok(self.0.lock().execute(
            "DELETE FROM app_access WHERE app = ?1 AND user_id = ?2",
            params![app, user_id],
        )? > 0)
    }

    pub fn bump_revision(&self, name: &str) -> anyhow::Result<u64> {
        Ok(self.0.lock().query_row(
            "UPDATE apps SET revision = revision + 1 WHERE name = ?1 RETURNING revision",
            [name],
            |r| r.get(0),
        )?)
    }

    pub fn delete_app(&self, name: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM apps WHERE name = ?1", [name])?;
        Ok(())
    }

    // ---- env

    pub fn list_env(&self, app: &str) -> anyhow::Result<Vec<EnvRow>> {
        let conn = self.0.lock();
        let mut stmt =
            conn.prepare("SELECT key, value, sensitive FROM env WHERE app = ?1 ORDER BY key")?;
        let rows = stmt
            .query_map([app], |r| {
                Ok(EnvRow {
                    key: r.get(0)?,
                    sealed: r.get(1)?,
                    sensitive: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn set_env(
        &self,
        app: &str,
        key: &str,
        sealed: &[u8],
        sensitive: bool,
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT INTO env (app, key, value, sensitive) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(app, key) DO UPDATE SET value = excluded.value, sensitive = excluded.sensitive",
            params![app, key, sealed, sensitive],
        )?;
        Ok(())
    }

    pub fn set_env_sensitivity(&self, app: &str, key: &str, sensitive: bool) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE env SET sensitive = ?3 WHERE app = ?1 AND key = ?2",
            params![app, key, sensitive],
        )?;
        Ok(())
    }

    pub fn delete_env(&self, app: &str, key: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM env WHERE app = ?1 AND key = ?2", [app, key])?;
        Ok(())
    }

    // ---- deployments

    pub fn create_deployment(&self, id: Uuid, app: &str, revision: u64) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT INTO deployments (id, app, revision, status, started_at) VALUES (?1, ?2, ?3, 'running', ?4)",
            params![id.to_string(), app, revision, facade_core::now()],
        )?;
        Ok(())
    }

    pub fn append_deploy_log(&self, id: Uuid, line: &str) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE deployments SET log = log || ?2 || char(10) WHERE id = ?1 AND status = 'running'",
            params![id.to_string(), line],
        )?;
        Ok(())
    }

    pub fn finish_deployment(
        &self,
        id: Uuid,
        error: Option<&str>,
        commit: Option<&str>,
    ) -> anyhow::Result<()> {
        let status = if error.is_some() {
            DeploymentStatus::Failed
        } else {
            DeploymentStatus::Succeeded
        };
        self.0.lock().execute(
            "UPDATE deployments SET status = ?2, error = ?3, finished_at = ?4, git_commit = ?5
             WHERE id = ?1 AND status = 'running'",
            params![
                id.to_string(),
                status_str(status),
                error,
                facade_core::now(),
                commit
            ],
        )?;
        Ok(())
    }

    /// Commit built by the app's last successful deployment from git.
    pub fn deployed_commit(&self, app: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .query_row(
                "SELECT git_commit FROM deployments
                 WHERE app = ?1 AND status = 'succeeded' AND git_commit IS NOT NULL
                 ORDER BY started_at DESC, rowid DESC LIMIT 1",
                [app],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Deployments that can no longer finish because their node went away.
    pub fn fail_running_deployments(&self, apps: &[String], reason: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        for app in apps {
            conn.execute(
                "UPDATE deployments SET status = 'failed', error = ?2, finished_at = ?3
                 WHERE app = ?1 AND status = 'running'",
                params![app, reason, facade_core::now()],
            )?;
        }
        Ok(())
    }

    pub fn list_deployments(&self, app: &str, limit: u32) -> anyhow::Result<Vec<DeploymentView>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(
            "SELECT id, app, revision, status, started_at, finished_at, error, '', git_commit FROM deployments
             WHERE app = ?1 ORDER BY started_at DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![app, limit], Self::deployment_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn get_deployment(&self, id: Uuid) -> anyhow::Result<Option<DeploymentView>> {
        Ok(self
            .0
            .lock()
            .query_row(
                "SELECT id, app, revision, status, started_at, finished_at, error, log, git_commit
                 FROM deployments WHERE id = ?1",
                [id.to_string()],
                Self::deployment_row,
            )
            .optional()?)
    }

    fn deployment_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeploymentView> {
        let id: String = r.get(0)?;
        let status: String = r.get(3)?;
        Ok(DeploymentView {
            id: id.parse().unwrap_or_default(),
            app: r.get(1)?,
            revision: r.get(2)?,
            status: parse_status(&status),
            started_at: r.get(4)?,
            finished_at: r.get(5)?,
            error: r.get(6)?,
            log: r.get(7)?,
            commit: r.get(8)?,
        })
    }

    // ---- certificates

    pub fn put_cert(&self, row: &CertRow) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT OR REPLACE INTO certs (domain, cert_pem, key_pem, not_before, not_after)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.domain,
                row.cert_pem,
                row.sealed_key,
                row.not_before,
                row.not_after
            ],
        )?;
        Ok(())
    }

    pub fn list_certs(&self) -> anyhow::Result<Vec<CertRow>> {
        let conn = self.0.lock();
        let mut stmt =
            conn.prepare("SELECT domain, cert_pem, key_pem, not_before, not_after FROM certs")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(CertRow {
                    domain: r.get(0)?,
                    cert_pem: r.get(1)?,
                    sealed_key: r.get(2)?,
                    not_before: r.get(3)?,
                    not_after: r.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    // ---- key/value

    pub fn kv_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn kv_put(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)",
            [key, value],
        )?;
        Ok(())
    }

    pub fn kv_delete(&self, key: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM kv WHERE key = ?1", [key])?;
        Ok(())
    }

    // ---- DNS records facade created

    /// `(domain, zone, ip)` of every record facade manages.
    pub fn dns_records(&self) -> anyhow::Result<Vec<(String, String, String)>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare("SELECT domain, zone, ip FROM dns_records ORDER BY domain")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn put_dns_record(&self, domain: &str, zone: &str, ip: &str) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT OR REPLACE INTO dns_records (domain, zone, ip, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params![domain, zone, ip, facade_core::now()],
        )?;
        Ok(())
    }

    pub fn delete_dns_record(&self, domain: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM dns_records WHERE domain = ?1", [domain])?;
        Ok(())
    }

    // ---- registry and git logins

    /// Logins of `owner` (a user id, or [`ADMINS`]), or everyone's.
    pub fn list_credentials(&self, owner: Option<i64>) -> anyhow::Result<Vec<CredentialRow>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(
            "SELECT owner_id, kind, host, username, secret FROM credentials
             WHERE ?1 IS NULL OR owner_id = ?1 ORDER BY owner_id, kind, host",
        )?;
        let rows = stmt
            .query_map([owner], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(owner_id, kind, host, username, sealed_secret)| {
                Some(CredentialRow {
                    owner_id,
                    kind: parse_kind(&kind)?,
                    host,
                    username,
                    sealed_secret,
                })
            })
            .collect())
    }

    pub fn put_credential(
        &self,
        owner: i64,
        kind: CredentialKind,
        host: &str,
        username: &str,
        sealed_secret: &[u8],
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT OR REPLACE INTO credentials (owner_id, kind, host, username, secret)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![owner, kind.to_string(), host, username, sealed_secret],
        )?;
        Ok(())
    }

    /// Whether there was one to delete.
    pub fn delete_credential(
        &self,
        owner: i64,
        kind: CredentialKind,
        host: &str,
    ) -> anyhow::Result<bool> {
        Ok(self.0.lock().execute(
            "DELETE FROM credentials WHERE owner_id = ?1 AND kind = ?2 AND host = ?3",
            params![owner, kind.to_string(), host],
        )? > 0)
    }

    /// A consistent copy of the whole database, safe while the server runs.
    pub fn snapshot(&self, dest: &Path) -> anyhow::Result<()> {
        let dest = dest.to_str().context("backup path is not UTF-8")?;
        self.0.lock().execute("VACUUM INTO ?1", [dest])?;
        Ok(())
    }
}

/// Logins used to belong to everyone: they become the admins'.
fn migrate_credentials(conn: &Connection) -> anyhow::Result<()> {
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(credentials)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    if columns.is_empty() {
        conn.execute_batch(CREDENTIALS)?;
    } else if !columns.iter().any(|c| c == "owner_id") {
        conn.execute_batch(&format!(
            "BEGIN;
             ALTER TABLE credentials RENAME TO credentials_old;
             {CREDENTIALS}
             INSERT INTO credentials (kind, host, owner_id, username, secret)
                 SELECT kind, host, 0, username, secret FROM credentials_old;
             DROP TABLE credentials_old;
             COMMIT;"
        ))?;
    }
    Ok(())
}

fn parse_kind(s: &str) -> Option<CredentialKind> {
    match s {
        "registry" => Some(CredentialKind::Registry),
        "git" => Some(CredentialKind::Git),
        _ => None,
    }
}
