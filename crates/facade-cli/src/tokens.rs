//! `facade token`: long-lived API tokens for scripts and widgets, which
//! `facade login`'s session (30 days) would leave stranded.

use anyhow::bail;
use clap::Subcommand;
use facade_core::api::{ApiTokenView, CreateApiToken, CreatedApiToken, TokenScope};
use reqwest::Method;

use crate::client::Api;
use crate::output::{ago, date, print_json, table};

#[derive(Subcommand)]
pub enum TokenCommand {
    /// Your tokens (never their secrets).
    #[command(alias = "ls")]
    List,
    /// Make a token, e.g. `facade token create "desktop widget" --read-only`;
    /// its secret is printed once. Use it as `Authorization: Bearer fct_…`, or
    /// as FACADE_TOKEN.
    Create {
        /// What uses it.
        name: String,
        /// Only look: GET requests, and not a shell, secret values or backups.
        #[arg(long)]
        read_only: bool,
        /// Stop working after this many days (never by default).
        #[arg(long, value_name = "DAYS")]
        expires: Option<u64>,
    },
    /// Revoke a token, by id or name.
    #[command(alias = "rm")]
    Revoke { token: String },
}

pub async fn run(api: &Api, cmd: TokenCommand, json: bool) -> anyhow::Result<()> {
    match cmd {
        TokenCommand::List => {
            let list: Vec<ApiTokenView> = api.get("/api/tokens").await?;
            if json {
                return print_json(&list);
            }
            if list.is_empty() {
                println!("no tokens; make one with `facade token create <name> [--read-only]`");
                return Ok(());
            }
            let now = facade_core::now();
            table(
                &["ID", "NAME", "SCOPE", "CREATED", "LAST USED", "EXPIRES"],
                list.iter().map(|t| {
                    vec![
                        t.id.to_string(),
                        t.name.clone(),
                        t.scope.as_str().to_string(),
                        date(t.created_at),
                        t.last_used.map(ago).unwrap_or_else(|| "never".into()),
                        match t.expires_at {
                            None => "never".into(),
                            Some(at) if at <= now => format!("expired {}", date(at)),
                            Some(at) => date(at),
                        },
                    ]
                }),
            );
        }
        TokenCommand::Create {
            name,
            read_only,
            expires,
        } => {
            let req = CreateApiToken {
                name,
                scope: if read_only {
                    TokenScope::ReadOnly
                } else {
                    TokenScope::Full
                },
                expires_days: expires,
            };
            let created: CreatedApiToken = api
                .json(api.request(Method::POST, "/api/tokens").json(&req))
                .await?;
            if json {
                return print_json(&created);
            }
            // The secret alone on stdout, so it can be piped into a file.
            println!("{}", created.secret);
            eprintln!(
                "token {} ({}, {}): copy it now, it is not shown again",
                created.token.name,
                created.token.scope.as_str(),
                match created.token.expires_at {
                    Some(at) => format!("expires {}", date(at)),
                    None => "does not expire".into(),
                }
            );
        }
        TokenCommand::Revoke { token } => {
            let list: Vec<ApiTokenView> = api.get("/api/tokens").await?;
            let found: Vec<&ApiTokenView> = list
                .iter()
                .filter(|t| t.id.to_string() == token || t.name == token)
                .collect();
            let t = match found.as_slice() {
                [t] => *t,
                [] => bail!("you have no token {token} (facade token list)"),
                // A name that is another token's id: say so rather than guess.
                _ => bail!("{token} is both a token's id and another's name: revoke by id"),
            };
            api.send(api.request(Method::DELETE, &format!("/api/tokens/{}", t.id)))
                .await?;
            if json {
                return print_json(t);
            }
            eprintln!("revoked token {} ({})", t.name, t.id);
        }
    }
    Ok(())
}
