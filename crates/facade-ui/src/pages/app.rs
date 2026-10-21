use std::time::Duration;

use facade_core::api::{
    AppMetric, AppView, BackupReport, BackupRequest, BackupView, CommitView, DeployRequest,
    DeploymentStatus, DeploymentView, DomainStatus, EnvPatch, EnvView, NodeView, SetEnv, SetOwner,
    UpdateCheck, UserView, VolumeView,
};
use facade_core::proto::AppState;
use facade_core::{Build, Source};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};
use uuid::Uuid;

use super::domains::DomainTable;
use super::spec_form::{SpecFields, SpecForm};
use crate::api;
use crate::components::{ErrorNote, Loading, Modal, Sparkline, StateBadge};
use crate::util::{ago, bytes, clock, date, me, polled, run, short_hash, strip_ansi, toasts};

const TABS: &[(&str, &str)] = &[
    ("overview", "Overview"),
    ("logs", "Logs"),
    ("deployments", "Deployments"),
    ("env", "Environment"),
    ("volumes", "Volumes"),
    ("settings", "Settings"),
];

#[component]
pub fn AppPage() -> impl IntoView {
    let me = me();
    let params = use_params_map();
    let query = use_query_map();
    let navigate = use_navigate();
    let name = Memo::new(move |_| params.read().get("name").unwrap_or_default());
    let tab = Memo::new(move |_| query.read().get("tab").unwrap_or_else(|| "overview".into()));
    let selected_deployment = RwSignal::new(None::<Uuid>);

    let app = polled(Some(Duration::from_secs(3)), move || {
        let name = name.get();
        async move { api::get::<AppView>(&format!("/api/apps/{name}")).await }
    });
    let set_tab = {
        let navigate = navigate.clone();
        move |t: &str| {
            navigate(
                &format!("/apps/{}?tab={t}", name.get_untracked()),
                leptos_router::NavigateOptions {
                    replace: true,
                    scroll: false,
                    ..Default::default()
                },
            )
        }
    };

    let busy = RwSignal::new(false);
    // The stack a service belongs to, which deploys it.
    let stack = move || {
        app.data.with(|a| {
            a.as_ref()
                .and_then(|a| a.spec.stack.as_ref().map(|s| s.stack.clone()))
        })
    };
    let deploy = {
        let set_tab = set_tab.clone();
        let navigate = navigate.clone();
        // `commit`: a specific one to deploy (the update check's), or the branch's latest.
        move |commit: Option<String>| {
            busy.set(true);
            let set_tab = set_tab.clone();
            if let Some(stack) = stack() {
                let navigate = navigate.clone();
                spawn_local(async move {
                    match api::act_json::<facade_core::api::StackDeploymentView>(&format!(
                        "/api/stacks/{stack}/deploy"
                    ))
                    .await
                    {
                        Ok(_) => navigate(
                            &format!("/stacks/{stack}?tab=deployments"),
                            Default::default(),
                        ),
                        Err(e) => toasts().err(e),
                    }
                    busy.set(false);
                });
                return;
            }
            let path = format!("/api/apps/{}/deploy", name.get_untracked());
            let body = DeployRequest {
                commit,
                rebuild: false,
            };
            spawn_local(async move {
                match api::post::<_, DeploymentView>(&path, &body).await {
                    Ok(d) => {
                        selected_deployment.set(Some(d.id));
                        set_tab("deployments");
                        app.reload();
                    }
                    Err(e) => toasts().err(e),
                }
                busy.set(false);
            });
        }
    };
    // For the Update button of the overview's update check.
    let on_deploy = Callback::new({
        let deploy = deploy.clone();
        move |commit: Option<String>| deploy(commit)
    });
    let simple = move |action: &'static str, done: &'static str| {
        let path = format!("/api/apps/{}/{action}", name.get_untracked());
        run(done, async move { api::act(&path).await }, move || {
            app.reload()
        });
    };

    let confirm_delete = RwSignal::new(false);
    let typed = RwSignal::new(String::new());
    let delete_volumes = RwSignal::new(false);
    let delete = {
        let navigate = navigate.clone();
        move |_| {
            let navigate = navigate.clone();
            let n = name.get_untracked();
            let volumes = delete_volumes.get_untracked();
            run(
                format!("{n} deleted"),
                async move { api::delete(&format!("/api/apps/{n}?volumes={volumes}")).await },
                move || navigate("/apps", Default::default()),
            );
        }
    };

    let view_of = move || app.data.get();
    let state = Signal::derive(move || view_of().map(|a| a.state).unwrap_or(AppState::Unknown));
    let enabled = move || view_of().is_some_and(|a| a.spec.enabled);
    let deployed = move || view_of().is_some_and(|a| a.revision > 0);
    let has_volumes = Memo::new(move |_| {
        app.data
            .with(|a| a.as_ref().is_some_and(|a| !a.spec.volumes.is_empty()))
    });
    // A memo, so the tabs are only rebuilt when this changes, not at every poll
    // (which would reset the settings form while it is being edited).
    let stack_service = Memo::new(move |_| {
        app.data.with(|a| {
            let spec = &a.as_ref()?.spec;
            Some((
                spec.stack.as_ref()?.stack.clone(),
                spec.service()?.to_string(),
            ))
        })
    });

    view! {
        <div class="page">
            <header class="page-head">
                <div>
                    {move || match stack() {
                        Some(stack) => view! {
                            <A href=format!("/stacks/{stack}") attr:class="crumb">{format!("← Stack {stack}")}</A>
                        }.into_any(),
                        None => view! { <A href="/apps" attr:class="crumb">"← Apps"</A> }.into_any(),
                    }}
                    <h1 class="title-row">
                        {move || name.get()}
                        <Show when=move || app.loaded()>
                            <StateBadge state=state/>
                        </Show>
                    </h1>
                    <div class="muted small subtitle">
                        {move || view_of().map(|a| subtitle(&a))}
                        {move || {
                            view_of()
                                .and_then(|a| a.owner)
                                .filter(|_| me.is_admin())
                                .map(|o| format!(" · owned by {o}"))
                        }}
                    </div>
                </div>
                <div class="actions">
                    <button class="button button-primary" disabled=move || busy.get() || !app.loaded()
                        on:click={
                            let deploy = deploy.clone();
                            move |_| deploy(None)
                        }
                        title="Fetch the latest code or image, rebuild and restart">
                        {move || if busy.get() {
                            "Starting…"
                        } else if stack().is_some() {
                            "Deploy stack"
                        } else if deployed() {
                            "Deploy"
                        } else {
                            "First deploy"
                        }}
                    </button>
                    <button class="button" disabled=move || !deployed()
                        on:click=move |_| simple("restart", "Restarted")>"Restart"</button>
                    <Show
                        when=enabled
                        fallback=move || view! {
                            <button class="button" on:click=move |_| simple("start", "Started")>"Start"</button>
                        }
                    >
                        <button class="button" disabled=move || !deployed()
                            on:click=move |_| simple("stop", "Stopped")>"Stop"</button>
                    </Show>
                </div>
            </header>
            <ErrorNote error=app.error/>
            {move || view_of().and_then(|a| {
                let stack = a.spec.stack.clone()?;
                let service = a.spec.service()?.to_string();
                Some(view! {
                    <div class="note">
                        "Service " <strong>{service}</strong> " of stack "
                        <A href=format!("/stacks/{}", stack.stack)>{stack.stack.clone()}</A>
                        ", reached by the other services as " <code>{stack.host.clone()}</code>
                        ". It is deployed with its stack; its settings come from the stack (and its compose file)."
                    </div>
                })
            })}
            <Show when=move || state.get() == AppState::Crashed && view_of().is_some_and(|a| a.message.is_none())>
                <div class="note note-error">
                    "The container keeps stopping. Its "
                    <a href=move || format!("/apps/{}?tab=logs", name.get())>"logs"</a>
                    " usually say why."
                </div>
            </Show>
            {move || view_of().and_then(|a| a.message).map(|m| view! {
                <div class=move || if state.get() == AppState::Failed { "note note-error" } else { "note" }>
                    {m}
                </div>
            })}

            <div class="tabs" role="tablist">
                {move || TABS.iter().filter(|(key, _)| *key != "volumes" || has_volumes.get()).map(|(key, label)| {
                    let key = *key;
                    let set_tab = set_tab.clone();
                    view! {
                        <button role="tab" class=move || if tab.get() == key { "tab active" } else { "tab" }
                            aria-selected=move || (tab.get() == key).to_string()
                            on:click=move |_| set_tab(key)>
                            {*label}
                        </button>
                    }
                }).collect_view()}
            </div>

            <Show when=move || app.loaded() fallback=Loading>
                {move || match tab.get().as_str() {
                    "logs" => view! { <LogsTab name=name/> }.into_any(),
                    "deployments" => view! {
                        <DeploymentsTab name=name selected=selected_deployment
                            current=Signal::derive(move || view_of().and_then(|a| a.commit))
                            on_deployed=move || app.reload()/>
                    }.into_any(),
                    "env" => view! {
                        <EnvTab path=Signal::derive(move || format!("/api/apps/{}/env", name.get()))
                            saved=Signal::derive(move || if deployed() {
                                "Saved. The app restarts with the new values.".to_string()
                            } else {
                                "Saved. It will be used from the first deploy.".to_string()
                            })/>
                    }.into_any(),
                    "volumes" => view! { <VolumesTab name=name app=Signal::derive(move || app.data.get())/> }.into_any(),
                    // Stacks are managed by admins; members edit a service as a plain app.
                    "settings" => match stack_service.get().filter(|_| me.is_admin()) {
                        Some((stack, service)) => view! {
                            <super::stack::ServiceSettings stack=stack service=service
                                on_saved=Callback::new(move |_| app.reload())/>
                        }.into_any(),
                        None => view! {
                            <SettingsTab app=app.data.get_untracked().expect("loaded")
                                live=Signal::derive(move || app.data.get())
                                on_saved=move || app.reload() on_delete=move || confirm_delete.set(true)/>
                        }.into_any(),
                    },
                    _ => view! {
                        <OverviewTab name=name app=Signal::derive(move || app.data.get())
                            deploying=busy on_deploy=on_deploy/>
                    }.into_any(),
                }}
            </Show>

            <Modal open=confirm_delete title="Delete app">
                <p>"This removes the container, its image and all its environment variables. It cannot be undone."</p>
                <Show when=move || has_volumes.get()>
                    <label class="check">
                        <input type="checkbox" bind:checked=delete_volumes/>
                        "Also delete its volumes and the data in them"
                    </label>
                    <p class="muted small">
                        {move || if delete_volumes.get() {
                            "The volumes and their files are deleted with the app. Backups are kept.".to_string()
                        } else {
                            format!(
                                "Otherwise they stay on {} (Nodes page), and an app created with the same name gets them back.",
                                view_of().map(|a| a.spec.node).unwrap_or_default()
                            )
                        }}
                    </p>
                </Show>
                <label class="field">
                    <span>"Type " <code>{move || name.get()}</code> " to confirm"</span>
                    <input type="text" bind:value=typed autocomplete="off"/>
                </label>
                <div class="form-actions">
                    <button class="button" on:click=move |_| confirm_delete.set(false)>"Cancel"</button>
                    <button class="button button-danger"
                        disabled=move || typed.get() != name.get()
                        on:click=delete.clone()>"Delete"</button>
                </div>
            </Modal>
        </div>
    }
}

fn subtitle(app: &AppView) -> String {
    let source = match &app.spec.source {
        Source::Image { image } => format!("image {image}"),
        Source::Executable { filename, arch, .. } => format!("executable {filename} ({arch})"),
        Source::Git { repo, build, .. } => {
            let kind = match build {
                Build::Dockerfile { .. } => "Dockerfile",
                Build::Rust { .. } => "Rust",
                Build::Python { .. } => "Python",
                Build::Static { .. } => "static site",
            };
            let commit = app
                .commit
                .as_deref()
                .map(|c| format!(" · {}", short_hash(c)))
                .unwrap_or_default();
            format!("{} · {kind}{commit}", repo.trim_start_matches("https://"))
        }
    };
    let deployed = match (&app.commit, app.revision) {
        (_, 0) => "never deployed".to_string(),
        (Some(commit), _) => format!("commit {}", &commit[..commit.len().min(7)]),
        (None, revision) => format!("revision {revision}"),
    };
    format!("on {} · {source} · {deployed}", app.spec.node)
}

// ---------------------------------------------------------------- overview

#[component]
fn OverviewTab(
    name: Memo<String>,
    #[prop(into)] app: Signal<Option<AppView>>,
    deploying: RwSignal<bool>,
    on_deploy: Callback<Option<String>>,
) -> impl IntoView {
    let metrics = polled(Some(Duration::from_secs(15)), move || {
        let name = name.get();
        async move { api::get::<Vec<AppMetric>>(&format!("/api/apps/{name}/metrics")).await }
    });
    let series = move |f: fn(&AppMetric) -> f64| {
        Signal::derive(move || {
            metrics.data.with(|m| {
                m.iter()
                    .flatten()
                    .filter(|m| m.stats.state == AppState::Running)
                    .map(|m| (m.at, f(m)))
                    .collect::<Vec<_>>()
            })
        })
    };
    let cpu = series(|m| m.stats.cpu_percent as f64);
    let mem = series(|m| m.stats.mem_used as f64);
    let spec = move || app.get().map(|a| a.spec);
    let stats = move || app.get().and_then(|a| a.stats);
    let cpu_max = Signal::derive(move || spec().and_then(|s| s.limits.cpus).map(|c| c * 100.0));
    let mem_max = Signal::derive(move || {
        spec()
            .and_then(|s| s.limits.memory_mb)
            .map(|m| (m * 1024 * 1024) as f64)
    });
    let is_static = move || spec().is_some_and(|s| s.is_static());
    let from_git = move || spec().is_some_and(|s| matches!(s.source, Source::Git { .. }));
    let commit = move || app.get().and_then(|a| a.commit);
    let domains = polled(Some(Duration::from_secs(30)), || async {
        api::get::<Vec<DomainStatus>>("/api/domains").await
    });

    view! {
        <Show when=move || !is_static() fallback=|| view! {
            <p class="muted">"Static sites are served by the agent directly; they have no container to measure."</p>
        }>
            <div class="tiles">
                <div class="tile">
                    <span class="tile-label">"CPU"</span>
                    <span class="tile-value">{move || stats().map(|s| format!("{:.1} %", s.cpu_percent)).unwrap_or_else(|| "—".into())}</span>
                    <span class="tile-sub">{move || spec().and_then(|s| s.limits.cpus).map(|c| format!("limit {c} CPU")).unwrap_or_else(|| "no limit".into())}</span>
                </div>
                <div class="tile">
                    <span class="tile-label">"Memory"</span>
                    <span class="tile-value">{move || stats().filter(|s| s.mem_used > 0).map(|s| bytes(s.mem_used)).unwrap_or_else(|| "—".into())}</span>
                    <span class="tile-sub">{move || spec().and_then(|s| s.limits.memory_mb).map(|m| format!("limit {m} MB")).unwrap_or_else(|| "no limit".into())}</span>
                </div>
                <div class="tile">
                    <span class="tile-label">"Restarts"</span>
                    <span class="tile-value">{move || stats().map(|s| s.restarts.to_string()).unwrap_or_else(|| "—".into())}</span>
                    <span class="tile-sub">"since the container was created"</span>
                </div>
                <div class="tile">
                    <span class="tile-label">"Network"</span>
                    <span class="tile-value">{move || stats().map(|s| format!("↓ {}", bytes(s.net_rx))).unwrap_or_else(|| "—".into())}</span>
                    <span class="tile-sub">{move || stats().map(|s| format!("↑ {} sent", bytes(s.net_tx))).unwrap_or_default()}</span>
                </div>
            </div>
            <div class="charts">
                <Sparkline title="CPU, last hour" points=cpu max=cpu_max format=|v| format!("{v:.1} %")/>
                <Sparkline title="Memory, last hour" points=mem max=mem_max format=|v| bytes(v as u64)/>
            </div>
        </Show>
        <Show when=move || spec().is_some_and(|s| !s.domains.is_empty())>
            <section class="app-domains">
                <h2 class="section-title">"Domains"</h2>
                <DomainTable domains=Signal::derive(move || {
                    let name = name.get();
                    domains.data.get().unwrap_or_default().into_iter()
                        .filter(|d| d.app.as_deref() == Some(name.as_str()))
                        .collect::<Vec<_>>()
                }) show_app=false/>
            </section>
        </Show>
        <section class="card details">
            <h2 class="section-title">"Configuration"</h2>
            {move || spec().map(|s| view! {
                <dl class="kv">
                    <dt>"Source"</dt>
                    <dd>{match &s.source {
                        Source::Image { image } => format!("Docker image {image}"),
                        Source::Executable { filename, arch, args, .. } => format!(
                            "Executable {filename} ({arch}){}",
                            if args.is_empty() { String::new() } else { format!(", run with {}", facade_core::join_command(args)) }
                        ),
                        Source::Git { repo, branch, .. } => format!("{repo} ({})", branch.clone().unwrap_or_else(|| "default branch".into())),
                    }}</dd>
                    {matches!(s.source, Source::Git { .. }).then(|| view! {
                        <dt>"Deployed commit"</dt>
                        <dd>{move || match commit() {
                            Some(c) => view! { <code title=c.clone()>{short_hash(&c).to_string()}</code> }.into_any(),
                            None => view! { <span>"—"</span> }.into_any(),
                        }}</dd>
                    })}
                    <dt>"HTTP port"</dt>
                    <dd>{s.http_port.map(|p| p.to_string()).unwrap_or_else(|| "—".into())}</dd>
                    <dt>"Published ports"</dt>
                    <dd>{if s.host_network { "node network".to_string() } else if s.ports.is_empty() { "—".into() } else {
                        s.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
                    }}</dd>
                    {(!s.command.is_empty()).then(|| view! {
                        <dt>"Command"</dt><dd><code>{facade_core::join_command(&s.command)}</code></dd>
                    })}
                    {s.release_command.clone().map(|r| view! {
                        <dt>"Release command"</dt><dd><code>{r}</code></dd>
                    })}
                    {(!s.redirects.is_empty()).then(|| view! {
                        <dt>"Redirects"</dt>
                        <dd>{format!("{} → {}", s.redirects.join(", "), s.domains.first().cloned().unwrap_or_default())}</dd>
                    })}
                    {(!s.volumes.is_empty()).then(|| view! {
                        <dt>"Volumes"</dt>
                        <dd>{s.volumes.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")}</dd>
                    })}
                    {(!s.links.is_empty()).then(|| view! {
                        <dt>"Links"</dt>
                        <dd>{s.links.iter().map(|l| if l.host() == l.app {
                            l.app.clone()
                        } else {
                            format!("{} (as {})", l.app, l.host())
                        }).collect::<Vec<_>>().join(", ")}</dd>
                    })}
                </dl>
            })}
            <Show when=from_git>
                <UpdateCheckPanel name=name deploying=deploying on_deploy=on_deploy/>
            </Show>
        </section>
    }
}

/// Compares the deployed commit with the branch on demand: fetching the repository
/// takes a few seconds.
#[component]
fn UpdateCheckPanel(
    name: Memo<String>,
    deploying: RwSignal<bool>,
    on_deploy: Callback<Option<String>>,
) -> impl IntoView {
    let checking = RwSignal::new(false);
    let result = RwSignal::new(None::<UpdateCheck>);
    let error = RwSignal::new(None::<String>);
    let check = move |_| {
        checking.set(true);
        error.set(None);
        let path = format!("/api/apps/{}/updates", name.get_untracked());
        spawn_local(async move {
            match api::act_json::<UpdateCheck>(&path).await {
                Ok(r) => {
                    result.try_set(Some(r));
                }
                Err(e) => {
                    result.try_set(None);
                    error.try_set(Some(e));
                }
            }
            checking.try_set(false);
        });
    };
    let summary = move || {
        result.get().map(|r| {
            let branch = r
                .branch
                .clone()
                .unwrap_or_else(|| "the default branch".into());
            match r.behind {
                Some(0) => format!("Up to date at {}.", short_hash(&r.latest.hash)),
                Some(n) => format!(
                    "{n} commit{} behind {branch}.",
                    if n == 1 { "" } else { "s" }
                ),
                None => format!(
                    "Latest on {branch}: {} {}",
                    short_hash(&r.latest.hash),
                    r.latest.subject
                ),
            }
        })
    };
    let outdated = move || result.with(|r| r.as_ref().is_some_and(|r| r.behind != Some(0)));
    view! {
        <div class="update-check">
            <div class="toolbar">
                <button class="button" disabled=checking on:click=check>
                    {move || if checking.get() { "Checking…" } else { "Check for updates" }}
                </button>
                <span>{summary}</span>
                <Show when=outdated>
                    <button class="button button-primary push-right" disabled=deploying
                        on:click=move |_| on_deploy.run(result.get_untracked().map(|r| r.latest.hash))>
                        "Update"
                    </button>
                </Show>
            </div>
            <ErrorNote error=error/>
            {move || result.get().filter(|r| r.behind != Some(0) && !r.commits.is_empty()).map(|r| view! {
                <div class="muted small">
                    {if r.behind.is_some() { "New commits" } else { "Recent commits" }}
                </div>
                <ul class="commit-list">
                    {r.commits.into_iter().map(|c| view! { <CommitRow c=c/> }).collect_view()}
                </ul>
            })}
        </div>
    }
}

#[component]
fn CommitRow(c: CommitView) -> impl IntoView {
    view! {
        <li class="commit">
            <code title=c.hash.clone()>{short_hash(&c.hash).to_string()}</code>
            <span class="commit-subject">{c.subject}</span>
            <span class="muted small">{format!("{} · {}", c.author, ago(c.time))}</span>
        </li>
    }
}

// ---------------------------------------------------------------- logs

const MAX_LINES: usize = 5000;

#[component]
fn LogsTab(name: Memo<String>) -> impl IntoView {
    let lines = RwSignal::new(Vec::<(usize, String)>::new());
    let follow = RwSignal::new(true);
    let tail = RwSignal::new(500u32);
    let streaming = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let controller = StoredValue::new_local(None::<web_sys::AbortController>);
    let pane = NodeRef::<leptos::html::Div>::new();

    Effect::new(move |_| {
        let (name, follow, tail) = (name.get(), follow.get(), tail.get());
        if let Some(previous) = controller.get_value() {
            previous.abort();
        }
        let Ok(abort) = web_sys::AbortController::new() else {
            return;
        };
        controller.set_value(Some(abort.clone()));
        lines.set(vec![]);
        error.set(None);
        streaming.set(true);
        spawn_local(async move {
            let mut partial = String::new();
            let mut counter = 0usize;
            let path = format!("/api/apps/{name}/logs?tail={tail}&follow={follow}");
            let result = api::stream_text(&path, &abort.signal(), |chunk| {
                partial.push_str(&chunk);
                let mut fresh = vec![];
                while let Some(i) = partial.find('\n') {
                    let line: String = partial.drain(..=i).collect();
                    counter += 1;
                    fresh.push((counter, strip_ansi(line.trim_end())));
                }
                if !fresh.is_empty() {
                    lines.try_update(|l| {
                        l.extend(fresh);
                        if l.len() > MAX_LINES {
                            l.drain(..l.len() - MAX_LINES);
                        }
                    });
                }
            })
            .await;
            if !abort.signal().aborted() {
                streaming.try_set(false);
                if let Err(e) = result {
                    error.try_set(Some(e));
                }
            }
        });
    });
    on_cleanup(move || {
        if let Some(c) = controller.try_get_value().flatten() {
            c.abort();
        }
    });
    // Keep the newest line in view while following.
    Effect::new(move |_| {
        lines.track();
        if follow.get_untracked()
            && let Some(el) = pane.get()
        {
            el.set_scroll_top(el.scroll_height());
        }
    });

    view! {
        <div class="toolbar">
            <label class="check">
                <input type="checkbox" bind:checked=follow/>
                "Follow"
            </label>
            <label class="inline-field">
                "Last"
                <select on:change=move |ev| tail.set(event_target_value(&ev).parse().unwrap_or(500))
                    prop:value=move || tail.get().to_string()>
                    <option value="100">"100"</option>
                    <option value="500">"500"</option>
                    <option value="2000">"2000"</option>
                </select>
                "lines"
            </label>
            <span class="muted small">
                {move || if streaming.get() && follow.get() { "● live" } else { "" }}
            </span>
        </div>
        <ErrorNote error=error/>
        <div class="logs" node_ref=pane>
            <Show when=move || lines.with(|l| !l.is_empty()) fallback=move || view! {
                <div class="logs-empty">{move || if streaming.get() { "Waiting for output…" } else { "No logs." }}</div>
            }>
                <For each=move || lines.get() key=|(i, _)| *i let:line>
                    <LogLine text=line.1/>
                </For>
            </Show>
        </div>
    }
}

#[component]
fn LogLine(text: String) -> impl IntoView {
    // Docker prefixes each line with an RFC 3339 timestamp.
    let (time, rest) = match text.split_once(' ') {
        Some((ts, rest)) if ts.len() > 19 && ts.as_bytes().get(10) == Some(&b'T') => (
            Some((ts[11..19].to_string(), ts.to_string())),
            rest.to_string(),
        ),
        _ => (None, text),
    };
    view! {
        <div class="log-line">
            {time.map(|(short, full)| view! { <span class="log-time" title=full>{short}</span> })}
            <span class="log-text">{rest}</span>
        </div>
    }
}

// ---------------------------------------------------------------- deployments

#[component]
fn DeploymentsTab(
    name: Memo<String>,
    selected: RwSignal<Option<Uuid>>,
    /// The commit the app runs now.
    #[prop(into)]
    current: Signal<Option<String>>,
    on_deployed: impl Fn() + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let list = polled(Some(Duration::from_secs(3)), move || {
        let name = name.get();
        async move { api::get::<Vec<DeploymentView>>(&format!("/api/apps/{name}/deployments")).await }
    });
    // Show the latest deployment until one is picked.
    Effect::new(move |_| {
        if selected.get_untracked().is_none()
            && let Some(first) = list
                .data
                .with(|l| l.as_ref().and_then(|l| l.first().map(|d| d.id)))
        {
            selected.set(Some(first));
        }
    });
    let detail = polled(Some(Duration::from_millis(1500)), move || {
        let id = selected.get();
        async move {
            match id {
                Some(id) => api::get::<DeploymentView>(&format!("/api/deployments/{id}"))
                    .await
                    .map(Some),
                None => Ok(None),
            }
        }
    });
    let log_pane = NodeRef::<leptos::html::Pre>::new();
    Effect::new(move |_| {
        let running = detail.data.with(|d| {
            d.as_ref()
                .and_then(|d| d.as_ref())
                .is_some_and(|d| !d.status.is_finished())
        });
        if running && let Some(el) = log_pane.get() {
            el.set_scroll_top(el.scroll_height());
        }
    });
    let cancel = move |id: Uuid| {
        run(
            "Deployment cancelled",
            async move { api::act(&format!("/api/deployments/{id}/cancel")).await },
            move || {
                list.reload();
                detail.reload();
            },
        );
    };
    let redeploy = Callback::new(move |commit: String| {
        let path = format!("/api/apps/{}/deploy", name.get_untracked());
        let on_deployed = on_deployed.clone();
        spawn_local(async move {
            let body = DeployRequest {
                commit: Some(commit),
                rebuild: false,
            };
            match api::post::<_, DeploymentView>(&path, &body).await {
                Ok(d) => {
                    selected.set(Some(d.id));
                    list.reload();
                    on_deployed();
                }
                Err(e) => toasts().err(e),
            }
        });
    });

    view! {
        <ErrorNote error=list.error/>
        <Show when=move || list.data.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))
            fallback=|| view! { <div class="empty"><p>"No deployments yet. Press Deploy to build and start the app."</p></div> }>
            <div class="split">
                <ul class="deploy-list">
                    <For each=move || list.data.get().unwrap_or_default()
                        key=|d| (d.id, d.status, d.finished_at, d.commit_title.clone())
                        let:d>
                        <li>
                            <button class=move || if selected.get() == Some(d.id) { "deploy-item active" } else { "deploy-item" }
                                on:click=move |_| selected.set(Some(d.id))>
                                <DeployStatus status=d.status/>
                                <span class="deploy-rev">
                                    {match &d.commit {
                                        Some(c) => short_hash(c).to_string(),
                                        None => format!("rev {}", d.revision),
                                    }}
                                </span>
                                <span class="muted small">{ago(d.created_at)}</span>
                                {d.commit_title.clone().map(|t| view! { <span class="deploy-title small">{t}</span> })}
                            </button>
                        </li>
                    </For>
                </ul>
                <div class="deploy-detail">
                    {move || detail.data.get().flatten().map(|d| {
                        let id = d.id;
                        let status = d.status;
                        let commit = d.commit.clone();
                        let is_current = commit.is_some() && commit == current.get();
                        // A stack's services are redeployed with their stack.
                        let member = name.get_untracked().contains('.');
                        let can_redeploy = status == DeploymentStatus::Succeeded
                            && commit.is_some()
                            && !is_current
                            && !member;
                        view! {
                            <div class="deploy-head">
                                <DeployStatus status=status/>
                                <span>{format!("Revision {}", d.revision)}</span>
                                {d.commit.clone().map(|c| view! {
                                    <code title=c.clone()>{c[..c.len().min(12)].to_string()}</code>
                                })}
                                <span class="muted small">
                                    {format!("requested {}", ago(d.created_at))}
                                    {match (d.started_at, d.finished_at) {
                                        (Some(s), Some(f)) => Some(format!(" · took {}s", f.saturating_sub(s))),
                                        (None, None) => Some(" · waiting for the build slot".to_string()),
                                        _ => None,
                                    }}
                                </span>
                                <span class="push-right">
                                    {(!status.is_finished()).then(|| view! {
                                        <button class="button" on:click=move |_| cancel(id)>"Cancel"</button>
                                    })}
                                    {can_redeploy.then(|| view! {
                                        <button class="button"
                                            title="Deploy this commit again (a rollback). Its image is reused if it is still on the node."
                                            on:click=move |_| { if let Some(c) = commit.clone() { redeploy.run(c) } }>
                                            "Redeploy this commit"
                                        </button>
                                    })}
                                    {(is_current && status == DeploymentStatus::Succeeded).then(|| view! {
                                        <span class="muted small">"running now"</span>
                                    })}
                                </span>
                            </div>
                            {d.commit_title.clone().map(|t| view! { <p class="deploy-commit-title">{t}</p> })}
                            {d.error.clone().filter(|_| status != DeploymentStatus::Cancelled).map(|e| view! { <div class="note note-error">{e}</div> })}
                        }
                    })}
                    <pre class="deploy-log" node_ref=log_pane>
                        {move || detail.data.get().flatten().map(|d| strip_ansi(&d.log)).unwrap_or_default()}
                    </pre>
                </div>
            </div>
        </Show>
    }
}

#[component]
pub fn DeployStatus(status: DeploymentStatus) -> impl IntoView {
    let (class, icon, label) = match status {
        DeploymentStatus::Queued => ("busy", "○", "queued"),
        DeploymentStatus::Running => ("busy", "◐", "running"),
        DeploymentStatus::Succeeded => ("good", "✓", "succeeded"),
        DeploymentStatus::Failed => ("critical", "✕", "failed"),
        DeploymentStatus::Cancelled => ("neutral", "■", "cancelled"),
    };
    view! {
        <span class=format!("badge badge-{class}")>
            <span class="badge-icon" aria-hidden="true">{icon}</span>{label}
        </span>
    }
}

// ---------------------------------------------------------------- environment

/// Variables at `path` (an app's, or a stack's); `saved` says what saving does.
#[component]
pub fn EnvTab(
    #[prop(into)] path: Signal<String>,
    #[prop(into)] saved: Signal<String>,
) -> impl IntoView {
    let reveal = RwSignal::new(false);
    let vars = polled(None, move || {
        let (path, reveal) = (path.get(), reveal.get());
        async move { api::get::<Vec<EnvView>>(&format!("{path}?reveal={reveal}")).await }
    });

    // One modal for adding and editing a variable.
    let editor_open = RwSignal::new(false);
    let editing_existing = RwSignal::new(false);
    let key = RwSignal::new(String::new());
    let value = RwSignal::new(String::new());
    let sensitive = RwSignal::new(true);
    let open_editor = move |var: Option<EnvView>| {
        match var {
            Some(v) => {
                key.set(v.key);
                value.set(v.value.unwrap_or_default());
                sensitive.set(v.sensitive);
                editing_existing.set(true);
            }
            None => {
                key.set(String::new());
                value.set(String::new());
                sensitive.set(true);
                editing_existing.set(false);
            }
        }
        editor_open.set(true);
    };
    let saved_message = move || saved.get_untracked();
    let save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let patch = EnvPatch {
            set: [(
                key.get_untracked().trim().to_string(),
                SetEnv {
                    value: value.get_untracked(),
                    sensitive: sensitive.get_untracked(),
                },
            )]
            .into(),
            unset: vec![],
        };
        let path = path.get_untracked();
        run(
            saved_message(),
            async move { api::patch(&path, &patch).await },
            move || {
                editor_open.set(false);
                vars.reload();
            },
        );
    };
    let remove = move |k: String| {
        let path = path.get_untracked();
        let patch = EnvPatch {
            set: Default::default(),
            unset: vec![k.clone()],
        };
        run(
            format!("{k} removed"),
            async move { api::patch(&path, &patch).await },
            move || vars.reload(),
        );
    };

    // Bulk import from .env text.
    let import_open = RwSignal::new(false);
    let import_text = RwSignal::new(String::new());
    let import_sensitive = RwSignal::new(true);
    let import = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let mut patch = EnvPatch::default();
        for line in import_text.get_untracked().lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            if let Some((k, v)) = line.split_once('=') {
                let v = v.trim();
                let v = v
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                    .unwrap_or(v);
                patch.set.insert(
                    k.trim().to_string(),
                    SetEnv {
                        value: v.to_string(),
                        sensitive: import_sensitive.get_untracked(),
                    },
                );
            }
        }
        if patch.set.is_empty() {
            return toasts().err("No KEY=VALUE lines found");
        }
        let count = patch.set.len();
        let path = path.get_untracked();
        run(
            format!("{count} variables saved"),
            async move { api::patch(&path, &patch).await },
            move || {
                import_open.set(false);
                import_text.set(String::new());
                vars.reload();
            },
        );
    };

    view! {
        <div class="toolbar">
            <button class="button button-primary" on:click=move |_| open_editor(None)>"Add variable"</button>
            <button class="button" on:click=move |_| import_open.set(true)>"Import .env"</button>
            <label class="check push-right">
                <input type="checkbox" bind:checked=reveal/>
                "Reveal sensitive values"
            </label>
        </div>
        <ErrorNote error=vars.error/>
        <Show when=move || vars.loaded() fallback=Loading>
            <Show when=move || vars.data.with(|v| v.as_ref().is_some_and(|v| !v.is_empty()))
                fallback=|| view! { <div class="empty"><p>"No variables yet."</p></div> }>
                <div class="table-wrap">
                    <table class="table env-table">
                        <thead><tr><th>"Name"</th><th>"Value"</th><th>"Sensitive"</th><th></th></tr></thead>
                        <tbody>
                            <For each=move || vars.data.get().unwrap_or_default()
                                key=|v| (v.key.clone(), v.value.clone(), v.sensitive)
                                let:var>
                                {
                                    let edit_var = var.clone();
                                    let key_for_remove = var.key.clone();
                                    view! {
                                        <tr>
                                            <td><code>{var.key.clone()}</code></td>
                                            <td class="env-value">
                                                {match var.value.clone() {
                                                    Some(v) => view! { <code>{v}</code> }.into_any(),
                                                    None => view! { <span class="muted" title="hidden">"••••••••"</span> }.into_any(),
                                                }}
                                            </td>
                                            <td>{if var.sensitive { "yes" } else { "no" }}</td>
                                            <td class="row-actions">
                                                <button class="link-button" on:click=move |_| open_editor(Some(edit_var.clone()))>"Edit"</button>
                                                <button class="link-button danger" on:click=move |_| remove(key_for_remove.clone())>"Remove"</button>
                                            </td>
                                        </tr>
                                    }
                                }
                            </For>
                        </tbody>
                    </table>
                </div>
            </Show>
        </Show>
        <p class="muted small">
            "Sensitive values are encrypted on the server, hidden unless revealed, and left out of exports."
        </p>

        <Modal open=editor_open title="Environment variable">
            <form class="form" on:submit=save>
                <label class="field">
                    <span>"Name"</span>
                    <input type="text" bind:value=key disabled=editing_existing required
                        placeholder="DATABASE_URL" pattern="[A-Za-z_][A-Za-z0-9_]*" autocomplete="off"/>
                </label>
                <label class="field">
                    <span>"Value"</span>
                    <textarea rows="3" bind:value=value
                        placeholder=move || if editing_existing.get() && sensitive.get() && !reveal.get() {
                            "Hidden. Type a new value to replace it."
                        } else { "" }></textarea>
                </label>
                <label class="check">
                    <input type="checkbox" bind:checked=sensitive/>
                    "Sensitive (hidden, encrypted, not exported)"
                </label>
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| editor_open.set(false)>"Cancel"</button>
                    <button type="submit" class="button button-primary">"Save"</button>
                </div>
            </form>
        </Modal>

        <Modal open=import_open title="Import .env">
            <form class="form" on:submit=import>
                <label class="field">
                    <span>"Paste KEY=VALUE lines"</span>
                    <textarea rows="10" class="mono" bind:value=import_text
                        placeholder="SIGNALLING_ADDR=0.0.0.0:9090\nTURN_PASSWORD=…"></textarea>
                </label>
                <label class="check">
                    <input type="checkbox" bind:checked=import_sensitive/>
                    "Mark all as sensitive"
                </label>
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| import_open.set(false)>"Cancel"</button>
                    <button type="submit" class="button button-primary">"Import"</button>
                </div>
            </form>
        </Modal>
    }
}

// ---------------------------------------------------------------- volumes

#[component]
fn VolumesTab(name: Memo<String>, #[prop(into)] app: Signal<Option<AppView>>) -> impl IntoView {
    // Where the app mounts a volume, if it still declares it.
    let mount = move |volume: &str| {
        app.with(|a| {
            a.as_ref()?
                .spec
                .volumes
                .iter()
                .find(|v| v.name == volume)
                .map(|v| v.path.clone())
        })
    };
    let volumes = polled(Some(Duration::from_secs(30)), move || {
        let name = name.get();
        async move {
            let all = api::get::<Vec<VolumeView>>("/api/volumes").await?;
            Ok(all
                .into_iter()
                .filter(|v| v.app == name)
                .collect::<Vec<_>>())
        }
    });
    let backups = polled(None, move || {
        let name = name.get();
        async move { api::get::<Vec<BackupView>>(&format!("/api/backups?app={name}")).await }
    });

    let backing_up = RwSignal::new(false);
    let back_up = move |_| {
        backing_up.set(true);
        let body = BackupRequest {
            app: Some(name.get_untracked()),
        };
        spawn_local(async move {
            match api::post::<_, BackupReport>("/api/backups", &body).await {
                Ok(report) => {
                    if !report.done.is_empty() {
                        let n = report.done.len();
                        toasts().ok(format!(
                            "{n} volume{} backed up",
                            if n == 1 { "" } else { "s" }
                        ));
                    }
                    for failure in report.failed {
                        toasts().err(failure);
                    }
                    backups.reload();
                    volumes.reload();
                }
                Err(e) => toasts().err(e),
            }
            backing_up.try_set(false);
        });
    };
    let download = move |path: String, fallback: String| {
        toasts().ok("Download starting…");
        spawn_local(async move {
            if let Err(e) = api::download(&path, &fallback).await {
                toasts().err(e);
            }
        });
    };

    // Replacing a volume's contents from an uploaded archive.
    let replace_open = RwSignal::new(false);
    let replace_target = RwSignal::new(None::<VolumeView>);
    let replace_file = StoredValue::new_local(None::<web_sys::File>);
    let replace_name = RwSignal::new(None::<String>);
    let replacing = RwSignal::new(false);
    let file_input = NodeRef::<leptos::html::Input>::new();
    let open_replace = move |v: VolumeView| {
        replace_target.set(Some(v));
        replace_file.set_value(None);
        replace_name.set(None);
        replace_open.set(true);
    };
    let replace = move |_| {
        let (Some(v), Some(file)) = (replace_target.get_untracked(), replace_file.get_value())
        else {
            return;
        };
        replacing.set(true);
        let path = format!("/api/apps/{}/volumes/{}/archive", v.app, v.volume);
        spawn_local(async move {
            match api::put_file(&path, &file).await {
                Ok(()) => {
                    toasts().ok(format!("{} replaced; the app is starting again", v.volume));
                    replace_open.try_set(false);
                    volumes.reload();
                }
                Err(e) => toasts().err(e),
            }
            replacing.try_set(false);
        });
    };

    let restore_open = RwSignal::new(false);
    let restore_target = RwSignal::new(None::<BackupView>);
    let restore = move |_| {
        let Some(b) = restore_target.get_untracked() else {
            return;
        };
        run(
            format!("{} restored; the app is starting again", b.volume),
            async move {
                api::act(&format!(
                    "/api/backups/{}/{}/{}/restore",
                    b.app, b.volume, b.id
                ))
                .await
            },
            move || {
                restore_open.set(false);
                volumes.reload();
            },
        );
    };

    view! {
        <div class="toolbar">
            <button class="button button-primary" disabled=backing_up on:click=back_up>
                {move || if backing_up.get() { "Backing up…" } else { "Back up now" }}
            </button>
            <span class="muted small">"The server also backs volumes up on a schedule (daily by default) and keeps the copies on its disk."</span>
        </div>
        <ErrorNote error=volumes.error/>
        <Show when=move || volumes.loaded() fallback=Loading>
            <div class="table-wrap">
                <table class="table">
                    <thead>
                        <tr>
                            <th>"Volume"</th>
                            <th>"Mounted at"</th>
                            <th class="num">"Size"</th>
                            <th>"Last backup"</th>
                            <th></th>
                        </tr>
                    </thead>
                    <tbody>
                        <For each=move || volumes.data.get().unwrap_or_default()
                            key=|v| (v.node.clone(), v.name.clone(), v.size, v.last_backup, v.declared)
                            let:v>
                            {
                                let export = format!("/api/apps/{}/volumes/{}/archive", v.app, v.volume);
                                let fallback = format!("{}-{}.tar.gz", v.app, v.volume);
                                let target = v.clone();
                                let mounted = mount(&v.volume).filter(|_| v.declared);
                                view! {
                                    <tr>
                                        <td><code>{v.volume.clone()}</code></td>
                                        <td>{match mounted {
                                            Some(p) => view! { <code>{p}</code> }.into_any(),
                                            None => view! { <span class="muted">"not used any more"</span> }.into_any(),
                                        }}</td>
                                        <td class="num">{v.size.map(bytes).unwrap_or_else(|| "—".into())}</td>
                                        <td class="muted">{v.last_backup.map(ago).unwrap_or_else(|| "never".into())}</td>
                                        <td class="row-actions">
                                            <button class="link-button"
                                                on:click=move |_| download(export.clone(), fallback.clone())>"Download"</button>
                                            <button class="link-button danger"
                                                on:click=move |_| open_replace(target.clone())>"Replace…"</button>
                                        </td>
                                    </tr>
                                }
                            }
                        </For>
                    </tbody>
                </table>
            </div>
        </Show>

        <section class="app-backups">
            <h2 class="section-title">"Backups"</h2>
            <ErrorNote error=backups.error/>
            <Show when=move || backups.data.with(|b| b.as_ref().is_some_and(|b| !b.is_empty()))
                fallback=|| view! { <div class="empty"><p>"No backups yet."</p></div> }>
                <div class="table-wrap">
                    <table class="table">
                        <thead>
                            <tr>
                                <th>"Taken"</th>
                                <th>"Volume"</th>
                                <th class="num">"Size"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            <For each=move || backups.data.get().unwrap_or_default()
                                key=|b| (b.volume.clone(), b.id)
                                let:b>
                                {
                                    let path = format!("/api/backups/{}/{}/{}", b.app, b.volume, b.id);
                                    let fallback = format!("{}-{}-{}.tar.gz", b.app, b.volume, b.id);
                                    let target = b.clone();
                                    view! {
                                        <tr>
                                            <td>
                                                {format!("{} {}", date(b.id), clock(b.id))}
                                                <span class="muted small">{format!(" · {}", ago(b.id))}</span>
                                            </td>
                                            <td><code>{b.volume.clone()}</code></td>
                                            <td class="num">{bytes(b.size)}</td>
                                            <td class="row-actions">
                                                <button class="link-button"
                                                    on:click=move |_| download(path.clone(), fallback.clone())>"Download"</button>
                                                <button class="link-button danger" on:click=move |_| {
                                                    restore_target.set(Some(target.clone()));
                                                    restore_open.set(true);
                                                }>"Restore…"</button>
                                            </td>
                                        </tr>
                                    }
                                }
                            </For>
                        </tbody>
                    </table>
                </div>
            </Show>
        </section>

        <Modal open=replace_open title="Replace volume contents">
            <p>
                "The app is stopped, everything in "
                <code>{move || replace_target.get().map(|v| mount(&v.volume).unwrap_or(v.volume)).unwrap_or_default()}</code>
                " is replaced by the archive's files, then the app starts again. Download a copy first if you may need the current files."
            </p>
            <label class="field">
                <span>"Archive (.tar or .tar.gz)"</span>
                <input type="file" node_ref=file_input accept=".tar,.tar.gz,.tgz,application/gzip,application/x-tar"
                    on:change=move |_| {
                        let file = file_input.get().and_then(|el| el.files()).and_then(|f| f.get(0));
                        replace_name.set(file.as_ref().map(|f| f.name()));
                        replace_file.set_value(file);
                    }/>
            </label>
            <div class="form-actions">
                <button class="button" on:click=move |_| replace_open.set(false)>"Cancel"</button>
                <button class="button button-danger"
                    disabled=move || replacing.get() || replace_name.with(Option::is_none)
                    on:click=replace>
                    {move || if replacing.get() { "Replacing…" } else { "Replace" }}
                </button>
            </div>
        </Modal>

        <Modal open=restore_open title="Restore backup">
            <p>
                {move || restore_target.get().map(|b| format!(
                    "Restore {} to its state of {} {}? The app is stopped and the volume's current files are replaced by this backup.",
                    b.volume, date(b.id), clock(b.id)
                ))}
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| restore_open.set(false)>"Cancel"</button>
                <button class="button button-danger" on:click=restore>"Restore"</button>
            </div>
        </Modal>
    }
}

// ---------------------------------------------------------------- settings

#[component]
fn SettingsTab(
    app: AppView,
    /// The app as it is now, for the access section (saved on its own).
    #[prop(into)]
    live: Signal<Option<AppView>>,
    on_saved: impl Fn() + Clone + Send + Sync + 'static,
    on_delete: impl Fn() + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let me = me();
    let nodes = polled(None, || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });
    let apps = polled(None, || async {
        api::get::<Vec<AppView>>("/api/apps").await
    });
    let form = SpecForm::from_spec(&app.spec);
    let error = RwSignal::new(None::<String>);
    let name = app.spec.name.clone();
    // Volumes only follow the app to another node when asked to.
    let old_node = StoredValue::new(app.spec.node.clone());
    let had_volumes = !app.spec.volumes.is_empty();
    let move_data = RwSignal::new(false);
    let moving = move || had_volumes && form.node.with(|n| old_node.with_value(|o| n != o));
    let on_access = on_saved.clone();
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let spec = match form.to_spec() {
            Ok(spec) => spec,
            Err(e) => return error.set(Some(e)),
        };
        error.set(None);
        let path = if moving() && move_data.get_untracked() {
            format!("/api/apps/{name}?move_data=true")
        } else {
            format!("/api/apps/{name}")
        };
        let on_saved = on_saved.clone();
        run(
            "Settings saved",
            async move { api::put_empty(&path, &spec).await },
            on_saved,
        );
    };
    view! {
        <form class="card form" on:submit=submit>
            <SpecFields form nodes=Signal::derive(move || nodes.data.get().unwrap_or_default())
                apps=Signal::derive(move || apps.data.get().unwrap_or_default()) editing=true/>
            <Show when=moving>
                <div class="note">
                    <label class="check">
                        <input type="checkbox" bind:checked=move_data/>
                        "Copy its volumes to the new node"
                    </label>
                    <div class="small">
                        {move || format!(
                            "The app keeps data on {}. Its volumes are copied over, then deleted from {0}. Without this, the move is refused.",
                            old_node.get_value()
                        )}
                    </div>
                </div>
            </Show>
            <ErrorNote error=error/>
            <div class="form-actions">
                <button class="button button-primary" type="submit">"Save changes"</button>
            </div>
            <p class="muted small">
                "Saving applies right away: the app restarts, and is rebuilt first if its source changed."
            </p>
        </form>
        <Show when=move || me.is_admin()>
            <AccessSection live=live on_change=on_access.clone()/>
        </Show>
        <section class="card danger-zone">
            <div>
                <h2 class="section-title">"Delete this app"</h2>
                <p class="muted small">"Removes its container, image and variables. Its volumes are kept unless you choose otherwise."</p>
            </div>
            <button class="button button-danger" on:click=move |_| on_delete()>"Delete app…"</button>
        </section>
    }
}

/// Who owns the app and which members may manage it. Admins only.
#[component]
fn AccessSection(
    #[prop(into)] live: Signal<Option<AppView>>,
    on_change: impl Fn() + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let users = polled(None, || async {
        api::get::<Vec<UserView>>("/api/users").await
    });
    let members = move || {
        users.data.with(|u| {
            u.iter()
                .flatten()
                .filter(|u| u.role != "admin")
                .map(|u| u.username.clone())
                .collect::<Vec<_>>()
        })
    };
    let name = move || live.with(|a| a.as_ref().map(|a| a.spec.name.clone()).unwrap_or_default());
    let owner = move || live.with(|a| a.as_ref().and_then(|a| a.owner.clone()));
    let access = move || live.with(|a| a.as_ref().map(|a| a.access.clone()).unwrap_or_default());
    let set_owner = {
        let on_change = on_change.clone();
        move |value: String| {
            let body = SetOwner {
                owner: Some(value).filter(|v| !v.is_empty()),
            };
            let done = match &body.owner {
                Some(o) => format!("Now owned by {o}"),
                None => "Now owned by the admins".to_string(),
            };
            let path = format!("/api/apps/{}/owner", name());
            run(
                done,
                async move { api::put_empty(&path, &body).await },
                on_change.clone(),
            );
        }
    };
    let grantee = RwSignal::new(String::new());
    let grant = {
        let on_change = on_change.clone();
        move |ev: leptos::ev::SubmitEvent| {
            ev.prevent_default();
            let user = grantee.get_untracked();
            if user.is_empty() {
                return;
            }
            let path = format!("/api/apps/{}/access/{user}", name());
            let on_change = on_change.clone();
            run(
                format!("{user} can now manage it"),
                async move { api::put_empty(&path, &()).await },
                move || {
                    grantee.set(String::new());
                    on_change();
                },
            );
        }
    };
    let revoke = Callback::new(move |user: String| {
        let path = format!("/api/apps/{}/access/{user}", name());
        run(
            format!("{user} no longer has access"),
            async move { api::delete(&path).await },
            on_change.clone(),
        );
    });
    // Members who could still be given access.
    let candidates = move || {
        let (owner, access) = (owner(), access());
        members()
            .into_iter()
            .filter(|m| Some(m) != owner.as_ref() && !access.contains(m))
            .collect::<Vec<_>>()
    };

    view! {
        <section class="card form">
            <h2 class="section-title">"Access"</h2>
            <ErrorNote error=users.error/>
            <label class="field">
                <span>"Owner"</span>
                <select on:change=move |ev| set_owner(event_target_value(&ev))
                    prop:value=move || owner().unwrap_or_default()>
                    <option value="" selected=move || owner().is_none()>"The admins"</option>
                    <For each=members key=|m| m.clone() let:m>
                        <option value=m.clone()
                            selected={
                                let name = m.clone();
                                move || owner().as_deref() == Some(name.as_str())
                            }>{m.clone()}</option>
                    </For>
                </select>
                <small>"The owner's memory and CPU caps count this app, and its builds use the owner's registry and git logins."</small>
            </label>
            <div class="field">
                <span>"Members with access"</span>
                <Show when=move || !access().is_empty()
                    fallback=|| view! { <p class="muted small">"Only the owner and the admins."</p> }>
                    <ul class="access-list">
                        <For each=access key=|u| u.clone() let:user>
                            <li>
                                {user.clone()}
                                <button class="link-button danger" on:click={
                                    let user = user.clone();
                                    move |_| revoke.run(user.clone())
                                }>"Remove"</button>
                            </li>
                        </For>
                    </ul>
                </Show>
            </div>
            <Show when=move || !candidates().is_empty()>
                <form class="form-row" on:submit=grant.clone()>
                    <select on:change=move |ev| grantee.set(event_target_value(&ev))
                        prop:value=move || grantee.get()>
                        <option value="">"Choose a member…"</option>
                        <For each=candidates key=|m| m.clone() let:m>
                            <option value=m.clone()>{m.clone()}</option>
                        </For>
                    </select>
                    <button type="submit" class="button" disabled=move || grantee.with(String::is_empty)>
                        "Give access"
                    </button>
                </form>
            </Show>
        </section>
    }
}
