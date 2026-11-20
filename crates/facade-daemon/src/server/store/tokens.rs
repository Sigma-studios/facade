//! API tokens: long-lived credentials a user makes for scripts and widgets.
//! Only a hash of each secret is kept, like sessions.

use facade_core::api::{ApiTokenView, TokenScope};
use rusqlite::{OptionalExtension, params};

use super::{Store, USER_COLUMNS, UserRow, user_from_row};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS api_tokens (
    -- Never reused, so a revoked token's id cannot come to mean another.
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    read_only INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    last_used INTEGER,
    UNIQUE (user_id, name)
);
";

/// A valid token's user, and what the token allows.
pub struct TokenUser {
    pub user: UserRow,
    pub id: i64,
    pub scope: TokenScope,
    pub last_used: Option<u64>,
}

fn scope(read_only: bool) -> TokenScope {
    if read_only {
        TokenScope::ReadOnly
    } else {
        TokenScope::Full
    }
}

impl Store {
    /// `None` when the user already has a token of that name.
    pub fn create_api_token(
        &self,
        user_id: i64,
        name: &str,
        token_hash: &str,
        scope: TokenScope,
        expires_at: Option<u64>,
        now: u64,
    ) -> anyhow::Result<Option<ApiTokenView>> {
        let conn = self.0.lock();
        let inserted = conn.execute(
            "INSERT INTO api_tokens (user_id, name, token_hash, read_only, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (user_id, name) DO NOTHING",
            params![
                user_id,
                name,
                token_hash,
                scope == TokenScope::ReadOnly,
                now,
                expires_at
            ],
        )?;
        Ok((inserted > 0).then(|| ApiTokenView {
            id: conn.last_insert_rowid(),
            name: name.to_string(),
            scope,
            created_at: now,
            expires_at,
            last_used: None,
        }))
    }

    /// A user's tokens, expired ones included (until revoked), oldest first.
    pub fn api_tokens(&self, user_id: i64) -> anyhow::Result<Vec<ApiTokenView>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, read_only, created_at, expires_at, last_used
             FROM api_tokens WHERE user_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([user_id], |r| {
            Ok(ApiTokenView {
                id: r.get(0)?,
                name: r.get(1)?,
                scope: scope(r.get(2)?),
                created_at: r.get(3)?,
                expires_at: r.get(4)?,
                last_used: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Whether the user had that token.
    pub fn delete_api_token(&self, user_id: i64, id: i64) -> anyhow::Result<bool> {
        Ok(self.0.lock().execute(
            "DELETE FROM api_tokens WHERE id = ?1 AND user_id = ?2",
            params![id, user_id],
        )? > 0)
    }

    /// The user of a token that has not expired.
    pub fn api_token_user(&self, token_hash: &str, now: u64) -> anyhow::Result<Option<TokenUser>> {
        Ok(self
            .0
            .lock()
            .query_row(
                &format!(
                    "SELECT {USER_COLUMNS}, t.id, t.read_only, t.last_used FROM api_tokens t
                     JOIN users u ON u.id = t.user_id
                     WHERE t.token_hash = ?1 AND (t.expires_at IS NULL OR t.expires_at > ?2)"
                ),
                params![token_hash, now],
                |r| {
                    Ok(TokenUser {
                        user: user_from_row(r)?,
                        id: r.get(7)?,
                        scope: scope(r.get(8)?),
                        last_used: r.get(9)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn touch_api_token(&self, id: i64, now: u64) -> anyhow::Result<()> {
        self.0.lock().execute(
            "UPDATE api_tokens SET last_used = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        Ok(())
    }
}
