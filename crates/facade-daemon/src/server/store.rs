//! SQLite persistence. Queries are tiny, so a single mutex-guarded connection is
//! plenty and keeps the API synchronous.

use std::path::Path;

use anyhow::Context;
use facade_core::AppSpec;
use facade_core::api::{
    DeploymentStatus, DeploymentView, GitCredentialKind, StackDeploymentService,
    StackDeploymentView, UsagePoint,
};
use facade_core::stack::StackSpec;
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
CREATE TABLE IF NOT EXISTS stacks (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    commit_sha TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS stack_vars (
    stack TEXT NOT NULL REFERENCES stacks(name) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value BLOB NOT NULL,
    sensitive INTEGER NOT NULL,
    PRIMARY KEY (stack, key)
);
CREATE TABLE IF NOT EXISTS stack_deployments (
    id TEXT PRIMARY KEY,
    stack TEXT NOT NULL,
    status TEXT NOT NULL,
    commit_sha TEXT,
    commit_title TEXT,
    created_at INTEGER NOT NULL,
    began_at INTEGER,
    finished_at INTEGER,
    error TEXT,
    services TEXT NOT NULL DEFAULT '[]',
    log TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS stack_deployments_stack ON stack_deployments(stack, created_at);
CREATE TABLE IF NOT EXISTS git_credentials (
    name TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    scope TEXT NOT NULL,
    username TEXT,
    secret BLOB NOT NULL,
    public_key TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS app_access (
    app TEXT NOT NULL REFERENCES apps(name) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (app, user_id)
);
CREATE TABLE IF NOT EXISTS registry_logins (
    host TEXT NOT NULL,
    owner_id INTEGER NOT NULL DEFAULT 0,
    username TEXT NOT NULL,
    secret BLOB NOT NULL,
    PRIMARY KEY (host, owner_id)
);
-- Apps' resource use, averaged per bucket (`USAGE_BUCKET`), kept `USAGE_KEEP`.
CREATE TABLE IF NOT EXISTS app_usage (
    app TEXT NOT NULL,
    at INTEGER NOT NULL,
    samples INTEGER NOT NULL,
    cpu REAL NOT NULL,
    cpu_peak REAL NOT NULL,
    mem REAL NOT NULL,
    mem_peak INTEGER NOT NULL,
    PRIMARY KEY (app, at)
) WITHOUT ROWID;
-- The same for nodes, with their disk use.
CREATE TABLE IF NOT EXISTS node_usage (
    node TEXT NOT NULL,
    at INTEGER NOT NULL,
    samples INTEGER NOT NULL,
    cpu REAL NOT NULL,
    cpu_peak REAL NOT NULL,
    mem REAL NOT NULL,
    mem_peak INTEGER NOT NULL,
    disk REAL NOT NULL,
    PRIMARY KEY (node, at)
) WITHOUT ROWID;
"#;

/// Seconds of app usage stored as one row.
pub const USAGE_BUCKET: u64 = 300;
/// How long usage is kept, in seconds.
pub const USAGE_KEEP: u64 = 30 * 86400;

/// Whose usage: an app's or a node's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageOf {
    App,
    Node,
}

impl UsageOf {
    /// Its table, the column naming the app or node, and what it reads as disk.
    fn sql(self) -> (&'static str, &'static str, &'static str) {
        match self {
            UsageOf::App => ("app_usage", "app", "0"),
            UsageOf::Node => ("node_usage", "node", "disk"),
        }
    }
}

/// One report's figures for an app or a node: CPU in percent, bytes otherwise.
pub struct UsageSample {
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
    pub disk: u64,
}

/// The owner id of logins and credentials shared by every admin.
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
    /// Commit of the last deploy, for git sources.
    pub commit: Option<String>,
    /// What a stack service's image is built from (see `Workload::build_key`).
    pub build_key: Option<String>,
    /// Variables a stack's compose file gives the service, sealed JSON.
    pub stack_env: Option<Vec<u8>>,
    /// The member whose resources and logins the app uses; `None` for the admins.
    pub owner: Option<i64>,
}

pub struct StackRow {
    pub spec: StackSpec,
    pub commit: Option<String>,
}

type RawApp = (
    String,
    u64,
    Option<String>,
    Option<String>,
    Option<Vec<u8>>,
    Option<i64>,
);

pub struct RegistryLoginRow {
    pub owner_id: i64,
    pub host: String,
    pub username: String,
    pub sealed_secret: Vec<u8>,
}

pub struct GitCredentialRow {
    /// A user id, or [`ADMINS`].
    pub owner_id: i64,
    pub name: String,
    pub kind: GitCredentialKind,
    pub scope: String,
    pub username: Option<String>,
    pub sealed_secret: Vec<u8>,
    pub public_key: Option<String>,
    pub created_at: u64,
}

pub struct EnvRow {
    pub key: String,
    pub sealed: Vec<u8>,
    pub sensitive: bool,
    /// Handed to builds as a secret too.
    pub build: bool,
}

pub struct CertRow {
    pub domain: String,
    pub cert_pem: String,
    pub sealed_key: Vec<u8>,
    pub not_before: u64,
    pub not_after: u64,
}

fn parse_status(s: &str) -> DeploymentStatus {
    match s {
        "queued" => DeploymentStatus::Queued,
        "running" => DeploymentStatus::Running,
        "succeeded" => DeploymentStatus::Succeeded,
        "cancelled" => DeploymentStatus::Cancelled,
        _ => DeploymentStatus::Failed,
    }
}

const DEPLOYMENT_COLUMNS: &str =
    "id, app, revision, status, started_at, began_at, finished_at, error, commit_sha, commit_title";

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
        add_column(&conn, "apps", "commit_sha", "TEXT")?;
        add_column(&conn, "deployments", "commit_sha", "TEXT")?;
        add_column(&conn, "deployments", "commit_title", "TEXT")?;
        add_column(&conn, "deployments", "began_at", "INTEGER")?;
        add_column(&conn, "deployments", "built", "INTEGER NOT NULL DEFAULT 0")?;
        add_column(&conn, "apps", "build_key", "TEXT")?;
        add_column(&conn, "apps", "stack_env", "BLOB")?;
        add_column(&conn, "apps", "owner_id", "INTEGER")?;
        add_column(
            &conn,
            "users",
            "must_change_password",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column(&conn, "users", "memory_cap_mb", "INTEGER")?;
        add_column(&conn, "users", "cpu_cap", "REAL")?;
        add_column(
            &conn,
            "git_credentials",
            "owner_id",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        migrate_from_branch_logins(&conn)?;
        add_column(&conn, "env", "build", "INTEGER NOT NULL DEFAULT 0")?;
        // A server restart loses track of builds in progress.
        conn.execute(
            "UPDATE deployments SET status = 'failed', error = 'the server restarted during the deployment',
             finished_at = ?1 WHERE status IN ('queued', 'running')",
            [facade_core::now()],
        )?;
        conn.execute(
            "UPDATE stack_deployments SET status = 'failed', error = 'the server restarted during the deployment',
             finished_at = ?1 WHERE status IN ('queued', 'running')",
            [facade_core::now()],
        )?;
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

    fn app_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawApp> {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    }

    fn parse_app(
        (spec, revision, commit, build_key, stack_env, owner): RawApp,
    ) -> anyhow::Result<AppRow> {
        Ok(AppRow {
            spec: serde_json::from_str(&spec)?,
            revision,
            commit,
            build_key,
            stack_env,
            owner,
        })
    }

    pub fn list_apps(&self) -> anyhow::Result<Vec<AppRow>> {
        let raw: Vec<RawApp> = {
            let conn = self.0.lock();
            let mut stmt = conn.prepare(
                "SELECT spec, revision, commit_sha, build_key, stack_env, owner_id
                 FROM apps ORDER BY name",
            )?;
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
                "SELECT spec, revision, commit_sha, build_key, stack_env, owner_id
                 FROM apps WHERE name = ?1",
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

    /// Starts a new revision of the app, built from `commit`.
    pub fn bump_revision(&self, name: &str, commit: Option<&str>) -> anyhow::Result<u64> {
        Ok(self.0.lock().query_row(
            "UPDATE apps SET revision = revision + 1, commit_sha = ?2 WHERE name = ?1 RETURNING revision",
            params![name, commit],
            |r| r.get(0),
        )?)
    }

    /// A stack service's new release: its spec, what its image is built from and
    /// the variables its file gives it. Returns the new revision.
    pub fn release_service(
        &self,
        spec: &AppSpec,
        commit: Option<&str>,
        build_key: Option<&str>,
        sealed_env: &[u8],
    ) -> anyhow::Result<u64> {
        let now = facade_core::now();
        let conn = self.0.lock();
        conn.execute(
            "INSERT INTO apps (name, spec, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, updated_at = excluded.updated_at",
            params![spec.name, serde_json::to_string(spec)?, now],
        )?;
        Ok(conn.query_row(
            "UPDATE apps SET revision = revision + 1, commit_sha = ?2, build_key = ?3, stack_env = ?4
             WHERE name = ?1 RETURNING revision",
            params![spec.name, commit, build_key, sealed_env],
            |r| r.get(0),
        )?)
    }

    /// Puts the app back to an earlier revision (after a cancelled deploy).
    pub fn set_revision(
        &self,
        name: &str,
        revision: u64,
        commit: Option<&str>,
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE apps SET revision = ?2, commit_sha = ?3 WHERE name = ?1",
            params![name, revision, commit],
        )?;
        Ok(())
    }

    /// Revision and commit of the app's last successful deployment.
    pub fn last_success(&self, app: &str) -> anyhow::Result<Option<(u64, Option<String>)>> {
        Ok(self
            .0
            .lock()
            .query_row(
                "SELECT revision, commit_sha FROM deployments
                 WHERE app = ?1 AND status = 'succeeded'
                 ORDER BY started_at DESC, rowid DESC LIMIT 1",
                [app],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn delete_app(&self, name: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        conn.execute("DELETE FROM apps WHERE name = ?1", [name])?;
        conn.execute("DELETE FROM app_usage WHERE app = ?1", [name])?;
        Ok(())
    }

    // ---- usage

    /// Folds one report's samples into their buckets.
    pub fn record_usage(
        &self,
        of: UsageOf,
        at: u64,
        samples: &[UsageSample],
    ) -> anyhow::Result<()> {
        let (table, key, _) = of.sql();
        let bucket = at - at % USAGE_BUCKET;
        let mut conn = self.0.lock();
        let tx = conn.transaction()?;
        {
            // The app table has no disk column: its value is bound and unused.
            let (disk_col, disk_val, disk_set) = match of {
                UsageOf::App => ("", "", ""),
                UsageOf::Node => (
                    ", disk",
                    ", ?5",
                    ", disk = (disk * samples + excluded.disk) / (samples + 1)",
                ),
            };
            let mut stmt = tx.prepare_cached(&format!(
                "INSERT INTO {table} ({key}, at, samples, cpu, cpu_peak, mem, mem_peak{disk_col})
                 VALUES (?1, ?2, 1, ?3, ?3, ?4, ?4{disk_val})
                 ON CONFLICT ({key}, at) DO UPDATE SET
                    cpu = (cpu * samples + excluded.cpu) / (samples + 1),
                    cpu_peak = max(cpu_peak, excluded.cpu),
                    mem = (mem * samples + excluded.mem) / (samples + 1),
                    mem_peak = max(mem_peak, excluded.mem_peak){disk_set},
                    samples = samples + 1"
            ))?;
            for s in samples {
                let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![
                    Box::new(s.name.clone()),
                    Box::new(bucket as i64),
                    Box::new(f64::from(s.cpu)),
                    Box::new(s.mem as i64),
                ];
                if of == UsageOf::Node {
                    values.push(Box::new(s.disk as f64));
                }
                stmt.execute(rusqlite::params_from_iter(values.iter()))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Usage since `since`, in buckets of `step` seconds (a multiple of
    /// `USAGE_BUCKET`), oldest first.
    pub fn usage(
        &self,
        of: UsageOf,
        name: &str,
        since: u64,
        step: u64,
    ) -> anyhow::Result<Vec<UsagePoint>> {
        let (table, key, disk) = of.sql();
        let step = step.max(USAGE_BUCKET) as i64;
        let conn = self.0.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT at / ?3 * ?3 AS b, SUM(cpu * samples) / SUM(samples), MAX(cpu_peak),
                    SUM(mem * samples) / SUM(samples), MAX(mem_peak),
                    SUM({disk} * samples) / SUM(samples)
             FROM {table} WHERE {key} = ?1 AND at >= ?2 GROUP BY b ORDER BY b"
        ))?;
        let rows = stmt
            .query_map(params![name, since as i64, step], |r| {
                Ok(UsagePoint {
                    at: r.get::<_, i64>(0)? as u64,
                    cpu: r.get::<_, f64>(1)? as f32,
                    cpu_peak: r.get::<_, f64>(2)? as f32,
                    mem: r.get::<_, f64>(3)? as u64,
                    mem_peak: r.get::<_, i64>(4)? as u64,
                    disk: (of == UsageOf::Node).then(|| r.get::<_, f64>(5).unwrap_or(0.0) as u64),
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Drops usage older than `before`.
    pub fn prune_usage(&self, before: u64) -> anyhow::Result<()> {
        let conn = self.0.lock();
        for (table, _, _) in [UsageOf::App.sql(), UsageOf::Node.sql()] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE at < ?1"),
                [before as i64],
            )?;
        }
        Ok(())
    }

    // ---- env

    pub fn list_env(&self, app: &str) -> anyhow::Result<Vec<EnvRow>> {
        let conn = self.0.lock();
        let mut stmt = conn
            .prepare("SELECT key, value, sensitive, build FROM env WHERE app = ?1 ORDER BY key")?;
        let rows = stmt
            .query_map([app], |r| {
                Ok(EnvRow {
                    key: r.get(0)?,
                    sealed: r.get(1)?,
                    sensitive: r.get(2)?,
                    build: r.get(3)?,
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

    pub fn set_env_build(&self, app: &str, key: &str, build: bool) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE env SET build = ?3 WHERE app = ?1 AND key = ?2",
            params![app, key, build],
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

    pub fn create_deployment(
        &self,
        id: Uuid,
        app: &str,
        revision: u64,
        commit: Option<&str>,
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT INTO deployments (id, app, revision, status, started_at, commit_sha)
             VALUES (?1, ?2, ?3, 'queued', ?4, ?5)",
            params![id.to_string(), app, revision, facade_core::now(), commit],
        )?;
        Ok(())
    }

    pub fn start_deployment(&self, id: Uuid) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE deployments SET status = 'running', began_at = ?2 WHERE id = ?1 AND status = 'queued'",
            params![id.to_string(), facade_core::now()],
        )?;
        Ok(())
    }

    /// The build phase of a stack service's deployment is over.
    pub fn deployment_built(&self, id: Uuid) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE deployments SET built = 1 WHERE id = ?1",
            [id.to_string()],
        )?;
        Ok(())
    }

    pub fn is_built(&self, id: Uuid) -> anyhow::Result<bool> {
        Ok(self
            .0
            .lock()
            .query_row(
                "SELECT built FROM deployments WHERE id = ?1",
                [id.to_string()],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            == Some(1))
    }

    pub fn set_deployment_commit(&self, id: Uuid, commit: &str, title: &str) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE deployments SET commit_sha = ?2, commit_title = ?3 WHERE id = ?1",
            params![id.to_string(), commit, title],
        )?;
        Ok(())
    }

    /// The subject of a commit, from any deployment that checked it out.
    pub fn commit_title(&self, commit: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .query_row(
                "SELECT commit_title FROM deployments
                 WHERE commit_sha = ?1 AND commit_title IS NOT NULL LIMIT 1",
                [commit],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn append_deploy_log(&self, id: Uuid, line: &str) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE deployments SET log = log || ?2 || char(10)
             WHERE id = ?1 AND status IN ('queued', 'running')",
            params![id.to_string(), line],
        )?;
        Ok(())
    }

    pub fn finish_deployment(
        &self,
        id: Uuid,
        error: Option<&str>,
        cancelled: bool,
    ) -> anyhow::Result<()> {
        let status = match (error, cancelled) {
            (_, true) => "cancelled",
            (Some(_), false) => "failed",
            (None, false) => "succeeded",
        };
        self.0.lock().execute(
            "UPDATE deployments SET status = ?2, error = ?3, finished_at = ?4
             WHERE id = ?1 AND status IN ('queued', 'running')",
            params![id.to_string(), status, error, facade_core::now()],
        )?;
        Ok(())
    }

    /// Deployments that can no longer finish because their node went away.
    pub fn fail_running_deployments(&self, apps: &[String], reason: &str) -> anyhow::Result<()> {
        let conn = self.0.lock();
        for app in apps {
            conn.execute(
                "UPDATE deployments SET status = 'failed', error = ?2, finished_at = ?3
                 WHERE app = ?1 AND status IN ('queued', 'running')",
                params![app, reason, facade_core::now()],
            )?;
        }
        Ok(())
    }

    pub fn list_deployments(&self, app: &str, limit: u32) -> anyhow::Result<Vec<DeploymentView>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DEPLOYMENT_COLUMNS}, '' FROM deployments
             WHERE app = ?1 ORDER BY started_at DESC, rowid DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(params![app, limit], Self::deployment_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// The latest deployments of every app.
    pub fn recent_deployments(&self, limit: u32) -> anyhow::Result<Vec<DeploymentView>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DEPLOYMENT_COLUMNS}, '' FROM deployments
             ORDER BY started_at DESC, rowid DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map(params![limit], Self::deployment_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn get_deployment(&self, id: Uuid) -> anyhow::Result<Option<DeploymentView>> {
        Ok(self
            .0
            .lock()
            .query_row(
                &format!("SELECT {DEPLOYMENT_COLUMNS}, log FROM deployments WHERE id = ?1"),
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
            created_at: r.get(4)?,
            started_at: r.get(5)?,
            finished_at: r.get(6)?,
            error: r.get(7)?,
            commit: r.get(8)?,
            commit_title: r.get(9)?,
            log: r.get(10)?,
        })
    }

    // ---- stacks

    pub fn list_stacks(&self) -> anyhow::Result<Vec<StackRow>> {
        let raw: Vec<(String, Option<String>)> = {
            let conn = self.0.lock();
            let mut stmt = conn.prepare("SELECT spec, commit_sha FROM stacks ORDER BY name")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        raw.into_iter()
            .map(|(spec, commit)| {
                Ok(StackRow {
                    spec: serde_json::from_str(&spec)?,
                    commit,
                })
            })
            .collect()
    }

    pub fn get_stack(&self, name: &str) -> anyhow::Result<Option<StackRow>> {
        Ok(self
            .list_stacks()?
            .into_iter()
            .find(|s| s.spec.name == name))
    }

    pub fn put_stack(&self, spec: &StackSpec) -> anyhow::Result<()> {
        let now = facade_core::now();
        self.0.lock().execute(
            "INSERT INTO stacks (name, spec, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, updated_at = excluded.updated_at",
            params![spec.name, serde_json::to_string(spec)?, now],
        )?;
        Ok(())
    }

    pub fn set_stack_commit(&self, name: &str, commit: Option<&str>) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE stacks SET commit_sha = ?2 WHERE name = ?1",
            params![name, commit],
        )?;
        Ok(())
    }

    pub fn delete_stack(&self, name: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM stacks WHERE name = ?1", [name])?;
        Ok(())
    }

    pub fn list_stack_vars(&self, stack: &str) -> anyhow::Result<Vec<EnvRow>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(
            "SELECT key, value, sensitive FROM stack_vars WHERE stack = ?1 ORDER BY key",
        )?;
        let rows = stmt
            .query_map([stack], |r| {
                Ok(EnvRow {
                    key: r.get(0)?,
                    sealed: r.get(1)?,
                    sensitive: r.get(2)?,
                    build: false,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn set_stack_var(
        &self,
        stack: &str,
        key: &str,
        sealed: &[u8],
        sensitive: bool,
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT INTO stack_vars (stack, key, value, sensitive) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(stack, key) DO UPDATE SET value = excluded.value, sensitive = excluded.sensitive",
            params![stack, key, sealed, sensitive],
        )?;
        Ok(())
    }

    pub fn delete_stack_var(&self, stack: &str, key: &str) -> anyhow::Result<()> {
        self.0.lock().execute(
            "DELETE FROM stack_vars WHERE stack = ?1 AND key = ?2",
            [stack, key],
        )?;
        Ok(())
    }

    pub fn create_stack_deployment(
        &self,
        id: Uuid,
        stack: &str,
        commit: Option<&str>,
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT INTO stack_deployments (id, stack, status, commit_sha, created_at)
             VALUES (?1, ?2, 'queued', ?3, ?4)",
            params![id.to_string(), stack, commit, facade_core::now()],
        )?;
        Ok(())
    }

    pub fn start_stack_deployment(&self, id: Uuid, title: Option<&str>) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE stack_deployments SET status = 'running', began_at = ?2, commit_title = ?3
             WHERE id = ?1 AND status = 'queued'",
            params![id.to_string(), facade_core::now(), title],
        )?;
        Ok(())
    }

    pub fn set_stack_deployment_commit(
        &self,
        id: Uuid,
        commit: Option<&str>,
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE stack_deployments SET commit_sha = ?2 WHERE id = ?1",
            params![id.to_string(), commit],
        )?;
        Ok(())
    }

    pub fn stack_deploy_log(&self, id: Uuid, line: &str) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE stack_deployments SET log = log || ?2 || char(10) WHERE id = ?1",
            params![id.to_string(), line],
        )?;
        Ok(())
    }

    pub fn set_stack_deployment_services(
        &self,
        id: Uuid,
        services: &[StackDeploymentService],
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE stack_deployments SET services = ?2 WHERE id = ?1",
            params![id.to_string(), serde_json::to_string(services)?],
        )?;
        Ok(())
    }

    pub fn finish_stack_deployment(
        &self,
        id: Uuid,
        error: Option<&str>,
        cancelled: bool,
    ) -> anyhow::Result<()> {
        let status = match (error, cancelled) {
            (_, true) => "cancelled",
            (Some(_), false) => "failed",
            (None, false) => "succeeded",
        };
        self.0.lock().execute(
            "UPDATE stack_deployments SET status = ?2, error = ?3, finished_at = ?4
             WHERE id = ?1 AND status IN ('queued', 'running')",
            params![id.to_string(), status, error, facade_core::now()],
        )?;
        Ok(())
    }

    const STACK_DEPLOYMENT_COLUMNS: &str = "id, stack, status, commit_sha, commit_title, created_at, began_at, finished_at, error, services";

    fn stack_deployment_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StackDeploymentView> {
        let id: String = r.get(0)?;
        let status: String = r.get(2)?;
        let services: String = r.get(9)?;
        Ok(StackDeploymentView {
            id: id.parse().unwrap_or_default(),
            stack: r.get(1)?,
            status: parse_status(&status),
            commit: r.get(3)?,
            commit_title: r.get(4)?,
            created_at: r.get(5)?,
            started_at: r.get(6)?,
            finished_at: r.get(7)?,
            error: r.get(8)?,
            services: serde_json::from_str(&services).unwrap_or_default(),
            log: r.get(10)?,
        })
    }

    pub fn list_stack_deployments(
        &self,
        stack: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<StackDeploymentView>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {}, '' FROM stack_deployments WHERE stack = ?1
             ORDER BY created_at DESC, rowid DESC LIMIT ?2",
            Self::STACK_DEPLOYMENT_COLUMNS
        ))?;
        let rows = stmt
            .query_map(params![stack, limit], Self::stack_deployment_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn get_stack_deployment(&self, id: Uuid) -> anyhow::Result<Option<StackDeploymentView>> {
        Ok(self
            .0
            .lock()
            .query_row(
                &format!(
                    "SELECT {}, log FROM stack_deployments WHERE id = ?1",
                    Self::STACK_DEPLOYMENT_COLUMNS
                ),
                [id.to_string()],
                Self::stack_deployment_row,
            )
            .optional()?)
    }

    // ---- git credentials

    pub fn put_git_credential(&self, row: &GitCredentialRow) -> anyhow::Result<()> {
        let kind = match row.kind {
            GitCredentialKind::SshKey => "ssh_key",
            GitCredentialKind::Token => "token",
        };
        self.0.lock().execute(
            "INSERT INTO git_credentials
                 (name, kind, scope, username, secret, public_key, created_at, owner_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.name,
                kind,
                row.scope,
                row.username,
                row.sealed_secret,
                row.public_key,
                row.created_at,
                row.owner_id
            ],
        )?;
        Ok(())
    }

    /// Credentials of `owner` (a user id, or [`ADMINS`]), or everyone's.
    pub fn list_git_credentials(
        &self,
        owner: Option<i64>,
    ) -> anyhow::Result<Vec<GitCredentialRow>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(
            "SELECT name, kind, scope, username, secret, public_key, created_at, owner_id
             FROM git_credentials WHERE ?1 IS NULL OR owner_id = ?1 ORDER BY name",
        )?;
        let rows = stmt
            .query_map([owner], |r| {
                let kind: String = r.get(1)?;
                Ok(GitCredentialRow {
                    name: r.get(0)?,
                    kind: if kind == "token" {
                        GitCredentialKind::Token
                    } else {
                        GitCredentialKind::SshKey
                    },
                    scope: r.get(2)?,
                    username: r.get(3)?,
                    sealed_secret: r.get(4)?,
                    public_key: r.get(5)?,
                    created_at: r.get(6)?,
                    owner_id: r.get(7)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Deletes `owner`'s credential (anyone's with `None`).
    pub fn delete_git_credential(&self, name: &str, owner: Option<i64>) -> anyhow::Result<bool> {
        Ok(self.0.lock().execute(
            "DELETE FROM git_credentials WHERE name = ?1 AND (?2 IS NULL OR owner_id = ?2)",
            params![name, owner],
        )? > 0)
    }

    // ---- registry logins

    /// Logins of `owner` (a user id, or [`ADMINS`]), or everyone's.
    pub fn list_registry_logins(
        &self,
        owner: Option<i64>,
    ) -> anyhow::Result<Vec<RegistryLoginRow>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(
            "SELECT owner_id, host, username, secret FROM registry_logins
             WHERE ?1 IS NULL OR owner_id = ?1 ORDER BY owner_id, host",
        )?;
        let rows = stmt
            .query_map([owner], |r| {
                Ok(RegistryLoginRow {
                    owner_id: r.get(0)?,
                    host: r.get(1)?,
                    username: r.get(2)?,
                    sealed_secret: r.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn put_registry_login(
        &self,
        owner: i64,
        host: &str,
        username: &str,
        sealed_secret: &[u8],
    ) -> anyhow::Result<()> {
        self.0.lock().execute(
            "INSERT OR REPLACE INTO registry_logins (owner_id, host, username, secret)
             VALUES (?1, ?2, ?3, ?4)",
            params![owner, host, username, sealed_secret],
        )?;
        Ok(())
    }

    /// Whether there was one to delete.
    pub fn delete_registry_login(&self, owner: i64, host: &str) -> anyhow::Result<bool> {
        Ok(self.0.lock().execute(
            "DELETE FROM registry_logins WHERE owner_id = ?1 AND host = ?2",
            params![owner, host],
        )? > 0)
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

    /// A consistent copy of the whole database, safe while the server runs.
    pub fn snapshot(&self, dest: &Path) -> anyhow::Result<()> {
        let dest = dest.to_str().context("backup path is not UTF-8")?;
        self.0.lock().execute("VACUUM INTO ?1", [dest])?;
        Ok(())
    }
}

/// Data written by the user-accounts branch before it was merged: commits in
/// `deployments.git_commit`, and logins in a `credentials` table.
fn migrate_from_branch_logins(conn: &Connection) -> anyhow::Result<()> {
    let columns = |table: &str| -> anyhow::Result<Vec<String>> {
        Ok(conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<_, _>>()?)
    };
    if columns("deployments")?.iter().any(|c| c == "git_commit") {
        conn.execute_batch(
            "UPDATE deployments SET commit_sha = git_commit
                 WHERE commit_sha IS NULL AND git_commit IS NOT NULL;
             UPDATE apps SET commit_sha = (
                 SELECT d.commit_sha FROM deployments d
                 WHERE d.app = apps.name AND d.status = 'succeeded' AND d.commit_sha IS NOT NULL
                 ORDER BY d.started_at DESC LIMIT 1)
                 WHERE commit_sha IS NULL;
             ALTER TABLE deployments DROP COLUMN git_commit;",
        )?;
    }
    let old = columns("credentials")?;
    if old.iter().any(|c| c == "owner_id") {
        conn.execute_batch(
            "INSERT OR IGNORE INTO registry_logins (host, owner_id, username, secret)
                 SELECT host, owner_id, username, secret FROM credentials WHERE kind = 'registry';
             INSERT OR IGNORE INTO git_credentials
                 (name, kind, scope, username, secret, created_at, owner_id)
                 SELECT host || '-' || owner_id, 'token', host, username, secret, 0, owner_id
                 FROM credentials WHERE kind = 'git';
             DROP TABLE credentials;",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_bucketed_and_merged() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let sample = |name: &str, cpu: f32, mem: u64| UsageSample {
            name: name.into(),
            cpu,
            mem,
            disk: mem * 10,
        };
        let app = UsageOf::App;
        // Two samples in one 5-minute bucket, one in the next.
        store
            .record_usage(app, 600, &[sample("a", 10.0, 100)])
            .unwrap();
        store
            .record_usage(app, 615, &[sample("a", 30.0, 300), sample("b", 1.0, 1)])
            .unwrap();
        store
            .record_usage(app, 900, &[sample("a", 50.0, 200), sample("b", 1.0, 1)])
            .unwrap();
        let points = store.usage(app, "a", 0, USAGE_BUCKET).unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(
            (points[0].at, points[0].cpu, points[0].cpu_peak),
            (600, 20.0, 30.0)
        );
        assert_eq!(
            (points[0].mem, points[0].mem_peak, points[0].disk),
            (200, 300, None)
        );
        // Merged into one 30-minute bucket, weighted by samples.
        let merged = store.usage(app, "a", 0, 1800).unwrap();
        assert_eq!(merged.len(), 1);
        assert!((merged[0].cpu - 30.0).abs() < 1e-4);
        assert_eq!(merged[0].cpu_peak, 50.0);
        // Nodes keep their disk too, apart from apps of the same name.
        store
            .record_usage(
                UsageOf::Node,
                600,
                &[sample("a", 5.0, 100), sample("a", 7.0, 300)],
            )
            .unwrap();
        let node = store.usage(UsageOf::Node, "a", 0, USAGE_BUCKET).unwrap();
        assert_eq!(
            (node.len(), node[0].cpu, node[0].disk),
            (1, 6.0, Some(2000))
        );
        store.prune_usage(900).unwrap();
        assert_eq!(store.usage(app, "a", 0, USAGE_BUCKET).unwrap().len(), 1);
        assert!(
            store
                .usage(UsageOf::Node, "a", 0, USAGE_BUCKET)
                .unwrap()
                .is_empty()
        );
        store.delete_app("a").unwrap();
        assert!(store.usage(app, "a", 0, USAGE_BUCKET).unwrap().is_empty());
        assert_eq!(store.usage(app, "b", 0, USAGE_BUCKET).unwrap().len(), 1);
    }
}
