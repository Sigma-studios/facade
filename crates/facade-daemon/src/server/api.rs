//! The HTTP API used by the CLI (and later the web UI). Served on `api_listen` with
//! the server's pinned certificate, and on `api_domain` through the ingress.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use facade_core::api::*;
use facade_core::manifest::{AppManifest, EnvEntry, Manifest, NodeDecl};
use facade_core::proto::{AgentRequest, AppState};
use facade_core::{AppSpec, validate_name};
use serde::Deserialize;
use tokio::net::TcpListener;
use tracing::{info, warn};
use uuid::Uuid;

use super::{Server, hash_secret, random_token};
use crate::tls;

const SESSION_DAYS: u64 = 30;

type S = State<Arc<Server>>;

pub fn router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/whoami", get(whoami))
        .route("/api/password", put(change_password))
        .route("/api/status", get(status))
        .route("/api/alerts", get(alerts))
        .route("/api/alerts/test", post(test_alert))
        .route(
            "/api/alerts/settings",
            get(notify_settings).put(update_notify),
        )
        .route("/api/nodes", get(list_nodes).post(create_node))
        .route("/api/nodes/{name}", axum::routing::delete(delete_node))
        .route("/api/nodes/{name}/token", post(rotate_node_token))
        .route("/api/nodes/{name}/metrics", get(node_metrics))
        .route("/api/nodes/{name}/ip", put(set_node_ip))
        .route("/api/dns", get(dns_settings).put(update_dns))
        .route("/api/domains", get(domains))
        .route("/api/apps", get(list_apps).post(create_app))
        .route(
            "/api/apps/{name}",
            get(get_app).put(update_app).delete(delete_app),
        )
        .route("/api/apps/{name}/deploy", post(deploy_app))
        .route("/api/apps/{name}/restart", post(restart_app))
        .route("/api/apps/{name}/start", post(start_app))
        .route("/api/apps/{name}/stop", post(stop_app))
        .route("/api/apps/{name}/env", get(get_env).patch(patch_env))
        .route("/api/apps/{name}/logs", get(logs))
        .route("/api/apps/{name}/metrics", get(app_metrics))
        .route("/api/apps/{name}/deployments", get(list_deployments))
        .route("/api/deployments/{id}", get(get_deployment))
        .route("/api/export", get(export))
        .route("/api/apply", post(apply))
        .route(
            "/api/artifacts",
            post(super::artifacts::upload).layer(axum::extract::DefaultBodyLimit::max(
                super::artifacts::MAX_UPLOAD,
            )),
        )
        .route("/api/inspect", post(super::inspect::inspect))
        .route("/api/health", get(|| async { "ok" }))
        .fallback(super::ui::serve)
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024))
        .with_state(server)
}

/// The API on its own port, with the self-signed identity.
pub async fn serve_direct(server: Arc<Server>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(server.cfg.api_listen)
        .await
        .with_context(|| format!("binding API listener {}", server.cfg.api_listen))?;
    info!("API on https://{}", server.cfg.api_listen);
    let acceptor =
        tokio_rustls::TlsAcceptor::from(tls::single_cert_server(server.identity.key.clone()));
    let app = router(server);
    loop {
        let (tcp, _) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let Ok(Ok(tls)) =
                tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await
            else {
                return;
            };
            let service = hyper_util::service::TowerToHyperService::new(app);
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                    .await;
        });
    }
}

/// The API in plain HTTP on loopback, for this machine's entry point to forward
/// `api_domain` to.
pub async fn serve_loopback(server: Arc<Server>) -> anyhow::Result<()> {
    let addr = server.cfg.api_loopback;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    axum::serve(listener, router(server)).await?;
    Ok(())
}

/// Creates the first admin with a random password, printed once and saved next to
/// the database.
pub fn ensure_admin(server: &Server) -> anyhow::Result<()> {
    if server.store.user_count()? > 0 {
        return Ok(());
    }
    let password = random_token()[..24].to_string();
    server
        .store
        .create_user("admin", &hash_password(&password)?, "admin")?;
    let path = server.cfg.data_dir.join("initial-admin-password");
    tls::write_private(&path, format!("{password}\n").as_bytes())?;
    warn!(
        "created user `admin` with password {password} (also in {}); change it with `facade passwd`",
        path.display()
    );
    Ok(())
}

fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?
        .to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------- errors & auth

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

impl ApiError {
    pub fn bad_request(msg: impl ToString) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

fn bad_request(msg: impl ToString) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.to_string())
}
fn not_found(msg: impl ToString) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.to_string())
}
fn conflict(msg: impl ToString) -> ApiError {
    ApiError(StatusCode::CONFLICT, msg.to_string())
}
fn unavailable(msg: impl ToString) -> ApiError {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, msg.to_string())
}

type ApiResult<T> = Result<T, ApiError>;

/// An authenticated caller. Every user is an admin for now; the role column is
/// where per-app permissions will hang.
#[allow(dead_code)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: String,
    token_hash: String,
}

impl FromRequestParts<Arc<Server>> for User {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, server: &Arc<Server>) -> ApiResult<Self> {
        let unauthorized = || ApiError(StatusCode::UNAUTHORIZED, "not logged in".into());
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(unauthorized)?;
        let token_hash = hash_secret(token.trim());
        let user = server
            .store
            .session_user(&token_hash)?
            .ok_or_else(unauthorized)?;
        Ok(User {
            id: user.id,
            username: user.username,
            role: user.role,
            token_hash,
        })
    }
}

async fn login(State(server): S, Json(req): Json<LoginRequest>) -> ApiResult<Json<LoginResponse>> {
    let user = server.store.user_by_name(&req.username)?;
    let ok = user
        .as_ref()
        .is_some_and(|u| verify_password(&req.password, &u.password_hash));
    let Some(user) = user.filter(|_| ok) else {
        // Slow down guessing.
        tokio::time::sleep(Duration::from_millis(800)).await;
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "wrong username or password".into(),
        ));
    };
    let token = random_token();
    server.store.create_session(
        &hash_secret(&token),
        user.id,
        facade_core::now() + SESSION_DAYS * 86400,
    )?;
    info!(user = %user.username, "login");
    Ok(Json(LoginResponse { token }))
}

async fn logout(State(server): S, user: User) -> ApiResult<StatusCode> {
    server.store.delete_session(&user.token_hash)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn whoami(user: User) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "username": user.username, "role": user.role }))
}

async fn change_password(
    State(server): S,
    user: User,
    Json(req): Json<ChangePassword>,
) -> ApiResult<StatusCode> {
    if req.password.len() < 12 {
        return Err(bad_request("password must be at least 12 characters"));
    }
    server
        .store
        .set_password(user.id, &hash_password(&req.password)?)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- status

fn node_views(server: &Server) -> anyhow::Result<Vec<NodeView>> {
    let live = server.live.read();
    Ok(server
        .store
        .list_nodes()?
        .into_iter()
        .map(|n| {
            let conn = server.hub.connection(&n.name);
            NodeView {
                public_ip_manual: n.public_ip.is_some(),
                public_ip: server.node_ip(&n.name),
                connected: conn.is_some(),
                local: n.name == server.cfg.local_node,
                hostname: n.hostname,
                address: n.address,
                agent_version: n.agent_version,
                last_seen: n.last_seen,
                stats: live.nodes.get(&n.name).and_then(|l| l.stats.clone()),
                name: n.name,
            }
        })
        .collect())
}

fn app_view(server: &Server, spec: AppSpec, revision: u64) -> AppView {
    let live = server.live.read();
    let entry = live.apps.get(&spec.name);
    let mut state = entry.map(|e| e.state).unwrap_or(AppState::Unknown);
    let mut message = entry.and_then(|e| e.message.clone());
    if revision == 0 {
        state = AppState::Pending;
        message = Some("not deployed yet".into());
    } else if !server.hub.is_connected(&spec.node) {
        state = AppState::Unknown;
    }
    AppView {
        state,
        message,
        stats: entry.and_then(|e| e.stats.clone()),
        revision,
        spec,
    }
}

fn app_views(server: &Server) -> anyhow::Result<Vec<AppView>> {
    Ok(server
        .store
        .list_apps()?
        .into_iter()
        .map(|a| app_view(server, a.spec, a.revision))
        .collect())
}

async fn status(State(server): S, _: User) -> ApiResult<Json<StatusView>> {
    Ok(Json(StatusView {
        nodes: node_views(&server)?,
        apps: app_views(&server)?,
    }))
}

async fn alerts(State(server): S, _: User) -> Json<Vec<AlertView>> {
    Json(server.alerts.lock().views())
}

async fn notify_settings(State(server): S, _: User) -> Json<NotifySettings> {
    Json(server.notifier.settings())
}

async fn update_notify(
    State(server): S,
    _: User,
    Json(update): Json<NotifyUpdate>,
) -> ApiResult<Json<NotifySettings>> {
    server
        .notifier
        .update(&server.store, &server.secrets, update)
        .map_err(bad_request)?;
    Ok(Json(server.notifier.settings()))
}

async fn test_alert(State(server): S, _: User) -> ApiResult<StatusCode> {
    if !server.notifier.is_configured() {
        return Err(bad_request(
            "no Discord webhook set (facade alerts config --webhook <url>)",
        ));
    }
    server
        .notifier
        .test()
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- nodes

async fn list_nodes(State(server): S, _: User) -> ApiResult<Json<Vec<NodeView>>> {
    Ok(Json(node_views(&server)?))
}

async fn create_node(
    State(server): S,
    _: User,
    Json(req): Json<CreateNode>,
) -> ApiResult<(StatusCode, Json<CreatedNode>)> {
    validate_name("node name", &req.name).map_err(bad_request)?;
    if server.store.node_exists(&req.name)? {
        return Err(conflict(format!(
            "node {} exists (use `facade node token` to re-issue its token)",
            req.name
        )));
    }
    let join_token = server.issue_join_token(&req.name).map_err(bad_request)?;
    server
        .live
        .write()
        .nodes
        .entry(req.name.clone())
        .or_default()
        .offline_since = Some(facade_core::now());
    Ok((
        StatusCode::CREATED,
        Json(CreatedNode {
            name: req.name,
            join_token,
        }),
    ))
}

async fn rotate_node_token(
    State(server): S,
    _: User,
    Path(name): Path<String>,
) -> ApiResult<Json<CreatedNode>> {
    if !server.store.node_exists(&name)? {
        return Err(not_found(format!("no node named {name}")));
    }
    if name == server.cfg.local_node {
        return Err(bad_request("the server's own node needs no token"));
    }
    let join_token = server.issue_join_token(&name).map_err(bad_request)?;
    Ok(Json(CreatedNode { name, join_token }))
}

async fn delete_node(State(server): S, _: User, Path(name): Path<String>) -> ApiResult<StatusCode> {
    if name == server.cfg.local_node {
        return Err(bad_request("the server's own node cannot be removed"));
    }
    let apps: Vec<String> = server
        .store
        .list_apps()?
        .into_iter()
        .filter(|a| a.spec.node == name)
        .map(|a| a.spec.name)
        .collect();
    if !apps.is_empty() {
        return Err(conflict(format!(
            "move or delete its apps first: {}",
            apps.join(", ")
        )));
    }
    server.store.delete_node(&name)?;
    server.live.write().nodes.remove(&name);
    Ok(StatusCode::NO_CONTENT)
}

async fn set_node_ip(
    State(server): S,
    _: User,
    Path(name): Path<String>,
    Json(req): Json<SetNodeIp>,
) -> ApiResult<StatusCode> {
    if !server.store.node_exists(&name)? {
        return Err(not_found(format!("no node named {name}")));
    }
    let ip = req
        .public_ip
        .map(|ip| ip.trim().to_string())
        .filter(|ip| !ip.is_empty());
    if let Some(ip) = &ip
        && ip.parse::<std::net::IpAddr>().is_err()
    {
        return Err(bad_request(format!("`{ip}` is not an IP address")));
    }
    server.store.set_node_ip(&name, ip.as_deref())?;
    server.domains_wake.notify_one();
    Ok(StatusCode::NO_CONTENT)
}

async fn dns_settings(State(server): S, _: User) -> Json<DnsSettings> {
    Json(server.dns.settings())
}

async fn update_dns(
    State(server): S,
    _: User,
    Json(update): Json<DnsUpdate>,
) -> ApiResult<Json<DnsSettings>> {
    super::dns::update(&server, update)
        .await
        .map(Json)
        .map_err(bad_request)
}

async fn domains(State(server): S, _: User) -> Json<Vec<DomainStatus>> {
    Json(super::dns::statuses(&server).await)
}

async fn node_metrics(
    State(server): S,
    _: User,
    Path(name): Path<String>,
) -> Json<Vec<NodeMetric>> {
    let live = server.live.read();
    Json(
        live.nodes
            .get(&name)
            .map(|n| n.history.iter().cloned().collect())
            .unwrap_or_default(),
    )
}

// ---------------------------------------------------------------- apps

fn load_app(server: &Server, name: &str) -> ApiResult<super::store::AppRow> {
    server
        .store
        .get_app(name)?
        .ok_or_else(|| not_found(format!("no app named {name}")))
}

/// Checks that apply to a spec in the context of the whole installation.
fn check_spec(server: &Server, spec: &AppSpec) -> ApiResult<()> {
    spec.validate().map_err(bad_request)?;
    if let facade_core::Source::Executable {
        sha256,
        arch,
        filename,
        ..
    } = &spec.source
    {
        if !super::artifacts::exists(server, sha256) {
            return Err(bad_request(
                "the executable was not uploaded to this server",
            ));
        }
        let node_arch = server
            .live
            .read()
            .nodes
            .get(&spec.node)
            .and_then(|n| n.stats.as_ref())
            .map(|s| s.arch.clone())
            .filter(|a| !a.is_empty());
        if let Some(node_arch) = node_arch
            && node_arch != *arch
        {
            return Err(bad_request(format!(
                "{filename} is built for {arch}, but node {} is {node_arch}",
                spec.node
            )));
        }
    }
    if !server.store.node_exists(&spec.node)? {
        return Err(bad_request(format!("no node named {}", spec.node)));
    }
    let routes = server.routes.read();
    for domain in &spec.domains {
        if server.cfg.api_domain.as_deref() == Some(domain.as_str()) {
            return Err(conflict(format!("{domain} is the API domain")));
        }
        if let Some(owner) = routes.domains.get(domain)
            && *owner != spec.name
        {
            return Err(conflict(format!("{domain} is already used by {owner}")));
        }
    }
    Ok(())
}

async fn list_apps(State(server): S, _: User) -> ApiResult<Json<Vec<AppView>>> {
    Ok(Json(app_views(&server)?))
}

async fn get_app(State(server): S, _: User, Path(name): Path<String>) -> ApiResult<Json<AppView>> {
    let row = load_app(&server, &name)?;
    Ok(Json(app_view(&server, row.spec, row.revision)))
}

async fn create_app(
    State(server): S,
    _: User,
    Json(spec): Json<AppSpec>,
) -> ApiResult<(StatusCode, Json<AppView>)> {
    if server.store.get_app(&spec.name)?.is_some() {
        return Err(conflict(format!("app {} exists", spec.name)));
    }
    check_spec(&server, &spec)?;
    server.store.put_app(&spec)?;
    server.refresh_routes()?;
    // Created, not deployed: env vars usually come first, then `facade deploy`.
    Ok((StatusCode::CREATED, Json(app_view(&server, spec, 0))))
}

async fn update_app(
    State(server): S,
    _: User,
    Path(name): Path<String>,
    Json(spec): Json<AppSpec>,
) -> ApiResult<Json<AppView>> {
    if spec.name != name {
        return Err(bad_request("apps cannot be renamed"));
    }
    let old = load_app(&server, &name)?;
    check_spec(&server, &spec)?;
    save_spec(&server, &old.spec, &spec).await?;
    Ok(Json(app_view(&server, spec, old.revision)))
}

/// Stores a changed spec and moves the app between nodes if needed.
async fn save_spec(server: &Server, old: &AppSpec, new: &AppSpec) -> ApiResult<()> {
    server.store.put_app(new)?;
    server.refresh_routes()?;
    if old.node != new.node && server.hub.is_connected(&old.node) {
        server
            .hub
            .request(
                &old.node,
                AgentRequest::Remove {
                    app: new.name.clone(),
                },
            )
            .await
            .map_err(|e| unavailable(format!("removing from {}: {e:#}", old.node)))?;
    }
    if let Err(e) = server.push(&new.name).await {
        warn!(app = %new.name, "push failed: {e:#}");
    }
    Ok(())
}

async fn delete_app(State(server): S, _: User, Path(name): Path<String>) -> ApiResult<StatusCode> {
    let row = load_app(&server, &name)?;
    if server.hub.is_connected(&row.spec.node) {
        server
            .hub
            .request(&row.spec.node, AgentRequest::Remove { app: name.clone() })
            .await
            .map_err(|e| unavailable(format!("{e:#}")))?;
    }
    // An offline node drops the app when it reconnects and syncs.
    server.store.delete_app(&name)?;
    server.live.write().apps.remove(&name);
    server.refresh_routes()?;
    Ok(StatusCode::NO_CONTENT)
}

async fn deploy_app(
    State(server): S,
    _: User,
    Path(name): Path<String>,
) -> ApiResult<(StatusCode, Json<DeploymentView>)> {
    let row = load_app(&server, &name)?;
    if !server.hub.is_connected(&row.spec.node) {
        return Err(unavailable(format!("node {} is offline", row.spec.node)));
    }
    let revision = server.store.bump_revision(&name)?;
    let id = Uuid::new_v4();
    server.store.create_deployment(id, &name, revision)?;
    let mut workload = server.workload(&name)?;
    workload.spec.enabled = true;
    if !row.spec.enabled {
        let mut spec = row.spec.clone();
        spec.enabled = true;
        server.store.put_app(&spec)?;
    }
    server.set_app_state(&name, AppState::Pending, None);
    let request = AgentRequest::Apply {
        workload,
        deployment: Some(id),
    };
    if let Err(e) = server.hub.request(&row.spec.node, request).await {
        server
            .store
            .finish_deployment(id, Some(&format!("{e:#}")))?;
        return Err(unavailable(format!("{e:#}")));
    }
    let view = server
        .store
        .get_deployment(id)?
        .context("deployment vanished")?;
    Ok((StatusCode::ACCEPTED, Json(view)))
}

async fn restart_app(State(server): S, _: User, Path(name): Path<String>) -> ApiResult<StatusCode> {
    let row = load_app(&server, &name)?;
    server
        .hub
        .request(&row.spec.node, AgentRequest::Restart { app: name })
        .await
        .map_err(|e| unavailable(format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_enabled(server: &Server, name: &str, enabled: bool) -> ApiResult<StatusCode> {
    let row = load_app(server, name)?;
    let mut spec = row.spec.clone();
    spec.enabled = enabled;
    save_spec(server, &row.spec, &spec).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn start_app(State(server): S, _: User, Path(name): Path<String>) -> ApiResult<StatusCode> {
    set_enabled(&server, &name, true).await
}

async fn stop_app(State(server): S, _: User, Path(name): Path<String>) -> ApiResult<StatusCode> {
    set_enabled(&server, &name, false).await
}

// ---------------------------------------------------------------- env

#[derive(Deserialize)]
struct RevealQuery {
    #[serde(default)]
    reveal: bool,
}

async fn get_env(
    State(server): S,
    _: User,
    Path(name): Path<String>,
    Query(q): Query<RevealQuery>,
) -> ApiResult<Json<Vec<EnvView>>> {
    load_app(&server, &name)?;
    let vars = server
        .store
        .list_env(&name)?
        .into_iter()
        .map(|e| {
            let value = if e.sensitive && !q.reveal {
                None
            } else {
                Some(server.secrets.open_string(&e.sealed)?)
            };
            Ok(EnvView {
                key: e.key,
                value,
                sensitive: e.sensitive,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(Json(vars))
}

fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

async fn patch_env(
    State(server): S,
    _: User,
    Path(name): Path<String>,
    Json(patch): Json<EnvPatch>,
) -> ApiResult<StatusCode> {
    load_app(&server, &name)?;
    for key in patch.set.keys().chain(&patch.unset) {
        if !valid_env_key(key) {
            return Err(bad_request(format!(
                "`{key}` is not a valid variable name (A-Z, 0-9, _)"
            )));
        }
    }
    for (key, var) in &patch.set {
        let sealed = server.secrets.seal(var.value.as_bytes());
        server.store.set_env(&name, key, &sealed, var.sensitive)?;
    }
    for key in &patch.unset {
        server.store.delete_env(&name, key)?;
    }
    if let Err(e) = server.push(&name).await {
        warn!(app = %name, "push failed: {e:#}");
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- logs, metrics, deployments

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default = "default_tail")]
    tail: u32,
    #[serde(default)]
    follow: bool,
}

fn default_tail() -> u32 {
    200
}

async fn logs(
    State(server): S,
    _: User,
    Path(name): Path<String>,
    Query(q): Query<LogsQuery>,
) -> ApiResult<Response> {
    let row = load_app(&server, &name)?;
    let stream = server
        .hub
        .open_logs(&row.spec.node, &name, q.tail.min(100_000), q.follow)
        .await
        .map_err(|e| unavailable(format!("{e:#}")))?;
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(stream));
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response())
}

async fn app_metrics(State(server): S, _: User, Path(name): Path<String>) -> Json<Vec<AppMetric>> {
    let live = server.live.read();
    Json(
        live.apps
            .get(&name)
            .map(|a| a.history.iter().cloned().collect())
            .unwrap_or_default(),
    )
}

async fn list_deployments(
    State(server): S,
    _: User,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<DeploymentView>>> {
    Ok(Json(server.store.list_deployments(&name, 50)?))
}

async fn get_deployment(
    State(server): S,
    _: User,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DeploymentView>> {
    server
        .store
        .get_deployment(id)?
        .map(Json)
        .ok_or_else(|| not_found("no such deployment"))
}

// ---------------------------------------------------------------- export / apply

/// The whole configuration as TOML. Sensitive values are never included.
async fn export(State(server): S, _: User) -> ApiResult<Response> {
    let mut manifest = Manifest::default();
    for node in server.store.list_nodes()? {
        manifest.nodes.push(NodeDecl { name: node.name });
    }
    for app in server.store.list_apps()? {
        let mut env = BTreeMap::new();
        for var in server.store.list_env(&app.spec.name)? {
            let entry = if var.sensitive {
                EnvEntry::Detailed {
                    value: None,
                    sensitive: true,
                }
            } else {
                EnvEntry::Plain(server.secrets.open_string(&var.sealed)?)
            };
            env.insert(var.key, entry);
        }
        manifest.apps.push(AppManifest {
            spec: app.spec,
            env,
        });
    }
    Ok((
        [(header::CONTENT_TYPE, "application/toml")],
        manifest.to_toml(),
    )
        .into_response())
}

/// Creates or updates everything in a manifest. Nothing is deleted; apps already
/// running get the new config, new apps are created but not deployed.
async fn apply(State(server): S, _: User, body: String) -> ApiResult<Json<ApplyReport>> {
    let manifest = Manifest::parse(&body).map_err(bad_request)?;
    let mut report = ApplyReport {
        created: vec![],
        updated: vec![],
        unchanged: vec![],
        missing_secrets: vec![],
        new_nodes: vec![],
        missing_uploads: vec![],
    };

    for node in &manifest.nodes {
        validate_name("node name", &node.name).map_err(bad_request)?;
        if !server.store.node_exists(&node.name)? {
            let join_token = server.issue_join_token(&node.name).map_err(bad_request)?;
            report.new_nodes.push(CreatedNode {
                name: node.name.clone(),
                join_token,
            });
        }
    }
    // Validate everything before changing anything.
    {
        let mut seen = std::collections::HashMap::new();
        for app in &manifest.apps {
            for domain in &app.spec.domains {
                if let Some(other) = seen.insert(domain.clone(), app.spec.name.clone()) {
                    return Err(conflict(format!(
                        "{domain} is claimed by both {other} and {}",
                        app.spec.name
                    )));
                }
            }
        }
        let names: std::collections::HashSet<&str> =
            manifest.apps.iter().map(|a| a.spec.name.as_str()).collect();
        for app in &manifest.apps {
            check_spec_for_apply(&server, &app.spec, &names)?;
            for key in app.env.keys() {
                if !valid_env_key(key) {
                    return Err(bad_request(format!(
                        "{}: invalid variable `{key}`",
                        app.spec.name
                    )));
                }
            }
        }
    }

    for app in manifest.apps {
        let name = app.spec.name.clone();
        if let facade_core::Source::Executable {
            sha256, filename, ..
        } = &app.spec.source
            && !super::artifacts::exists(&server, sha256)
        {
            report.missing_uploads.push(format!("{name}: {filename}"));
        }
        let existing = server.store.get_app(&name)?;
        let current_env = server.store.list_env(&name)?;
        let mut changed = existing.as_ref().is_none_or(|e| e.spec != app.spec);

        for (key, entry) in &app.env {
            let (value, sensitive) = entry.resolve();
            let current = current_env.iter().find(|e| e.key == *key);
            match (value, current) {
                (Some(value), current) => {
                    let same = current.is_some_and(|c| {
                        c.sensitive == sensitive
                            && server.secrets.open(&c.sealed).ok().as_deref()
                                == Some(value.as_bytes())
                    });
                    if !same {
                        if existing.is_none() {
                            server.store.put_app(&app.spec)?;
                        }
                        server.store.set_env(
                            &name,
                            key,
                            &server.secrets.seal(value.as_bytes()),
                            sensitive,
                        )?;
                        changed = true;
                    }
                }
                (None, Some(current)) => {
                    if current.sensitive != sensitive {
                        server.store.set_env_sensitivity(&name, key, sensitive)?;
                    }
                }
                (None, None) => report.missing_secrets.push(format!("{name}: {key}")),
            }
        }

        match existing {
            None => {
                server.store.put_app(&app.spec)?;
                server.refresh_routes()?;
                report.created.push(name);
            }
            Some(old) if changed => {
                save_spec(&server, &old.spec, &app.spec).await?;
                report.updated.push(name);
            }
            Some(_) => report.unchanged.push(name),
        }
    }
    Ok(Json(report))
}

/// Like `check_spec`, but domains may move between apps of the same manifest.
fn check_spec_for_apply(
    server: &Server,
    spec: &AppSpec,
    manifest_apps: &std::collections::HashSet<&str>,
) -> ApiResult<()> {
    spec.validate()
        .map_err(|e| bad_request(format!("{}: {e}", spec.name)))?;
    if !server.store.node_exists(&spec.node)? {
        return Err(bad_request(format!(
            "{}: no node named {}",
            spec.name, spec.node
        )));
    }
    let routes = server.routes.read();
    for domain in &spec.domains {
        if server.cfg.api_domain.as_deref() == Some(domain.as_str()) {
            return Err(conflict(format!("{domain} is the API domain")));
        }
        if let Some(owner) = routes.domains.get(domain)
            && *owner != spec.name
            && !manifest_apps.contains(owner.as_str())
        {
            return Err(conflict(format!("{domain} is already used by {owner}")));
        }
    }
    Ok(())
}
