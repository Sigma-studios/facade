use std::time::Duration;

use facade_core::api::{
    AppMetric, AppView, DeploymentStatus, DeploymentView, DomainStatus, EnvPatch, EnvView,
    NodeView, SetEnv,
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
use crate::util::{ago, bytes, polled, run, strip_ansi, toasts};

const TABS: &[(&str, &str)] = &[
    ("overview", "Overview"),
    ("logs", "Logs"),
    ("deployments", "Deployments"),
    ("env", "Environment"),
    ("settings", "Settings"),
];

#[component]
pub fn AppPage() -> impl IntoView {
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
    let deploy = {
        let set_tab = set_tab.clone();
        move |_| {
            busy.set(true);
            let set_tab = set_tab.clone();
            spawn_local(async move {
                match api::act_json::<DeploymentView>(&format!(
                    "/api/apps/{}/deploy",
                    name.get_untracked()
                ))
                .await
                {
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
    let simple = move |action: &'static str, done: &'static str| {
        let path = format!("/api/apps/{}/{action}", name.get_untracked());
        run(done, async move { api::act(&path).await }, move || {
            app.reload()
        });
    };

    let confirm_delete = RwSignal::new(false);
    let typed = RwSignal::new(String::new());
    let delete = {
        let navigate = navigate.clone();
        move |_| {
            let navigate = navigate.clone();
            let n = name.get_untracked();
            run(
                format!("{n} deleted"),
                async move { api::delete(&format!("/api/apps/{n}")).await },
                move || navigate("/apps", Default::default()),
            );
        }
    };

    let view_of = move || app.data.get();
    let state = Signal::derive(move || view_of().map(|a| a.state).unwrap_or(AppState::Unknown));
    let enabled = move || view_of().is_some_and(|a| a.spec.enabled);
    let deployed = move || view_of().is_some_and(|a| a.revision > 0);

    view! {
        <div class="page">
            <header class="page-head">
                <div>
                    <A href="/apps" attr:class="crumb">"← Apps"</A>
                    <h1 class="title-row">
                        {move || name.get()}
                        <Show when=move || app.loaded()>
                            <StateBadge state=state/>
                        </Show>
                    </h1>
                    <div class="muted small subtitle">
                        {move || view_of().map(|a| subtitle(&a))}
                    </div>
                </div>
                <div class="actions">
                    <button class="button button-primary" disabled=move || busy.get() || !app.loaded()
                        on:click=deploy
                        title="Fetch the latest code or image, rebuild and restart">
                        {move || if busy.get() { "Starting…" } else if deployed() { "Deploy" } else { "First deploy" }}
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
                {TABS.iter().map(|(key, label)| {
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
                    "deployments" => view! { <DeploymentsTab name=name selected=selected_deployment/> }.into_any(),
                    "env" => view! { <EnvTab name=name deployed=Signal::derive(deployed)/> }.into_any(),
                    "settings" => view! {
                        <SettingsTab app=app.data.get_untracked().expect("loaded")
                            on_saved=move || app.reload() on_delete=move || confirm_delete.set(true)/>
                    }.into_any(),
                    _ => view! { <OverviewTab name=name app=Signal::derive(move || app.data.get())/> }.into_any(),
                }}
            </Show>

            <Modal open=confirm_delete title="Delete app">
                <p>"This removes the container, its image and all its environment variables. It cannot be undone."</p>
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
                Build::Static { .. } => "static site",
            };
            format!("{} · {kind}", repo.trim_start_matches("https://"))
        }
    };
    let deployed = if app.revision == 0 {
        "never deployed".to_string()
    } else {
        format!("revision {}", app.revision)
    };
    format!("on {} · {source} · {deployed}", app.spec.node)
}

// ---------------------------------------------------------------- overview

#[component]
fn OverviewTab(name: Memo<String>, #[prop(into)] app: Signal<Option<AppView>>) -> impl IntoView {
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
                            if args.is_empty() { String::new() } else { format!(", run with {}", args.join(" ")) }
                        ),
                        Source::Git { repo, branch, .. } => format!("{repo} ({})", branch.clone().unwrap_or_else(|| "default branch".into())),
                    }}</dd>
                    <dt>"HTTP port"</dt>
                    <dd>{s.http_port.map(|p| p.to_string()).unwrap_or_else(|| "—".into())}</dd>
                    <dt>"Published ports"</dt>
                    <dd>{if s.host_network { "node network".to_string() } else if s.ports.is_empty() { "—".into() } else {
                        s.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
                    }}</dd>
                </dl>
            })}
        </section>
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
fn DeploymentsTab(name: Memo<String>, selected: RwSignal<Option<Uuid>>) -> impl IntoView {
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
                .is_some_and(|d| d.status == DeploymentStatus::Running)
        });
        if running && let Some(el) = log_pane.get() {
            el.set_scroll_top(el.scroll_height());
        }
    });

    view! {
        <ErrorNote error=list.error/>
        <Show when=move || list.data.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))
            fallback=|| view! { <div class="empty"><p>"No deployments yet. Press Deploy to build and start the app."</p></div> }>
            <div class="split">
                <ul class="deploy-list">
                    <For each=move || list.data.get().unwrap_or_default()
                        key=|d| (d.id, d.status, d.finished_at)
                        let:d>
                        <li>
                            <button class=move || if selected.get() == Some(d.id) { "deploy-item active" } else { "deploy-item" }
                                on:click=move |_| selected.set(Some(d.id))>
                                <DeployStatus status=d.status/>
                                <span class="deploy-rev">{format!("rev {}", d.revision)}</span>
                                <span class="muted small">{ago(d.started_at)}</span>
                            </button>
                        </li>
                    </For>
                </ul>
                <div class="deploy-detail">
                    {move || detail.data.get().flatten().map(|d| view! {
                        <div class="deploy-head">
                            <DeployStatus status=d.status/>
                            <span>{format!("Revision {}", d.revision)}</span>
                            <span class="muted small">
                                {format!("started {}", ago(d.started_at))}
                                {d.finished_at.map(|f| format!(" · took {}s", f.saturating_sub(d.started_at)))}
                            </span>
                        </div>
                        {d.error.clone().map(|e| view! { <div class="note note-error">{e}</div> })}
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
fn DeployStatus(status: DeploymentStatus) -> impl IntoView {
    let (class, icon, label) = match status {
        DeploymentStatus::Running => ("busy", "◐", "running"),
        DeploymentStatus::Succeeded => ("good", "✓", "succeeded"),
        DeploymentStatus::Failed => ("critical", "✕", "failed"),
    };
    view! {
        <span class=format!("badge badge-{class}")>
            <span class="badge-icon" aria-hidden="true">{icon}</span>{label}
        </span>
    }
}

// ---------------------------------------------------------------- environment

#[component]
fn EnvTab(name: Memo<String>, #[prop(into)] deployed: Signal<bool>) -> impl IntoView {
    let reveal = RwSignal::new(false);
    let vars = polled(None, move || {
        let (name, reveal) = (name.get(), reveal.get());
        async move { api::get::<Vec<EnvView>>(&format!("/api/apps/{name}/env?reveal={reveal}")).await }
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
    let saved_message = move || {
        if deployed.get_untracked() {
            "Saved. The app restarts with the new values."
        } else {
            "Saved. It will be used from the first deploy."
        }
    };
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
        let path = format!("/api/apps/{}/env", name.get_untracked());
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
        let path = format!("/api/apps/{}/env", name.get_untracked());
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
        let path = format!("/api/apps/{}/env", name.get_untracked());
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

// ---------------------------------------------------------------- settings

#[component]
fn SettingsTab(
    app: AppView,
    on_saved: impl Fn() + Clone + Send + Sync + 'static,
    on_delete: impl Fn() + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let nodes = polled(None, || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });
    let form = SpecForm::from_spec(&app.spec);
    let error = RwSignal::new(None::<String>);
    let name = app.spec.name.clone();
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let spec = match form.to_spec() {
            Ok(spec) => spec,
            Err(e) => return error.set(Some(e)),
        };
        error.set(None);
        let path = format!("/api/apps/{name}");
        let on_saved = on_saved.clone();
        run(
            "Settings saved",
            async move { api::put_empty(&path, &spec).await },
            on_saved,
        );
    };
    view! {
        <form class="card form" on:submit=submit>
            <SpecFields form nodes=Signal::derive(move || nodes.data.get().unwrap_or_default()) editing=true/>
            <ErrorNote error=error/>
            <div class="form-actions">
                <button class="button button-primary" type="submit">"Save changes"</button>
            </div>
            <p class="muted small">
                "Saving applies right away: the app restarts, and is rebuilt first if its source changed."
            </p>
        </form>
        <section class="card danger-zone">
            <div>
                <h2 class="section-title">"Delete this app"</h2>
                <p class="muted small">"Removes its container, image and variables."</p>
            </div>
            <button class="button button-danger" on:click=move |_| on_delete()>"Delete app…"</button>
        </section>
    }
}
