use std::time::Duration;

use facade_core::api::{BuildsView, DeploymentView, QueuedBuild};
use facade_core::proto::BuildState;
use leptos::prelude::*;
use leptos_router::components::A;

use super::app::DeployStatus;
use crate::api;
use crate::components::{ErrorNote, Loading, Sort, SortKey, SortTh};
use crate::util::{ago, polled, run};

fn short(commit: &Option<String>) -> String {
    commit
        .as_deref()
        .map(|c| c[..c.len().min(7)].to_string())
        .unwrap_or_default()
}

/// Seconds a finished deployment took.
fn took(started: Option<u64>, finished: Option<u64>) -> Option<u64> {
    Some(finished?.saturating_sub(started?))
}

fn error_line(error: &Option<String>) -> &str {
    error
        .as_deref()
        .and_then(|e| e.lines().next())
        .unwrap_or("")
}

fn queued_key(col: usize, b: &QueuedBuild) -> SortKey {
    match col {
        0 => SortKey::text(&b.node),
        1 => SortKey::text(&b.app),
        // Building (0) first, then the line.
        2 => SortKey::num(Some(b.position as f64)),
        3 => SortKey::text(short(&b.commit)),
        _ => SortKey::num(Some(b.since as f64)),
    }
}

fn deployment_key(col: usize, d: &DeploymentView) -> SortKey {
    match col {
        0 => SortKey::text(&d.app),
        1 => SortKey::text(d.status.to_string()),
        // By its title when known: that is what the cell reads as.
        2 => SortKey::text(d.commit_title.clone().unwrap_or_else(|| short(&d.commit))),
        3 => SortKey::num(Some(d.created_at as f64)),
        4 => SortKey::num(took(d.started_at, d.finished_at).map(|t| t as f64)),
        _ => SortKey::text(error_line(&d.error)),
    }
}

/// What every node builds now, what waits, and the latest deployments.
#[component]
pub fn Builds() -> impl IntoView {
    let builds = polled(Some(Duration::from_secs(2)), || async {
        api::get::<BuildsView>("/api/builds").await
    });
    let cancel = move |id: uuid::Uuid| {
        run(
            "Deployment cancelled",
            async move { api::act(&format!("/api/deployments/{id}/cancel")).await },
            move || builds.reload(),
        );
    };
    let queue_sort = Sort::new();
    let recent_sort = Sort::new();
    view! {
        <div class="page">
            <header class="page-head"><h1>"Builds"</h1></header>
            <p class="muted lead">
                "Each node builds one app at a time; other deployments wait their turn here. A build that takes more than an hour is stopped."
            </p>
            <ErrorNote error=builds.error/>
            <Show when=move || builds.loaded() fallback=Loading>
                <section>
                    <h2 class="section-title">"Queue"</h2>
                    <Show when=move || builds.data.with(|b| b.as_ref().is_some_and(|b| !b.queue.is_empty()))
                        fallback=|| view! { <div class="empty"><p>"Nothing is building."</p></div> }>
                        <div class="table-wrap">
                            <table class="table">
                                <thead><tr>
                                    <SortTh sort=queue_sort col=0 label="Node"/>
                                    <SortTh sort=queue_sort col=1 label="App"/>
                                    <SortTh sort=queue_sort col=2 label="State"/>
                                    <SortTh sort=queue_sort col=3 label="Commit"/>
                                    <SortTh sort=queue_sort col=4 label="Since" desc=true/>
                                    <th></th>
                                </tr></thead>
                                <tbody>
                                    <For each=move || {
                                            let mut queue = builds.data.get().map(|b| b.queue).unwrap_or_default();
                                            queue_sort.apply(&mut queue, queued_key);
                                            queue
                                        }
                                        key=|b| (b.node.clone(), b.app.clone(), b.deployment, b.state, b.position)
                                        let:b>
                                        <tr>
                                            <td class="muted">{b.node.clone()}</td>
                                            <td><A href=format!("/apps/{}?tab=deployments", b.app) attr:class="strong-link">{b.app.clone()}</A></td>
                                            <td>{match b.state {
                                                BuildState::Building => view! {
                                                    <span class="badge badge-busy"><span class="badge-icon" aria-hidden="true">"◐"</span>"building"</span>
                                                }.into_any(),
                                                BuildState::Queued => view! {
                                                    <span class="badge badge-neutral"><span class="badge-icon" aria-hidden="true">"○"</span>{format!("queued, #{}", b.position)}</span>
                                                }.into_any(),
                                            }}</td>
                                            <td><code>{short(&b.commit)}</code></td>
                                            <td class="muted small">{ago(b.since)}</td>
                                            <td class="row-actions">
                                                {match b.deployment {
                                                    Some(id) => view! {
                                                        <button class="link-button danger" on:click=move |_| cancel(id)>"Cancel"</button>
                                                    }.into_any(),
                                                    None => view! { <span class="muted small" title="started by the node to repair the app">"repair"</span> }.into_any(),
                                                }}
                                            </td>
                                        </tr>
                                    </For>
                                </tbody>
                            </table>
                        </div>
                    </Show>
                </section>
                <section>
                    <h2 class="section-title">"Recent deployments"</h2>
                    <div class="table-wrap">
                        <table class="table">
                            <thead><tr>
                                <SortTh sort=recent_sort col=0 label="App"/>
                                <SortTh sort=recent_sort col=1 label="Status"/>
                                <SortTh sort=recent_sort col=2 label="Commit"/>
                                <SortTh sort=recent_sort col=3 label="Requested" desc=true/>
                                <SortTh sort=recent_sort col=4 label="Took" num=true desc=true/>
                                <SortTh sort=recent_sort col=5 label="Error"/>
                            </tr></thead>
                            <tbody>
                                <For each=move || {
                                        let mut recent = builds.data.get().map(|b| b.recent).unwrap_or_default();
                                        recent_sort.apply(&mut recent, deployment_key);
                                        recent
                                    }
                                    key=|d| (d.id, d.status, d.commit_title.clone())
                                    let:d>
                                    <tr>
                                        <td><A href=format!("/apps/{}?tab=deployments", d.app) attr:class="strong-link">{d.app.clone()}</A></td>
                                        <td><DeployStatus status=d.status/></td>
                                        <td>
                                            <code>{short(&d.commit)}</code>
                                            {d.commit_title.clone().map(|t| view! { <span class="muted small">" " {t}</span> })}
                                        </td>
                                        <td class="muted small">{ago(d.created_at)}</td>
                                        <td class="num small">{took(d.started_at, d.finished_at).map(|t| format!("{t}s"))}</td>
                                        <td class="small error-cell">{error_line(&d.error).to_string()}</td>
                                    </tr>
                                </For>
                            </tbody>
                        </table>
                    </div>
                </section>
            </Show>
        </div>
    }
}
