//! Auto-deploy: apps and stacks that deploy by themselves when their source
//! changes, a new commit on their branch (git apps, stacks) or a new image behind
//! their tag (image apps). Changes are noticed by polling the source (a remote
//! head lookup, a registry manifest digest), or right away when GitHub calls the
//! webhook facade put on the repository with an access token. Either way, what
//! follows is a normal deployment: build queue, health checks, rollback, alerts.
//!
//! Changes are edge-triggered: a target deploys when its source moves to
//! something not seen before, not because it differs from what runs. A failed
//! deploy of commit X is not retried for X, a rollback stays until the next push,
//! and a webhook and a poll noticing the same push deploy it once.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use facade_core::Source;
use facade_core::api::{
    AutoDeployMethod, AutoDeployTarget, AutoDeployView, DeployRequest, GitCredentialKind,
    ImageUpdateCheck, SetAutoDeploy, StackDeployRequest, WebhookInfo,
};
use facade_core::git::RepoLocation;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tracing::{info, warn};

use super::api::{ApiError, ApiResult, User};
use super::github::{self, Event};
use super::registry::{ImageRef, Registry};
use super::store::{ADMINS, AutoDeployRow, GithubDeliveryRow, GithubHookRow};
use super::users::{can_see_app, load_app_for};
use super::{Server, hash_secret, random_token};

/// How often the loop looks for targets due a check.
const TICK: Duration = Duration::from_secs(10);
/// How often webhooks are checked on GitHub when nothing changed.
const HOOKS_EVERY: Duration = Duration::from_secs(6 * 3600);
/// A working webhook makes polling a safety net only.
const WEBHOOK_SLOWDOWN: u64 = 10;
const KV_SECRET: &str = "github_webhook_secret";
const PUSH: &str = "push";
/// GitHub sends both for a container published to ghcr.io, to the webhooks of
/// the repository it is linked to.
const PACKAGE_EVENTS: [&str; 2] = ["package", "registry_package"];
/// A webhook facade did not make (one added by hand on a repository or an
/// organisation) counts as long as it delivered within this time. Nothing else
/// says it still exists.
const OUTSIDE_HOOK_FRESH: u64 = 30 * 24 * 3600;

/// State shared by the loop, the webhook endpoint and the API.
#[derive(Default)]
pub struct AutoDeploy {
    /// Targets a webhook asked to check now.
    pending: Mutex<HashSet<String>>,
    /// Targets whose change waits for a deployment in progress, and what
    /// noticed it.
    deferred: Mutex<HashMap<String, Via>>,
    wake: Notify,
    hooks_wake: Notify,
    secret: Mutex<Option<String>>,
    registry: Registry,
}

impl AutoDeploy {
    pub fn check_now(&self, target: &str) {
        self.pending.lock().insert(target.to_string());
        self.wake.notify_one();
    }

    /// Webhooks may have to be created or removed (targets, credentials or the
    /// server's domain changed).
    pub fn hooks_changed(&self) {
        self.hooks_wake.notify_one();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    Poll,
    Webhook,
}

impl Via {
    fn as_str(self) -> &'static str {
        match self {
            Via::Poll => "poll",
            Via::Webhook => "webhook",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    App(String),
    Stack(String),
}

impl Target {
    pub fn key(&self) -> String {
        match self {
            Target::App(name) => format!("app:{name}"),
            Target::Stack(name) => format!("stack:{name}"),
        }
    }

    fn parse(key: &str) -> Option<Self> {
        match key.split_once(':')? {
            ("app", name) => Some(Target::App(name.into())),
            ("stack", name) => Some(Target::Stack(name.into())),
            _ => None,
        }
    }
}

/// What a target watches.
enum Watched {
    Git {
        repo: String,
        branch: Option<String>,
        owner: i64,
    },
    Image {
        image: String,
        owner: i64,
    },
    /// Nothing that changes by itself, and why.
    Nothing(&'static str),
}

impl Watched {
    fn describe(&self) -> Option<String> {
        match self {
            Watched::Git { repo, branch, .. } => {
                let repo = facade_core::git::parse(repo).map_or_else(|| repo.clone(), |l| l.key());
                Some(format!(
                    "{repo} ({})",
                    branch.as_deref().unwrap_or("default branch")
                ))
            }
            Watched::Image { image, .. } => Some(ImageRef::parse(image).display()),
            Watched::Nothing(_) => None,
        }
    }
}

const EXECUTABLE: &str = "an uploaded executable changes only when you upload another";
const NO_REPO: &str = "the stack has no repository";

fn watched_app(spec: &facade_core::AppSpec, owner: Option<i64>) -> Watched {
    let owner = owner.unwrap_or(ADMINS);
    match &spec.source {
        Source::Git { repo, branch, .. } => Watched::Git {
            repo: repo.clone(),
            branch: branch.clone(),
            owner,
        },
        Source::Image { image } => Watched::Image {
            image: image.clone(),
            owner,
        },
        Source::Executable { .. } => Watched::Nothing(EXECUTABLE),
    }
}

fn watched_stack(spec: &facade_core::stack::StackSpec) -> Watched {
    match &spec.repo {
        Some(repo) => Watched::Git {
            repo: repo.clone(),
            branch: spec.branch.clone(),
            // Stacks belong to the admins: their credentials fetch it.
            owner: ADMINS,
        },
        None => Watched::Nothing(NO_REPO),
    }
}

/// What `target` watches; `None` once it is gone.
fn watched(server: &Server, target: &Target) -> anyhow::Result<Option<Watched>> {
    Ok(match target {
        Target::App(name) => server
            .store
            .get_app(name)?
            .map(|a| watched_app(&a.spec, a.owner)),
        Target::Stack(name) => server
            .store
            .get_stack(name)?
            .map(|s| watched_stack(&s.spec)),
    })
}

/// A repository on github.com, the only host webhooks are set up on.
fn on_github(repo: &str) -> Option<RepoLocation> {
    facade_core::git::parse(repo)
        .filter(|l| l.host == "github.com" && l.path.split('/').count() == 2)
}

/// The repository whose webhook would report the target's changes, and the
/// event it needs. GitHub only tells about images published to ghcr.io, to the
/// repository they are linked to (their source label, usually).
fn hook_for(
    watched: &Watched,
    row: Option<&AutoDeployRow>,
) -> Option<(RepoLocation, &'static str)> {
    match watched {
        Watched::Git { repo, .. } => on_github(repo).map(|l| (l, PUSH)),
        Watched::Image { image, .. } if ImageRef::parse(image).registry != "ghcr.io" => None,
        Watched::Image { .. } => row
            .and_then(|r| r.image_repo.as_deref())
            .filter(|r| !r.is_empty())
            .and_then(on_github)
            .map(|l| (l, PACKAGE_EVENTS[0])),
        Watched::Nothing(_) => None,
    }
}

/// Where GitHub delivers: the server's domain, which has a real certificate.
fn webhook_url(server: &Server) -> Option<String> {
    server
        .settings
        .api_domain()
        .map(|d| format!("https://{d}/api/hooks/github"))
}

/// The secret webhooks sign deliveries with, created on first use and stored
/// encrypted like the other secrets.
fn webhook_secret(server: &Server) -> anyhow::Result<String> {
    let mut cached = server.auto_deploy.secret.lock();
    if let Some(secret) = cached.as_ref() {
        return Ok(secret.clone());
    }
    let secret = match server.store.kv_get(KV_SECRET)? {
        Some(sealed) => server
            .secrets
            .open_string(&hex::decode(sealed).context("stored webhook secret")?)?,
        None => {
            let secret = random_token();
            server.store.kv_put(
                KV_SECRET,
                &hex::encode(server.secrets.seal(secret.as_bytes())),
            )?;
            secret
        }
    };
    *cached = Some(secret.clone());
    Ok(secret)
}

/// Whether auto-deploy can be turned on for this app.
pub fn supported(spec: &facade_core::AppSpec) -> Result<(), String> {
    if let Some(stack) = &spec.stack {
        return Err(format!(
            "{} is deployed with stack {}: facade stack update {} --auto-deploy on",
            spec.name, stack.stack, stack.stack
        ));
    }
    if matches!(spec.source, Source::Executable { .. }) {
        return Err(EXECUTABLE.into());
    }
    Ok(())
}

/// Turns auto-deploy on or off. Returns whether that changed anything.
pub fn set(server: &Server, target: &Target, enabled: bool) -> anyhow::Result<bool> {
    let key = target.key();
    let was = server.store.get_auto_deploy(&key)?.is_some();
    if enabled {
        server.store.enable_auto_deploy(&key)?;
        // Right away: if the source moved since the last deploy, that deploys.
        server.auto_deploy.check_now(&key);
    } else {
        server.store.disable_auto_deploy(&key)?;
        server.auto_deploy.pending.lock().remove(&key);
        server.auto_deploy.deferred.lock().remove(&key);
    }
    if was != enabled {
        info!(target = %key, enabled, "auto-deploy");
        server.auto_deploy.hooks_changed();
    }
    Ok(was != enabled)
}

/// An app or stack was deleted: its webhook goes if nothing else needs it.
pub fn forget(server: &Server, target: &Target) {
    if let Err(e) = set(server, target, false) {
        warn!("auto-deploy: {e:#}");
    }
}

pub fn is_enabled(server: &Server, target: &Target) -> bool {
    server
        .store
        .get_auto_deploy(&target.key())
        .ok()
        .flatten()
        .is_some()
}

// ---------------------------------------------------------------- checking

/// What to do about the source's current commit or digest (`head`).
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Seen before: acted on already, or deliberately not.
    Nothing,
    /// New, but nothing to deploy: it runs already, or there is nothing to
    /// compare it with (the baseline of an image whose deployed digest is unknown).
    Seen,
    Deploy,
}

fn decide(seen: Option<&str>, head: &str, deployed: Option<&str>) -> Decision {
    if seen == Some(head) {
        Decision::Nothing
    } else if deployed == Some(head) || (seen.is_none() && deployed.is_none()) {
        Decision::Seen
    } else {
        Decision::Deploy
    }
}

#[derive(Debug)]
enum Step {
    /// Checked, and the row says what was found.
    Done,
    /// A change waits for the deployment in progress.
    Deferred,
    /// The target is gone.
    Gone,
}

pub async fn run(server: Arc<Server>) {
    tokio::spawn(hooks_loop(server.clone()));
    loop {
        tokio::select! {
            _ = tokio::time::sleep(TICK) => {}
            _ = server.auto_deploy.wake.notified() => {}
        }
        if let Err(e) = tick(&server).await {
            warn!("auto-deploy: {e:#}");
        }
    }
}

async fn tick(server: &Arc<Server>) -> anyhow::Result<()> {
    let rows = server.store.list_auto_deploy()?;
    let pending = std::mem::take(&mut *server.auto_deploy.pending.lock());
    let hooks = Hooks::load(server)?;
    let url = webhook_url(server);
    let now = facade_core::now();
    for row in rows {
        let Some(target) = Target::parse(&row.target) else {
            continue;
        };
        let deferred = server.auto_deploy.deferred.lock().get(&row.target).copied();
        let via = if pending.contains(&row.target) {
            Via::Webhook
        } else if let Some(via) = deferred.filter(|_| !busy(server, &target)) {
            via
        } else {
            let Some(watched) = watched(server, &target)? else {
                server.store.disable_auto_deploy(&row.target)?;
                continue;
            };
            let every = poll_secs(server, &watched, &row, &hooks, url.as_deref());
            if row.checked_at.is_some_and(|at| now < at + every) {
                continue;
            }
            Via::Poll
        };
        server.auto_deploy.deferred.lock().remove(&row.target);
        check(server, &target, row, via).await;
    }
    Ok(())
}

/// Seconds between checks: longer while a webhook reports changes.
fn poll_secs(
    server: &Server,
    watched: &Watched,
    row: &AutoDeployRow,
    hooks: &Hooks,
    url: Option<&str>,
) -> u64 {
    let poll = server.cfg.auto_deploy.poll_secs.max(10);
    if hook_working(watched, Some(row), hooks, url).is_some() {
        poll * WEBHOOK_SLOWDOWN
    } else {
        poll
    }
}

/// The webhooks GitHub may deliver from: the ones facade made, and the ones it
/// only knows from their deliveries.
struct Hooks {
    own: Vec<GithubHookRow>,
    outside: Vec<GithubDeliveryRow>,
}

impl Hooks {
    fn load(server: &Server) -> anyhow::Result<Self> {
        Ok(Hooks {
            own: server.store.list_github_hooks()?,
            outside: server.store.list_github_deliveries()?,
        })
    }
}

/// A webhook reporting a target's changes: where it sits, and when it last
/// delivered.
struct Coverage {
    scope: String,
    delivered_at: Option<u64>,
}

/// The webhook reporting the target's changes, if one does: one facade made on
/// its repository, or one added by hand there or on its organisation.
fn hook_working(
    watched: &Watched,
    row: Option<&AutoDeployRow>,
    hooks: &Hooks,
    url: Option<&str>,
) -> Option<Coverage> {
    let (location, event) = hook_for(watched, row)?;
    let key = location.key();
    if let Some(h) = hooks.own.iter().find(|h| {
        h.repo == key
            && h.hook_id.is_some()
            && h.error.is_none()
            && Some(h.url.as_str()) == url
            && h.events.iter().any(|e| e == event)
    }) {
        return Some(Coverage {
            scope: h.repo.clone(),
            delivered_at: h.delivered_at,
        });
    }
    outside_hook(&hooks.outside, &location, event, facade_core::now())
}

/// A webhook facade did not make that covers the repository: on it or on its
/// owner (an organisation), sending `event` (or both package events), and
/// heard from lately.
fn outside_hook(
    outside: &[GithubDeliveryRow],
    location: &RepoLocation,
    event: &str,
    now: u64,
) -> Option<Coverage> {
    let key = location.key();
    let owner = key.rsplit_once('/').map(|(owner, _)| owner)?;
    let sends = |events: &[String]| {
        events.iter().any(|e| {
            e == "*"
                || e == event
                || (PACKAGE_EVENTS.contains(&event) && PACKAGE_EVENTS.contains(&e.as_str()))
        })
    };
    outside
        .iter()
        .filter(|d| d.scope == key || d.scope == owner)
        .filter(|d| now.saturating_sub(d.delivered_at) <= OUTSIDE_HOOK_FRESH)
        .filter(|d| sends(&d.events))
        // The repository's own webhook says more than its organisation's.
        .max_by_key(|d| (d.scope == key, d.delivered_at))
        .map(|d| Coverage {
            scope: if d.scope == key {
                d.scope.clone()
            } else {
                format!("{} (organisation)", d.scope)
            },
            delivered_at: Some(d.delivered_at),
        })
}

/// Whether a deployment of the target is queued or running.
fn busy(server: &Server, target: &Target) -> bool {
    match target {
        Target::App(name) => server
            .store
            .list_deployments(name, 1)
            .ok()
            .and_then(|d| d.into_iter().next())
            .is_some_and(|d| !d.status.is_finished()),
        Target::Stack(name) => server
            .store
            .list_stack_deployments(name, 5)
            .ok()
            .is_some_and(|d| d.iter().any(|d| !d.status.is_finished())),
    }
}

async fn check(server: &Arc<Server>, target: &Target, mut row: AutoDeployRow, via: Via) {
    row.checked_at = Some(facade_core::now());
    let result = match target {
        Target::App(name) => check_app(server, name, &mut row, via).await,
        Target::Stack(name) => check_stack(server, name, &mut row, via).await,
    };
    match result {
        Ok(Step::Done) => row.error = None,
        Ok(Step::Deferred) => {
            server
                .auto_deploy
                .deferred
                .lock()
                .insert(row.target.clone(), via);
            row.error = None;
        }
        Ok(Step::Gone) => {
            forget(server, target);
            return;
        }
        Err(e) => {
            let e = format!("{e:#}");
            if row.error.as_ref() != Some(&e) {
                warn!(target = %row.target, "auto-deploy: {e}");
            }
            row.error = Some(e);
        }
    }
    if let Err(e) = server.store.put_auto_deploy(&row) {
        warn!(target = %row.target, "auto-deploy: {e:#}");
    }
}

/// The registry login of `owner` for the image's registry.
fn registry_login(
    server: &Server,
    owner: i64,
    image: &ImageRef,
) -> anyhow::Result<Option<facade_core::proto::RegistryLogin>> {
    Ok(server
        .catalog()
        .registry_logins(owner)?
        .into_iter()
        .find(|l| l.host.eq_ignore_ascii_case(&image.registry)))
}

/// The digest of the image the app's last successful deploy pulled.
fn deployed_digest(server: &Server, app: &str) -> Option<String> {
    let log = server.store.last_success_log(app).ok().flatten()?;
    super::registry::pulled_digest(&log)
}

/// For the app page's update check: what the image's tag points to now, against
/// what the app's last deploy pulled. The poll's own lookup, auto-deploy or not.
pub async fn image_check(
    server: &Server,
    row: &super::store::AppRow,
) -> anyhow::Result<ImageUpdateCheck> {
    let Source::Image { image } = &row.spec.source else {
        bail!("{} does not run a registry image", row.spec.name);
    };
    let reference = ImageRef::parse(image);
    let login = registry_login(server, row.owner.unwrap_or(ADMINS), &reference)?;
    let latest = server
        .auto_deploy
        .registry
        .digest(image, login.as_ref())
        .await?;
    Ok(ImageUpdateCheck {
        image: reference.display(),
        latest,
        deployed: deployed_digest(server, &row.spec.name),
    })
}

async fn check_app(
    server: &Arc<Server>,
    name: &str,
    row: &mut AutoDeployRow,
    via: Via,
) -> anyhow::Result<Step> {
    let Some(app) = server.store.get_app(name)? else {
        return Ok(Step::Gone);
    };
    if supported(&app.spec).is_err() {
        return Ok(Step::Gone);
    }
    // Apps run from their first deploy by hand; stopped ones stay stopped.
    if app.revision == 0 || !app.spec.enabled {
        return Ok(Step::Done);
    }
    let (head, deployed, what) = match watched_app(&app.spec, app.owner) {
        Watched::Git {
            repo,
            branch,
            owner,
        } => {
            let (access, _) = super::git::access(server, owner, &repo)?;
            let head = access.resolve(branch.as_deref()).await?;
            let what = format!(
                "new commit {} on {}",
                &head[..head.len().min(7)],
                branch.as_deref().unwrap_or("the default branch")
            );
            (head, app.commit.clone(), what)
        }
        Watched::Image { image, owner } => {
            let reference = ImageRef::parse(&image);
            let login = registry_login(server, owner, &reference)?;
            let registry = &server.auto_deploy.registry;
            let digest = registry.digest(&image, login.as_ref()).await?;
            // Which repository it is built from (for a webhook, so ghcr.io only)
            // can change with each image.
            if reference.registry == "ghcr.io"
                && (row.image_repo.is_none() || row.seen.as_deref() != Some(digest.as_str()))
            {
                match registry.source(&image, login.as_ref()).await {
                    Ok(source) => {
                        let repo = source
                            .as_deref()
                            .and_then(on_github)
                            .map(|l| l.key())
                            .unwrap_or_default();
                        if row.image_repo.as_deref() != Some(repo.as_str()) {
                            row.image_repo = Some(repo);
                            server.auto_deploy.hooks_changed();
                        }
                    }
                    Err(e) => warn!(app = %name, "reading the labels of {image}: {e:#}"),
                }
            }
            let what = format!(
                "new image for {} ({})",
                reference.display(),
                &digest[..digest.len().min(19)]
            );
            (digest, deployed_digest(server, name), what)
        }
        Watched::Nothing(_) => return Ok(Step::Gone),
    };
    match decide(row.seen.as_deref(), &head, deployed.as_deref()) {
        Decision::Nothing => return Ok(Step::Done),
        Decision::Seen => {
            row.seen = Some(head);
            return Ok(Step::Done);
        }
        Decision::Deploy => {}
    }
    if busy(server, &Target::App(name.into())) {
        return Ok(Step::Deferred);
    }
    // Left unseen: tried again once the node is back.
    if !server.hub.is_connected(&app.spec.node) {
        bail!("node {} is offline", app.spec.node);
    }
    // Seen from now on, even if the deploy fails: no retrying it every poll.
    row.seen = Some(head.clone());
    let request = DeployRequest {
        commit: matches!(app.spec.source, Source::Git { .. }).then(|| head.clone()),
        rebuild: false,
    };
    let deployment = super::api::start_deploy(server, name, &request)
        .await
        .map_err(|ApiError(_, e)| anyhow!("deploying {what}: {e}"))?;
    info!(app = %name, via = via.as_str(), "auto-deploy: {what}");
    super::events::auto_deploy(server, name, &what, via.as_str(), deployment.id);
    let _ = server.store.append_deploy_log(
        deployment.id,
        &format!(
            "deploying automatically: {what} (noticed by {})",
            via.as_str()
        ),
    );
    row.triggered = Some(head);
    row.triggered_at = Some(facade_core::now());
    row.triggered_by = Some(via.as_str().into());
    Ok(Step::Done)
}

async fn check_stack(
    server: &Arc<Server>,
    name: &str,
    row: &mut AutoDeployRow,
    via: Via,
) -> anyhow::Result<Step> {
    let Some(stack) = server.store.get_stack(name)? else {
        return Ok(Step::Gone);
    };
    let Watched::Git {
        repo,
        branch,
        owner,
    } = watched_stack(&stack.spec)
    else {
        return Ok(Step::Gone);
    };
    // Deployed by hand first.
    if stack.commit.is_none() {
        return Ok(Step::Done);
    }
    let (access, _) = super::git::access(server, owner, &repo)?;
    let head = access.resolve(branch.as_deref()).await?;
    match decide(row.seen.as_deref(), &head, stack.commit.as_deref()) {
        Decision::Nothing => return Ok(Step::Done),
        Decision::Seen => {
            row.seen = Some(head);
            return Ok(Step::Done);
        }
        Decision::Deploy => {}
    }
    if busy(server, &Target::Stack(name.into())) {
        return Ok(Step::Deferred);
    }
    if !server.hub.is_connected(&stack.spec.node) {
        bail!("node {} is offline", stack.spec.node);
    }
    row.seen = Some(head.clone());
    let what = format!(
        "new commit {} on {}",
        &head[..head.len().min(7)],
        branch.as_deref().unwrap_or("the default branch")
    );
    let request = StackDeployRequest {
        commit: Some(head.clone()),
        rebuild: false,
        all: false,
    };
    let deployment = super::stack::start_deploy(server, name, request)
        .await
        .map_err(|ApiError(_, e)| anyhow!("deploying {what}: {e}"))?;
    info!(stack = %name, via = via.as_str(), "auto-deploy: {what}");
    super::events::auto_deploy(server, name, &what, via.as_str(), deployment.id);
    let _ = server.store.stack_deploy_log(
        deployment.id,
        &format!(
            "deploying automatically: {what} (noticed by {})",
            via.as_str()
        ),
    );
    row.triggered = Some(head);
    row.triggered_at = Some(facade_core::now());
    row.triggered_by = Some(via.as_str().into());
    Ok(Step::Done)
}

// ---------------------------------------------------------------- webhooks on GitHub

async fn hooks_loop(server: Arc<Server>) {
    loop {
        if let Err(e) = sync_hooks(&server).await {
            warn!("auto-deploy webhooks: {e:#}");
        }
        tokio::select! {
            _ = tokio::time::sleep(HOOKS_EVERY) => {}
            _ = server.auto_deploy.hooks_wake.notified() => {}
        }
        // Changes come in bursts (an app and its credential): one pass for all.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// A repository that needs a webhook: its `owner/name`, the events, and whose
/// tokens may create it.
struct Wanted {
    path: String,
    events: BTreeSet<String>,
    owners: BTreeSet<i64>,
}

fn wanted_hooks(server: &Server) -> anyhow::Result<BTreeMap<String, Wanted>> {
    let mut wanted: BTreeMap<String, Wanted> = BTreeMap::new();
    for row in server.store.list_auto_deploy()? {
        let Some(target) = Target::parse(&row.target) else {
            continue;
        };
        let Some(watched) = watched(server, &target)? else {
            continue;
        };
        let owner = match &watched {
            Watched::Git { owner, .. } | Watched::Image { owner, .. } => *owner,
            Watched::Nothing(_) => continue,
        };
        let Some((location, _)) = hook_for(&watched, Some(&row)) else {
            continue;
        };
        let events: &[&str] = match watched {
            Watched::Image { .. } => &PACKAGE_EVENTS,
            _ => &[PUSH],
        };
        let entry = wanted.entry(location.key()).or_insert_with(|| Wanted {
            path: location.path.clone(),
            events: BTreeSet::new(),
            owners: BTreeSet::new(),
        });
        entry.events.extend(events.iter().map(|e| e.to_string()));
        entry.owners.insert(owner);
    }
    Ok(wanted)
}

/// GitHub access tokens covering the repository, of these owners (`None`: of
/// anyone), the most specific first.
fn github_tokens(
    server: &Server,
    owners: Option<&BTreeSet<i64>>,
    location: &RepoLocation,
) -> anyhow::Result<Vec<String>> {
    let mut rows: Vec<_> = server
        .store
        .list_git_credentials(None)?
        .into_iter()
        .filter(|c| c.kind == GitCredentialKind::Token)
        .filter(|c| owners.is_none_or(|o| o.contains(&c.owner_id)))
        .filter(|c| facade_core::git::scope_covers(&c.scope, location))
        .collect();
    rows.sort_by_key(|c| std::cmp::Reverse(c.scope.len()));
    let mut tokens: Vec<String> = Vec::new();
    for row in rows {
        let token = server.secrets.open_string(&row.sealed_secret)?;
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    Ok(tokens)
}

fn no_token(repo: &str) -> String {
    format!(
        "add a GitHub access token covering {repo} that may manage webhooks ({}) for instant deploys: facade git token",
        github::HOOK_PERMISSION
    )
}

/// Puts webhooks on the repositories auto-deploy targets come from, and takes
/// down the ones no target needs any more. Best effort: a target without a
/// webhook is polled.
async fn sync_hooks(server: &Arc<Server>) -> anyhow::Result<()> {
    let Some(url) = webhook_url(server) else {
        return Ok(());
    };
    let secret = webhook_secret(server)?;
    let secret_hash = hash_secret(&secret);
    let api_base = server.cfg.auto_deploy.github_api.clone();
    let mut wanted = wanted_hooks(server)?;
    // A webhook added by hand (on the repository or its organisation) already
    // reports these: a second one would only deliver everything twice.
    let outside = server.store.list_github_deliveries()?;
    let now = facade_core::now();
    wanted.retain(|_, want| {
        let location = RepoLocation {
            host: "github.com".into(),
            port: None,
            path: want.path.clone(),
            ssh: false,
        };
        !want
            .events
            .iter()
            .all(|e| outside_hook(&outside, &location, e, now).is_some())
    });
    let existing: HashMap<String, GithubHookRow> = server
        .store
        .list_github_hooks()?
        .into_iter()
        .map(|h| (h.repo.clone(), h))
        .collect();

    // Hooks no longer needed, or delivering to a previous domain.
    for hook in existing.values() {
        let stale = !wanted.contains_key(&hook.repo) || hook.url != url;
        if !stale {
            continue;
        }
        if let Some(id) = hook.hook_id {
            let location = on_github(&format!("https://{}", hook.repo));
            let tokens = match &location {
                Some(l) => github_tokens(server, None, l)?,
                None => vec![],
            };
            let mut removed = false;
            for token in &tokens {
                match github::Api::new(&api_base, token)
                    .delete_hook(&hook.path, id)
                    .await
                {
                    Ok(()) => {
                        removed = true;
                        break;
                    }
                    Err(e) => warn!(repo = %hook.repo, "removing its webhook: {e:#}"),
                }
            }
            if removed {
                info!(repo = %hook.repo, "removed the auto-deploy webhook");
            } else {
                warn!(repo = %hook.repo, "could not remove its webhook {id}: delete it on GitHub");
            }
        }
        if !wanted.contains_key(&hook.repo) {
            server.store.delete_github_hook(&hook.repo)?;
        }
    }

    for (repo, want) in &wanted {
        let before = existing.get(repo).filter(|h| h.url == url);
        let events: Vec<String> = want.events.iter().cloned().collect();
        let location = RepoLocation {
            host: "github.com".into(),
            port: None,
            path: want.path.clone(),
            ssh: false,
        };
        let tokens = github_tokens(server, Some(&want.owners), &location)?;
        let reset_secret = before.is_none_or(|h| h.secret_hash.as_deref() != Some(&secret_hash));
        let mut outcome = Err(no_token(repo));
        for token in &tokens {
            match github::Api::new(&api_base, token)
                .ensure_hook(&want.path, &url, &secret, &events, reset_secret)
                .await
            {
                Ok(id) => {
                    outcome = Ok(id);
                    break;
                }
                Err(e) => outcome = Err(format!("{e:#}")),
            }
        }
        let row = match outcome {
            Ok(id) => {
                if before.and_then(|h| h.hook_id) != Some(id) {
                    info!(%repo, events = %events.join(","), "auto-deploy webhook in place");
                }
                GithubHookRow {
                    repo: repo.clone(),
                    path: want.path.clone(),
                    url: url.clone(),
                    hook_id: Some(id),
                    events,
                    secret_hash: Some(secret_hash.clone()),
                    error: None,
                    updated_at: facade_core::now(),
                    delivered_at: before.and_then(|h| h.delivered_at),
                }
            }
            Err(error) => GithubHookRow {
                repo: repo.clone(),
                path: want.path.clone(),
                url: url.clone(),
                // A hook made before may still deliver; its secret is unchanged.
                hook_id: before.and_then(|h| h.hook_id),
                events: before.map(|h| h.events.clone()).unwrap_or_default(),
                secret_hash: before.and_then(|h| h.secret_hash.clone()),
                error: Some(error),
                updated_at: facade_core::now(),
                delivered_at: before.and_then(|h| h.delivered_at),
            },
        };
        server.store.put_github_hook(&row)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- status

pub fn view(server: &Server, target: &Target) -> anyhow::Result<AutoDeployView> {
    let row = server.store.get_auto_deploy(&target.key())?;
    let watched = watched(server, target)?.context("no such app or stack")?;
    let poll = server.cfg.auto_deploy.poll_secs.max(10);
    let url = webhook_url(server);
    let hooks = Hooks::load(server)?;
    let working = hook_working(&watched, row.as_ref(), &hooks, url.as_deref());
    let mut view = AutoDeployView {
        enabled: row.is_some(),
        watching: watched.describe(),
        method: AutoDeployMethod::Off,
        poll_secs: poll,
        note: None,
        webhook_repo: None,
        webhook_delivered_at: None,
        checked_at: None,
        seen: None,
        triggered_at: None,
        triggered: None,
        triggered_by: None,
        error: None,
    };
    if let Watched::Nothing(why) = &watched {
        view.note = Some(why.to_string());
        return Ok(view);
    }
    let Some(row) = row else {
        return Ok(view);
    };
    view.checked_at = row.checked_at;
    view.seen = row.seen.clone();
    view.triggered = row.triggered.clone();
    view.triggered_at = row.triggered_at;
    view.triggered_by = row.triggered_by.clone();
    view.error = row.error.clone();
    if let Some(hook) = working {
        view.method = AutoDeployMethod::Webhook;
        view.poll_secs = poll * WEBHOOK_SLOWDOWN;
        view.webhook_repo = Some(hook.scope);
        view.webhook_delivered_at = hook.delivered_at;
    } else {
        view.method = AutoDeployMethod::Polling;
        view.note = Some(why_polling(&watched, &row, &hooks.own, url.as_deref()));
    }
    // What keeps it from deploying at all comes first.
    let state_note = match target {
        Target::App(name) => server.store.get_app(name)?.and_then(|a| {
            if a.revision == 0 {
                Some("deploys by itself once deployed by hand")
            } else if !a.spec.enabled {
                Some("stopped: not deployed until started")
            } else {
                None
            }
        }),
        Target::Stack(name) => server
            .store
            .get_stack(name)?
            .filter(|s| s.commit.is_none())
            .map(|_| "deploys by itself once deployed by hand"),
    };
    if let Some(note) = state_note {
        view.note = Some(match view.note {
            Some(other) => format!("{note}; {other}"),
            None => note.to_string(),
        });
    }
    Ok(view)
}

fn why_polling(
    watched: &Watched,
    row: &AutoDeployRow,
    hooks: &[GithubHookRow],
    url: Option<&str>,
) -> String {
    let location = match (watched, hook_for(watched, Some(row))) {
        (_, Some((location, _))) => location,
        (Watched::Git { .. }, None) => {
            return "webhooks are set up for GitHub repositories only".into();
        }
        (Watched::Image { image, .. }, None) => {
            return if ImageRef::parse(image).registry != "ghcr.io" {
                "webhooks are set up for images on ghcr.io only".into()
            } else if row.image_repo.is_none() {
                "reading which repository the image comes from".into()
            } else {
                "the image names no GitHub repository (label org.opencontainers.image.source), so no webhook".into()
            };
        }
        (Watched::Nothing(why), None) => return why.to_string(),
    };
    if url.is_none() {
        return "give the server a domain (facade server set --api-domain) so GitHub can reach it, for instant deploys".into();
    }
    match hooks.iter().find(|h| h.repo == location.key()) {
        Some(hook) => match &hook.error {
            Some(e) => e.clone(),
            None => "setting up the webhook".into(),
        },
        None => "setting up the webhook".into(),
    }
}

// ---------------------------------------------------------------- HTTP

type S = State<Arc<Server>>;

fn bad(e: impl ToString) -> ApiError {
    ApiError::bad_request(e)
}

pub async fn get_app(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<AutoDeployView>> {
    load_app_for(&server, &user, &name)?;
    Ok(Json(view(&server, &Target::App(name))?))
}

pub async fn put_app(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Json(req): Json<SetAutoDeploy>,
) -> ApiResult<Json<AutoDeployView>> {
    let row = load_app_for(&server, &user, &name)?;
    if req.enabled {
        supported(&row.spec).map_err(ApiError::conflict)?;
    }
    let target = Target::App(name.clone());
    if set(&server, &target, req.enabled)? {
        switched(&server, &user, &name, req.enabled);
    }
    Ok(Json(view(&server, &target)?))
}

pub async fn check_image(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<ImageUpdateCheck>> {
    let row = load_app_for(&server, &user, &name)?;
    image_check(&server, &row)
        .await
        .map(Json)
        .map_err(|e| bad(format!("{e:#}")))
}

pub async fn get_stack(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<AutoDeployView>> {
    user.require_admin()?;
    let target = Target::Stack(name);
    if watched(&server, &target)?.is_none() {
        return Err(ApiError::not_found("no such stack"));
    }
    Ok(Json(view(&server, &target)?))
}

pub async fn put_stack(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Json(req): Json<SetAutoDeploy>,
) -> ApiResult<Json<AutoDeployView>> {
    user.require_admin()?;
    let target = Target::Stack(name.clone());
    match watched(&server, &target)? {
        None => return Err(ApiError::not_found("no such stack")),
        Some(Watched::Nothing(why)) if req.enabled => return Err(ApiError::conflict(why)),
        _ => {}
    }
    if set(&server, &target, req.enabled)? {
        switched(&server, &user, &name, req.enabled);
    }
    Ok(Json(view(&server, &target)?))
}

/// Auto-deploy turned on or off, for the Activity of an app or a stack.
fn switched(server: &Server, user: &User, target: &str, on: bool) {
    super::events::audit(
        server,
        user,
        target,
        facade_core::events::EventKind::AutoDeploySwitch,
        format!(
            "Auto-deploy turned {} by {}",
            if on { "on" } else { "off" },
            user.username
        ),
    );
}

/// Every app and stack with auto-deploy on that the caller may see.
pub async fn list(State(server): S, user: User) -> ApiResult<Json<Vec<AutoDeployTarget>>> {
    let mut out = Vec::new();
    for row in server.store.list_auto_deploy()? {
        let (kind, name, target) = match Target::parse(&row.target) {
            Some(Target::App(name)) if can_see_app(&server, &user, &name) => {
                ("app", name.clone(), Target::App(name))
            }
            Some(Target::Stack(name)) if user.is_admin() => {
                ("stack", name.clone(), Target::Stack(name))
            }
            _ => continue,
        };
        if let Ok(status) = view(&server, &target) {
            out.push(AutoDeployTarget {
                kind: kind.into(),
                name,
                status,
            });
        }
    }
    Ok(Json(out))
}

/// The URL and secret to set up a webhook by hand (on a repository facade has no
/// token for, say).
pub async fn hook_info(State(server): S, user: User) -> ApiResult<Json<WebhookInfo>> {
    user.require_admin()?;
    Ok(Json(WebhookInfo {
        url: webhook_url(&server),
        secret: webhook_secret(&server)?,
    }))
}

/// `POST /api/hooks/github`: a delivery from a repository webhook. Signed with
/// the server's webhook secret; answered at once, the checks it asks for run in
/// the auto-deploy loop.
pub async fn github_hook(State(server): S, headers: HeaderMap, body: Bytes) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let secret = match webhook_secret(&server) {
        Ok(s) => s,
        Err(e) => return ApiError::from(e).into_response(),
    };
    if !github::verify(secret.as_bytes(), &body, &header("x-hub-signature-256")) {
        return ApiError(StatusCode::UNAUTHORIZED, "bad or missing signature".into())
            .into_response();
    }
    let event = match github::parse_event(&header("x-github-event"), &body) {
        Ok(e) => e,
        Err(e) => return bad(format!("{e:#}")).into_response(),
    };
    let now = facade_core::now();
    if let Some(repo) = event.repo()
        && let Err(e) = server.store.github_hook_delivered(repo, now)
    {
        warn!("recording a webhook delivery: {e:#}");
    }
    // A webhook facade did not make (added by hand, maybe on an organisation):
    // remember where it sits and what it sends, so the targets it covers count
    // as reported by a webhook.
    let hook_id = header("x-github-hook-id").parse::<i64>().ok();
    let own = server
        .store
        .list_github_hooks()
        .map(|hooks| {
            hooks
                .iter()
                .any(|h| h.hook_id.is_some() && h.hook_id == hook_id)
        })
        .unwrap_or(false);
    if !own
        && let Some((scope, subscribed)) =
            github::delivery_scope(&header("x-github-hook-installation-target-type"), &body)
        && let Err(e) = server.store.github_delivery(
            &scope,
            &header("x-github-event"),
            subscribed.as_deref(),
            now,
        )
    {
        warn!("recording a webhook delivery: {e:#}");
    }
    let targets = match matching(&server, &event) {
        Ok(t) => t,
        Err(e) => return ApiError::from(e).into_response(),
    };
    for target in &targets {
        server.auto_deploy.check_now(target);
    }
    if matches!(event, Event::Ping { .. }) {
        return (StatusCode::OK, Json(serde_json::json!({"pong": true}))).into_response();
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "checking": targets })),
    )
        .into_response()
}

/// The targets a delivery may concern: checked right away (the check itself
/// decides whether anything deploys).
fn matching(server: &Server, event: &Event) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for row in server.store.list_auto_deploy()? {
        let Some(target) = Target::parse(&row.target) else {
            continue;
        };
        let Some(watched) = watched(server, &target)? else {
            continue;
        };
        let hit = match (event, &watched) {
            (
                Event::Push {
                    repo,
                    branch,
                    default_branch,
                },
                Watched::Git {
                    repo: watched_repo,
                    branch: watched_branch,
                    ..
                },
            ) => {
                facade_core::git::parse(watched_repo).is_some_and(|l| l.key() == *repo)
                    && match watched_branch {
                        Some(b) => b == branch,
                        None => default_branch.as_ref().is_none_or(|d| d == branch),
                    }
            }
            (Event::Package { image, tag, .. }, Watched::Image { image: watched, .. }) => {
                let r = ImageRef::parse(watched);
                format!("{}/{}", r.registry, r.repository).to_lowercase() == *image
                    && tag.as_ref().is_none_or(|t| *t == r.reference)
            }
            _ => false,
        };
        if hit {
            out.push(row.target);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhooks_added_by_hand_cover_their_repository_or_organisation() {
        let now = 10_000_000;
        let valet = on_github("https://github.com/Sigma-studios/valet").unwrap();
        let elsewhere = on_github("https://github.com/someone/valet").unwrap();
        let row = |scope: &str, events: &[&str], ago: u64| GithubDeliveryRow {
            scope: scope.into(),
            events: events.iter().map(|e| e.to_string()).collect(),
            delivered_at: now - ago,
        };
        let org = [row("github.com/sigma-studios", &["push", "package"], 60)];
        let hit = outside_hook(&org, &valet, PUSH, now).unwrap();
        assert_eq!(hit.scope, "github.com/sigma-studios (organisation)");
        assert!(outside_hook(&org, &elsewhere, PUSH, now).is_none());
        // Either package event stands for both.
        assert!(outside_hook(&org, &valet, "registry_package", now).is_some());
        // Not sending pushes, or silent for too long: polled.
        let packages = [row("github.com/sigma-studios", &["package"], 60)];
        assert!(outside_hook(&packages, &valet, PUSH, now).is_none());
        let stale = [row(
            "github.com/sigma-studios",
            &["push"],
            OUTSIDE_HOOK_FRESH + 1,
        )];
        assert!(outside_hook(&stale, &valet, PUSH, now).is_none());
        // The repository's own webhook wins over its organisation's.
        let both = [
            row("github.com/sigma-studios", &["*"], 10),
            row("github.com/sigma-studios/valet", &["push"], 3600),
        ];
        assert_eq!(
            outside_hook(&both, &valet, PUSH, now).unwrap().scope,
            "github.com/sigma-studios/valet"
        );
    }

    #[test]
    fn decides_on_edges() {
        // Turned on while behind: catches up.
        assert_eq!(decide(None, "b", Some("a")), Decision::Deploy);
        // Turned on while current.
        assert_eq!(decide(None, "a", Some("a")), Decision::Seen);
        // An image whose deployed digest is unknown: a baseline, not a deploy.
        assert_eq!(decide(None, "d1", None), Decision::Seen);
        // A new push.
        assert_eq!(decide(Some("a"), "b", Some("a")), Decision::Deploy);
        // The deploy of b failed, or was rolled back to a: b is not retried.
        assert_eq!(decide(Some("b"), "b", Some("a")), Decision::Nothing);
        // Deployed by hand before the poll noticed.
        assert_eq!(decide(Some("a"), "b", Some("b")), Decision::Seen);
        // The next push after a failure deploys.
        assert_eq!(decide(Some("b"), "c", Some("a")), Decision::Deploy);
    }

    #[test]
    fn names_targets() {
        for t in [Target::App("api".into()), Target::Stack("shop".into())] {
            assert_eq!(Target::parse(&t.key()), Some(t));
        }
        assert_eq!(Target::parse("nope"), None);
        assert!(on_github("https://github.com/Me/Shop.git").is_some());
        assert!(on_github("git@github.com:me/shop.git").is_some());
        assert!(on_github("https://gitlab.com/me/shop").is_none());
        assert!(on_github("https://github.com/me").is_none());
    }
}
