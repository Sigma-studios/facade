use std::time::Duration;

use facade_core::api::{AppView, BuildsView, DeploymentView, QueuedBuild};
use facade_core::proto::BuildState;
use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_query_map};

use super::app::DeployStatus;
use crate::api;
use crate::components::{ErrorNote, Loading, MultiSelect, Sort, SortKey, SortTh};
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

/// The statuses the filter offers, as the API writes them.
const STATUSES: [&str; 5] = ["queued", "running", "succeeded", "failed", "cancelled"];

/// What every node builds now, what waits, and the latest deployments; filtered
/// by app and status from the URL (`?app=web,api&status=failed`).
#[component]
pub fn Builds() -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let picked = Memo::new(move |_| {
        query
            .read()
            .get("app")
            .map(|a| {
                a.split(',')
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let status = Memo::new(move |_| query.read().get("status").unwrap_or_default());
    let set_filters = move |apps: Vec<String>, status: String| {
        let mut params = vec![];
        if !apps.is_empty() {
            params.push(format!("app={}", apps.join(",")));
        }
        if !status.is_empty() {
            params.push(format!("status={status}"));
        }
        let url = if params.is_empty() {
            "/builds".to_string()
        } else {
            format!("/builds?{}", params.join("&"))
        };
        navigate(
            &url,
            leptos_router::NavigateOptions {
                replace: true,
                scroll: false,
                ..Default::default()
            },
        );
    };

    let builds = polled(Some(Duration::from_secs(2)), || async {
        api::get::<BuildsView>("/api/builds").await
    });
    let apps = polled(Some(Duration::from_secs(30)), || async {
        api::get::<Vec<AppView>>("/api/apps").await
    });
    // The latest across every app are only the last few dozen: with apps picked,
    // their own histories.
    let histories = polled(Some(Duration::from_secs(5)), move || {
        let picked = picked.get();
        async move {
            let lists = futures_util::future::join_all(picked.iter().map(|app| {
                let path = format!("/api/apps/{app}/deployments");
                async move { api::get::<Vec<DeploymentView>>(&path).await }
            }))
            .await;
            // A deleted app (left in a bookmark, say) just has none.
            let mut all = lists.into_iter().flatten().flatten().collect::<Vec<_>>();
            all.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            Ok((picked, all))
        }
    });
    let options = Signal::derive(move || {
        let mut names = apps
            .data
            .with(|a| a.iter().flatten().map(|a| a.spec.name.clone()).collect::<Vec<_>>());
        builds.data.with(|b| {
            if let Some(b) = b {
                names.extend(b.queue.iter().map(|q| q.app.clone()));
                names.extend(b.recent.iter().map(|d| d.app.clone()));
            }
        });
        names.extend(picked.get());
        names.sort();
        names.dedup();
        names
    });
    let shows = move |app: &str| picked.with(|p| p.is_empty() || p.iter().any(|x| x == app));
    let filtered = move || picked.with(|p| !p.is_empty()) || status.with(|s| !s.is_empty());

    let cancel = move |id: uuid::Uuid| {
        run(
            "Deployment cancelled",
            async move { api::act(&format!("/api/deployments/{id}/cancel")).await },
            move || builds.reload(),
        );
    };
    let queue_sort = Sort::new();
    let recent_sort = Sort::new();
    let queue = Memo::new(move |_| {
        let mut queue = builds
            .data
            .get()
            .map(|b| b.queue)
            .unwrap_or_default()
            .into_iter()
            .filter(|b| shows(&b.app))
            .collect::<Vec<_>>();
        queue_sort.apply(&mut queue, queued_key);
        queue
    });
    let recent = Signal::derive(move || {
        let picked = picked.get();
        let status = status.get();
        // The picked apps' histories once they arrive; until then what the latest have.
        let mut recent = histories
            .data
            .get()
            .filter(|(p, _)| *p == picked && !picked.is_empty())
            .map(|(_, all)| all)
            .unwrap_or_else(|| {
                builds
                    .data
                    .get()
                    .map(|b| b.recent)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|d| shows(&d.app))
                    .collect()
            });
        recent.retain(|d| status.is_empty() || d.status.to_string() == status);
        recent_sort.apply(&mut recent, deployment_key);
        recent
    });
    let total_queue = move || builds.data.with(|b| b.as_ref().map_or(0, |b| b.queue.len()));

    view! {
        <div class="page">
            <header class="page-head"><h1>"Builds"</h1></header>
            <p class="muted lead">
                "Each node builds one app at a time; other deployments wait their turn here. A build that takes more than an hour is stopped."
            </p>
            <div class="toolbar filters" role="group" aria-label="Filters">
                <MultiSelect noun="apps" options picked
                    on_change=Callback::new({
                        let set_filters = set_filters.clone();
                        move |apps| set_filters(apps, status.get_untracked())
                    })/>
                <select class="filter-select" aria-label="Status"
                    class:filter-active=move || status.with(|s| !s.is_empty())
                    on:change={
                        let set_filters = set_filters.clone();
                        move |ev| set_filters(picked.get_untracked(), event_target_value(&ev))
                    }
                    prop:value=move || status.get()>
                    <option value="">"Any status"</option>
                    {STATUSES.into_iter().map(|s| view! {
                        <option value=s selected=move || status.with(|x| x == s)>{s}</option>
                    }).collect_view()}
                </select>
                <Show when=filtered>
                    <button type="button" class="link-button" on:click={
                        let set_filters = set_filters.clone();
                        move |_| set_filters(vec![], String::new())
                    }>"Clear filters"</button>
                </Show>
            </div>
            <ErrorNote error=builds.error/>
            <Show when=move || builds.loaded() fallback=Loading>
                <section>
                    <h2 class="section-title">"Queue"</h2>
                    <Show when=move || queue.with(|q| !q.is_empty())
                        fallback=move || view! { <div class="empty"><p>{move || if total_queue() > 0 {
                            "None of these apps is building."
                        } else {
                            "Nothing is building."
                        }}</p></div> }>
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
                                    <For each=move || queue.get()
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
                    <h2 class="section-title">
                        {move || if picked.with(|p| !p.is_empty()) { "Deployments" } else { "Recent deployments" }}
                    </h2>
                    <Show when=move || recent.with(|r| !r.is_empty())
                        fallback=move || view! { <div class="empty"><p>{move || if filtered() {
                            "No deployment matches."
                        } else {
                            "No deployments yet."
                        }}</p></div> }>
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
                                    <For each=move || recent.get()
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
                    </Show>
                </section>
            </Show>
        </div>
    }
}
