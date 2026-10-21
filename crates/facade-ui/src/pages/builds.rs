use std::time::Duration;

use facade_core::api::BuildsView;
use facade_core::proto::BuildState;
use leptos::prelude::*;
use leptos_router::components::A;

use super::app::DeployStatus;
use crate::api;
use crate::components::{ErrorNote, Loading};
use crate::util::{ago, polled, run};

fn short(commit: &Option<String>) -> String {
    commit
        .as_deref()
        .map(|c| c[..c.len().min(7)].to_string())
        .unwrap_or_default()
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
                                    <th>"Node"</th><th>"App"</th><th>"State"</th><th>"Commit"</th><th>"Since"</th><th></th>
                                </tr></thead>
                                <tbody>
                                    <For each=move || builds.data.get().map(|b| b.queue).unwrap_or_default()
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
                                <th>"App"</th><th>"Status"</th><th>"Commit"</th><th>"Requested"</th><th class="num">"Took"</th><th>"Error"</th>
                            </tr></thead>
                            <tbody>
                                <For each=move || builds.data.get().map(|b| b.recent).unwrap_or_default()
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
                                        <td class="num small">{match (d.started_at, d.finished_at) {
                                            (Some(s), Some(f)) => format!("{}s", f.saturating_sub(s)),
                                            _ => String::new(),
                                        }}</td>
                                        <td class="small error-cell">{d.error.clone().unwrap_or_default().lines().next().unwrap_or("").to_string()}</td>
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
