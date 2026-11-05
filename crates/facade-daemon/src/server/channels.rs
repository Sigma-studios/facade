//! How far a NixOS node's nixpkgs is behind its branch. The node says which
//! revision it was built from and which branch it follows; the server asks the
//! channel for its head (`channels.nixos.org/<branch>/git-revision`) and GitHub
//! for the commits and days between them, and says when the release is past
//! its end of life.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use facade_core::events::{Updates, Upkeep};
use parking_lot::RwLock;
use tokio::sync::Notify;
use tracing::{debug, warn};

use super::Server;

/// The channel's head and the distance to it are looked up this often.
const CHECK_EVERY: u64 = 6 * 3600;

#[derive(Default)]
pub struct Channels {
    /// `(branch, revision)` → how far behind, and when it was looked up.
    behind: RwLock<HashMap<(String, String), (Behind, u64)>>,
    wake: Notify,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Behind {
    days: u32,
    commits: u32,
}

impl Channels {
    /// Adds what the server knows to a node's upkeep: the distance to its
    /// branch's head, and whether its release is past its end of life.
    pub fn enrich(&self, upkeep: &mut Upkeep) {
        let facade_core::platform::Checked::Known(Updates::Channel(c)) = &mut upkeep.updates else {
            return;
        };
        c.eol = past_end_of_life(&c.release, &c.branch, facade_core::now());
        let key = (c.branch.clone(), c.revision.clone());
        match self.behind.read().get(&key) {
            Some((b, _)) => {
                c.behind_days = Some(b.days);
                c.behind_commits = Some(b.commits);
            }
            None => self.wake.notify_one(),
        }
    }
}

/// NixOS releases are supported for seven months: `YY.05` until the end of
/// December, `YY.11` until the end of June after. Unstable never ends.
fn past_end_of_life(release: &str, branch: &str, now: u64) -> bool {
    if branch.contains("unstable") {
        return false;
    }
    let Some((yy, mm)) = release.split_once('.') else {
        return false;
    };
    let (Ok(yy), Ok(mm)) = (yy.parse::<u64>(), mm.parse::<u64>()) else {
        return false;
    };
    let year = 2000 + yy;
    let end = match mm {
        5 => format!("{year}-12-31T23:59:59"),
        11 => format!("{}-06-30T23:59:59", year + 1),
        _ => return false,
    };
    super::vulns::oval::parse_time(&end).is_some_and(|end| now > end)
}

pub async fn run(server: Arc<Server>) {
    loop {
        let wanted: HashSet<(String, String)> = server
            .activity
            .all_upkeep()
            .into_iter()
            .filter_map(|u| match u.updates {
                facade_core::platform::Checked::Known(Updates::Channel(c)) => {
                    Some((c.branch, c.revision))
                }
                _ => None,
            })
            .collect();
        let now = facade_core::now();
        let mut heads: HashMap<String, String> = HashMap::new();
        let mut changed = false;
        for (branch, revision) in wanted {
            let fresh = server
                .channels
                .behind
                .read()
                .get(&(branch.clone(), revision.clone()))
                .is_some_and(|(_, at)| now.saturating_sub(*at) < CHECK_EVERY);
            if fresh {
                continue;
            }
            match look_up(&mut heads, &branch, &revision).await {
                Ok(behind) => {
                    debug!(%branch, %revision, ?behind, "nixpkgs");
                    server
                        .channels
                        .behind
                        .write()
                        .insert((branch, revision), (behind, now));
                    changed = true;
                }
                Err(e) => warn!(%branch, "how far behind its branch a NixOS node is: {e:#}"),
            }
        }
        if changed {
            server.activity.enrich_upkeep(|u| server.channels.enrich(u));
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1800)) => {}
            _ = server.channels.wake.notified() => {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("facade/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

async fn look_up(
    heads: &mut HashMap<String, String>,
    branch: &str,
    revision: &str,
) -> anyhow::Result<Behind> {
    if !branch
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        || !revision.chars().all(|c| c.is_ascii_hexdigit())
    {
        bail!("unexpected branch or revision: {branch} {revision}");
    }
    let client = client()?;
    let head = match heads.get(branch) {
        Some(h) => h.clone(),
        None => {
            let url = format!("https://channels.nixos.org/{branch}/git-revision");
            let resp = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("GET {url}"))?;
            if !resp.status().is_success() {
                bail!("GET {url}: {}", resp.status());
            }
            let head = resp.text().await?.trim().to_string();
            heads.insert(branch.to_string(), head.clone());
            head
        }
    };
    if head.starts_with(revision) || revision.starts_with(&head) {
        return Ok(Behind::default());
    }
    let url = format!("https://api.github.com/repos/NixOS/nixpkgs/compare/{revision}...{head}");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url}: {}", resp.status());
    }
    let compare: serde_json::Value = resp.json().await?;
    let commits = compare["ahead_by"].as_u64().unwrap_or(0) as u32;
    let date = |v: &serde_json::Value| {
        v["commit"]["committer"]["date"]
            .as_str()
            .and_then(super::vulns::oval::parse_time)
    };
    let base = date(&compare["base_commit"]);
    // The head's date: the last commit listed, or its own page when the list
    // was cut short.
    let head_date = match compare["commits"].as_array().and_then(|c| c.last()) {
        Some(last) if last["sha"].as_str() == Some(head.as_str()) => date(last),
        _ => {
            let url = format!("https://api.github.com/repos/NixOS/nixpkgs/commits/{head}");
            let v: serde_json::Value = client
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await?
                .json()
                .await?;
            date(&v)
        }
    };
    let days = match (base, head_date) {
        (Some(b), Some(h)) => (h.saturating_sub(b) / 86_400) as u32,
        _ => 0,
    };
    Ok(Behind { days, commits })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_end_seven_months_on() {
        let at = |s: &str| super::super::vulns::oval::parse_time(s).unwrap();
        assert!(!past_end_of_life(
            "26.05",
            "nixos-26.05",
            at("2026-12-31T12:00:00")
        ));
        assert!(past_end_of_life(
            "26.05",
            "nixos-26.05",
            at("2027-01-01T00:00:00")
        ));
        assert!(!past_end_of_life(
            "25.11",
            "nixos-25.11",
            at("2026-06-30T00:00:00")
        ));
        assert!(past_end_of_life(
            "25.11",
            "nixos-25.11",
            at("2026-07-01T00:00:00")
        ));
        assert!(!past_end_of_life(
            "26.11",
            "nixos-unstable",
            at("2030-01-01T00:00:00")
        ));
    }
}
