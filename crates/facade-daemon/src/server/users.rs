//! Accounts and what each may do.
//!
//! * **Admins** manage everything.
//! * **Members** manage the apps they own or were given access to, and nothing
//!   global (nodes, DNS, server settings, alerts, users, other apps). Their apps
//!   share one memory/CPU cap, build with their own registry and git logins
//!   only, cannot use the node's network, and can only take domain names that do
//!   not resolve yet (an admin hands over existing ones).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use facade_core::AppSpec;
use facade_core::api::{CreateUser, OneTimePassword, ROLES, SetOwner, UpdateUser, UserView};

use super::Server;
use super::api::{ApiError, ApiResult, User, bad_request, conflict, hash_password, not_found};
use super::store::AppRow;

type S = State<Arc<Server>>;

pub fn forbidden(msg: impl ToString) -> ApiError {
    ApiError(StatusCode::FORBIDDEN, msg.to_string())
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }

    pub fn require_admin(&self) -> ApiResult<()> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(forbidden("only an admin can do this"))
        }
    }

    /// Whose registry and git logins this user manages: their own, or the
    /// admins' shared ones.
    pub fn login_owner(&self) -> i64 {
        if self.is_admin() {
            super::store::ADMINS
        } else {
            self.id
        }
    }
}

pub fn can_access(server: &Server, user: &User, row: &AppRow) -> anyhow::Result<bool> {
    Ok(user.is_admin()
        || row.owner == Some(user.id)
        || server.store.app_access(&row.spec.name)?.contains(&user.id))
}

/// The app, if `user` may manage it. Apps they cannot see do not exist for them.
pub fn load_app_for(server: &Server, user: &User, name: &str) -> ApiResult<AppRow> {
    match server.store.get_app(name)? {
        Some(row) if can_access(server, user, &row)? => Ok(row),
        _ => Err(not_found(format!("no app named {name}"))),
    }
}

/// Apps `user` may manage.
pub fn visible_apps(server: &Server, user: &User) -> anyhow::Result<Vec<AppRow>> {
    let mut out = Vec::new();
    for row in server.store.list_apps()? {
        if can_access(server, user, &row)? {
            out.push(row);
        }
    }
    Ok(out)
}

pub fn can_see_app(server: &Server, user: &User, app: &str) -> bool {
    user.is_admin()
        || server
            .store
            .get_app(app)
            .ok()
            .flatten()
            .is_some_and(|row| can_access(server, user, &row).unwrap_or(false))
}

/// Memory (MB) and CPUs reserved by `owner`'s enabled apps, but `except`.
pub fn allocated(server: &Server, owner: i64, except: Option<&str>) -> anyhow::Result<(u64, f64)> {
    let mut memory = 0;
    let mut cpus = 0.0;
    for row in server.store.list_apps()? {
        let spec = &row.spec;
        if row.owner != Some(owner)
            || Some(spec.name.as_str()) == except
            || !spec.enabled
            || spec.is_static()
        {
            continue;
        }
        memory += spec.limits.memory_mb.unwrap_or(0);
        cpus += spec.limits.cpus.unwrap_or(0.0);
    }
    Ok((memory, cpus))
}

/// The caps of `owner` still hold with `spec` running.
pub fn check_caps(server: &Server, owner: Option<i64>, spec: &AppSpec) -> ApiResult<()> {
    let Some(owner) = owner else { return Ok(()) };
    let Some(user) = server.store.user_by_id(owner)? else {
        return Ok(());
    };
    if !spec.enabled || spec.is_static() {
        return Ok(());
    }
    let (memory, cpus) = allocated(server, owner, Some(&spec.name))?;
    let who = &user.username;
    if let Some(cap) = user.memory_cap_mb {
        let Some(mb) = spec.limits.memory_mb else {
            return Err(bad_request(format!(
                "{who}'s apps share {cap} MB of memory: give this one a memory limit ({} MB left)",
                cap.saturating_sub(memory)
            )));
        };
        if memory + mb > cap {
            return Err(conflict(format!(
                "{mb} MB would exceed {who}'s memory cap: {memory} of {cap} MB are taken by their other apps"
            )));
        }
    }
    if let Some(cap) = user.cpu_cap {
        let Some(c) = spec.limits.cpus else {
            return Err(bad_request(format!(
                "{who}'s apps share {cap} CPUs: give this one a CPU limit ({:.2} left)",
                (cap - cpus).max(0.0)
            )));
        };
        if cpus + c > cap + 1e-9 {
            return Err(conflict(format!(
                "{c} CPUs would exceed {who}'s CPU cap: {cpus} of {cap} are taken by their other apps"
            )));
        }
    }
    Ok(())
}

/// What only admins may set on an app: the node's network, and domain names
/// that already point somewhere (they may be someone else's site).
pub async fn check_member_spec(
    user: &User,
    old: Option<&AppSpec>,
    spec: &AppSpec,
) -> ApiResult<()> {
    if user.is_admin() {
        return Ok(());
    }
    if spec.host_network && !old.is_some_and(|o| o.host_network) {
        return Err(forbidden(
            "only an admin can put an app on the node's network; publish its ports instead",
        ));
    }
    let before: HashSet<&String> = old
        .map(|o| o.domains.iter().chain(&o.redirects).collect())
        .unwrap_or_default();
    for domain in spec.domains.iter().chain(&spec.redirects) {
        if !before.contains(domain) && resolves(domain).await {
            return Err(forbidden(format!(
                "{domain} already points somewhere: you can add names that do not resolve yet, \
                 and an admin can give you this one"
            )));
        }
    }
    Ok(())
}

async fn resolves(domain: &str) -> bool {
    match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((domain, 80)),
    )
    .await
    {
        Ok(Ok(mut addrs)) => addrs.next().is_some(),
        _ => false,
    }
}

fn user_view(server: &Server, user: &super::store::UserRow) -> anyhow::Result<UserView> {
    let (memory, cpus) = allocated(server, user.id, None)?;
    let mut apps = Vec::new();
    let mut shared_apps = Vec::new();
    for row in server.store.list_apps()? {
        if row.owner == Some(user.id) {
            apps.push(row.spec.name);
        } else if server.store.app_access(&row.spec.name)?.contains(&user.id) {
            shared_apps.push(row.spec.name);
        }
    }
    Ok(UserView {
        username: user.username.clone(),
        role: user.role.clone(),
        memory_cap_mb: user.memory_cap_mb,
        cpu_cap: user.cpu_cap,
        memory_allocated_mb: memory,
        cpus_allocated: cpus,
        apps,
        shared_apps,
        must_change_password: user.must_change_password,
    })
}

fn find_user(server: &Server, name: &str) -> ApiResult<super::store::UserRow> {
    server
        .store
        .user_by_name(name)?
        .ok_or_else(|| not_found(format!("no user named {name}")))
}

fn check_role_and_caps(role: &str, memory: Option<u64>, cpus: Option<f64>) -> ApiResult<()> {
    if !ROLES.contains(&role) {
        return Err(bad_request(format!(
            "role must be one of {}",
            ROLES.join(", ")
        )));
    }
    if memory == Some(0) {
        return Err(bad_request("a memory cap must be at least 1 MB"));
    }
    if cpus.is_some_and(|c| !(c > 0.0 && c <= 1024.0)) {
        return Err(bad_request("a CPU cap must be more than 0"));
    }
    Ok(())
}

fn admin_count(server: &Server) -> anyhow::Result<usize> {
    Ok(server
        .store
        .list_users()?
        .iter()
        .filter(|u| u.is_admin())
        .count())
}

/// A random password for an admin to hand over; it must be changed at first login.
fn one_time_password(server: &Server, user: &super::store::UserRow) -> ApiResult<OneTimePassword> {
    let password = super::random_token()[..20].to_string();
    server
        .store
        .reset_password(user.id, &hash_password(&password)?)?;
    Ok(OneTimePassword {
        username: user.username.clone(),
        password,
    })
}

pub async fn list_users(State(server): S, user: User) -> ApiResult<Json<Vec<UserView>>> {
    user.require_admin()?;
    let users = server.store.list_users()?;
    Ok(Json(
        users
            .iter()
            .map(|u| user_view(&server, u))
            .collect::<anyhow::Result<_>>()?,
    ))
}

pub async fn create_user(
    State(server): S,
    user: User,
    Json(req): Json<CreateUser>,
) -> ApiResult<(StatusCode, Json<OneTimePassword>)> {
    user.require_admin()?;
    facade_core::validate_name("username", &req.username).map_err(bad_request)?;
    check_role_and_caps(&req.role, req.memory_cap_mb, req.cpu_cap)?;
    if server.store.user_by_name(&req.username)?.is_some() {
        return Err(conflict(format!("{} exists", req.username)));
    }
    // Locked until the one-time password below replaces this one.
    let placeholder = hash_password(&super::random_token())?;
    server
        .store
        .create_user(&req.username, &placeholder, &req.role, true)?;
    let created = find_user(&server, &req.username)?;
    server
        .store
        .set_user_caps(created.id, req.memory_cap_mb, req.cpu_cap)?;
    tracing::info!(by = %user.username, user = %req.username, role = %req.role, "user created");
    Ok((
        StatusCode::CREATED,
        Json(one_time_password(&server, &created)?),
    ))
}

pub async fn update_user(
    State(server): S,
    user: User,
    Path(name): Path<String>,
    Json(req): Json<UpdateUser>,
) -> ApiResult<Json<UserView>> {
    user.require_admin()?;
    check_role_and_caps(&req.role, req.memory_cap_mb, req.cpu_cap)?;
    let target = find_user(&server, &name)?;
    if target.is_admin() && req.role != "admin" && admin_count(&server)? == 1 {
        return Err(conflict("there must be at least one admin"));
    }
    server.store.set_user_role(target.id, &req.role)?;
    server
        .store
        .set_user_caps(target.id, req.memory_cap_mb, req.cpu_cap)?;
    let updated = find_user(&server, &name)?;
    Ok(Json(user_view(&server, &updated)?))
}

pub async fn reset_password(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<OneTimePassword>> {
    user.require_admin()?;
    let target = find_user(&server, &name)?;
    Ok(Json(one_time_password(&server, &target)?))
}

pub async fn delete_user(
    State(server): S,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    user.require_admin()?;
    let target = find_user(&server, &name)?;
    if target.id == user.id {
        return Err(conflict("you cannot delete your own account"));
    }
    if target.is_admin() && admin_count(&server)? == 1 {
        return Err(conflict("there must be at least one admin"));
    }
    server.store.delete_user(target.id)?;
    tracing::info!(by = %user.username, user = %name, "user deleted; their apps now belong to the admins");
    Ok(StatusCode::NO_CONTENT)
}

/// Gives an app to a member (its resources count against their cap and its
/// builds use their logins), or back to the admins.
pub async fn set_owner(
    State(server): S,
    user: User,
    Path(app): Path<String>,
    Json(req): Json<SetOwner>,
) -> ApiResult<StatusCode> {
    user.require_admin()?;
    super::api::load_app(&server, &app)?;
    let owner = match &req.owner {
        Some(name) => Some(find_user(&server, name)?.id),
        None => None,
    };
    server.store.set_app_owner(&app, owner)?;
    // Its builds now use other logins.
    if let Err(e) = server.push(&app).await {
        tracing::warn!(%app, "push failed: {e:#}");
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn grant_access(
    State(server): S,
    user: User,
    Path((app, name)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    user.require_admin()?;
    super::api::load_app(&server, &app)?;
    let target = find_user(&server, &name)?;
    server.store.grant_access(&app, target.id)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn revoke_access(
    State(server): S,
    user: User,
    Path((app, name)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    user.require_admin()?;
    let target = find_user(&server, &name)?;
    if !server.store.revoke_access(&app, target.id)? {
        return Err(not_found(format!("{name} has no access to {app}")));
    }
    Ok(StatusCode::NO_CONTENT)
}
