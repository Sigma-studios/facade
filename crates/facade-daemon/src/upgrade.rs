//! Commands for `deploy/upgrade.sh`, run on the server with the binary about to
//! be installed:
//!
//! * `facaded upgrade-check`: what installing this binary would do to the apps
//!   running on the server's node. A new build can change how container
//!   fingerprints or image names are computed; then containers are recreated,
//!   or rebuilt from the latest commit of their branch, at the next convergence.
//! * `facaded backup-state`: a copy of the database and master key, next to the
//!   scheduled ones.

use std::collections::HashMap;

use anyhow::Context;
use bollard::Docker;
use bollard::query_parameters::ListContainersOptions;
use facade_core::{Build, Source};
use serde::Serialize;

use crate::config::ServerConfig;
use crate::server::{Catalog, secrets::SecretBox, store::Store};

#[derive(Debug, Serialize)]
pub struct Planned {
    pub app: String,
    pub change: Change,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Unchanged,
    /// Its container is recreated (a few seconds down).
    Restart,
    /// Its image is rebuilt (git: from the branch's latest commit) or pulled.
    Rebuild,
    /// Not running on this node now; nothing to compare.
    NotRunning,
}

pub async fn check(cfg: &ServerConfig, json: bool) -> anyhow::Result<()> {
    // The running server's database may predate this binary's schema: read a
    // copy, brought up to date the way this binary would, and never touch the
    // real one.
    let copy = cfg
        .data_dir
        .join("tmp")
        .join(format!("upgrade-check-{}.db", uuid::Uuid::new_v4()));
    crate::tls::create_private_dir(copy.parent().expect("has a parent"))?;
    Store::open_read_only(&cfg.data_dir.join("facade.db"))?.snapshot(&copy)?;
    let result = check_copy(cfg, &copy, json).await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", copy.display()));
    }
    result
}

async fn check_copy(cfg: &ServerConfig, copy: &std::path::Path, json: bool) -> anyhow::Result<()> {
    let store = Store::open(copy)?;
    let secrets = SecretBox::load_or_create(&cfg.data_dir.join("master.key"))?;
    let docker = Docker::connect_with_local_defaults().context("connecting to Docker")?;

    // What runs now: app → (run key, image) from container labels.
    let filters = HashMap::from([(
        "label".to_string(),
        vec![format!("facade.node={}", cfg.local_node)],
    )]);
    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        }))
        .await?;
    let running: HashMap<String, String> = containers
        .into_iter()
        .filter_map(|c| {
            let labels = c.labels?;
            Some((
                labels.get("facade.app")?.clone(),
                labels.get("facade.run")?.clone(),
            ))
        })
        .collect();

    let mut planned = Vec::new();
    for row in store.list_apps()? {
        let spec = &row.spec;
        if spec.node != cfg.local_node || row.revision == 0 || !spec.enabled || spec.is_static() {
            continue;
        }
        let w = Catalog {
            store: &store,
            secrets: &secrets,
        }
        .workload(&spec.name)?;
        let image = crate::agent::planned_image(&w).expect("only static sites have no image");
        let change = match running.get(&spec.name) {
            None => Change::NotRunning,
            Some(_) if docker.inspect_image(&image).await.is_err() => Change::Rebuild,
            Some(key) if *key != crate::agent::run_key(&w, &image) => Change::Restart,
            Some(_) => Change::Unchanged,
        };
        planned.push(Planned {
            app: spec.name.clone(),
            change,
        });
        if json {
            continue;
        }
        let what = match change {
            Change::Unchanged => "unchanged".to_string(),
            Change::Restart => "RESTARTS: its container is recreated (a few seconds down)".into(),
            Change::Rebuild => match &spec.source {
                Source::Git { build, .. } if !matches!(build, Build::Static { .. }) => {
                    "REBUILDS from the latest commit of its branch, then restarts".into()
                }
                Source::Image { .. } => "PULLS its image again, then restarts".into(),
                _ => "REBUILDS its image, then restarts".into(),
            },
            Change::NotRunning => "not running now".into(),
        };
        println!("  {:<24} {what}", spec.name);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&planned)?);
    } else if planned.is_empty() {
        println!("  (no running apps on {})", cfg.local_node);
    }
    Ok(())
}

pub fn backup_state(cfg: &ServerConfig) -> anyhow::Result<()> {
    let store = Store::open_read_only(&cfg.data_dir.join("facade.db"))?;
    let dir = crate::server::backups::snapshot_state(&cfg.data_dir, &store, cfg.backups.keep)?;
    println!("{}", dir.display());
    Ok(())
}
