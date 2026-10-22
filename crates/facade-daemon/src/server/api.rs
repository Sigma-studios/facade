//! The HTTP API used by the CLI and the web UI. Served on `api_listen` with the
//! server's pinned certificate, and on `api_domain` through the ingress.

use std::collections::{BTreeMap, HashMap};
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
use facade_core::proto::{AgentRequest, AppState, Listener};
use facade_core::{AppSpec, PortMap, validate_name};
use serde::Deserialize;
use tokio::net::TcpListener;
use tracing::{info, warn};
use uuid::Uuid;

use super::users::{can_see_app, check_caps, check_member_spec, load_app_for, visible_apps};
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
        .route(
            "/api/server-settings",
            get(server_settings).put(update_server_settings),
        )
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
        .route("/api/nodes/{name}/prune", post(prune_node))
        .route("/api/nodes/{name}/ports", get(node_ports))
        .route(
            "/api/nodes/{node}/volumes/{volume}",
            axum::routing::delete(delete_volume),
        )
        .route("/api/volumes", get(list_volumes))
        .route("/api/dns", get(dns_settings).put(update_dns))
        .route("/api/dns/zones", get(dns_zones))
        .route("/api/domains", get(domains))
        .route("/api/apps", get(list_apps).post(create_app))
        .route(
            "/api/apps/{name}",
            get(get_app).put(update_app).delete(delete_app),
        )
        .route("/api/apps/{name}/deploy", post(deploy_app))
        .route("/api/apps/{name}/updates", post(check_updates))
        .route(
            "/api/apps/{name}/volumes/{volume}/archive",
            get(export_volume)
                .put(import_volume)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/api/apps/{name}/owner", put(super::users::set_owner))
        .route(
            "/api/apps/{name}/access/{user}",
            put(super::users::grant_access).delete(super::users::revoke_access),
        )
        .route("/api/apps/{name}/restart", post(restart_app))
        .route("/api/apps/{name}/start", post(start_app))
        .route("/api/apps/{name}/stop", post(stop_app))
        .route("/api/apps/{name}/env", get(get_env).patch(patch_env))
        .route("/api/apps/{name}/logs", get(logs))
        .route("/api/apps/{name}/metrics", get(app_metrics))
        .route("/api/apps/{name}/usage", get(app_usage))
        .route("/api/apps/{name}/deployments", get(list_deployments))
        .route("/api/deployments/{id}", get(get_deployment))
        .route("/api/deployments/{id}/cancel", post(cancel_deployment))
        .route("/api/builds", get(builds))
        .route("/api/databases", post(create_database))
        .route(
            "/api/stacks",
            get(super::stack::list).post(super::stack::create),
        )
        .route("/api/stacks/plan", post(super::stack::plan))
        .route(
            "/api/stacks/{name}",
            get(super::stack::get)
                .put(super::stack::update)
                .delete(super::stack::delete),
        )
        .route("/api/stacks/{name}/deploy", post(super::stack::deploy))
        .route(
            "/api/stacks/{name}/deployments",
            get(super::stack::deployments),
        )
        .route(
            "/api/stacks/{name}/env",
            get(super::stack::get_env).patch(super::stack::patch_env),
        )
        .route(
            "/api/stack-deployments/{id}",
            get(super::stack::get_deployment),
        )
        .route(
            "/api/stack-deployments/{id}/cancel",
            post(super::stack::cancel),
        )
        .route(
            "/api/git/credentials",
            get(super::git::list).post(super::git::create),
        )
        .route(
            "/api/git/credentials/{name}",
            axum::routing::delete(super::git::delete),
        )
        .route("/api/registries", get(list_registry_logins))
        .route(
            "/api/registries/{host}",
            put(set_registry_login).delete(delete_registry_login),
        )
        .route("/api/backups", get(list_backups).post(run_backups))
        .route("/api/backups/{app}/{volume}/{id}", get(download_backup))
        .route(
            "/api/backups/{app}/{volume}/{id}/restore",
            post(restore_backup),
        )
        .route("/api/server-backup", get(server_backup))
        .route(
            "/api/users",
            get(super::users::list_users).post(super::users::create_user),
        )
        .route(
            "/api/users/{name}",
            put(super::users::update_user).delete(super::users::delete_user),
        )
        .route(
            "/api/users/{name}/password",
            post(super::users::reset_password),
        )
        .route("/api/export", get(export))
        .route("/api/apply", post(apply))
        .route(
            "/api/artifacts",
            post(super::artifacts::upload).layer(axum::extract::DefaultBodyLimit::max(
                super::artifacts::MAX_UPLOAD,
            )),
        )
        .route("/api/inspect", post(super::inspect::inspect))
        .route("/api/health", get(health))
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
    let app = router(server.clone());
    loop {
        let (tcp, peer) = listener.accept().await?;
        // Once the API domain works, the direct port can be kept for SSH tunnels.
        if server.settings.get().direct_api_local_only && !peer.ip().is_loopback() {
            continue;
        }
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

/// Creates the first admin with a random password, saved next to the database
/// (readable by root only, never logged) until it is changed.
pub fn ensure_admin(server: &Server) -> anyhow::Result<()> {
    if server.store.user_count()? > 0 {
        // Installs from before the flag existed: still on the generated password?
        if let Ok(generated) = std::fs::read_to_string(initial_password_file(server))
            && let Some(admin) = server.store.user_by_name("admin")?
            && verify_password(generated.trim(), &admin.password_hash)
        {
            server.store.flag_password_change(admin.id)?;
        }
        return Ok(());
    }
    let password = random_token()[..24].to_string();
    server
        .store
        .create_user("admin", &hash_password(&password)?, "admin", true)?;
    let path = initial_password_file(server);
    tls::write_private(&path, format!("{password}\n").as_bytes())?;
    warn!(
        "created user `admin`; its password is in {} until it is changed",
        path.display()
    );
    Ok(())
}

fn initial_password_file(server: &Server) -> std::path::PathBuf {
    server.cfg.data_dir.join("initial-admin-password")
}

pub(super) fn hash_password(password: &str) -> anyhow::Result<String> {
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

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

impl ApiError {
    pub fn bad_request(msg: impl ToString) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.to_string())
    }
    pub fn not_found(msg: impl ToString) -> Self {
        ApiError(StatusCode::NOT_FOUND, msg.to_string())
    }
    pub fn conflict(msg: impl ToString) -> Self {
        ApiError(StatusCode::CONFLICT, msg.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

pub(super) fn bad_request(msg: impl ToString) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.to_string())
}
pub(super) fn not_found(msg: impl ToString) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.to_string())
}
pub(super) fn conflict(msg: impl ToString) -> ApiError {
    ApiError(StatusCode::CONFLICT, msg.to_string())
}
pub(super) fn unavailable(msg: impl ToString) -> ApiError {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, msg.to_string())
}

pub(super) type ApiResult<T> = Result<T, ApiError>;

/// An authenticated caller. What they may do: see `users`.
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

/// `ok` once the server's own agent is connected (so it serves and converges);
/// 503 before that, e.g. just after a restart.
async fn health(State(server): S) -> (StatusCode, &'static str) {
    if server.hub.is_connected(&server.cfg.local_node) {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "starting")
    }
}

async fn whoami(State(server): S, user: User) -> ApiResult<Json<WhoAmI>> {
    let row = server
        .store
        .user_by_id(user.id)?
        .ok_or_else(|| not_found("no such user"))?;
    let (memory, cpus) = super::users::allocated(&server, user.id, None)?;
    Ok(Json(WhoAmI {
        must_change_password: row.must_change_password,
        username: user.username,
        role: user.role,
        memory_cap_mb: row.memory_cap_mb,
        cpu_cap: row.cpu_cap,
        memory_allocated_mb: memory,
        cpus_allocated: cpus,
    }))
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
    // The generated password is no longer valid: no reason to keep it around.
    let _ = std::fs::remove_file(initial_password_file(&server));
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- server settings

fn settings_view(server: &Server) -> anyhow::Result<ServerSettings> {
    let settings = server.settings.get();
    let api_domain_ready = match &settings.api_domain {
        Some(domain) => {
            let name = super::acme::covering_name(server, domain);
            let now = facade_core::now();
            server
                .store
                .list_certs()?
                .iter()
                .any(|c| c.domain == name && c.not_after > now)
        }
        None => false,
    };
    Ok(ServerSettings {
        api_domain: settings.api_domain,
        api_domain_ready,
        acme_email: settings.acme_email,
        api_listen: server.cfg.api_listen.to_string(),
        direct_api_local_only: settings.direct_api_local_only,
    })
}

async fn server_settings(State(server): S, user: User) -> ApiResult<Json<ServerSettings>> {
    user.require_admin()?;
    Ok(Json(settings_view(&server)?))
}

async fn update_server_settings(
    State(server): S,
    user: User,
    Json(update): Json<ServerSettingsUpdate>,
) -> ApiResult<Json<ServerSettings>> {
    user.require_admin()?;
    let clean = |v: String| {
        let v = v.trim().to_lowercase();
        (!v.is_empty()).then_some(v)
    };
    let domain = update.api_domain.map(clean);
    let email = update.acme_email.map(clean);
    if let Some(Some(domain)) = &domain {
        facade_core::validate_domain(domain).map_err(bad_request)?;
        if let Some(app) = server.routes.read().domains.get(domain) {
            return Err(conflict(format!("{domain} is used by app {app}")));
        }
    }
    if let Some(Some(email)) = &email
        && (!email.contains('@') || email.contains(char::is_whitespace))
    {
        return Err(bad_request(format!("`{email}` is not an email address")));
    }
    if update.direct_api_local_only == Some(true) {
        // Checked against the domain as it will be after this update.
        let domain = match &domain {
            Some(d) => d.clone(),
            None => server.settings.api_domain(),
        };
        let current = settings_view(&server)?;
        let ready = domain.is_some() && domain == current.api_domain && current.api_domain_ready;
        if !ready {
            return Err(conflict(
                "set an API domain and wait for its certificate first: \
                 otherwise the API is only reachable through an SSH tunnel",
            ));
        }
    }
    if let Some(domain) = domain {
        server.settings.set_api_domain(&server.store, domain)?;
        server.domains_wake.notify_one();
        super::acme::push_ingress_all(&server).await;
    }
    if let Some(email) = email {
        server.settings.set_acme_email(&server.store, email)?;
        if let Err(e) = super::acme::update_contact(&server).await {
            warn!("{e:#}");
        }
    }
    if let Some(local_only) = update.direct_api_local_only {
        server
            .settings
            .set_direct_api_local_only(&server.store, local_only)?;
    }
    Ok(Json(settings_view(&server)?))
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
                storage: live.nodes.get(&n.name).and_then(|l| l.storage.clone()),
                ingress_errors: live
                    .nodes
                    .get(&n.name)
                    .filter(|_| conn.is_some())
                    .map(|l| l.ingress_errors.clone())
                    .unwrap_or_default(),
                name: n.name,
            }
        })
        .collect())
}

pub fn app_view(
    server: &Server,
    spec: AppSpec,
    revision: u64,
    commit: Option<String>,
    owner: Option<i64>,
) -> AppView {
    let username = |id: i64| {
        server
            .store
            .user_by_id(id)
            .ok()
            .flatten()
            .map(|u| u.username)
    };
    let owner = owner.and_then(username);
    let access = server
        .store
        .app_access(&spec.name)
        .unwrap_or_default()
        .into_iter()
        .filter_map(username)
        .collect();
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
        commit,
        owner,
        access,
        spec,
    }
}

fn app_views(server: &Server, user: &User) -> anyhow::Result<Vec<AppView>> {
    Ok(visible_apps(server, user)?
        .into_iter()
        .map(|a| app_view(server, a.spec, a.revision, a.commit, a.owner))
        .collect())
}

async fn status(State(server): S, user: User) -> ApiResult<Json<StatusView>> {
    Ok(Json(StatusView {
        nodes: node_views(&server)?,
        apps: app_views(&server, &user)?,
    }))
}

/// Members see the alerts of nodes and of their own apps.
async fn alerts(State(server): S, user: User) -> Json<Vec<AlertView>> {
    let all = server.alerts.lock().views();
    Json(
        all.into_iter()
            .filter(|a| {
                let app = a
                    .key
                    .strip_prefix("app:")
                    .and_then(|k| k.split(':').next())
                    .or_else(|| {
                        a.key
                            .strip_prefix("backup:")
                            .and_then(|k| k.split('/').next())
                    });
                match app {
                    Some(app) => can_see_app(&server, &user, app),
                    None => user.is_admin() || a.key.starts_with("node:"),
                }
            })
            .collect(),
    )
}

async fn notify_settings(State(server): S, user: User) -> ApiResult<Json<NotifySettings>> {
    user.require_admin()?;
    Ok(Json(server.notifier.settings()))
}

async fn update_notify(
    State(server): S,
    user: User,
    Json(update): Json<NotifyUpdate>,
) -> ApiResult<Json<NotifySettings>> {
    user.require_admin()?;
    server
        .notifier
        .update(&server.store, &server.secrets, update)
        .map_err(bad_request)?;
    Ok(Json(server.notifier.settings()))
}

async fn test_alert(State(server): S, user: User) -> ApiResult<StatusCode> {
    user.require_admin()?;
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
    user: User,
    Json(req): Json<CreateNode>,
) -> ApiResult<(StatusCode, Json<CreatedNode>)> {
    user.require_admin()?;
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
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<CreatedNode>> {
    user.require_admin()?;
    if !server.store.node_exists(&name)? {
        return Err(not_found(format!("no node named {name}")));
    }
    if name == server.cfg.local_node {
        return Err(bad_request("the server's own node needs no token"));
    }
    let join_token = server.issue_join_token(&name).map_err(bad_request)?;
    Ok(Json(CreatedNode { name, join_token }))
}

async fn delete_node(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    user.require_admin()?;
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
    user: User,
    Path(name): Path<String>,
    Json(req): Json<SetNodeIp>,
) -> ApiResult<StatusCode> {
    user.require_admin()?;
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

async fn dns_settings(State(server): S, user: User) -> ApiResult<Json<DnsSettings>> {
    user.require_admin()?;
    Ok(Json(server.dns.settings()))
}

/// Domains the DNS provider manages, offered when choosing an app's domains.
async fn dns_zones(State(server): S, _user: User) -> Json<Vec<String>> {
    Json(server.dns.settings().zones)
}

async fn update_dns(
    State(server): S,
    user: User,
    Json(update): Json<DnsUpdate>,
) -> ApiResult<Json<DnsSettings>> {
    user.require_admin()?;
    super::dns::update(&server, update)
        .await
        .map(Json)
        .map_err(bad_request)
}

async fn domains(State(server): S, user: User) -> Json<Vec<DomainStatus>> {
    let all = super::dns::statuses(&server).await;
    Json(
        all.into_iter()
            .filter(|d| match &d.app {
                Some(app) => can_see_app(&server, &user, app),
                None => user.is_admin(),
            })
            .collect(),
    )
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

pub(super) fn load_app(server: &Server, name: &str) -> ApiResult<super::store::AppRow> {
    server
        .store
        .get_app(name)?
        .ok_or_else(|| not_found(format!("no app named {name}")))
}

/// Links only work between apps on the same node, to apps with a private network.
/// `apps` is every app as it will be once the change is made.
fn check_links(spec: &AppSpec, apps: &HashMap<String, AppSpec>) -> ApiResult<()> {
    for link in &spec.links {
        let Some(target) = apps.get(&link.app) else {
            return Err(bad_request(format!(
                "{}: links to {}, which does not exist",
                spec.name, link.app
            )));
        };
        if target.node != spec.node {
            return Err(bad_request(format!(
                "{} runs on {} and {} on {}: links only work between apps on the same node",
                spec.name, spec.node, target.name, target.node
            )));
        }
        if target.host_network || target.is_static() {
            return Err(bad_request(format!(
                "{} has no private network to link to ({})",
                target.name,
                if target.is_static() {
                    "static site"
                } else {
                    "it uses the node's network"
                }
            )));
        }
    }
    // Apps linking to this one must stay with it.
    for other in apps.values() {
        if other.name != spec.name
            && other.links.iter().any(|l| l.app == spec.name)
            && (other.node != spec.node || spec.host_network || spec.is_static())
        {
            return Err(bad_request(format!(
                "{} links to {}: move them together (links only work on one node, to apps with a private network)",
                other.name, spec.name
            )));
        }
    }
    Ok(())
}

/// Every app as it would be with `changed` applied.
fn apps_with(server: &Server, changed: &[&AppSpec]) -> ApiResult<HashMap<String, AppSpec>> {
    let mut apps: HashMap<String, AppSpec> = server
        .store
        .list_apps()?
        .into_iter()
        .map(|a| (a.spec.name.clone(), a.spec))
        .collect();
    for spec in changed {
        apps.insert(spec.name.clone(), (*spec).clone());
    }
    Ok(apps)
}

/// Checks that apply to a spec in the context of the whole installation.
fn check_spec(server: &Server, spec: &AppSpec) -> ApiResult<()> {
    spec.validate().map_err(bad_request)?;
    check_links(spec, &apps_with(server, &[spec])?)?;
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
    {
        let routes = server.routes.read();
        for domain in spec.domains.iter().chain(&spec.redirects) {
            if server.settings.api_domain().as_deref() == Some(domain.as_str()) {
                return Err(conflict(format!("{domain} is the API domain")));
            }
            if let Some(owner) = routes.domains.get(domain)
                && *owner != spec.name
            {
                return Err(conflict(format!("{domain} is already used by {owner}")));
            }
        }
    }
    // A deployed, enabled app restarts with the change: the ports must be free now.
    let starts = spec.enabled
        && server
            .store
            .get_app(&spec.name)?
            .is_some_and(|row| row.revision > 0);
    if let Some(problem) = port_conflict(server, spec, starts)? {
        return Err(conflict(problem));
    }
    Ok(())
}

/// Why `spec` cannot have its ports on its node: another app of the node declares
/// them, or (with `listening`) something is listening on them right now. A port
/// held by the app itself is fine.
fn port_conflict(
    server: &Server,
    spec: &AppSpec,
    listening: bool,
) -> anyhow::Result<Option<String>> {
    let wanted = spec.host_ports();
    // The entry point listens on TLS ports itself.
    let tls = spec.ingress_ports();
    if wanted.is_empty() && tls.is_empty() {
        return Ok(None);
    }
    for other in server.store.list_apps()? {
        let other = other.spec;
        if other.name == spec.name || other.node != spec.node {
            continue;
        }
        let theirs: Vec<PortMap> = other
            .host_ports()
            .into_iter()
            .chain(other.ingress_ports())
            .collect();
        for a in wanted.iter().chain(&tls) {
            if let Some(b) = theirs
                .iter()
                .find(|b| a.protocol == b.protocol && a.overlaps(b))
            {
                return Ok(Some(format!(
                    "{a} overlaps {b}, which app {} uses on node {}",
                    other.name, spec.node
                )));
            }
        }
    }
    if !listening {
        return Ok(None);
    }
    let live = server.live.read();
    let Some(node) = live.nodes.get(&spec.node) else {
        return Ok(None);
    };
    for l in &node.listeners {
        if l.app.as_deref() == Some(spec.name.as_str()) {
            continue;
        }
        let entry_point =
            l.app.as_deref() == Some("facade") || l.process.as_deref() == Some("facaded");
        if wanted
            .iter()
            .chain(tls.iter().filter(|_| !entry_point))
            .any(|w| w.protocol == l.protocol && w.contains(l.port))
        {
            return Ok(Some(format!(
                "port {}/{} is in use on node {} by {} (listening on {}); stop it first, or see `facade node ports {}`",
                l.port,
                l.protocol,
                spec.node,
                holder(l),
                l.address,
                spec.node
            )));
        }
    }
    Ok(None)
}

fn holder(l: &Listener) -> String {
    match (&l.app, &l.process, l.pid) {
        (Some(app), _, _) if app == "facade" => "facade itself".into(),
        (Some(app), _, _) => format!("app {app}"),
        (None, Some(process), Some(pid)) => format!("{process} (pid {pid})"),
        (None, Some(process), None) => process.clone(),
        _ => "another program".into(),
    }
}

/// Before (re)starting an app by hand: its ports must be free, as the node sees
/// them now (an old program stopped a second ago no longer counts).
async fn check_ports_free(server: &Server, spec: &AppSpec) -> ApiResult<()> {
    if !(spec.host_ports().is_empty() && spec.tls_ports.is_empty())
        && let Ok(listeners) = server
            .hub
            .query::<Vec<Listener>>(&spec.node, AgentRequest::Ports)
            .await
    {
        server
            .live
            .write()
            .nodes
            .entry(spec.node.clone())
            .or_default()
            .listeners = listeners;
    }
    match port_conflict(server, spec, true)? {
        Some(problem) => Err(conflict(problem)),
        None => Ok(()),
    }
}

async fn list_apps(State(server): S, user: User) -> ApiResult<Json<Vec<AppView>>> {
    Ok(Json(app_views(&server, &user)?))
}

async fn get_app(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<AppView>> {
    let row = load_app_for(&server, &user, &name)?;
    Ok(Json(app_view(
        &server,
        row.spec,
        row.revision,
        row.commit,
        row.owner,
    )))
}

async fn create_app(
    State(server): S,
    user: User,
    Json(spec): Json<AppSpec>,
) -> ApiResult<(StatusCode, Json<AppView>)> {
    if spec.stack.is_some() || spec.name.contains('.') {
        return Err(bad_request(
            "the services of a stack are created by the stack (`facade stack`)",
        ));
    }
    if server.store.get_app(&spec.name)?.is_some() {
        return Err(conflict(format!("app {} exists", spec.name)));
    }
    if server.store.get_stack(&spec.name)?.is_some() {
        return Err(conflict(format!("{} is the name of a stack", spec.name)));
    }
    // A member's apps are theirs: their cap, their logins.
    let owner = (!user.is_admin()).then_some(user.id);
    check_member_spec(&user, None, &spec).await?;
    check_member_links(&server, &user, &spec)?;
    check_caps(&server, owner, &spec)?;
    check_spec(&server, &spec)?;
    server.store.put_app(&spec)?;
    server.store.set_app_owner(&spec.name, owner)?;
    server.refresh_routes()?;
    push_link_targets(&server, None, &spec).await;
    // Created, not deployed: env vars usually come first, then `facade deploy`.
    Ok((
        StatusCode::CREATED,
        Json(app_view(&server, spec, 0, None, owner)),
    ))
}

#[derive(Deserialize)]
struct UpdateQuery {
    /// Copy the app's volumes when it moves to another node.
    #[serde(default)]
    move_data: bool,
}

async fn update_app(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Query(q): Query<UpdateQuery>,
    Json(spec): Json<AppSpec>,
) -> ApiResult<Json<AppView>> {
    if spec.name != name {
        return Err(bad_request("apps cannot be renamed"));
    }
    let old = load_app_for(&server, &user, &name)?;
    managed_by_stack(&old.spec)?;
    if spec.stack.is_some() {
        return Err(bad_request("apps cannot join a stack"));
    }
    check_member_spec(&user, Some(&old.spec), &spec).await?;
    check_member_links(&server, &user, &spec)?;
    check_caps(&server, old.owner, &spec)?;
    check_spec(&server, &spec)?;
    save_spec(&server, &old.spec, &spec, q.move_data).await?;
    Ok(Json(app_view(
        &server,
        spec,
        old.revision,
        old.commit,
        old.owner,
    )))
}

/// A member's app can only link to apps they may manage: a link reaches the
/// other app's private network.
fn check_member_links(server: &Server, user: &User, spec: &AppSpec) -> ApiResult<()> {
    for link in &spec.links {
        if !can_see_app(server, user, &link.app) {
            return Err(super::users::forbidden(format!(
                "{} is not one of your apps: you can only link to apps you manage",
                link.app
            )));
        }
    }
    Ok(())
}

/// A stack's services are changed through their stack, and deployed with it.
fn managed_by_stack(spec: &AppSpec) -> ApiResult<()> {
    match (&spec.stack, spec.service()) {
        (Some(stack), Some(service)) => Err(conflict(format!(
            "{} is service {service} of stack {}: change it in the stack (`facade stack service update {} {service} …`, or its compose file) and deploy the stack",
            spec.name, stack.stack, stack.stack
        ))),
        _ => Ok(()),
    }
}

/// Stores a changed spec and moves the app between nodes if needed. An app with
/// volumes only moves with `move_data`: its data is copied to the new node first,
/// and deleted from the old one once the copy succeeded.
async fn save_spec(
    server: &Server,
    old: &AppSpec,
    new: &AppSpec,
    move_data: bool,
) -> ApiResult<()> {
    let moving = old.node != new.node;
    let copied = moving && !old.volumes.is_empty();
    if copied {
        if !move_data {
            return Err(conflict(format!(
                "{} keeps data in volumes on {}: move it with --move-data (copies the volumes to {}, then deletes them from {})",
                new.name, old.node, new.node, old.node
            )));
        }
        move_volumes(server, old, new).await?;
    }
    server.store.put_app(new)?;
    server.refresh_routes()?;
    if moving && !copied && server.hub.is_connected(&old.node) {
        server
            .hub
            .request(
                &old.node,
                AgentRequest::Remove {
                    app: new.name.clone(),
                    volumes: false,
                },
            )
            .await
            .map_err(|e| unavailable(format!("removing from {}: {e:#}", old.node)))?;
    }
    if let Err(e) = server.push(&new.name).await {
        warn!(app = %new.name, "push failed: {e:#}");
    }
    push_link_targets(server, Some(old), new).await;
    Ok(())
}

/// Stops the app on its old node and copies each volume it keeps to the new one.
/// On failure the app is started again where it was.
async fn move_volumes(server: &Server, old: &AppSpec, new: &AppSpec) -> ApiResult<()> {
    for node in [&old.node, &new.node] {
        if !server.hub.is_connected(node) {
            return Err(unavailable(format!(
                "node {node} is offline: both nodes must be connected to move data"
            )));
        }
    }
    let app = &new.name;
    server
        .hub
        .request(
            &old.node,
            AgentRequest::Remove {
                app: app.clone(),
                volumes: false,
            },
        )
        .await
        .map_err(|e| unavailable(format!("stopping {app} on {}: {e:#}", old.node)))?;
    let result: anyhow::Result<()> = async {
        for volume in old
            .volumes
            .iter()
            .filter(|v| new.volumes.iter().any(|n| n.name == v.name))
        {
            info!(%app, volume = %volume.name, from = %old.node, to = %new.node, "copying volume");
            let tmp = scratch_path(server)?;
            let copied = async {
                let mut stream = server
                    .hub
                    .open_volume_export(&old.node, app, &old.volume_name(volume))
                    .await?;
                let mut file = tls::private_file(&tmp).await?;
                tokio::io::copy(&mut stream, &mut file).await?;
                file.sync_all().await?;
                server
                    .hub
                    .import_volume(
                        &new.node,
                        app,
                        &new.volume_name(volume),
                        Some(new_volume(new, volume)),
                        &tmp,
                    )
                    .await
            }
            .await;
            let _ = tokio::fs::remove_file(&tmp).await;
            copied.with_context(|| format!("copying volume {}", volume.name))?;
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        // Back where it was, untouched.
        if let Err(again) = server.push(app).await {
            warn!(%app, "restarting on {} after a failed move: {again:#}", old.node);
        }
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            format!("{e:#}; {app} stays on {}", old.node),
        ));
    }
    if let Err(e) = server
        .hub
        .request(
            &old.node,
            AgentRequest::Remove {
                app: app.clone(),
                volumes: true,
            },
        )
        .await
    {
        warn!(%app, "deleting the moved volumes from {}: {e:#}", old.node);
    }
    Ok(())
}

/// A fresh temporary file path on the server.
fn scratch_path(server: &Server) -> anyhow::Result<std::path::PathBuf> {
    let dir = server.cfg.data_dir.join("tmp");
    tls::create_private_dir(&dir)?;
    Ok(dir.join(Uuid::new_v4().to_string()))
}

/// How an agent creates `volume` of `spec` if it does not exist yet.
fn new_volume(spec: &AppSpec, volume: &facade_core::Volume) -> facade_core::proto::NewVolume {
    facade_core::proto::NewVolume {
        name: volume.name.clone(),
        stack: spec.stack.as_ref().map(|s| s.stack.clone()),
    }
}

/// Apps linked to (before or after a change) learn the names they are reached by.
async fn push_link_targets(server: &Server, old: Option<&AppSpec>, new: &AppSpec) {
    let mut targets: Vec<&str> = new.links.iter().map(|l| l.app.as_str()).collect();
    targets.extend(
        old.into_iter()
            .flat_map(|o| o.links.iter().map(|l| l.app.as_str())),
    );
    targets.sort();
    targets.dedup();
    for target in targets {
        if let Err(e) = server.push(target).await {
            warn!(app = %target, "push failed: {e:#}");
        }
    }
}

#[derive(Deserialize)]
struct DeleteQuery {
    /// Delete the app's volumes (its data) too.
    #[serde(default)]
    volumes: bool,
}

async fn delete_app(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<StatusCode> {
    let row = load_app_for(&server, &user, &name)?;
    if let Some(stack) = &row.spec.stack {
        return Err(conflict(format!(
            "{name} is a service of stack {}: remove it from the stack (or its file) and deploy, or delete the stack",
            stack.stack
        )));
    }
    let linked: Vec<String> = server
        .store
        .list_apps()?
        .into_iter()
        .filter(|a| a.spec.links.iter().any(|l| l.app == name))
        .map(|a| a.spec.name)
        .collect();
    if !linked.is_empty() {
        return Err(conflict(format!(
            "{} link to {name}: remove those links first",
            linked.join(", ")
        )));
    }
    let dependents = server.dependents(&name)?;
    if !dependents.is_empty() {
        return Err(conflict(format!(
            "variables of {} refer to {name}'s: change them first",
            dependents.join(", ")
        )));
    }
    if server.hub.is_connected(&row.spec.node) {
        server
            .hub
            .request(
                &row.spec.node,
                AgentRequest::Remove {
                    app: name.clone(),
                    volumes: q.volumes,
                },
            )
            .await
            .map_err(|e| unavailable(format!("{e:#}")))?;
    } else if q.volumes && !row.spec.volumes.is_empty() {
        return Err(unavailable(format!(
            "node {} is offline: its volumes cannot be deleted now",
            row.spec.node
        )));
    }
    // An offline node drops the app when it reconnects and syncs.
    super::updates::forget(&server, &name).await;
    server.store.delete_app(&name)?;
    server.live.write().apps.remove(&name);
    server.refresh_routes()?;
    Ok(StatusCode::NO_CONTENT)
}

async fn deploy_app(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<DeploymentView>)> {
    let req: DeployRequest = if body.iter().all(u8::is_ascii_whitespace) {
        DeployRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| bad_request(format!("invalid body: {e}")))?
    };
    let row = load_app_for(&server, &user, &name)?;
    if let Some(stack) = row.spec.stack {
        return Err(conflict(format!(
            "{name} is deployed with its stack: facade deploy {}",
            stack.stack
        )));
    }
    let view = start_deploy(&server, &name, &req).await?;
    Ok((StatusCode::ACCEPTED, Json(view)))
}

/// Resolves the commit, records a deployment and hands it to the app's node, where
/// it waits for the build slot.
async fn start_deploy(
    server: &Server,
    name: &str,
    req: &DeployRequest,
) -> ApiResult<DeploymentView> {
    let row = load_app(server, name)?;
    if !server.hub.is_connected(&row.spec.node) {
        return Err(unavailable(format!("node {} is offline", row.spec.node)));
    }
    // Another app declaring the ports is a mistake; another program holding
    // them is usually the old version being replaced: the agent builds first,
    // then waits for them (see the deployment log).
    if let Some(problem) = port_conflict(server, &row.spec, false)? {
        return Err(conflict(problem));
    }
    // Deploying (re)starts it.
    {
        let mut running = row.spec.clone();
        running.enabled = true;
        check_caps(server, row.owner, &running)?;
    }
    let owner = row.owner.unwrap_or(super::store::ADMINS);
    let commit = super::git::resolve_commit(server, owner, &row.spec.source, req.commit.as_deref())
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    // Check the variables resolve before anything changes.
    server
        .workload(name)
        .map_err(|e| bad_request(format!("{e:#}")))?;
    let revision = server.store.bump_revision(name, commit.as_deref())?;
    let id = Uuid::new_v4();
    server
        .store
        .create_deployment(id, name, revision, commit.as_deref())?;
    // A commit built before is not checked out again: its subject is known already.
    if let Some(commit) = &commit
        && let Some(title) = server.store.commit_title(commit)?
    {
        server.store.set_deployment_commit(id, commit, &title)?;
    }
    let mut workload = server.workload(name)?;
    workload.spec.enabled = true;
    if !row.spec.enabled {
        let mut spec = row.spec.clone();
        spec.enabled = true;
        server.store.put_app(&spec)?;
    }
    server.set_app_state(name, AppState::Pending, None);
    let request = AgentRequest::Apply {
        workload,
        deployment: Some(id),
        rebuild: req.rebuild,
        phase: facade_core::proto::ApplyPhase::All,
    };
    if let Err(e) = server.hub.request(&row.spec.node, request).await {
        server
            .store
            .finish_deployment(id, Some(&format!("{e:#}")), false)?;
        return Err(unavailable(format!("{e:#}")));
    }
    Ok(server
        .store
        .get_deployment(id)?
        .context("deployment vanished")?)
}

async fn cancel_deployment(
    State(server): S,
    user: User,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let d = server
        .store
        .get_deployment(id)?
        .filter(|d| can_see_app(&server, &user, &d.app))
        .ok_or_else(|| not_found("no such deployment"))?;
    if d.status.is_finished() {
        return Err(conflict(format!("the deployment already {}", d.status)));
    }
    let node = server.store.get_app(&d.app)?.map(|row| row.spec.node);
    let online = node.as_deref().is_some_and(|n| server.hub.is_connected(n));
    if let Some(node) = node.filter(|_| online) {
        // The agent stops it and reports it cancelled before answering.
        server
            .hub
            .request(&node, AgentRequest::Cancel { deployment: id })
            .await
            .map_err(|e| unavailable(format!("{e:#}")))?;
    }
    let now = server
        .store
        .get_deployment(id)?
        .ok_or_else(|| not_found("no such deployment"))?;
    match now.status {
        DeploymentStatus::Running if online => {
            return Err(conflict(
                "too late to cancel: the build is done and the new version is starting",
            ));
        }
        DeploymentStatus::Queued | DeploymentStatus::Running => {
            // Never reached the node, or the node is gone.
            server
                .store
                .finish_deployment(id, Some("cancelled"), true)?;
        }
        _ => {}
    }
    if server.store.get_deployment(id)?.map(|d| d.status) == Some(DeploymentStatus::Cancelled) {
        restore_after_cancel(&server, &d).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A cancelled deploy must not come back on the next sync: the app's desired state
/// goes back to its last successful deployment (or to "not deployed").
async fn restore_after_cancel(server: &Server, d: &DeploymentView) -> ApiResult<()> {
    let Some(row) = server.store.get_app(&d.app)? else {
        return Ok(());
    };
    // A later deploy was asked for meanwhile: that one stands.
    if row.revision != d.revision {
        return Ok(());
    }
    let (revision, commit) = server.store.last_success(&d.app)?.unwrap_or((0, None));
    server
        .store
        .set_revision(&d.app, revision, commit.as_deref())?;
    let node = row.spec.node.clone();
    if !server.hub.is_connected(&node) {
        return Ok(());
    }
    let result = if revision == 0 {
        // Never deployed: the node forgets it (it has nothing running for it).
        server
            .hub
            .request(
                &node,
                AgentRequest::Remove {
                    app: d.app.clone(),
                    volumes: false,
                },
            )
            .await
    } else {
        server.push(&d.app).await
    };
    if let Err(e) = result {
        warn!(app = %d.app, "restoring after cancel: {e:#}");
    }
    Ok(())
}

async fn builds(State(server): S, user: User) -> ApiResult<Json<BuildsView>> {
    let mut queue = Vec::new();
    {
        let live = server.live.read();
        let mut nodes: Vec<_> = live.nodes.iter().collect();
        nodes.sort_by(|a, b| a.0.cmp(b.0));
        for (node, l) in nodes {
            let mut position = 0;
            for b in &l.builds {
                let pos = if b.state == facade_core::proto::BuildState::Building {
                    0
                } else {
                    position += 1;
                    position
                };
                queue.push(QueuedBuild {
                    node: node.clone(),
                    app: b.app.clone(),
                    deployment: b.deployment,
                    commit: b.commit.clone(),
                    state: b.state,
                    since: b.since,
                    position: pos,
                });
            }
        }
    }
    // Deployments waiting for an earlier one of the same app are not in the node's
    // queue yet; they come after it.
    let recent = server.store.recent_deployments(40)?;
    let mut waiting: Vec<&DeploymentView> = recent
        .iter()
        .filter(|d| {
            d.status == DeploymentStatus::Queued
                && !queue.iter().any(|q| q.deployment == Some(d.id))
        })
        .collect();
    waiting.sort_by_key(|d| d.created_at);
    for d in waiting {
        let Some(node) = server.routes.read().nodes.get(&d.app).cloned() else {
            continue;
        };
        let position = queue.iter().filter(|q| q.node == node).count();
        queue.push(QueuedBuild {
            node,
            app: d.app.clone(),
            deployment: Some(d.id),
            commit: d.commit.clone(),
            state: facade_core::proto::BuildState::Queued,
            since: d.created_at,
            position: position.max(1),
        });
    }
    // Members see their apps' builds.
    queue.retain(|q| can_see_app(&server, &user, &q.app));
    let recent = recent
        .into_iter()
        .filter(|d| can_see_app(&server, &user, &d.app))
        .collect();
    Ok(Json(BuildsView { queue, recent }))
}

async fn restart_app(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let row = load_app_for(&server, &user, &name)?;
    server
        .hub
        .request(&row.spec.node, AgentRequest::Restart { app: name })
        .await
        .map_err(|e| unavailable(format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_enabled(
    server: &Server,
    user: &User,
    name: &str,
    enabled: bool,
) -> ApiResult<StatusCode> {
    let row = load_app_for(server, user, name)?;
    let mut spec = row.spec.clone();
    spec.enabled = enabled;
    if enabled && !row.spec.enabled {
        check_caps(server, row.owner, &spec)?;
        check_ports_free(server, &spec).await?;
    }
    save_spec(server, &row.spec, &spec, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn start_app(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    set_enabled(&server, &user, &name, true).await
}

async fn stop_app(State(server): S, user: User, Path(name): Path<String>) -> ApiResult<StatusCode> {
    set_enabled(&server, &user, &name, false).await
}

// ---------------------------------------------------------------- env

#[derive(Deserialize)]
struct RevealQuery {
    #[serde(default)]
    reveal: bool,
}

async fn get_env(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Query(q): Query<RevealQuery>,
) -> ApiResult<Json<Vec<EnvView>>> {
    load_app_for(&server, &user, &name)?;
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

pub fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

async fn patch_env(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Json(patch): Json<EnvPatch>,
) -> ApiResult<StatusCode> {
    load_app_for(&server, &user, &name)?;
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
    // Apps whose variables refer to these get the new values too.
    let mut apps = vec![name.clone()];
    apps.extend(server.dependents(&name)?);
    for app in apps {
        if let Err(e) = server.push(&app).await {
            warn!(%app, "push failed: {e:#}");
        }
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
    user: User,
    Path(name): Path<String>,
    Query(q): Query<LogsQuery>,
) -> ApiResult<Response> {
    let row = load_app_for(&server, &user, &name)?;
    let text_response = |message: String| {
        (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            message,
        )
            .into_response()
    };
    if row.revision == 0 {
        return Ok(text_response(format!(
            "[façade] {name} is not deployed yet: its logs appear here after its first deploy\n"
        )));
    }
    if !server.hub.is_connected(&row.spec.node) {
        return Ok(text_response(format!(
            "[façade] node {} is offline: logs are read from the node itself\n",
            row.spec.node
        )));
    }
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

async fn app_metrics(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<AppMetric>>> {
    load_app_for(&server, &user, &name)?;
    let live = server.live.read();
    Ok(Json(
        live.apps
            .get(&name)
            .map(|a| a.history.iter().cloned().collect())
            .unwrap_or_default(),
    ))
}

#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default = "one_hour")]
    range: String,
}

fn one_hour() -> String {
    "1h".into()
}

/// `?range=1h` is every sample the agents sent (kept in memory); `1d`, `7d` and
/// `30d` come from the stored 5-minute buckets, merged to keep about 300 points.
async fn app_usage(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<UsagePoint>>> {
    load_app_for(&server, &user, &name)?;
    let (span, step) = match q.range.as_str() {
        "1h" => {
            let live = server.live.read();
            let points = live
                .apps
                .get(&name)
                .map(|a| {
                    a.history
                        .iter()
                        .filter(|m| m.stats.state == AppState::Running)
                        .map(|m| UsagePoint {
                            at: m.at,
                            cpu: m.stats.cpu_percent,
                            cpu_peak: m.stats.cpu_percent,
                            mem: m.stats.mem_used,
                            mem_peak: m.stats.mem_used,
                        })
                        .collect()
                })
                .unwrap_or_default();
            return Ok(Json(points));
        }
        "1d" => (86400, 300),
        "7d" => (7 * 86400, 1800),
        "30d" => (30 * 86400, 7200),
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown range {other:?}: 1h, 1d, 7d or 30d"
            )));
        }
    };
    let since = facade_core::now().saturating_sub(span);
    Ok(Json(server.store.usage(
        &name,
        since - since % step,
        step,
    )?))
}

async fn list_deployments(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<DeploymentView>>> {
    load_app_for(&server, &user, &name)?;
    Ok(Json(server.store.list_deployments(&name, 50)?))
}

async fn get_deployment(
    State(server): S,
    user: User,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DeploymentView>> {
    server
        .store
        .get_deployment(id)?
        .filter(|d| can_see_app(&server, &user, &d.app))
        .map(Json)
        .ok_or_else(|| not_found("no such deployment"))
}

// ---------------------------------------------------------------- export / apply

/// The whole configuration as TOML. Sensitive values are never included.
async fn export(State(server): S, user: User) -> ApiResult<Response> {
    user.require_admin()?;
    let mut manifest = Manifest::default();
    for node in server.store.list_nodes()? {
        manifest.nodes.push(NodeDecl { name: node.name });
    }
    let exported = |rows: Vec<super::store::EnvRow>| -> anyhow::Result<BTreeMap<String, EnvEntry>> {
        let mut env = BTreeMap::new();
        for var in rows {
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
        Ok(env)
    };
    let apps = server.store.list_apps()?;
    for app in &apps {
        if app.spec.stack.is_some() {
            continue;
        }
        manifest.apps.push(AppManifest {
            spec: app.spec.clone(),
            env: exported(server.store.list_env(&app.spec.name)?)?,
        });
    }
    for stack in server.store.list_stacks()? {
        let mut env = BTreeMap::new();
        for app in &apps {
            if let (Some(s), Some(service)) = (&app.spec.stack, app.spec.service())
                && s.stack == stack.spec.name
            {
                let vars = exported(server.store.list_env(&app.spec.name)?)?;
                if !vars.is_empty() {
                    env.insert(service.to_string(), vars);
                }
            }
        }
        manifest.stacks.push(facade_core::manifest::StackManifest {
            variables: exported(server.store.list_stack_vars(&stack.spec.name)?)?,
            spec: stack.spec,
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
async fn apply(State(server): S, user: User, body: String) -> ApiResult<Json<ApplyReport>> {
    user.require_admin()?;
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
        let specs: Vec<&AppSpec> = manifest.apps.iter().map(|a| &a.spec).collect();
        let all = apps_with(&server, &specs)?;
        for app in &manifest.apps {
            check_spec_for_apply(&server, &app.spec, &names)?;
            check_links(&app.spec, &all)?;
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
                save_spec(&server, &old.spec, &app.spec, false).await?;
                report.updated.push(name);
            }
            Some(_) => report.unchanged.push(name),
        }
    }
    for stack in manifest.stacks {
        super::stack::apply(&server, stack, &mut report).await?;
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

// ---------------------------------------------------------------- databases, import

async fn create_database(
    State(server): S,
    user: User,
    Json(req): Json<CreateDatabase>,
) -> ApiResult<(StatusCode, Json<AppView>)> {
    // Databases are apps of the admins for now (no owner, no cap).
    user.require_admin()?;
    validate_name("app name", &req.name).map_err(bad_request)?;
    if server.store.get_stack(&req.name)?.is_some() {
        return Err(conflict(format!("{} is the name of a stack", req.name)));
    }
    if server.store.get_app(&req.name)?.is_some() {
        return Err(conflict(format!("app {} exists", req.name)));
    }
    let node = req
        .node
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| server.cfg.local_node.clone());
    let db = facade_core::templates::database(&req, &node, || random_token()[..32].to_string())
        .map_err(bad_request)?;
    let var = req
        .env_var
        .clone()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| db.url_var.to_string());
    if !valid_env_key(&var) {
        return Err(bad_request(format!("`{var}` is not a valid variable name")));
    }
    // The apps that will use it, with their link added.
    let mut consumers = Vec::new();
    for app in &req.link {
        let row = load_app(&server, app)?;
        let mut spec = row.spec.clone();
        if !spec.links.iter().any(|l| l.app == req.name) {
            spec.links.push(facade_core::Link {
                app: req.name.clone(),
                alias: None,
            });
        }
        consumers.push((row.spec, spec));
    }
    check_spec(&server, &db.spec)?;
    let mut changed: Vec<&AppSpec> = vec![&db.spec];
    changed.extend(consumers.iter().map(|(_, new)| new));
    let all = apps_with(&server, &changed)?;
    for spec in &changed {
        spec.validate().map_err(bad_request)?;
        check_links(spec, &all)?;
    }

    server.store.put_app(&db.spec)?;
    for (key, value, sensitive) in &db.env {
        server.store.set_env(
            &req.name,
            key,
            &server.secrets.seal(value.as_bytes()),
            *sensitive,
        )?;
    }
    let reference = facade_core::env::reference(&req.name, db.url_var);
    for (old, new) in &consumers {
        server.store.set_env(
            &new.name,
            &var,
            &server.secrets.seal(reference.as_bytes()),
            false,
        )?;
        save_spec(&server, old, new, false).await?;
    }
    server.refresh_routes()?;
    if server.hub.is_connected(&node) {
        start_deploy(&server, &req.name, &DeployRequest::default()).await?;
    }
    let row = load_app(&server, &req.name)?;
    Ok((
        StatusCode::CREATED,
        Json(app_view(
            &server,
            row.spec,
            row.revision,
            row.commit,
            row.owner,
        )),
    ))
}

// ---------------------------------------------------------------- volumes & disk

async fn list_volumes(State(server): S, user: User) -> ApiResult<Json<Vec<VolumeView>>> {
    let apps = server.store.list_apps()?;
    let live = server.live.read();
    let mut views = Vec::new();
    let mut nodes: Vec<_> = live.nodes.iter().collect();
    nodes.sort_by(|a, b| a.0.cmp(b.0));
    for (node, l) in nodes {
        for v in l.storage.iter().flat_map(|s| &s.volumes) {
            let declared = apps.iter().any(|a| {
                a.spec.node == *node
                    && a.spec
                        .volumes
                        .iter()
                        .any(|d| a.spec.volume_name(d) == v.name)
            });
            views.push(VolumeView {
                node: node.clone(),
                name: v.name.clone(),
                app: v.app.clone(),
                volume: v.volume.clone(),
                size: v.size,
                in_use: v.in_use,
                declared,
                last_backup: None,
            });
        }
    }
    // Members see their apps' volumes; leftovers are the admins' to clean up.
    views.retain(|v| user.is_admin() || (v.declared && can_see_app(&server, &user, &v.app)));
    let backups = super::backups::list(&server, None)?;
    for v in &mut views {
        v.last_backup = backups
            .iter()
            .filter(|b| b.app == v.app && b.volume == v.volume)
            .map(|b| b.id)
            .max();
    }
    Ok(Json(views))
}

async fn delete_volume(
    State(server): S,
    user: User,
    Path((node, volume)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    user.require_admin()?;
    let declared = server.store.list_apps()?.into_iter().find(|a| {
        a.spec.node == node
            && a.spec
                .volumes
                .iter()
                .any(|v| a.spec.volume_name(v) == volume)
    });
    if let Some(app) = declared {
        return Err(conflict(match &app.spec.stack {
            Some(stack) => format!(
                "{volume} is used by service {} of stack {}: remove it from the service first",
                app.spec.service().unwrap_or_default(),
                stack.stack
            ),
            None => format!(
                "{volume} is {}'s: remove it from the app's volumes first",
                app.spec.name
            ),
        }));
    }
    server
        .hub
        .request(&node, AgentRequest::RemoveVolume { name: volume })
        .await
        .map_err(|e| unavailable(format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PruneQuery {
    #[serde(default)]
    aggressive: bool,
}

async fn prune_node(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Query(q): Query<PruneQuery>,
) -> ApiResult<StatusCode> {
    user.require_admin()?;
    server
        .hub
        .request(
            &name,
            AgentRequest::Prune {
                aggressive: q.aggressive,
            },
        )
        .await
        .map_err(|e| unavailable(format!("{e:#}")))?;
    Ok(StatusCode::ACCEPTED)
}

/// Fetches the app's branch and compares it with the deployed commit.
async fn check_updates(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<UpdateCheck>> {
    let row = load_app_for(&server, &user, &name)?;
    super::updates::check(&server, &row)
        .await
        .map(Json)
        .map_err(|e| bad_request(format!("{e:#}")))
}

// ---------------------------------------------------------------- node ports

async fn node_ports(
    State(server): S,
    _: User,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<Listener>>> {
    if !server.store.node_exists(&name)? {
        return Err(not_found(format!("no node named {name}")));
    }
    if !server.hub.is_connected(&name) {
        return Err(unavailable(format!("node {name} is offline")));
    }
    let live = server.live.read();
    Ok(Json(
        live.nodes
            .get(&name)
            .map(|n| n.listeners.clone())
            .unwrap_or_default(),
    ))
}

// ---------------------------------------------------------------- registry logins

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".-:".contains(c))
        && !host.starts_with(['-', '.'])
}

/// An admin sees every login (members' included, never their secrets); a member
/// sees theirs.
async fn list_registry_logins(
    State(server): S,
    user: User,
) -> ApiResult<Json<Vec<RegistryLoginView>>> {
    let scope = (!user.is_admin()).then_some(user.id);
    let rows = server.store.list_registry_logins(scope)?;
    Ok(Json(
        rows.into_iter()
            .map(|r| RegistryLoginView {
                owner: (r.owner_id != super::store::ADMINS)
                    .then(|| server.store.user_by_id(r.owner_id).ok().flatten())
                    .flatten()
                    .map(|u| u.username),
                host: r.host,
                username: r.username,
            })
            .collect(),
    ))
}

async fn set_registry_login(
    State(server): S,
    user: User,
    Path(host): Path<String>,
    Json(req): Json<SetRegistryLogin>,
) -> ApiResult<StatusCode> {
    let host = host.trim().to_ascii_lowercase();
    if !valid_host(&host) {
        return Err(bad_request(format!(
            "`{host}` is not a registry host like ghcr.io or docker.io"
        )));
    }
    if req.username.trim().is_empty() || req.secret.is_empty() {
        return Err(bad_request("a username and a token are required"));
    }
    server.store.put_registry_login(
        user.login_owner(),
        &host,
        req.username.trim(),
        &server.secrets.seal(req.secret.as_bytes()),
    )?;
    info!(%host, by = %user.username, "registry login saved");
    resync_nodes(&server).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_registry_login(
    State(server): S,
    user: User,
    Path(host): Path<String>,
) -> ApiResult<StatusCode> {
    if !server
        .store
        .delete_registry_login(user.login_owner(), &host.to_ascii_lowercase())?
    {
        return Err(not_found(format!("no registry login for {host}")));
    }
    resync_nodes(&server).await;
    Ok(StatusCode::NO_CONTENT)
}
/// Sends every connected node its workloads again (with current credentials).
/// Running containers are not restarted by this.
async fn resync_nodes(server: &Server) {
    let Ok(nodes) = server.store.list_nodes() else {
        return;
    };
    for node in nodes {
        if !server.hub.is_connected(&node.name) {
            continue;
        }
        let result = match server.node_workloads(&node.name) {
            Ok(workloads) => {
                server
                    .hub
                    .request(&node.name, AgentRequest::Sync { workloads })
                    .await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            warn!(node = %node.name, "resync failed: {e:#}");
        }
    }
}

// ---------------------------------------------------------------- volume archives

/// The node an app's declared volume lives on.
/// The node an app's declared volume lives on, its Docker name, and how to
/// create it if the app never ran.
fn volume_node(
    server: &Server,
    user: &User,
    app: &str,
    volume: &str,
) -> ApiResult<(String, String, facade_core::proto::NewVolume)> {
    let row = load_app_for(server, user, app)?;
    let Some(declared) = row.spec.volumes.iter().find(|v| v.name == volume) else {
        return Err(not_found(format!("{app} has no volume named {volume}")));
    };
    if !server.hub.is_connected(&row.spec.node) {
        return Err(unavailable(format!("node {} is offline", row.spec.node)));
    }
    Ok((
        row.spec.node.clone(),
        row.spec.volume_name(declared),
        new_volume(&row.spec, declared),
    ))
}

async fn export_volume(
    State(server): S,
    user: User,
    Path((app, volume)): Path<(String, String)>,
) -> ApiResult<Response> {
    let (node, docker_name, _) = volume_node(&server, &user, &app, &volume)?;
    let stream = server
        .hub
        .open_volume_export(&node, &app, &docker_name)
        .await
        .map_err(|e| unavailable(format!("{e:#}")))?;
    let filename = format!("{app}-{volume}-{}.tar.gz", facade_core::now());
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(stream)),
    )
        .into_response())
}

/// Replaces a volume's contents with the uploaded tar (gzipped or not). The app is
/// stopped while its files are replaced, then started again.
async fn import_volume(
    State(server): S,
    user: User,
    Path((app, volume)): Path<(String, String)>,
    body: Body,
) -> ApiResult<StatusCode> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let (node, docker_name, create) = volume_node(&server, &user, &app, &volume)?;
    let tmp = scratch_path(&server)?;
    let result = async {
        let mut file = tls::private_file(&tmp).await?;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk?).await?;
        }
        file.sync_all().await?;
        server
            .hub
            .import_volume(&node, &app, &docker_name, Some(create), &tmp)
            .await
    }
    .await;
    let _ = tokio::fs::remove_file(&tmp).await;
    result.map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    info!(%app, %volume, "volume replaced from an upload");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- backups

#[derive(Deserialize)]
struct BackupQuery {
    app: Option<String>,
}

async fn list_backups(
    State(server): S,
    user: User,
    Query(q): Query<BackupQuery>,
) -> ApiResult<Json<Vec<BackupView>>> {
    let mut list = super::backups::list(&server, q.app.as_deref())?;
    list.retain(|b| can_see_app(&server, &user, &b.app));
    Ok(Json(list))
}

async fn run_backups(
    State(server): S,
    user: User,
    Json(req): Json<BackupRequest>,
) -> ApiResult<Json<BackupReport>> {
    if let Some(app) = &req.app {
        let row = load_app_for(&server, &user, app)?;
        if row.spec.volumes.is_empty() {
            return Err(bad_request(format!("{app} has no volumes")));
        }
    }
    if req.app.is_some() || user.is_admin() {
        return Ok(Json(
            super::backups::backup_all(&server, req.app.as_deref()).await,
        ));
    }
    // A member's "everything" is their apps.
    let mut report = BackupReport {
        done: vec![],
        failed: vec![],
    };
    for row in visible_apps(&server, &user)? {
        if row.spec.volumes.is_empty() {
            continue;
        }
        let one = super::backups::backup_all(&server, Some(&row.spec.name)).await;
        report.done.extend(one.done);
        report.failed.extend(one.failed);
    }
    Ok(Json(report))
}

async fn download_backup(
    State(server): S,
    user: User,
    Path((app, volume, id)): Path<(String, String, u64)>,
) -> ApiResult<Response> {
    load_app_for(&server, &user, &app)?;
    let path = super::backups::path(&server, &app, &volume, id).map_err(not_found)?;
    let file = tokio::fs::File::open(&path)
        .await
        .context("opening backup")?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{app}-{volume}-{id}.tar.gz\""),
            ),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

async fn restore_backup(
    State(server): S,
    user: User,
    Path((app, volume, id)): Path<(String, String, u64)>,
) -> ApiResult<StatusCode> {
    volume_node(&server, &user, &app, &volume)?;
    super::backups::restore(&server, &app, &volume, id)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    info!(%app, %volume, id, "volume restored from backup");
    Ok(StatusCode::NO_CONTENT)
}

/// A fresh copy of the server's database and master key, as a gzipped tar. With
/// both, `apply` is not even needed: put them in a new data_dir and start.
async fn server_backup(State(server): S, user: User) -> ApiResult<Response> {
    user.require_admin()?;
    let bytes = super::backups::server_archive(&server).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"facade-server-{}.tar.gz\"",
                    facade_core::now()
                ),
            ),
        ],
        bytes,
    )
        .into_response())
}
