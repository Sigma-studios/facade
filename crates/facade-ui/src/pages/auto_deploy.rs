//! Auto-deploy on an app's or a stack's page: the switch, and how changes are
//! noticed (a webhook, or polling and what a webhook would need).

use std::time::Duration;

use facade_core::api::{AutoDeployMethod, AutoDeployView, SetAutoDeploy};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{ago, polled, toasts};

/// A commit's 7 characters, or a digest's first 12 hex digits.
fn short(hash: &str) -> String {
    match hash.strip_prefix("sha256:") {
        Some(hex) => format!("sha256:{}", hex.get(..12).unwrap_or(hex)),
        None => hash.get(..7).unwrap_or(hash).to_string(),
    }
}

fn every(secs: u64) -> String {
    if secs >= 120 {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// `path` is `/api/apps/<name>/auto-deploy` or `/api/stacks/<name>/auto-deploy`.
/// Shows nothing for sources that never change by themselves.
#[component]
pub fn AutoDeployPanel(#[prop(into)] path: Signal<String>) -> impl IntoView {
    let status = polled(Some(Duration::from_secs(20)), move || {
        let path = path.get();
        async move { api::get::<AutoDeployView>(&path).await }
    });
    let saving = RwSignal::new(false);
    let set = move |enabled: bool| {
        saving.set(true);
        let path = path.get_untracked();
        spawn_local(async move {
            match api::put::<_, AutoDeployView>(&path, &SetAutoDeploy { enabled }).await {
                Ok(v) => {
                    status.data.try_set(Some(v));
                }
                Err(e) => {
                    toasts().err(e);
                    status.reload();
                }
            }
            saving.try_set(false);
        });
    };
    move || {
        status.data.get().filter(|s| s.watching.is_some()).map(|s| {
            let watching = s.watching.clone().unwrap_or_default();
            let how = match s.method {
                AutoDeployMethod::Webhook => format!(
                    "Webhook on {}{} · checked every {} as well",
                    s.webhook_repo.clone().unwrap_or_default(),
                    s.webhook_delivered_at
                        .map(|at| format!(", last delivery {}", ago(at)))
                        .unwrap_or_default(),
                    every(s.poll_secs)
                ),
                _ => format!("Polling every {}", every(s.poll_secs)),
            };
            let last_check = s.checked_at.map(|at| {
                format!(
                    "{}{}",
                    ago(at),
                    s.seen
                        .as_deref()
                        .map(|h| format!(" · {}", short(h)))
                        .unwrap_or_default()
                )
            });
            let last_deploy = match (s.triggered_at, &s.triggered) {
                (Some(at), Some(what)) => format!(
                    "{} {} (noticed by {})",
                    short(what),
                    ago(at),
                    s.triggered_by.as_deref().unwrap_or("poll")
                ),
                _ => "—".into(),
            };
            view! {
                <section class="card details auto-deploy">
                    <h2 class="section-title">"Auto-deploy"</h2>
                    <label class="check">
                        <input type="checkbox" prop:checked=s.enabled disabled=move || saving.get()
                            on:change=move |ev| set(event_target_checked(&ev))/>
                        {format!("Deploy when {watching} changes")}
                    </label>
                    {if s.enabled {
                        view! {
                            <dl class="kv">
                                <dt>"Noticed by"</dt>
                                <dd>{how}</dd>
                                <dt>"Last check"</dt>
                                <dd>{last_check.unwrap_or_else(|| "not yet".into())}</dd>
                                <dt>"Last auto deploy"</dt>
                                <dd>{last_deploy}</dd>
                            </dl>
                        }.into_any()
                    } else {
                        view! {
                            <p class="muted small">
                                "A new commit on the branch, or a new image for the tag, is deployed like Deploy does. \
                                 Turning it on catches up at once if behind."
                            </p>
                        }.into_any()
                    }}
                    {s.note.clone().filter(|_| s.enabled).map(|note| view! {
                        <p class="muted small">{note}</p>
                    })}
                    {s.error.clone().map(|e| view! {
                        <div class="note note-error" role="alert">
                            <span aria-hidden="true">"⚠ "</span>{e}
                        </div>
                    })}
                </section>
            }
        })
    }
}
