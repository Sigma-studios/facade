//! Reports of offsite copies to Réplique (`[backups.replique]`), the service
//! watching every backup of its user's devices: this server is one more device
//! there, its offsite copy one backup job, each copy a run (and each check, and
//! each clean-up deleting old backups). The messages are Réplique's protocol v1
//! (its `docs/protocol.md`), written again here rather than taken from its
//! crate, which is not published; the tests read Réplique's own examples
//! (`testdata/replique`) to keep the two in step.
//!
//! Reports never hold a copy up nor make it fail: they are sent from tasks of
//! their own. They are spooled rather than dropped when Réplique cannot be
//! reached, because Réplique is meant to run as an app on this very server: it
//! is down while it is redeployed or restarts, and a copy reported into that
//! gap would leave a hole in its history and fire its alert for a silent job.
//! A run's start carries a UUID chosen here, so replaying it is harmless. The
//! spool lives in the database, holds at most [`SPOOL_MAX`] runs (the oldest
//! go, with a log line), and is sent again with the next report. Progress is
//! never spooled: late, it is useless. A report Réplique refuses (4xx: a revoked
//! token, a bug) is dropped with a log line instead of retried forever.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tracing::{debug, info, warn};

use super::{CopyReport, VerifyReport};
use crate::config::RepliqueConfig;
use crate::server::Server;
use crate::server::store::Store;

const KV_SPOOL: &str = "replique.spool";
/// Runs kept while Réplique is unreachable: a month of daily copies and checks.
pub const SPOOL_MAX: usize = 100;
/// How often a running copy's progress is sent (the protocol's minimum).
const PROGRESS_EVERY: Duration = Duration::from_secs(10);
/// Errors a finish carries at most (the protocol's `MAX_REPORTED_ERRORS`).
const MAX_ERRORS: usize = 50;

// ---------------------------------------------------------------- messages

/// `PUT /api/v1/jobs/{name}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    /// `backup` (or `mirror`, not ours).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_interval_secs: Option<u64>,
    pub rules: JobRules,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct JobRules {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub exclude_caches: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated_excludes_for: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_subset: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "kebab-case")]
pub enum Retention {
    NeverPrune,
    KeepDeleted {
        keep: KeepRules,
    },
    /// What an offsite copy does: the newest backups are kept, older ones go.
    Versions {
        keep: KeepRules,
    },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct KeepRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hourly: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yearly: Option<KeepCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<String>,
}

/// A count, or `"unlimited"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepCount {
    Count(u32),
    Unlimited,
}

impl Serialize for KeepCount {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            KeepCount::Count(n) => s.serialize_u32(*n),
            KeepCount::Unlimited => s.serialize_str("unlimited"),
        }
    }
}

impl<'de> Deserialize<'de> for KeepCount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Count(u32),
            Word(String),
        }
        match Raw::deserialize(d)? {
            Raw::Count(n) => Ok(KeepCount::Count(n)),
            Raw::Word(w) if w == "unlimited" => Ok(KeepCount::Unlimited),
            Raw::Word(w) => Err(serde::de::Error::custom(format!(
                "expected a count or \"unlimited\", got {w:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Backup,
    Check,
    Prune,
    Mirror,
}

/// `POST /api/v1/runs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartRun {
    pub client_run_id: String,
    pub job: String,
    pub kind: RunKind,
    /// RFC 3339, UTC.
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restic_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartRunReply {
    pub run_id: String,
}

/// `POST /api/v1/runs/{id}/progress`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Progress {
    pub at: String,
    #[serde(default)]
    pub percent_done: f64,
    #[serde(default)]
    pub seconds_elapsed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_remaining: Option<u64>,
    #[serde(default)]
    pub total_files: u64,
    #[serde(default)]
    pub files_done: u64,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub error_count: u64,
    #[serde(default)]
    pub current_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Success,
    Partial,
    RepositoryMissing,
    LockFailed,
    WrongPassword,
    Interrupted,
    Failed,
    Lost,
}

/// `POST /api/v1/runs/{id}/finish`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinishRun {
    pub status: RunStatus,
    pub exit_code: i32,
    pub finished_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<BackupSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune: Option<PruneSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckSummary>,
    #[serde(default)]
    pub errors: Vec<RunError>,
    #[serde(default)]
    pub error_count: u64,
    #[serde(default)]
    pub unreadable_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub during: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
}

/// restic's backup summary. An offsite copy fills it with chunks for files.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BackupSummary {
    #[serde(default)]
    pub files_new: u64,
    #[serde(default)]
    pub files_changed: u64,
    #[serde(default)]
    pub files_unmodified: u64,
    #[serde(default)]
    pub dirs_new: u64,
    #[serde(default)]
    pub dirs_changed: u64,
    #[serde(default)]
    pub dirs_unmodified: u64,
    #[serde(default)]
    pub data_blobs: u64,
    #[serde(default)]
    pub tree_blobs: u64,
    #[serde(default)]
    pub data_added: u64,
    #[serde(default)]
    pub data_added_packed: u64,
    #[serde(default)]
    pub total_files_processed: u64,
    #[serde(default)]
    pub total_bytes_processed: u64,
    #[serde(default)]
    pub total_duration: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_end: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PruneSummary {
    pub policy: String,
    #[serde(default)]
    pub snapshots_before: u64,
    #[serde(default)]
    pub snapshots_removed: u64,
    #[serde(default)]
    pub snapshots_tagged_keep: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_freed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_data_subset: Option<String>,
    #[serde(default)]
    pub num_errors: u64,
    #[serde(default)]
    pub snapshot_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
}

/// The body of Réplique's non-2xx replies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    pub message: String,
}

/// Réplique's rule for job names: they go into its URLs as they are.
fn valid_job_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
}

pub fn check_config(cfg: &RepliqueConfig) -> anyhow::Result<()> {
    if !cfg.url.starts_with("https://") && !cfg.url.starts_with("http://") {
        anyhow::bail!("url must start with https://");
    }
    if !valid_job_name(&cfg.job) {
        anyhow::bail!("job must be 1 to 64 of a-z, 0-9, - and _, starting with a letter or digit");
    }
    Ok(())
}

// ---------------------------------------------------------------- delivery

/// A run waiting to reach Réplique: its start, the id Réplique gave it once
/// the start got there, and its finish once it ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Spooled {
    start: StartRun,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    finish: Option<FinishRun>,
}

#[derive(Default)]
pub struct Reporter {
    /// `None` until read from the database.
    spool: Mutex<Option<Vec<Spooled>>>,
    /// One delivery at a time, so reports go in order.
    sending: tokio::sync::Mutex<()>,
    /// The job as Réplique last accepted it.
    declared: Mutex<Option<JobSpec>>,
}

enum Sent {
    Ok(Vec<u8>),
    /// Network errors and 5xx: try again later.
    Unreachable(String),
    /// 4xx: a bug or a revoked token, not worth retrying.
    Refused(u16, String),
}

impl Reporter {
    fn with_spool<T>(&self, store: &Store, f: impl FnOnce(&mut Vec<Spooled>) -> T) -> T {
        let mut spool = self.spool.lock();
        let list = spool.get_or_insert_with(|| {
            store
                .kv_get(KV_SPOOL)
                .ok()
                .flatten()
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default()
        });
        let out = f(list);
        if let Err(e) = serde_json::to_string(list)
            .map_err(anyhow::Error::from)
            .and_then(|json| store.kv_put(KV_SPOOL, &json))
        {
            warn!("Réplique: saving the reports not sent yet: {e:#}");
        }
        out
    }

    fn push(&self, store: &Store, start: StartRun) {
        self.with_spool(store, |spool| {
            spool.push(Spooled {
                start,
                run_id: None,
                finish: None,
            });
            while spool.len() > SPOOL_MAX {
                let dropped = spool.remove(0);
                warn!(
                    "Réplique unreachable for {SPOOL_MAX} runs: the report of the run started at {} is dropped",
                    dropped.start.started_at
                );
            }
        });
    }

    fn finish(&self, store: &Store, client_run_id: &str, finish: FinishRun) {
        self.with_spool(store, |spool| {
            if let Some(s) = spool
                .iter_mut()
                .find(|s| s.start.client_run_id == client_run_id)
            {
                s.finish = Some(finish);
            }
        });
    }

    /// The id Réplique gave the run, once its start got there.
    fn run_id(&self, store: &Store, client_run_id: &str) -> Option<String> {
        self.with_spool(store, |spool| {
            spool
                .iter()
                .find(|s| s.start.client_run_id == client_run_id)
                .and_then(|s| s.run_id.clone())
        })
    }

    /// Sends the job's declaration if it changed, then every run spooled, in
    /// order, until Réplique cannot be reached.
    pub async fn deliver(&self, cfg: &RepliqueConfig, store: &Store, spec: &JobSpec) {
        let _sending = self.sending.lock().await;
        let client = match Client::new(cfg) {
            Ok(client) => client,
            Err(e) => {
                warn!("Réplique: {e:#}");
                return;
            }
        };
        if self.declared.lock().as_ref() != Some(spec) {
            match client
                .send(
                    reqwest::Method::PUT,
                    &format!("/api/v1/jobs/{}", cfg.job),
                    spec,
                )
                .await
            {
                Sent::Ok(body) => {
                    if !serde_json::from_slice::<Ack>(&body).is_ok_and(|a| a.ok) {
                        warn!(
                            "Réplique: unexpected reply to the declaration of job {}",
                            cfg.job
                        );
                    }
                    *self.declared.lock() = Some(spec.clone());
                }
                Sent::Unreachable(e) => {
                    debug!("Réplique unreachable, reports kept for later: {e}");
                    return;
                }
                Sent::Refused(status, e) => {
                    warn!(
                        "Réplique refused the declaration of job {}: {status} {e}",
                        cfg.job
                    )
                }
            }
        }
        let pending = self.with_spool(store, |spool| spool.clone());
        for run in pending {
            let id = run.start.client_run_id.clone();
            let run_id = match run.run_id {
                Some(run_id) => run_id,
                None => match client
                    .send(reqwest::Method::POST, "/api/v1/runs", &run.start)
                    .await
                {
                    Sent::Ok(body) => match serde_json::from_slice::<StartRunReply>(&body) {
                        Ok(reply) => {
                            self.with_spool(store, |spool| {
                                if let Some(s) =
                                    spool.iter_mut().find(|s| s.start.client_run_id == id)
                                {
                                    s.run_id = Some(reply.run_id.clone());
                                }
                            });
                            reply.run_id
                        }
                        Err(e) => {
                            warn!(
                                "Réplique: unexpected reply to a run's start ({e}), report dropped"
                            );
                            self.drop_run(store, &id);
                            continue;
                        }
                    },
                    Sent::Unreachable(e) => {
                        debug!("Réplique unreachable, reports kept for later: {e}");
                        return;
                    }
                    Sent::Refused(status, e) => {
                        warn!("Réplique refused a run's start ({status} {e}), report dropped");
                        self.drop_run(store, &id);
                        continue;
                    }
                },
            };
            // The run is still going: its finish comes later.
            let Some(finish) = run.finish else {
                continue;
            };
            let path = format!("/api/v1/runs/{run_id}/finish");
            match client.send(reqwest::Method::POST, &path, &finish).await {
                Sent::Ok(_) => self.drop_run(store, &id),
                Sent::Unreachable(e) => {
                    debug!("Réplique unreachable, reports kept for later: {e}");
                    return;
                }
                Sent::Refused(status, e) => {
                    warn!("Réplique refused a run's finish ({status} {e}), report dropped");
                    self.drop_run(store, &id);
                }
            }
        }
    }

    fn drop_run(&self, store: &Store, client_run_id: &str) {
        self.with_spool(store, |spool| {
            spool.retain(|s| s.start.client_run_id != client_run_id)
        });
    }

    /// Runs not delivered yet, for tests.
    #[cfg(test)]
    fn pending(&self, store: &Store) -> usize {
        self.with_spool(store, |spool| spool.len())
    }
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    fn new(cfg: &RepliqueConfig) -> anyhow::Result<Self> {
        use anyhow::Context;
        // Read at each delivery, so that a new token works without a restart.
        let token = std::fs::read_to_string(&cfg.token_file)
            .with_context(|| format!("reading {}", cfg.token_file.display()))?
            .trim()
            .to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(format!("facade/{}", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base: cfg.url.trim_end_matches('/').to_string(),
            token,
        })
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: &impl Serialize) -> Sent {
        let request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(body);
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => return Sent::Unreachable(e.without_url().to_string()),
        };
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default();
        if status.is_success() {
            return Sent::Ok(bytes);
        }
        let message = serde_json::from_slice::<ApiError>(&bytes)
            .map(|e| format!("{}: {}", e.error, e.message))
            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).chars().take(200).collect());
        if status.is_client_error() {
            Sent::Refused(status.as_u16(), message)
        } else {
            Sent::Unreachable(format!("{status} {message}"))
        }
    }
}

// ---------------------------------------------------------------- what the copy reports

/// The job an offsite copy is on Réplique.
pub fn job_spec(server: &Server) -> JobSpec {
    let backups = &server.cfg.backups;
    let every = backups.every_hours.max(1);
    let keep = super::settings(server).map_or(backups.keep, |s| s.keep);
    JobSpec {
        kind: "backup".into(),
        schedule: Some(format!("every {every}h")),
        expected_interval_secs: Some(every * 3600),
        rules: JobRules {
            paths: vec![crate::server::backups::root(server).display().to_string()],
            retention: Some(Retention::Versions {
                keep: KeepRules {
                    last: Some(KeepCount::Count(keep as u32)),
                    ..Default::default()
                },
            }),
            ..Default::default()
        },
    }
}

fn deliver_soon(server: &Arc<Server>) {
    let server = server.clone();
    tokio::spawn(async move {
        let Some(cfg) = &server.cfg.backups.replique else {
            return;
        };
        let spec = job_spec(&server);
        server
            .offsite
            .replique
            .deliver(cfg, &server.store, &spec)
            .await;
    });
}

fn client_version() -> Option<String> {
    Some(format!("facade {}", crate::selfbin::version_line()))
}

/// One copy, check or clean-up, as Réplique sees it.
pub struct Run {
    client_run_id: String,
    started: std::time::Instant,
}

impl Run {
    /// Reports that a run starts; `None` without `[backups.replique]`.
    pub fn start(server: &Arc<Server>, kind: RunKind, at: u64) -> Option<Self> {
        let cfg = server.cfg.backups.replique.as_ref()?;
        let run = Self {
            client_run_id: uuid::Uuid::new_v4().to_string(),
            started: std::time::Instant::now(),
        };
        server.offsite.replique.push(
            &server.store,
            StartRun {
                client_run_id: run.client_run_id.clone(),
                job: cfg.job.clone(),
                kind,
                started_at: facade_core::rfc3339(at),
                restic_version: None,
                client_version: client_version(),
            },
        );
        deliver_soon(server);
        Some(run)
    }

    /// Sends the copy's progress every 10 seconds while it runs.
    pub fn follow(&self, server: Arc<Server>) -> tokio::task::JoinHandle<()> {
        let id = self.client_run_id.clone();
        let started = self.started;
        tokio::spawn(async move {
            let Some(cfg) = server.cfg.backups.replique.clone() else {
                return;
            };
            let Ok(client) = Client::new(&cfg) else {
                return;
            };
            loop {
                tokio::time::sleep(PROGRESS_EVERY).await;
                let Some(run_id) = server.offsite.replique.run_id(&server.store, &id) else {
                    continue;
                };
                let p = &server.offsite.progress;
                let done = p.bytes_done.load(Ordering::Relaxed);
                let total = p.bytes_total.load(Ordering::Relaxed);
                let elapsed = started.elapsed().as_secs();
                let remaining = (done > 0 && total > done).then(|| elapsed * (total - done) / done);
                let progress = Progress {
                    at: facade_core::rfc3339(facade_core::now()),
                    percent_done: if total > 0 {
                        done as f64 / total as f64
                    } else {
                        0.0
                    },
                    seconds_elapsed: elapsed,
                    seconds_remaining: remaining,
                    total_files: p.chunks_total.load(Ordering::Relaxed),
                    files_done: p.chunks_done.load(Ordering::Relaxed),
                    total_bytes: total,
                    bytes_done: done,
                    phase: Some(p.phase.lock().to_string()),
                    ..Default::default()
                };
                let path = format!("/api/v1/runs/{run_id}/progress");
                if let Sent::Unreachable(e) | Sent::Refused(_, e) =
                    client.send(reqwest::Method::POST, &path, &progress).await
                {
                    debug!("Réplique: progress not sent: {e}");
                }
            }
        })
    }

    fn end(self, server: &Arc<Server>, finish: FinishRun) {
        server
            .offsite
            .replique
            .finish(&server.store, &self.client_run_id, finish);
        deliver_soon(server);
    }

    pub fn finish_copy(
        self,
        server: &Arc<Server>,
        result: &anyhow::Result<CopyReport>,
        at: u64,
        secs: u64,
        keep: usize,
    ) {
        let end = facade_core::rfc3339(at + secs);
        let finish = match result {
            Ok(r) => FinishRun {
                summary: Some(BackupSummary {
                    // A chunk is what a file is to restic: the unit sent.
                    files_new: r.uploaded_chunks,
                    files_unmodified: r.chunks.saturating_sub(r.uploaded_chunks),
                    data_added: r.uploaded_bytes,
                    data_added_packed: r.uploaded_bytes,
                    total_files_processed: r.chunks,
                    total_bytes_processed: r.size,
                    total_duration: secs as f64,
                    backup_start: Some(facade_core::rfc3339(at)),
                    backup_end: Some(end.clone()),
                    ..Default::default()
                }),
                ..finished(end.clone(), &r.unreadable)
            },
            Err(e) => failed(end.clone(), format!("{e:#}")),
        };
        self.end(server, finish);
        // Old backups deleted from the target: a clean-up run of the same job.
        if let Ok(r) = result
            && !r.removed.is_empty()
            && let Some(prune) = Run::start(server, RunKind::Prune, at + secs)
        {
            prune.end(
                server,
                FinishRun {
                    prune: Some(PruneSummary {
                        policy: "versions".into(),
                        snapshots_before: r.backups + r.removed.len() as u64,
                        snapshots_removed: r.removed.len() as u64,
                        snapshots_tagged_keep: 0,
                        bytes_freed: Some(r.bytes_deleted),
                    }),
                    ..finished(end, &[])
                },
            );
            info!(
                "Réplique: {} old offsite backup(s) reported deleted (keep {keep})",
                r.removed.len()
            );
        }
    }

    pub fn finish_check(
        self,
        server: &Arc<Server>,
        result: &anyhow::Result<VerifyReport>,
        at: u64,
        sample: Option<usize>,
    ) {
        let end = facade_core::rfc3339(at + self.started.elapsed().as_secs());
        let finish = match result {
            Ok(r) => FinishRun {
                check: Some(CheckSummary {
                    read_data_subset: Some(match sample {
                        Some(n) => format!("{n} chunks"),
                        None => "100%".into(),
                    }),
                    num_errors: r.problems.len() as u64,
                    snapshot_ids: r.manifests.clone(),
                }),
                ..finished(end, &r.problems)
            },
            Err(e) => failed(end, format!("{e:#}")),
        };
        self.end(server, finish);
    }
}

/// A finish: a success unless there are `problems`.
fn finished(at: String, problems: &[String]) -> FinishRun {
    FinishRun {
        status: if problems.is_empty() {
            RunStatus::Success
        } else {
            RunStatus::Failed
        },
        exit_code: if problems.is_empty() { 0 } else { 1 },
        finished_at: at,
        summary: None,
        prune: None,
        check: None,
        errors: problems
            .iter()
            .take(MAX_ERRORS)
            .map(|p| RunError {
                message: p.clone(),
                during: None,
                item: None,
            })
            .collect(),
        error_count: problems.len() as u64,
        unreadable_paths: vec![],
        message: None,
    }
}

fn failed(at: String, message: String) -> FinishRun {
    FinishRun {
        message: Some(message.clone()),
        ..finished(at, &[message])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Réplique's fixtures (its `proto/fixtures`), copied as they are.
    fn fixture(name: &str) -> serde_json::Value {
        let path = format!(
            "{}/src/server/offsite/testdata/replique/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    /// Parsed into our type, then written back, a fixture is the same JSON.
    fn same<T: Serialize + serde::de::DeserializeOwned>(name: &str) -> T {
        let json = fixture(name);
        let parsed: T =
            serde_json::from_value(json.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json, "{name}");
        parsed
    }

    #[test]
    fn reads_and_writes_the_protocol_fixtures() {
        let job: JobSpec = same("job_spec_backup.json");
        assert_eq!(job.expected_interval_secs, Some(86400));
        let manual: JobSpec = same("job_spec_manual.json");
        let Some(Retention::Versions { keep }) = manual.rules.retention else {
            panic!("versions expected");
        };
        assert_eq!(keep.monthly, Some(KeepCount::Unlimited));
        let start: StartRun = same("run_start.json");
        assert_eq!(start.kind, RunKind::Backup);
        let reply: StartRunReply = same("run_start_reply.json");
        assert!(reply.run_id.starts_with("run_"));
        same::<Progress>("run_progress.json");
        let phase: Progress = same("run_progress_phase.json");
        assert_eq!(phase.phase.as_deref(), Some("diffing snapshots"));
        let backup: FinishRun = same("run_finish_backup.json");
        assert_eq!(backup.status, RunStatus::Success);
        same::<FinishRun>("run_finish_check.json");
        same::<FinishRun>("run_finish_prune.json");
        let failed: FinishRun = same("run_finish_failed.json");
        assert_eq!(failed.status, RunStatus::Failed);
        same::<Ack>("ack.json");
        same::<ApiError>("error.json");
    }

    /// What facade sends has the shape of the fixtures: the same fields, in
    /// the same types, nothing else.
    #[test]
    fn writes_what_replique_reads() {
        fn shape(v: &serde_json::Value) -> serde_json::Value {
            use serde_json::Value;
            match v {
                Value::Object(map) => {
                    Value::Object(map.iter().map(|(k, v)| (k.clone(), shape(v))).collect())
                }
                Value::Array(items) => Value::Array(items.iter().take(1).map(shape).collect()),
                Value::Number(_) => Value::String("number".into()),
                Value::String(_) => Value::String("string".into()),
                other => other.clone(),
            }
        }
        let start = StartRun {
            client_run_id: uuid::Uuid::new_v4().to_string(),
            job: "hetzner".into(),
            kind: RunKind::Backup,
            started_at: facade_core::rfc3339(1_794_625_965),
            restic_version: Some("0.19.1".into()),
            client_version: client_version(),
        };
        assert_eq!(
            shape(&serde_json::to_value(&start).unwrap()),
            shape(&fixture("run_start.json"))
        );
        assert_eq!(start.started_at, "2026-11-14T03:12:45Z");

        let report = CopyReport {
            uploaded_chunks: 3,
            uploaded_bytes: 1000,
            chunks: 10,
            size: 5000,
            ..Default::default()
        };
        let finish = FinishRun {
            summary: Some(BackupSummary {
                files_new: report.uploaded_chunks,
                total_duration: 12.5,
                backup_start: Some(facade_core::rfc3339(0)),
                backup_end: Some(facade_core::rfc3339(12)),
                snapshot_id: Some("x".into()),
                ..Default::default()
            }),
            ..finished(facade_core::rfc3339(12), &[])
        };
        assert_eq!(
            shape(&serde_json::to_value(&finish).unwrap()),
            shape(&fixture("run_finish_backup.json"))
        );
        let failed = failed(
            facade_core::rfc3339(1_794_686_402),
            "/mnt/windows/c is not a mountpoint".into(),
        );
        let mut expected = fixture("run_finish_failed.json");
        // Ours carries the reason as an error too.
        expected["errors"] =
            serde_json::json!([{ "message": "/mnt/windows/c is not a mountpoint" }]);
        expected["error_count"] = 1.into();
        assert_eq!(serde_json::to_value(&failed).unwrap(), expected);
    }

    /// A tiny Réplique: records what it is sent, or is down.
    #[derive(Default)]
    struct Mock {
        up: std::sync::atomic::AtomicBool,
        seen: Mutex<Vec<(String, serde_json::Value)>>,
    }

    async fn serve(mock: Arc<Mock>) -> String {
        use axum::extract::{Path, State};
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::{post, put};
        async fn record(
            mock: &Mock,
            what: String,
            headers: &HeaderMap,
            body: serde_json::Value,
        ) -> Result<(), StatusCode> {
            if !mock.up.load(Ordering::SeqCst) {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer rpl_test")
            {
                return Err(StatusCode::UNAUTHORIZED);
            }
            mock.seen.lock().push((what, body));
            Ok(())
        }
        let app = axum::Router::new()
            .route(
                "/api/v1/jobs/{name}",
                put(
                    |State(m): State<Arc<Mock>>,
                     Path(name): Path<String>,
                     h: HeaderMap,
                     axum::Json(b): axum::Json<serde_json::Value>| async move {
                        record(&m, format!("job {name}"), &h, b).await?;
                        Ok::<_, StatusCode>(axum::Json(serde_json::json!({ "ok": true })))
                    },
                ),
            )
            .route(
                "/api/v1/runs",
                post(
                    |State(m): State<Arc<Mock>>,
                     h: HeaderMap,
                     axum::Json(b): axum::Json<serde_json::Value>| async move {
                        let id = format!("run_{}", b["client_run_id"].as_str().unwrap_or(""));
                        record(&m, "start".into(), &h, b).await?;
                        Ok::<_, StatusCode>(axum::Json(serde_json::json!({ "run_id": id })))
                    },
                ),
            )
            .route(
                "/api/v1/runs/{id}/finish",
                post(
                    |State(m): State<Arc<Mock>>,
                     Path(id): Path<String>,
                     h: HeaderMap,
                     axum::Json(b): axum::Json<serde_json::Value>| async move {
                        record(&m, format!("finish {id}"), &h, b).await?;
                        Ok::<_, StatusCode>(axum::Json(serde_json::json!({ "ok": true })))
                    },
                ),
            )
            .with_state(mock);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn spools_while_replique_is_down() {
        let dir = std::env::temp_dir().join(format!("facade-replique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("token"), "rpl_test\n").unwrap();
        let store = Store::open(&dir.join("facade.db")).unwrap();
        let mock = Arc::new(Mock::default());
        let cfg = RepliqueConfig {
            url: serve(mock.clone()).await,
            token_file: dir.join("token"),
            job: "offsite".into(),
        };
        let spec = JobSpec {
            kind: "backup".into(),
            schedule: Some("every 24h".into()),
            expected_interval_secs: Some(86400),
            rules: JobRules::default(),
        };
        let start = |id: &str| StartRun {
            client_run_id: id.into(),
            job: "offsite".into(),
            kind: RunKind::Backup,
            started_at: facade_core::rfc3339(1000),
            restic_version: None,
            client_version: client_version(),
        };

        // Down: a run starts and finishes, nothing gets there, it is kept.
        let reporter = Reporter::default();
        reporter.push(&store, start("a"));
        reporter.deliver(&cfg, &store, &spec).await;
        reporter.finish(&store, "a", finished(facade_core::rfc3339(1010), &[]));
        reporter.deliver(&cfg, &store, &spec).await;
        assert!(mock.seen.lock().is_empty());
        assert_eq!(reporter.pending(&store), 1);

        // A restart in between: the spool is in the database.
        let reporter = Reporter::default();
        reporter.push(&store, start("b"));
        mock.up.store(true, Ordering::SeqCst);
        reporter.deliver(&cfg, &store, &spec).await;
        let seen: Vec<String> = mock.seen.lock().iter().map(|(w, _)| w.clone()).collect();
        // The job first, then the runs in order; b is still going.
        assert_eq!(seen, ["job offsite", "start", "finish run_a", "start"]);
        assert_eq!(mock.seen.lock()[1].1["client_run_id"], "a");
        assert_eq!(reporter.pending(&store), 1);
        assert_eq!(reporter.run_id(&store, "b").as_deref(), Some("run_b"));

        // b's finish goes alone: its start got there, the job did not change.
        reporter.finish(&store, "b", finished(facade_core::rfc3339(1020), &[]));
        reporter.deliver(&cfg, &store, &spec).await;
        assert_eq!(mock.seen.lock().last().unwrap().0, "finish run_b");
        assert_eq!(mock.seen.lock().len(), 5);
        assert_eq!(reporter.pending(&store), 0);

        // A refused token drops the report instead of keeping it forever.
        std::fs::write(dir.join("token"), "rpl_revoked").unwrap();
        reporter.push(&store, start("c"));
        reporter.deliver(&cfg, &store, &spec).await;
        assert_eq!(reporter.pending(&store), 0);

        // At most SPOOL_MAX runs wait.
        mock.up.store(false, Ordering::SeqCst);
        for i in 0..SPOOL_MAX + 5 {
            reporter.push(&store, start(&format!("d{i}")));
        }
        assert_eq!(reporter.pending(&store), SPOOL_MAX);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
