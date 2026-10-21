//! Stacks: what a stack runs at a commit, and deploying it as one.
//!
//! A deploy resolves one commit, reads the stack's compose file at that commit (or
//! takes the services defined in facade), applies the settings given in facade,
//! and works out what changed. Changed services are built first, all of them, and
//! nothing is replaced if one fails; then they are replaced in dependency order.
//! Services whose build context, settings and variables did not change are left
//! running.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use facade_core::api::{
    AppView, DeploymentStatus, EnvPatch, EnvView, PlannedService, ServiceChange,
    StackDeployRequest, StackDeploymentService, StackDeploymentView, StackPlan, StackPlanRequest,
    StackView,
};
use facade_core::manifest::EnvEntry;
use facade_core::proto::{AgentRequest, ApplyPhase};
use facade_core::stack::{ServiceSpec, StackSpec, deploy_order};
use facade_core::{AppSpec, Build, Source};
use serde::Deserialize;
use tracing::{info, warn};
use uuid::Uuid;

use super::api::{ApiError, User};
use super::{Server, random_token};
use crate::gitauth::Access;

type ApiResult<T> = Result<T, ApiError>;

/// A stack deployment in progress.
#[derive(Default)]
pub struct StackRun {
    pub cancelled: bool,
    /// Services are being replaced: too late to cancel.
    pub releasing: bool,
}

/// A stack at a commit: what each service runs.
pub struct Resolved {
    pub commit: Option<String>,
    pub title: Option<String>,
    /// In deploy order.
    pub services: Vec<ResolvedService>,
    pub levels: Vec<Vec<String>>,
    pub notes: Vec<String>,
    /// Values made for secrets the file expects, to keep as stack variables.
    pub generated: Vec<(String, String)>,
}

pub struct ResolvedService {
    pub service: String,
    pub app: AppSpec,
    pub depends_on: Vec<String>,
    /// From the compose file: `(key, value, sensitive)`.
    pub file_env: Vec<(String, String, bool)>,
    /// Variables the file declares without a value, and that facade does not set.
    pub missing: Vec<String>,
    pub build_key: Option<String>,
}

/// A directory removed when dropped.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stack_vars(server: &Server, stack: &str) -> anyhow::Result<HashMap<String, String>> {
    server
        .store
        .list_stack_vars(stack)?
        .into_iter()
        .map(|v| Ok((v.key, server.secrets.open_string(&v.sealed)?)))
        .collect()
}

/// What `build` produces depends on: the content of its context (and Dockerfile).
async fn build_key(dir: &std::path::Path, build: &Build) -> anyhow::Result<String> {
    Ok(match build {
        Build::Dockerfile {
            dockerfile,
            context,
        } => format!(
            "{}:{}",
            Access::object_id(dir, context).await?,
            Access::object_id(dir, dockerfile).await?
        ),
        Build::Rust { .. } => Access::object_id(dir, ".").await?,
        Build::Static { dir: d } => Access::object_id(dir, d).await?,
    })
}

/// What `spec` runs at `requested` (default: its branch's latest commit).
pub async fn resolve(
    server: &Server,
    spec: &StackSpec,
    requested: Option<&str>,
) -> anyhow::Result<Resolved> {
    spec.validate()?;
    let repo_source = spec.repo.as_ref().map(|repo| Source::Git {
        repo: repo.clone(),
        branch: spec.branch.clone(),
        build: Build::default(),
    });
    let commit = match &repo_source {
        Some(source) => super::git::resolve_commit(server, source, requested).await?,
        None if requested.is_some() => bail!("this stack has no repository to take a commit from"),
        None => None,
    };

    // The file and the build contexts, at that commit.
    let builds_here = spec.services.values().any(|s| s.build.is_some());
    let mut checkout = None;
    let mut title = None;
    if let (Some(repo), Some(commit)) = (&spec.repo, &commit)
        && (spec.file.is_some() || builds_here)
    {
        let dir = Scratch(
            server
                .cfg
                .data_dir
                .join("inspect")
                .join(Uuid::new_v4().to_string()),
        );
        let (access, _) = super::git::access(server, repo)?;
        title =
            Some(access.checkout(commit, &dir.0).await.with_context(|| {
                format!("fetching {repo} at {}", &commit[..12.min(commit.len())])
            })?);
        checkout = Some(dir);
    }

    let vars = stack_vars(server, &spec.name)?;
    let mut notes = Vec::new();
    let mut generated = Vec::new();
    // (service, host, settings from the file, variables from the file)
    let mut base: Vec<(String, String, ServiceSpec, BTreeMap<String, EnvEntry>)> = Vec::new();
    match &spec.file {
        Some(file) => {
            let dir = &checkout.as_ref().expect("checked out for the file").0;
            let secret = || random_token()[..32].to_string();
            let plan = super::compose::plan(dir, file, &spec.name, &vars, &secret)?;
            notes = plan.notes;
            generated = plan.generated;
            for s in plan.services {
                base.push((s.service, s.host, s.spec, s.env));
            }
            for name in spec.services.keys() {
                if !base.iter().any(|(s, ..)| s == name) {
                    notes.push(format!(
                        "settings for service {name} are ignored: {file} has no such service"
                    ));
                }
            }
        }
        None => {
            for (service, s) in &spec.services {
                base.push((
                    service.clone(),
                    service.clone(),
                    ServiceSpec::default(),
                    BTreeMap::new(),
                ));
                let _ = s;
            }
        }
    }

    let mut services = Vec::new();
    for (service, host, file_spec, env) in base {
        let settings = spec.services.get(&service).cloned().unwrap_or_default();
        let merged = settings.merged_over(&file_spec);
        let name = spec.app_name(&service);
        let current = server.store.get_app(&name)?;
        let enabled = current.as_ref().is_none_or(|r| r.spec.enabled);
        let app = merged.to_app(spec, &service, &host, enabled)?;
        let build_key = match (&app.source, &checkout) {
            (Source::Git { build, .. }, Some(dir)) => Some(
                build_key(&dir.0, build)
                    .await
                    .with_context(|| format!("service {service}"))?,
            ),
            _ => None,
        };
        let set_here: Vec<String> = server
            .store
            .list_env(&name)?
            .into_iter()
            .map(|e| e.key)
            .collect();
        let mut file_env = Vec::new();
        let mut missing = Vec::new();
        for (key, entry) in env {
            match entry.resolve() {
                (Some(value), sensitive) => file_env.push((key, value.to_string(), sensitive)),
                (None, _) if !set_here.contains(&key) => missing.push(key),
                (None, _) => {}
            }
        }
        services.push(ResolvedService {
            depends_on: merged.depends_on.clone().unwrap_or_default(),
            service,
            app,
            file_env,
            missing,
            build_key,
        });
    }
    let deps: Vec<(String, Vec<String>)> = services
        .iter()
        .map(|s| (s.service.clone(), s.depends_on.clone()))
        .collect();
    let levels = deploy_order(&deps)?;
    let order: Vec<&String> = levels.iter().flatten().collect();
    services.sort_by_key(|s| order.iter().position(|o| **o == s.service));
    if title.is_none()
        && let Some(commit) = &commit
    {
        title = server.store.commit_title(commit)?;
    }
    Ok(Resolved {
        commit,
        title,
        services,
        levels,
        notes,
        generated,
    })
}

fn change(server: &Server, s: &ResolvedService) -> anyhow::Result<ServiceChange> {
    let Some(row) = server
        .store
        .get_app(&s.app.name)?
        .filter(|r| r.revision > 0)
    else {
        return Ok(ServiceChange::New);
    };
    // A service whose last deployment did not go through is retried.
    let deployed = server
        .store
        .list_deployments(&s.app.name, 1)?
        .first()
        .is_none_or(|d| d.status == DeploymentStatus::Succeeded);
    let same = deployed
        && row.spec == s.app
        && row.build_key == s.build_key
        && server.stack_env(&row)? == s.file_env;
    Ok(if same {
        ServiceChange::Unchanged
    } else {
        ServiceChange::Changed
    })
}

/// The apps running a stack's services.
fn members(server: &Server, stack: &str) -> anyhow::Result<Vec<super::store::AppRow>> {
    Ok(server
        .store
        .list_apps()?
        .into_iter()
        .filter(|a| a.spec.stack.as_ref().is_some_and(|s| s.stack == stack))
        .collect())
}

/// Saves generated secrets as stack variables (sensitive), once.
fn keep_generated(
    server: &Server,
    stack: &str,
    generated: &[(String, String)],
) -> anyhow::Result<()> {
    for (key, value) in generated {
        server
            .store
            .set_stack_var(stack, key, &server.secrets.seal(value.as_bytes()), true)?;
    }
    Ok(())
}

/// Domains a stack's services claim must not belong to anything else.
fn check_domains(server: &Server, stack: &str, resolved: &Resolved) -> Result<(), ApiError> {
    let routes = server.routes.read();
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for s in &resolved.services {
        for domain in &s.app.domains {
            if server.cfg.api_domain.as_deref() == Some(domain.as_str()) {
                return Err(ApiError::conflict(format!("{domain} is the API domain")));
            }
            if let Some(owner) = routes.domains.get(domain)
                && !owner.starts_with(&format!("{stack}."))
            {
                return Err(ApiError::conflict(format!(
                    "{domain} is already used by {owner}"
                )));
            }
            if let Some(other) = seen.insert(domain, &s.service) {
                return Err(ApiError::conflict(format!(
                    "{domain} is claimed by both {other} and {}",
                    s.service
                )));
            }
        }
    }
    Ok(())
}

/// Creates the apps of services that have none yet (not deployed), so their
/// variables can be set before the first deploy.
fn add_new_members(server: &Server, resolved: &Resolved) -> anyhow::Result<()> {
    for s in &resolved.services {
        if server.store.get_app(&s.app.name)?.is_none() {
            server.store.put_app(&s.app)?;
        }
    }
    server.refresh_routes()
}

fn name_taken(server: &Server, name: &str) -> anyhow::Result<bool> {
    Ok(server.store.get_app(name)?.is_some() || server.store.get_stack(name)?.is_some())
}

fn view(server: &Server, spec: StackSpec, commit: Option<String>) -> anyhow::Result<StackView> {
    let services: Vec<AppView> = members(server, &spec.name)?
        .into_iter()
        .map(|a| super::api::app_view(server, a.spec, a.revision, a.commit))
        .collect();
    let deploying = server
        .store
        .list_stack_deployments(&spec.name, 5)?
        .into_iter()
        .find(|d| !d.status.is_finished())
        .map(|d| d.id);
    Ok(StackView {
        spec,
        commit,
        services,
        deploying,
    })
}

fn load(server: &Server, name: &str) -> ApiResult<super::store::StackRow> {
    server
        .store
        .get_stack(name)?
        .ok_or_else(|| ApiError::not_found(format!("no stack named {name}")))
}

fn bad(e: anyhow::Error) -> ApiError {
    ApiError::bad_request(format!("{e:#}"))
}

// ---------------------------------------------------------------- handlers

type S = State<Arc<Server>>;

pub async fn list(State(server): S, _: User) -> ApiResult<Json<Vec<StackView>>> {
    Ok(Json(
        server
            .store
            .list_stacks()?
            .into_iter()
            .map(|s| view(&server, s.spec, s.commit))
            .collect::<anyhow::Result<_>>()?,
    ))
}

pub async fn get(
    State(server): S,
    _: User,
    Path(name): Path<String>,
) -> ApiResult<Json<StackView>> {
    let row = load(&server, &name)?;
    Ok(Json(view(&server, row.spec, row.commit)?))
}

pub async fn plan(
    State(server): S,
    _: User,
    Json(req): Json<StackPlanRequest>,
) -> ApiResult<Json<StackPlan>> {
    let resolved = resolve(&server, &req.spec, req.commit.as_deref())
        .await
        .map_err(bad)?;
    let mut services = Vec::new();
    for s in &resolved.services {
        services.push(PlannedService {
            service: s.service.clone(),
            app: s.app.clone(),
            depends_on: s.depends_on.clone(),
            env: s
                .file_env
                .iter()
                .map(|(k, v, sensitive)| EnvView {
                    key: k.clone(),
                    value: (!sensitive).then(|| v.clone()),
                    sensitive: *sensitive,
                })
                .collect(),
            missing: s.missing.clone(),
            change: change(&server, s)?,
        });
    }
    let removed = members(&server, &req.spec.name)?
        .into_iter()
        .filter(|m| !resolved.services.iter().any(|s| s.app.name == m.spec.name))
        .filter_map(|m| m.spec.service().map(str::to_string))
        .collect();
    Ok(Json(StackPlan {
        commit: resolved.commit,
        commit_title: resolved.title,
        services,
        removed,
        notes: resolved.notes,
    }))
}

pub async fn create(
    State(server): S,
    _: User,
    Json(spec): Json<StackSpec>,
) -> ApiResult<(StatusCode, Json<StackView>)> {
    spec.validate().map_err(ApiError::bad_request)?;
    if name_taken(&server, &spec.name)? {
        return Err(ApiError::conflict(format!(
            "an app or stack named {} exists",
            spec.name
        )));
    }
    if !server.store.node_exists(&spec.node)? {
        return Err(ApiError::bad_request(format!(
            "no node named {}",
            spec.node
        )));
    }
    let resolved = resolve(&server, &spec, None).await.map_err(bad)?;
    check_domains(&server, &spec.name, &resolved)?;
    server.store.put_stack(&spec)?;
    keep_generated(&server, &spec.name, &resolved.generated)?;
    add_new_members(&server, &resolved)?;
    info!(stack = %spec.name, services = resolved.services.len(), "stack created");
    Ok((StatusCode::CREATED, Json(view(&server, spec, None)?)))
}

pub async fn update(
    State(server): S,
    _: User,
    Path(name): Path<String>,
    Json(spec): Json<StackSpec>,
) -> ApiResult<Json<StackView>> {
    if spec.name != name {
        return Err(ApiError::bad_request("stacks cannot be renamed"));
    }
    let old = load(&server, &name)?;
    spec.validate().map_err(ApiError::bad_request)?;
    if !server.store.node_exists(&spec.node)? {
        return Err(ApiError::bad_request(format!(
            "no node named {}",
            spec.node
        )));
    }
    if old.spec.node != spec.node {
        let with_data: Vec<String> = members(&server, &name)?
            .into_iter()
            .filter(|m| m.revision > 0 && !m.spec.volumes.is_empty())
            .filter_map(|m| m.spec.service().map(str::to_string))
            .collect();
        if !with_data.is_empty() {
            return Err(ApiError::conflict(format!(
                "{} keep their data in volumes on {}, which do not move with the stack",
                with_data.join(", "),
                old.spec.node
            )));
        }
    }
    // Checked now, applied by the next deploy.
    let resolved = resolve(&server, &spec, None).await.map_err(bad)?;
    check_domains(&server, &name, &resolved)?;
    server.store.put_stack(&spec)?;
    keep_generated(&server, &name, &resolved.generated)?;
    add_new_members(&server, &resolved)?;
    Ok(Json(view(&server, spec, old.commit)?))
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    volumes: bool,
}

pub async fn delete(
    State(server): S,
    _: User,
    Path(name): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<StatusCode> {
    let row = load(&server, &name)?;
    let node = row.spec.node.clone();
    let online = server.hub.is_connected(&node);
    let services = members(&server, &name)?;
    if q.volumes && !online {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("node {node} is offline: the stack's volumes cannot be deleted now"),
        ));
    }
    let outside: Vec<String> = server
        .store
        .list_apps()?
        .into_iter()
        .filter(|a| a.spec.stack.as_ref().is_none_or(|s| s.stack != name))
        .filter(|a| {
            a.spec
                .links
                .iter()
                .any(|l| l.app.starts_with(&format!("{name}.")))
        })
        .map(|a| a.spec.name)
        .collect();
    if !outside.is_empty() {
        return Err(ApiError::conflict(format!(
            "{} link to services of {name}: remove those links first",
            outside.join(", ")
        )));
    }
    let mut volumes: Vec<String> = Vec::new();
    for m in &services {
        if online {
            server
                .hub
                .request(
                    &m.spec.node,
                    AgentRequest::Remove {
                        app: m.spec.name.clone(),
                        volumes: false,
                    },
                )
                .await
                .map_err(|e| ApiError(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")))?;
        }
        volumes.extend(m.spec.volumes.iter().map(|v| m.spec.volume_name(v)));
        server.store.delete_app(&m.spec.name)?;
        server.live.write().apps.remove(&m.spec.name);
    }
    if q.volumes {
        // Also the ones a removed service left behind.
        if let Some(storage) = server
            .live
            .read()
            .nodes
            .get(&node)
            .and_then(|n| n.storage.clone())
        {
            volumes.extend(
                storage
                    .volumes
                    .iter()
                    .filter(|v| v.name.starts_with(&format!("facade.{name}.")))
                    .map(|v| v.name.clone()),
            );
        }
        volumes.sort();
        volumes.dedup();
        for volume in volumes {
            if let Err(e) = server
                .hub
                .request(
                    &node,
                    AgentRequest::RemoveVolume {
                        name: volume.clone(),
                    },
                )
                .await
            {
                warn!(%volume, "deleting volume: {e:#}");
            }
        }
    }
    server.store.delete_stack(&name)?;
    server.refresh_routes()?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- variables

#[derive(Deserialize)]
pub struct RevealQuery {
    #[serde(default)]
    reveal: bool,
}

pub async fn get_env(
    State(server): S,
    _: User,
    Path(name): Path<String>,
    Query(q): Query<RevealQuery>,
) -> ApiResult<Json<Vec<EnvView>>> {
    load(&server, &name)?;
    let vars = server
        .store
        .list_stack_vars(&name)?
        .into_iter()
        .map(|v| {
            let value = if v.sensitive && !q.reveal {
                None
            } else {
                Some(server.secrets.open_string(&v.sealed)?)
            };
            Ok(EnvView {
                key: v.key,
                value,
                sensitive: v.sensitive,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(Json(vars))
}

/// Stack variables feed the compose file: they apply from the next deploy.
pub async fn patch_env(
    State(server): S,
    _: User,
    Path(name): Path<String>,
    Json(patch): Json<EnvPatch>,
) -> ApiResult<StatusCode> {
    load(&server, &name)?;
    for key in patch.set.keys().chain(&patch.unset) {
        if !super::api::valid_env_key(key) {
            return Err(ApiError::bad_request(format!(
                "`{key}` is not a valid variable name (A-Z, 0-9, _)"
            )));
        }
    }
    for (key, var) in &patch.set {
        let sealed = server.secrets.seal(var.value.as_bytes());
        server
            .store
            .set_stack_var(&name, key, &sealed, var.sensitive)?;
    }
    for key in &patch.unset {
        server.store.delete_stack_var(&name, key)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- deploying

pub async fn deploy(
    State(server): S,
    _: User,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<StackDeploymentView>)> {
    let req: StackDeployRequest = if body.iter().all(u8::is_ascii_whitespace) {
        StackDeployRequest::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid body: {e}")))?
    };
    let view = start_deploy(&server, &name, req).await?;
    Ok((StatusCode::ACCEPTED, Json(view)))
}

pub async fn start_deploy(
    server: &Arc<Server>,
    name: &str,
    req: StackDeployRequest,
) -> ApiResult<StackDeploymentView> {
    let row = load(server, name)?;
    if !server.hub.is_connected(&row.spec.node) {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("node {} is offline", row.spec.node),
        ));
    }
    if let Some(running) = server
        .store
        .list_stack_deployments(name, 5)?
        .into_iter()
        .find(|d| !d.status.is_finished())
    {
        return Err(ApiError::conflict(format!(
            "{name} is being deployed ({}): wait for it, or cancel it",
            running.id
        )));
    }
    let id = Uuid::new_v4();
    server.store.create_stack_deployment(id, name, None)?;
    server.stack_runs.lock().insert(id, StackRun::default());
    let server2 = server.clone();
    let spec = row.spec.clone();
    tokio::spawn(async move {
        let result = run(&server2, id, &spec, &req).await;
        let cancelled = server2
            .stack_runs
            .lock()
            .remove(&id)
            .is_some_and(|r| r.cancelled);
        if result.is_err() {
            stop_services(&server2, id, &spec.node).await;
        }
        let outcome = match &result {
            Ok(()) => server2.store.finish_stack_deployment(id, None, false),
            Err(_) if cancelled => {
                let _ = server2.store.stack_deploy_log(id, "cancelled");
                server2
                    .store
                    .finish_stack_deployment(id, Some("cancelled"), true)
            }
            Err(e) => {
                let message = format!("{e:#}");
                let _ = server2
                    .store
                    .stack_deploy_log(id, &format!("error: {message}"));
                server2
                    .store
                    .finish_stack_deployment(id, Some(&message), false)
            }
        };
        if let Err(e) = outcome {
            warn!(stack = %spec.name, "recording the deployment: {e:#}");
        }
    });
    Ok(server
        .store
        .get_stack_deployment(id)?
        .context("deployment vanished")?)
}

/// Ends the service deployments a stopped stack deployment leaves behind: builds
/// still going are stopped, built ones are not released.
async fn stop_services(server: &Server, id: Uuid, node: &str) {
    let Ok(Some(d)) = server.store.get_stack_deployment(id) else {
        return;
    };
    for s in d.services {
        let unfinished = server
            .store
            .get_deployment(s.deployment)
            .ok()
            .flatten()
            .is_some_and(|d| !d.status.is_finished());
        if !unfinished {
            continue;
        }
        if server.hub.is_connected(node) {
            let _ = server
                .hub
                .request(
                    node,
                    AgentRequest::Cancel {
                        deployment: s.deployment,
                    },
                )
                .await;
        }
        let _ = server.store.finish_deployment(
            s.deployment,
            Some("not replaced: the stack deployment stopped"),
            true,
        );
    }
}

fn cancelled(server: &Server, id: Uuid) -> bool {
    server
        .stack_runs
        .lock()
        .get(&id)
        .is_some_and(|r| r.cancelled)
}

/// Waits until a service's deployment is built (`built`) or done.
async fn wait(server: &Server, stack_deploy: Uuid, id: Uuid, built: bool) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3 * 3600);
    loop {
        if cancelled(server, stack_deploy) {
            bail!("cancelled");
        }
        let d = server
            .store
            .get_deployment(id)?
            .ok_or_else(|| anyhow!("deployment {id} vanished"))?;
        match d.status {
            DeploymentStatus::Succeeded => return Ok(()),
            DeploymentStatus::Failed | DeploymentStatus::Cancelled => {
                bail!("{}", d.error.unwrap_or_else(|| d.status.to_string()))
            }
            _ if built && server.store.is_built(id)? => return Ok(()),
            _ => {}
        }
        if tokio::time::Instant::now() > deadline {
            bail!("gave up waiting for {}", d.app);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn run(
    server: &Arc<Server>,
    id: Uuid,
    spec: &StackSpec,
    req: &StackDeployRequest,
) -> anyhow::Result<()> {
    let log = |line: String| {
        let _ = server.store.stack_deploy_log(id, &line);
    };
    let name = &spec.name;
    let node = spec.node.clone();
    log(match &spec.file {
        Some(file) => format!("reading {file}"),
        None => "services defined in facade".to_string(),
    });
    let resolved = resolve(server, spec, req.commit.as_deref()).await?;
    server
        .store
        .start_stack_deployment(id, resolved.title.as_deref())?;
    server
        .store
        .set_stack_deployment_commit(id, resolved.commit.as_deref())?;
    if let Some(commit) = &resolved.commit {
        log(format!(
            "commit {} {}",
            &commit[..12.min(commit.len())],
            resolved.title.as_deref().unwrap_or("")
        ));
    }
    for note in &resolved.notes {
        log(format!("note: {note}"));
    }
    keep_generated(server, name, &resolved.generated)?;
    let missing: Vec<String> = resolved
        .services
        .iter()
        .flat_map(|s| s.missing.iter().map(move |k| format!("{}: {k}", s.service)))
        .collect();
    if !missing.is_empty() {
        bail!(
            "variables without a value: {} (set them with `facade env set {name}.<service> KEY`)",
            missing.join(", ")
        );
    }
    check_domains(server, name, &resolved).map_err(|e| anyhow!(e.1))?;

    // What changes.
    let mut changed: Vec<&ResolvedService> = Vec::new();
    for s in &resolved.services {
        let c = change(server, s)?;
        let changes = c != ServiceChange::Unchanged || req.all || req.rebuild;
        log(format!(
            "{}: {}",
            s.service,
            match (c, changes) {
                (ServiceChange::New, _) => "new",
                (ServiceChange::Changed, _) => "changed",
                (ServiceChange::Unchanged, true) => "unchanged, replaced anyway",
                (ServiceChange::Unchanged, false) => "unchanged",
            }
        ));
        if changes {
            changed.push(s);
        }
    }
    let removed: Vec<super::store::AppRow> = members(server, name)?
        .into_iter()
        .filter(|m| !resolved.services.iter().any(|s| s.app.name == m.spec.name))
        .collect();
    if changed.is_empty() && removed.is_empty() {
        log("nothing to do".into());
        server
            .store
            .set_stack_commit(name, resolved.commit.as_deref())?;
        return Ok(());
    }

    // Phase 1: build (or pull) every changed service. Nothing changes yet.
    let mut deployments: HashMap<String, Uuid> = HashMap::new();
    let mut records = Vec::new();
    for s in &changed {
        let current = server.store.get_app(&s.app.name)?;
        let revision = current.as_ref().map_or(0, |r| r.revision) + 1;
        let did = Uuid::new_v4();
        server
            .store
            .create_deployment(did, &s.app.name, revision, resolved.commit.as_deref())?;
        if let (Some(commit), Some(title)) = (&resolved.commit, &resolved.title) {
            server.store.set_deployment_commit(did, commit, title)?;
        }
        deployments.insert(s.service.clone(), did);
        records.push(StackDeploymentService {
            service: s.service.clone(),
            deployment: did,
        });
    }
    server.store.set_stack_deployment_services(id, &records)?;
    log(format!("building {} service(s)", changed.len()));
    for s in &changed {
        let workload = server.workload_of(
            s.app.clone(),
            0,
            resolved.commit.clone(),
            s.build_key.clone(),
            &s.file_env,
        )?;
        server
            .hub
            .request(
                &node,
                AgentRequest::Apply {
                    workload,
                    deployment: Some(deployments[&s.service]),
                    rebuild: req.rebuild,
                    phase: ApplyPhase::Build,
                },
            )
            .await?;
    }
    // The first failure stops the others: the stack will not be replaced anyway.
    let mut pending: futures_util::stream::FuturesUnordered<_> = changed
        .iter()
        .map(|s| {
            let did = deployments[&s.service];
            async move { (s.service.clone(), wait(server, id, did, true).await) }
        })
        .collect();
    while let Some((service, result)) = futures_util::StreamExt::next(&mut pending).await {
        if let Err(e) = result {
            if cancelled(server, id) {
                bail!("cancelled");
            }
            bail!("nothing was replaced, {service} failed to build: {e:#}");
        }
    }
    log("built".into());
    if cancelled(server, id) {
        bail!("cancelled");
    }

    // Phase 2: replace services, each group after the ones it depends on.
    if let Some(run) = server.stack_runs.lock().get_mut(&id) {
        run.releasing = true;
    }
    for level in &resolved.levels {
        let group: Vec<&&ResolvedService> = changed
            .iter()
            .filter(|s| level.contains(&s.service))
            .collect();
        if group.is_empty() {
            continue;
        }
        for s in &group {
            log(format!("replacing {}", s.service));
            if let Some(old) = server.store.get_app(&s.app.name)?
                && old.revision > 0
                && old.spec.node != s.app.node
                && server.hub.is_connected(&old.spec.node)
            {
                server
                    .hub
                    .request(
                        &old.spec.node,
                        AgentRequest::Remove {
                            app: s.app.name.clone(),
                            volumes: false,
                        },
                    )
                    .await?;
            }
            let sealed = server.secrets.seal(&serde_json::to_vec(&s.file_env)?);
            server.store.release_service(
                &s.app,
                resolved.commit.as_deref(),
                s.build_key.as_deref(),
                &sealed,
            )?;
            server.refresh_routes()?;
            let workload = server.workload(&s.app.name)?;
            server
                .hub
                .request(
                    &node,
                    AgentRequest::Apply {
                        workload,
                        deployment: Some(deployments[&s.service]),
                        rebuild: false,
                        phase: ApplyPhase::Release,
                    },
                )
                .await?;
        }
        let waits = group.iter().map(|s| {
            let did = deployments[&s.service];
            async move { (s.service.clone(), wait(server, id, did, false).await) }
        });
        for (service, result) in futures_util::future::join_all(waits).await {
            match result {
                Ok(()) => log(format!("{service}: running")),
                Err(e) => bail!(
                    "{service} failed to start: {e:#} (the services after it were not replaced)"
                ),
            }
        }
    }

    for m in removed {
        let service = m.spec.service().unwrap_or(&m.spec.name).to_string();
        log(format!("removing {service} (no longer in the stack)"));
        if server.hub.is_connected(&m.spec.node) {
            server
                .hub
                .request(
                    &m.spec.node,
                    AgentRequest::Remove {
                        app: m.spec.name.clone(),
                        volumes: false,
                    },
                )
                .await?;
        }
        server.store.delete_app(&m.spec.name)?;
        server.live.write().apps.remove(&m.spec.name);
    }
    server.refresh_routes()?;
    server
        .store
        .set_stack_commit(name, resolved.commit.as_deref())?;
    log("done".into());
    Ok(())
}

pub async fn deployments(
    State(server): S,
    _: User,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<StackDeploymentView>>> {
    Ok(Json(server.store.list_stack_deployments(&name, 50)?))
}

pub async fn get_deployment(
    State(server): S,
    _: User,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<StackDeploymentView>> {
    server
        .store
        .get_stack_deployment(id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("no such deployment"))
}

pub async fn cancel(State(server): S, _: User, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    let d = server
        .store
        .get_stack_deployment(id)?
        .ok_or_else(|| ApiError::not_found("no such deployment"))?;
    if d.status.is_finished() {
        return Err(ApiError::conflict(format!(
            "the deployment already {}",
            d.status
        )));
    }
    {
        let mut runs = server.stack_runs.lock();
        match runs.get_mut(&id) {
            Some(run) if run.releasing => {
                return Err(ApiError::conflict(
                    "too late to cancel: the services are being replaced",
                ));
            }
            Some(run) => run.cancelled = true,
            None => {}
        }
    }
    // Builds in progress stop right away; the deployment ends at its next step.
    if let Some(row) = server.store.get_stack(&d.stack)? {
        stop_services(&server, id, &row.spec.node).await;
    }
    if !server.stack_runs.lock().contains_key(&id) {
        server
            .store
            .finish_stack_deployment(id, Some("cancelled"), true)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Creates or updates a stack from a manifest (`facade apply`), without deploying.
pub async fn apply(
    server: &Server,
    m: facade_core::manifest::StackManifest,
    report: &mut facade_core::api::ApplyReport,
) -> ApiResult<()> {
    let name = m.spec.name.clone();
    let at = |e: ApiError| ApiError(e.0, format!("stack {name}: {}", e.1));
    m.spec
        .validate()
        .map_err(|e| at(ApiError::bad_request(e)))?;
    if server.store.get_app(&name)?.is_some() {
        return Err(at(ApiError::conflict("an app has that name")));
    }
    if !server.store.node_exists(&m.spec.node)? {
        return Err(at(ApiError::bad_request(format!(
            "no node named {}",
            m.spec.node
        ))));
    }
    let existing = server.store.get_stack(&name)?;
    let mut changed = existing.as_ref().is_none_or(|e| e.spec != m.spec);
    if changed {
        let resolved = resolve(server, &m.spec, None)
            .await
            .map_err(|e| at(bad(e)))?;
        check_domains(server, &name, &resolved).map_err(at)?;
        server.store.put_stack(&m.spec)?;
        keep_generated(server, &name, &resolved.generated)?;
        add_new_members(server, &resolved)?;
    }
    // Whether `key` already holds `value`.
    let holds = |rows: &[super::store::EnvRow], key: &str, value: &str, sensitive: bool| {
        rows.iter().any(|r| {
            r.key == key
                && r.sensitive == sensitive
                && server.secrets.open(&r.sealed).ok().as_deref() == Some(value.as_bytes())
        })
    };
    let current = server.store.list_stack_vars(&name)?;
    for (key, entry) in &m.variables {
        match entry.resolve() {
            (Some(value), sensitive) if !holds(&current, key, value, sensitive) => {
                server.store.set_stack_var(
                    &name,
                    key,
                    &server.secrets.seal(value.as_bytes()),
                    sensitive,
                )?;
                changed = true;
            }
            (None, _) if !current.iter().any(|v| v.key == *key) => {
                report.missing_secrets.push(format!("{name}: {key}"));
            }
            _ => {}
        }
    }
    for (service, vars) in &m.env {
        let app = m.spec.app_name(service);
        if server.store.get_app(&app)?.is_none() {
            return Err(at(ApiError::bad_request(format!(
                "variables for service {service}, which the stack does not have"
            ))));
        }
        let current = server.store.list_env(&app)?;
        for (key, entry) in vars {
            match entry.resolve() {
                (Some(value), sensitive) if !holds(&current, key, value, sensitive) => {
                    server.store.set_env(
                        &app,
                        key,
                        &server.secrets.seal(value.as_bytes()),
                        sensitive,
                    )?;
                    changed = true;
                }
                (None, _) if !current.iter().any(|v| v.key == *key) => {
                    report.missing_secrets.push(format!("{app}: {key}"));
                }
                _ => {}
            }
        }
    }
    match (existing, changed) {
        (None, _) => report.created.push(name),
        (Some(_), true) => report.updated.push(name),
        (Some(_), false) => report.unchanged.push(name),
    }
    Ok(())
}
