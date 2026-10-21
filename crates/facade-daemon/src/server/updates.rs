//! "How far behind is this app?": the server keeps a treeless clone of each git
//! app's repository (commits only, no files), fetches the branch on demand and
//! compares its head with the commit the last deployment built.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use facade_core::Source;
use facade_core::api::{CommitView, UpdateCheck};

use super::Server;
use super::store::AppRow;
use crate::gitauth::Access;

/// Commits listed at most.
const MAX_COMMITS: usize = 50;
const SEP: char = '\u{1f}';

fn repo_dir(server: &Server, app: &str) -> PathBuf {
    server.cfg.data_dir.join("repos").join(format!("{app}.git"))
}

/// The app's repository is fetched with its owner's credential, if one covers it.
pub async fn check(server: &Server, row: &AppRow) -> anyhow::Result<UpdateCheck> {
    let spec = &row.spec;
    let Source::Git { repo, branch, .. } = &spec.source else {
        bail!("{} is not built from git", spec.name);
    };
    let dir = repo_dir(server, &spec.name);
    let owner = row.owner.unwrap_or(super::store::ADMINS);
    let (access, _) = super::git::access(server, owner, repo)?;
    let env = &access;
    // A repository URL (or credential kind) change starts over.
    let url = git(&dir, &["config", "--get", "remote.origin.url"], env)
        .await
        .unwrap_or_default();
    if url.trim() != access.url {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await?;
        git(&dir, &["init", "--bare", "--quiet"], env).await?;
        git(&dir, &["remote", "add", "--", "origin", &access.url], env).await?;
    }
    let refspec = match branch {
        Some(b) => format!("+refs/heads/{b}:refs/facade/head"),
        None => "+HEAD:refs/facade/head".to_string(),
    };
    git(
        &dir,
        &["fetch", "--quiet", "--filter=tree:0", "origin", &refspec],
        env,
    )
    .await
    .context("fetching the repository")?;

    let latest = log(&dir, &["-1", "refs/facade/head"], env)
        .await?
        .into_iter()
        .next()
        .context("the branch has no commits")?;
    let deployed = row.commit.clone();
    let known = match &deployed {
        Some(c) => git(
            &dir,
            &["merge-base", "--is-ancestor", c, "refs/facade/head"],
            env,
        )
        .await
        .is_ok(),
        None => false,
    };
    let (behind, commits) = match (&deployed, known) {
        (Some(c), true) => {
            let range = format!("{c}..refs/facade/head");
            let count: u32 = git(&dir, &["rev-list", "--count", &range], env)
                .await?
                .trim()
                .parse()?;
            let max = format!("-{MAX_COMMITS}");
            (Some(count), log(&dir, &[&max, &range], env).await?)
        }
        _ => (None, log(&dir, &["-5", "refs/facade/head"], env).await?),
    };
    Ok(UpdateCheck {
        branch: branch.clone(),
        deployed,
        latest,
        behind,
        commits,
    })
}

/// Drops the cached repository of a deleted app.
pub async fn forget(server: &Server, app: &str) {
    let _ = tokio::fs::remove_dir_all(repo_dir(server, app)).await;
}

async fn log(dir: &Path, args: &[&str], access: &Access) -> anyhow::Result<Vec<CommitView>> {
    let format = format!("--format=%H{SEP}%s{SEP}%an{SEP}%ct");
    let mut all = vec!["log", format.as_str()];
    all.extend_from_slice(args);
    let out = git(dir, &all, access).await?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(SEP);
            Some(CommitView {
                hash: parts.next()?.to_string(),
                subject: parts.next()?.to_string(),
                author: parts.next()?.to_string(),
                time: parts.next()?.parse().ok()?,
            })
        })
        .collect())
}

/// Runs git in `dir` with the app's credential, returning stdout, or the last
/// line of stderr as the error.
async fn git(dir: &Path, args: &[&str], access: &Access) -> anyhow::Result<String> {
    let mut cmd = access.git();
    cmd.arg("-C").arg(dir).args(args);
    let out = tokio::time::timeout(Duration::from_secs(120), cmd.output())
        .await
        .context("git took longer than two minutes")?
        .context("running git (is it installed on the server?)")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("git failed")
            .trim_start_matches("fatal: ")
            .to_string();
        bail!("{reason}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
