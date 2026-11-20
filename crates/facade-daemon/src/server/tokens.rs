//! API tokens: what a script or a desktop widget uses instead of a login,
//! which lasts 30 days. Each user makes, lists and revokes their own; a token
//! acts as its user, with everything they may do (`full`) or only looking
//! (`read-only`). It lasts until revoked unless given days to live. Secrets
//! start with `fct_` and are shown once: only their hash is kept.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use facade_core::api::{API_TOKEN_PREFIX, ApiTokenView, CreateApiToken, CreatedApiToken};
use tracing::{info, warn};

use super::api::{ApiResult, User, bad_request, conflict, not_found};
use super::store::Store;
use super::{Server, hash_secret, random_token};

type S = State<Arc<Server>>;

/// When a token was last used is noted at most this often, so a widget
/// polling every few seconds does not write the database each time.
const NOTE_USE_EVERY: u64 = 60;
const NAME_CHARS: usize = 64;

pub async fn list(State(server): S, user: User) -> ApiResult<Json<Vec<ApiTokenView>>> {
    Ok(Json(server.store.api_tokens(user.id)?))
}

pub async fn create(
    State(server): S,
    user: User,
    Json(req): Json<CreateApiToken>,
) -> ApiResult<(StatusCode, Json<CreatedApiToken>)> {
    let created = create_for(&server.store, &user, req, facade_core::now())?;
    info!(user = %user.username, token = %created.token.name, scope = created.token.scope.as_str(), "API token created");
    Ok((StatusCode::CREATED, Json(created)))
}

fn create_for(
    store: &Store,
    user: &User,
    req: CreateApiToken,
    now: u64,
) -> ApiResult<CreatedApiToken> {
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > NAME_CHARS || name.chars().any(char::is_control) {
        return Err(bad_request(format!(
            "a token needs a name of 1 to {NAME_CHARS} characters (what uses it: \"desktop widget\")"
        )));
    }
    let expires_at = match req.expires_days {
        Some(0) => return Err(bad_request("a token lasts at least a day")),
        Some(days) => Some(now.saturating_add(days.saturating_mul(86_400))),
        None => None,
    };
    let secret = format!("{API_TOKEN_PREFIX}{}", random_token());
    let token = store
        .create_api_token(
            user.id,
            name,
            &hash_secret(&secret),
            req.scope,
            expires_at,
            now,
        )?
        .ok_or_else(|| conflict(format!("you already have a token named {name}")))?;
    Ok(CreatedApiToken { token, secret })
}

pub async fn revoke(State(server): S, user: User, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    if !server.store.delete_api_token(user.id, id)? {
        return Err(not_found(format!("you have no token {id}")));
    }
    info!(user = %user.username, token = id, "API token revoked");
    Ok(StatusCode::NO_CONTENT)
}

/// Notes that a token was just used, unless that was noted less than a minute ago.
pub(super) fn note_use(store: &Store, id: i64, last_used: Option<u64>, now: u64) {
    if last_used.is_some_and(|at| now.saturating_sub(at) < NOTE_USE_EVERY) {
        return;
    }
    if let Err(e) = store.touch_api_token(id, now) {
        warn!("noting the use of API token {id}: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::http::{Method, StatusCode};
    use facade_core::api::TokenScope;

    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn setup() -> (Store, User) {
        let store = Store::open(Path::new(":memory:")).unwrap();
        store.create_user("alice", "x", "member", false).unwrap();
        let id = store.user_by_name("alice").unwrap().unwrap().id;
        let session = "s".repeat(64);
        store
            .create_session(&hash_secret(&session), id, facade_core::now() + 86_400)
            .unwrap();
        let user = User::authenticate(&store, &Method::GET, &session, NOW).unwrap();
        (store, user)
    }

    fn make(
        store: &Store,
        user: &User,
        name: &str,
        scope: TokenScope,
        days: Option<u64>,
    ) -> String {
        let req = CreateApiToken {
            name: name.into(),
            scope,
            expires_days: days,
        };
        create_for(store, user, req, NOW).unwrap().secret
    }

    fn status(r: ApiResult<User>) -> StatusCode {
        match r {
            Ok(_) => StatusCode::OK,
            Err(e) => e.0,
        }
    }

    #[test]
    fn a_token_acts_as_its_user_and_notes_its_use() {
        let (store, alice) = setup();
        let secret = make(&store, &alice, "desktop widget", TokenScope::Full, None);
        assert!(secret.starts_with("fct_"));

        let user = User::authenticate(&store, &Method::POST, &secret, NOW + 10).unwrap();
        assert_eq!(user.username, "alice");
        assert_eq!(user.role, "member");
        assert!(user.require_full_access().is_ok());

        let listed = store.api_tokens(alice.id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "desktop widget");
        assert_eq!(listed[0].expires_at, None);
        assert_eq!(listed[0].last_used, Some(NOW + 10));
        // Used again within the minute: not written again.
        User::authenticate(&store, &Method::GET, &secret, NOW + 40).unwrap();
        assert_eq!(
            store.api_tokens(alice.id).unwrap()[0].last_used,
            Some(NOW + 10)
        );
        User::authenticate(&store, &Method::GET, &secret, NOW + 80).unwrap();
        assert_eq!(
            store.api_tokens(alice.id).unwrap()[0].last_used,
            Some(NOW + 80)
        );

        // Names are the user's own.
        let again = create_for(
            &store,
            &alice,
            CreateApiToken {
                name: "desktop widget".into(),
                scope: TokenScope::Full,
                expires_days: None,
            },
            NOW,
        );
        assert_eq!(again.err().map(|e| e.0), Some(StatusCode::CONFLICT));
    }

    #[test]
    fn a_read_only_token_only_looks() {
        let (store, alice) = setup();
        let secret = make(&store, &alice, "widget", TokenScope::ReadOnly, None);
        let user = User::authenticate(&store, &Method::GET, &secret, NOW).unwrap();
        assert_eq!(user.api_token.map(|t| t.1), Some(TokenScope::ReadOnly));
        // A shell, secrets, backups: not even through GET.
        assert!(user.require_full_access().is_err());
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert_eq!(
                status(User::authenticate(&store, &method, &secret, NOW)),
                StatusCode::FORBIDDEN
            );
        }
    }

    #[test]
    fn revoked_and_expired_tokens_stop_working() {
        let (store, alice) = setup();
        let secret = make(&store, &alice, "script", TokenScope::Full, None);
        let id = store.api_tokens(alice.id).unwrap()[0].id;
        assert!(store.delete_api_token(alice.id, id).unwrap());
        assert_eq!(
            status(User::authenticate(&store, &Method::GET, &secret, NOW)),
            StatusCode::UNAUTHORIZED
        );

        let secret = make(&store, &alice, "for a week", TokenScope::Full, Some(7));
        assert_eq!(
            store.api_tokens(alice.id).unwrap()[0].expires_at,
            Some(NOW + 7 * 86_400)
        );
        assert_eq!(
            status(User::authenticate(
                &store,
                &Method::GET,
                &secret,
                NOW + 86_400
            )),
            StatusCode::OK
        );
        assert_eq!(
            status(User::authenticate(
                &store,
                &Method::GET,
                &secret,
                NOW + 7 * 86_400
            )),
            StatusCode::UNAUTHORIZED
        );

        // Unknown, and another user's to revoke.
        assert_eq!(
            status(User::authenticate(&store, &Method::GET, "fct_nope", NOW)),
            StatusCode::UNAUTHORIZED
        );
        let id = store.api_tokens(alice.id).unwrap()[0].id;
        assert!(!store.delete_api_token(alice.id + 1, id).unwrap());
    }

    #[test]
    fn tokens_go_with_their_user() {
        let (store, alice) = setup();
        let secret = make(&store, &alice, "script", TokenScope::Full, None);
        store.delete_user(alice.id).unwrap();
        assert_eq!(
            status(User::authenticate(&store, &Method::GET, &secret, NOW)),
            StatusCode::UNAUTHORIZED
        );
    }
}
