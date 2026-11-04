//! The server's side of [`facade_core::resources`]: what each report adds to
//! the history (counters, health checks, volume sizes), the last minutes'
//! figures for an app's page, and the routes reading the history.

use std::collections::VecDeque;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use facade_core::api::{AppMetric, VolumeSizes};
use facade_core::proto::{AppStats, NodeReport};
use facade_core::resources::{AppResources, HealthCheck, NodeResources, ResourcePoint};
use tracing::warn;

use super::Server;
use super::api::{ApiResult, User};
use super::store::USAGE_KEEP;
use super::users::load_app_for;

/// Seconds the "recent" figures of an app's page cover.
const RECENT: u64 = 300;

/// What a report brings to keep, gathered while the live state is updated and
/// written once it is released.
#[derive(Default)]
pub struct Pending {
    apps: Vec<(String, AppResources)>,
    health: Vec<(String, Vec<HealthCheck>)>,
    volumes: Option<(u64, Vec<facade_core::proto::VolumeUsage>)>,
}

impl Pending {
    pub fn new(report: &NodeReport) -> Self {
        Self {
            volumes: report.storage.as_ref().map(|s| (s.at, s.volumes.clone())),
            ..Default::default()
        }
    }

    /// An app's reading, `before` its previous one.
    pub fn app(&mut self, stats: &AppStats, before: Option<&AppMetric>) {
        if stats.resources.is_empty() {
            return;
        }
        self.apps.push((stats.app.clone(), stats.resources.clone()));
        // Docker lists its last five checks: those after the newest of the
        // previous report are new.
        let seen = before
            .and_then(|m| m.stats.resources.health.last())
            .map(|c| c.at_ms);
        let fresh: Vec<HealthCheck> = stats
            .resources
            .health
            .iter()
            .filter(|c| seen.is_none_or(|s| c.at_ms > s))
            .cloned()
            .collect();
        if !fresh.is_empty() {
            self.health.push((stats.app.clone(), fresh));
        }
    }

    pub fn store(self, server: &Server, at: u64, node: &str, resources: &NodeResources) {
        let store = &server.store;
        let result = store
            .record_resources(at, &self.apps, (node, resources))
            .and_then(|()| {
                self.health
                    .iter()
                    .try_for_each(|(app, checks)| store.record_health(app, checks))
            })
            .and_then(|()| match &self.volumes {
                Some((at, volumes)) => store.record_volume_sizes(*at, volumes),
                None => Ok(()),
            });
        if let Err(e) = result {
            warn!("recording resources: {e:#}");
        }
    }
}

/// An app's figures over the last minutes, from its live history; `None`
/// without any.
pub fn recent(history: &VecDeque<AppMetric>) -> Option<ResourcePoint> {
    let last = history.back()?;
    let first = history.iter().find(|m| m.at + RECENT >= last.at)?;
    let point = ResourcePoint::app_between(
        (first.at, &first.stats.resources),
        (last.at, &last.stats.resources),
    );
    (!point.is_empty()).then_some(point)
}

/// `GET /api/apps/{name}/volume-sizes`: its volumes' sizes over the last 30 days.
pub async fn volume_sizes(
    State(server): State<Arc<Server>>,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<VolumeSizes>>> {
    load_app_for(&server, &user, &name)?;
    let since = facade_core::now().saturating_sub(USAGE_KEEP);
    Ok(Json(server.store.volume_sizes(&name, since)?))
}

/// `GET /api/apps/{name}/health-checks`: its last health checks, oldest first.
pub async fn health_checks(
    State(server): State<Arc<Server>>,
    user: User,
    Path(name): Path<String>,
) -> ApiResult<Json<Vec<HealthCheck>>> {
    load_app_for(&server, &user, &name)?;
    Ok(Json(server.store.health_checks(&name)?))
}
