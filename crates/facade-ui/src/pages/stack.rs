//! Stacks: services deployed together from one repository, on one node.

use std::time::Duration;

use facade_core::api::{
    AppView, DatabaseKind, DeploymentStatus, DeploymentView, EnvPatch, InspectReport,
    InspectRequest, NodeView, ServiceChange, SetEnv, StackDeploymentView, StackPlan,
    StackPlanRequest, StackView,
};
use facade_core::proto::AppState;
use facade_core::stack::{ServiceSpec, StackSpec};
use facade_core::templates::EnvVar;
use facade_core::{AppSpec, Build, HealthCheck, Limits, PortMap, Source, Volume};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};
use uuid::Uuid;

use super::app::{DeployStatus, EnvTab};
use super::overview::AppsTable;
use crate::api;
use crate::components::{ErrorNote, Loading, Modal};
use crate::util::{ago, polled, run, strip_ansi, toasts};

const TABS: &[(&str, &str)] = &[
    ("services", "Services"),
    ("deployments", "Deployments"),
    ("variables", "Variables"),
    ("settings", "Settings"),
];

fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
}

fn source_summary(spec: &AppSpec) -> String {
    match &spec.source {
        Source::Image { image } => image.clone(),
        Source::Git {
            build:
                Build::Dockerfile {
                    dockerfile,
                    context,
                },
            ..
        } => {
            if context == "." {
                format!("build {dockerfile}")
            } else {
                format!("build {dockerfile} in {context}")
            }
        }
        Source::Git {
            build: Build::Rust { bin, .. },
            ..
        } => format!("cargo binary {bin}"),
        _ => String::new(),
    }
}

/// Every stack, with its services: the Apps page's second half.
#[component]
pub fn StacksList(#[prop(into)] stacks: Signal<Vec<StackView>>) -> impl IntoView {
    view! {
        <For each=move || stacks.get() key=|s| (s.spec.name.clone(), s.commit.clone(), s.services.iter().map(|a| (a.state, a.spec.name.clone())).collect::<Vec<_>>()) let:stack>
            {
                let name = stack.spec.name.clone();
                let running = stack.services.iter().filter(|a| a.state == AppState::Running).count();
                let total = stack.services.len();
                let services = stack.services.clone();
                view! {
                    <section class="stack-block">
                        <div class="stack-head">
                            <A href=format!("/stacks/{name}") attr:class="strong-link stack-name">{name.clone()}</A>
                            <span class="muted small">
                                {format!("{running}/{total} running")}
                                {stack.commit.as_deref().map(|c| format!(" · commit {}", short(c)))}
                                {match (&stack.spec.repo, &stack.spec.file) {
                                    (Some(_), Some(file)) => format!(" · from {file}"),
                                    _ => " · services defined here".to_string(),
                                }}
                            </span>
                        </div>
                        <AppsTable apps=Signal::derive(move || services.clone())/>
                    </section>
                }
            }
        </For>
    }
}

#[component]
pub fn StackPage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let navigate = use_navigate();
    let name = Memo::new(move |_| params.read().get("name").unwrap_or_default());
    let tab = Memo::new(move |_| query.read().get("tab").unwrap_or_else(|| "services".into()));
    let stack = polled(Some(Duration::from_secs(3)), move || {
        let name = name.get();
        async move { api::get::<StackView>(&format!("/api/stacks/{name}")).await }
    });
    let selected = RwSignal::new(None::<Uuid>);
    let set_tab = {
        let navigate = navigate.clone();
        move |t: &str| {
            navigate(
                &format!("/stacks/{}?tab={t}", name.get_untracked()),
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
        move |all: bool| {
            busy.set(true);
            let set_tab = set_tab.clone();
            let body = facade_core::api::StackDeployRequest {
                all,
                ..Default::default()
            };
            spawn_local(async move {
                match api::post::<_, StackDeploymentView>(
                    &format!("/api/stacks/{}/deploy", name.get_untracked()),
                    &body,
                )
                .await
                {
                    Ok(d) => {
                        selected.set(Some(d.id));
                        set_tab("deployments");
                        stack.reload();
                    }
                    Err(e) => toasts().err(e),
                }
                busy.set(false);
            });
        }
    };
    let deploy_all = deploy.clone();
    let view_of = move || stack.data.get();
    let summary = move || {
        view_of().map(|s| {
            let running = s
                .services
                .iter()
                .filter(|a| a.state == AppState::Running)
                .count();
            let source = match (&s.spec.repo, &s.spec.file) {
                (Some(repo), Some(file)) => {
                    format!("{} · {file}", repo.trim_start_matches("https://"))
                }
                (Some(repo), None) => format!(
                    "{} · services defined here",
                    repo.trim_start_matches("https://")
                ),
                _ => "services defined here".to_string(),
            };
            format!(
                "on {} · {source} · {running}/{} running{}",
                s.spec.node,
                s.services.len(),
                s.commit
                    .as_deref()
                    .map(|c| format!(" · commit {}", short(c)))
                    .unwrap_or_default()
            )
        })
    };

    view! {
        <div class="page">
            <header class="page-head">
                <div>
                    <A href="/apps" attr:class="crumb">"← Apps"</A>
                    <h1 class="title-row">{move || name.get()} <span class="badge badge-neutral">"stack"</span></h1>
                    <div class="muted small subtitle">{summary}</div>
                </div>
                <div class="actions">
                    <button class="button button-primary" disabled=move || busy.get() || !stack.loaded()
                        title="Deploy the latest commit: services whose code, settings or variables changed are rebuilt and replaced"
                        on:click=move |_| deploy(false)>
                        {move || if busy.get() { "Starting…" } else { "Deploy" }}
                    </button>
                    <button class="button" disabled=move || busy.get() || !stack.loaded()
                        title="Rebuild and replace every service, changed or not (images are pulled again)"
                        on:click=move |_| deploy_all(true)>"Redeploy all"</button>
                </div>
            </header>
            <ErrorNote error=stack.error/>
            {move || view_of().and_then(|s| s.deploying).map(|id| view! {
                <div class="note">
                    "A deployment is in progress. "
                    <a href="#" on:click=move |ev| { ev.prevent_default(); selected.set(Some(id)); }>
                        "Follow it in Deployments"
                    </a>
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
            <Show when=move || stack.loaded() fallback=Loading>
                {move || match tab.get().as_str() {
                    "deployments" => view! { <StackDeployments name=name selected=selected
                        current=Signal::derive(move || view_of().and_then(|s| s.commit))
                        on_deployed=Callback::new(move |_| stack.reload())/> }.into_any(),
                    "variables" => view! {
                        <p class="muted small">
                            "The stack's variables are what its compose file reads, like " <code>"${DB_PASSWORD}"</code>
                            " (as a " <code>".env"</code> " file would give them). A service's own variables are on its page."
                        </p>
                        <EnvTab path=Signal::derive(move || format!("/api/stacks/{}/env", name.get()))
                            saved=Signal::derive(|| "Saved. It applies from the next deploy.".to_string())/>
                    }.into_any(),
                    "settings" => view! {
                        <StackSettings stack=stack.data.get_untracked().expect("loaded")
                            on_saved=Callback::new(move |_| stack.reload())/>
                    }.into_any(),
                    _ => view! { <StackServices stack=Signal::derive(view_of)/> }.into_any(),
                }}
            </Show>
        </div>
    }
}

#[component]
fn StackServices(#[prop(into)] stack: Signal<Option<StackView>>) -> impl IntoView {
    let plan = RwSignal::new(None::<StackPlan>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let preview = move |_| {
        let Some(s) = stack.get_untracked() else {
            return;
        };
        busy.set(true);
        error.set(None);
        let body = StackPlanRequest {
            spec: s.spec,
            commit: None,
        };
        spawn_local(async move {
            match api::post::<_, StackPlan>("/api/stacks/plan", &body).await {
                Ok(p) => {
                    plan.try_set(Some(p));
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            busy.try_set(false);
        });
    };
    view! {
        <Show when=move || stack.with(|s| s.as_ref().is_some_and(|s| !s.services.is_empty()))
            fallback=|| view! { <div class="empty"><p>"No services yet. Add them in Settings."</p></div> }>
            <div class="table-wrap">
                <table class="table">
                    <thead><tr>
                        <th>"Service"</th><th>"State"</th><th>"Runs"</th><th>"Commit"</th><th>"Domains"</th>
                    </tr></thead>
                    <tbody>
                        <For each=move || stack.get().map(|s| s.services).unwrap_or_default()
                            key=|a| (a.spec.name.clone(), a.state, a.commit.clone(), a.spec.domains.clone())
                            let:a>
                            {
                                let service = a.spec.service().unwrap_or_default().to_string();
                                let host = a.spec.stack.as_ref().map(|s| s.host.clone()).filter(|h| *h != service);
                                let source = source_summary(&a.spec);
                                let commit = a.commit.as_deref().map(short).unwrap_or_default();
                                let domains = a.spec.domains.clone();
                                view! {
                                    <tr>
                                        <td>
                                            <A href=format!("/apps/{}", a.spec.name) attr:class="strong-link">{service}</A>
                                            {host.map(|h| view! { <div class="muted small">{format!("reached as {h}")}</div> })}
                                        </td>
                                        <td><StateBadge state=a.state/></td>
                                        <td class="small">{source}</td>
                                        <td><code>{commit}</code></td>
                                        <td class="small">
                                            <div class="domains">
                                                {domains.into_iter().map(|d| view! {
                                                    <a href=format!("https://{d}") target="_blank" rel="noopener">{d.clone()}</a>
                                                }).collect_view()}
                                            </div>
                                        </td>
                                    </tr>
                                }
                            }
                        </For>
                    </tbody>
                </table>
            </div>
        </Show>
        <div class="form-actions">
            <button class="button" on:click=preview disabled=busy
                title="Read the latest commit and say what a deploy would change">
                {move || if busy.get() { "Reading the repository…" } else { "Preview the next deploy" }}
            </button>
        </div>
        <ErrorNote error=error/>
        {move || plan.get().map(|p| view! { <PlanView plan=p/> })}
    }
}

use crate::components::StateBadge;

/// What deploying would do.
#[component]
fn PlanView(plan: StackPlan) -> impl IntoView {
    view! {
        <section class="card plan">
            <h2 class="section-title">
                {match (&plan.commit, &plan.commit_title) {
                    (Some(c), Some(t)) => format!("At {} {t}", short(c)),
                    (Some(c), None) => format!("At {}", short(c)),
                    _ => "Services".to_string(),
                }}
            </h2>
            <div class="table-wrap">
                <table class="table">
                    <thead><tr><th>"Service"</th><th>"Change"</th><th>"Runs"</th><th>"After"</th><th>"Variables"</th></tr></thead>
                    <tbody>
                        {plan.services.iter().map(|s| {
                            let (class, label) = match s.change {
                                ServiceChange::New => ("badge badge-busy", "new"),
                                ServiceChange::Changed => ("badge badge-busy", "changed"),
                                ServiceChange::Unchanged => ("badge badge-neutral", "unchanged"),
                            };
                            view! {
                                <tr>
                                    <td><strong>{s.service.clone()}</strong>
                                        {s.app.http_port.map(|p| view! { <div class="muted small">{format!("HTTP on {p}")}</div> })}</td>
                                    <td><span class=class>{label}</span></td>
                                    <td class="small">{source_summary(&s.app)}</td>
                                    <td class="small">{s.depends_on.join(", ")}</td>
                                    <td class="small">
                                        {format!("{} from the file", s.env.len())}
                                        {(!s.missing.is_empty()).then(|| view! {
                                            <div class="error-cell">{format!("to set: {}", s.missing.join(", "))}</div>
                                        })}
                                    </td>
                                </tr>
                            }
                        }).collect_view()}
                    </tbody>
                </table>
            </div>
            {(!plan.removed.is_empty()).then(|| view! {
                <p class="small">"Removed: " {plan.removed.join(", ")}</p>
            })}
            {(!plan.notes.is_empty()).then(|| view! {
                <ul class="notes">
                    {plan.notes.iter().map(|n| view! { <li class="small">{n.clone()}</li> }).collect_view()}
                </ul>
            })}
        </section>
    }
}

#[component]
fn StackDeployments(
    name: Memo<String>,
    selected: RwSignal<Option<Uuid>>,
    #[prop(into)] current: Signal<Option<String>>,
    on_deployed: Callback<()>,
) -> impl IntoView {
    let list = polled(Some(Duration::from_secs(3)), move || {
        let name = name.get();
        async move {
            api::get::<Vec<StackDeploymentView>>(&format!("/api/stacks/{name}/deployments")).await
        }
    });
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
                Some(id) => {
                    api::get::<StackDeploymentView>(&format!("/api/stack-deployments/{id}"))
                        .await
                        .map(Some)
                }
                None => Ok(None),
            }
        }
    });
    let cancel = move |id: Uuid| {
        run(
            "Deployment cancelled",
            async move { api::act(&format!("/api/stack-deployments/{id}/cancel")).await },
            move || {
                list.reload();
                detail.reload();
            },
        );
    };
    let redeploy = Callback::new(move |commit: String| {
        let path = format!("/api/stacks/{}/deploy", name.get_untracked());
        spawn_local(async move {
            let body = facade_core::api::StackDeployRequest {
                commit: Some(commit),
                ..Default::default()
            };
            match api::post::<_, StackDeploymentView>(&path, &body).await {
                Ok(d) => {
                    selected.set(Some(d.id));
                    list.reload();
                    on_deployed.run(());
                }
                Err(e) => toasts().err(e),
            }
        });
    });
    view! {
        <ErrorNote error=list.error/>
        <Show when=move || list.data.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))
            fallback=|| view! { <div class="empty"><p>"No deployments yet. Press Deploy to build and start the stack."</p></div> }>
            <div class="split">
                <ul class="deploy-list">
                    <For each=move || list.data.get().unwrap_or_default()
                        key=|d| (d.id, d.status, d.finished_at, d.commit_title.clone())
                        let:d>
                        <li>
                            <button class=move || if selected.get() == Some(d.id) { "deploy-item active" } else { "deploy-item" }
                                on:click=move |_| selected.set(Some(d.id))>
                                <DeployStatus status=d.status/>
                                <span class="deploy-rev">{d.commit.as_deref().map(short).unwrap_or_else(|| "—".into())}</span>
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
                        let can_redeploy = status == DeploymentStatus::Succeeded && commit.is_some() && !is_current;
                        view! {
                            <div class="deploy-head">
                                <DeployStatus status=status/>
                                {d.commit.clone().map(|c| view! { <code title=c.clone()>{c.chars().take(12).collect::<String>()}</code> })}
                                <span class="muted small">
                                    {format!("requested {}", ago(d.created_at))}
                                    {match (d.started_at, d.finished_at) {
                                        (Some(s), Some(f)) => Some(format!(" · took {}s", f.saturating_sub(s))),
                                        _ => None,
                                    }}
                                </span>
                                <span class="push-right">
                                    {(!status.is_finished()).then(|| view! {
                                        <button class="button" on:click=move |_| cancel(id)>"Cancel"</button>
                                    })}
                                    {can_redeploy.then(|| view! {
                                        <button class="button" title="Deploy this commit again (a rollback); images still on the node are reused"
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
                            <pre class="deploy-log stack-log">{strip_ansi(&d.log)}</pre>
                            {d.services.iter().map(|s| view! {
                                <ServiceLog service=s.service.clone() id=s.deployment/>
                            }).collect_view()}
                        }
                    })}
                </div>
            </div>
        </Show>
    }
}

/// One service's build and start output, folded.
#[component]
fn ServiceLog(service: String, id: Uuid) -> impl IntoView {
    let open = RwSignal::new(false);
    let log = polled(Some(Duration::from_secs(2)), move || {
        let id = id;
        async move { api::get::<DeploymentView>(&format!("/api/deployments/{id}")).await }
    });
    view! {
        <details class="service-log" on:toggle=move |_| open.update(|o| *o = !*o)>
            <summary>
                <strong>{service}</strong>
                {move || log.data.get().map(|d| view! { <DeployStatus status=d.status/> })}
            </summary>
            <Show when=move || open.get()>
                <pre class="deploy-log">{move || log.data.get().map(|d| strip_ansi(&d.log)).unwrap_or_default()}</pre>
            </Show>
        </details>
    }
}

// ---------------------------------------------------------------- settings

#[component]
fn StackSettings(stack: StackView, on_saved: Callback<()>) -> impl IntoView {
    let spec = StoredValue::new(stack.spec.clone());
    let nodes = polled(None, || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });
    let repo = RwSignal::new(stack.spec.repo.clone().unwrap_or_default());
    let branch = RwSignal::new(stack.spec.branch.clone().unwrap_or_default());
    let file = RwSignal::new(stack.spec.file.clone().unwrap_or_default());
    let use_file = RwSignal::new(stack.spec.file.is_some());
    let node = RwSignal::new(stack.spec.node.clone());
    let error = RwSignal::new(None::<String>);
    let saving = RwSignal::new(false);
    let save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let mut s = spec.get_value();
        s.repo = Some(repo.get_untracked().trim().to_string()).filter(|r| !r.is_empty());
        s.branch = Some(branch.get_untracked().trim().to_string()).filter(|b| !b.is_empty());
        s.file = use_file
            .get_untracked()
            .then(|| file.get_untracked().trim().to_string())
            .filter(|f| !f.is_empty());
        s.node = node.get_untracked();
        save_spec(s, error, saving, on_saved);
    };
    let services: Vec<String> = {
        let mut names: Vec<String> = stack
            .services
            .iter()
            .filter_map(|a| a.spec.service().map(str::to_string))
            .collect();
        names.extend(stack.spec.services.keys().cloned());
        names.sort();
        names.dedup();
        names
    };
    let has_file = stack.spec.file.is_some();
    let editing = RwSignal::new(None::<String>);
    let edit_open = RwSignal::new(false);
    let adding = RwSignal::new(false);
    let new_service = RwSignal::new(String::new());
    let stack_name = StoredValue::new(stack.spec.name.clone());
    let confirm_delete = RwSignal::new(false);
    let typed = RwSignal::new(String::new());
    let delete_volumes = RwSignal::new(false);
    let navigate = use_navigate();
    let delete = {
        move |_| {
            let name = stack_name.get_value();
            let navigate = navigate.clone();
            let volumes = delete_volumes.get_untracked();
            run(
                format!("Stack {name} deleted"),
                async move { api::delete(&format!("/api/stacks/{name}?volumes={volumes}")).await },
                move || navigate("/apps", Default::default()),
            );
        }
    };
    let name_for_check = stack_name.get_value();
    view! {
        <form class="card form" on:submit=save>
            <h2 class="section-title">"Source"</h2>
            <div class="form-row repo-row">
                <label class="field grow">
                    <span>"Repository"</span>
                    <input type="text" inputmode="url" spellcheck="false" autocomplete="off" bind:value=repo
                        placeholder="https://github.com/me/shop (optional when every service is an image)"/>
                </label>
                <label class="field">
                    <span>"Branch"</span>
                    <input type="text" bind:value=branch placeholder="default branch"/>
                </label>
            </div>
            <div class="form-row">
                <label class="check">
                    <input type="checkbox" bind:checked=use_file/>
                    "Services come from a compose file"
                </label>
                <Show when=move || use_file.get()>
                    <label class="field grow">
                        <span>"Compose file"</span>
                        <input type="text" bind:value=file placeholder="docker-compose.yml"/>
                    </label>
                </Show>
                <label class="field">
                    <span>"Node"</span>
                    <select on:change=move |ev| node.set(event_target_value(&ev)) prop:value=move || node.get()>
                        <For each=move || nodes.data.get().unwrap_or_default() key=|n| n.name.clone() let:n>
                            <option value=n.name.clone() selected={ let v = n.name.clone(); move || node.with(|x| *x == v) }>{n.name.clone()}</option>
                        </For>
                    </select>
                </label>
            </div>
            <ErrorNote error=error/>
            <div class="form-actions">
                <button type="submit" class="button button-primary" disabled=saving>"Save"</button>
            </div>
            <p class="muted small">
                "Saved settings are checked against the repository, and applied by the next deploy. "
                "Without a file, the services are the ones listed below."
            </p>
        </form>

        <section class="card">
            <h2 class="section-title">"Services"</h2>
            <p class="muted small">
                {if has_file {
                    "The compose file defines the services. What you set here (domains, limits, release command, health check, …) applies over it."
                } else {
                    "Each service runs an image, or is built from the stack's repository."
                }}
            </p>
            <div class="service-list">
                {services.into_iter().map(|svc| {
                    let label = svc.clone();
                    let overrides = stack.spec.services.get(&svc).is_some_and(|s| !s.is_empty());
                    let in_stack = stack.services.iter().any(|a| a.spec.service() == Some(svc.as_str()));
                    view! {
                        <div class="service-row">
                            <strong>{label}</strong>
                            {(has_file && overrides).then(|| view! { <span class="muted small">"settings set here"</span> })}
                            {(has_file && !in_stack).then(|| view! { <span class="muted small">"· not in the file (its settings are ignored)"</span> })}
                            <button type="button" class="link-button push-right"
                                on:click=move |_| { editing.set(Some(svc.clone())); edit_open.set(true) }>"Edit"</button>
                        </div>
                    }
                }).collect_view()}
            </div>
            <div class="form-actions">
                <button type="button" class="button" on:click=move |_| { new_service.set(String::new()); adding.set(true) }>
                    {if has_file { "Settings for a service" } else { "Add service" }}
                </button>
            </div>
        </section>

        <section class="card danger-zone">
            <div>
                <h2 class="section-title">"Delete this stack"</h2>
                <p class="muted small">"Removes its services and their variables."</p>
            </div>
            <button class="button button-danger" on:click=move |_| confirm_delete.set(true)>"Delete stack…"</button>
        </section>

        <Modal open=edit_open title="Service settings">
            {
                move || editing.get().map(|svc| view! {
                    <ServiceSettings stack=stack_name.get_value() service=svc
                        on_saved=Callback::new(move |_| { edit_open.set(false); on_saved.run(()) })/>
                })
            }
        </Modal>
        <Modal open=adding title="New service">
            {
                move || {
                    view! {
                        <label class="field">
                            <span>"Service name"</span>
                            <input type="text" bind:value=new_service placeholder="api" autocomplete="off"
                                pattern="[a-z0-9]([a-z0-9\\-]*[a-z0-9])?"/>
                            <small>"Also its host name for the other services."</small>
                        </label>
                        <Show when=move || facade_core::validate_name("service name", &new_service.get()).is_ok()>
                            {
                                move || view! {
                                    <ServiceSettings stack=stack_name.get_value() service=new_service.get()
                                        on_saved=Callback::new(move |_| { adding.set(false); on_saved.run(()) })/>
                                }
                            }
                        </Show>
                    }
                }
            }
        </Modal>
        <Modal open=confirm_delete title="Delete stack">
            <p>"This removes every service of the stack, their containers, images and variables."</p>
            <label class="check">
                <input type="checkbox" bind:checked=delete_volumes/>
                "Also delete the stack's volumes and the data in them"
            </label>
            <label class="field">
                <span>"Type " <code>{stack_name.get_value()}</code> " to confirm"</span>
                <input type="text" bind:value=typed autocomplete="off"/>
            </label>
            <div class="form-actions">
                <button class="button" on:click=move |_| confirm_delete.set(false)>"Cancel"</button>
                <button class="button button-danger"
                    disabled={
                        let name = name_for_check.clone();
                        move || typed.get() != name
                    }
                    on:click=delete.clone()>"Delete"</button>
            </div>
        </Modal>
    }
}

fn save_spec(
    spec: StackSpec,
    error: RwSignal<Option<String>>,
    saving: RwSignal<bool>,
    on_saved: Callback<()>,
) {
    saving.set(true);
    error.set(None);
    spawn_local(async move {
        match api::put::<_, StackView>(&format!("/api/stacks/{}", spec.name), &spec).await {
            Ok(_) => {
                toasts().ok("Saved. It applies from the next deploy.");
                on_saved.run(());
            }
            Err(e) => {
                error.try_set(Some(e));
            }
        }
        saving.try_set(false);
    });
}

/// Random hex from the browser's generator, for database passwords.
fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    if let Some(crypto) = web_sys::window().and_then(|w| w.crypto().ok()) {
        let _ = crypto.get_random_values_with_u8_array(&mut bytes);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A service's settings in its stack. For stacks with a file, empty fields keep
/// what the file says (shown greyed).
#[component]
pub fn ServiceSettings(stack: String, service: String, on_saved: Callback<()>) -> impl IntoView {
    let data = polled(None, {
        let stack = stack.clone();
        move || {
            let stack = stack.clone();
            async move { api::get::<StackView>(&format!("/api/stacks/{stack}")).await }
        }
    });
    let service = StoredValue::new(service);
    view! {
        <ErrorNote error=data.error/>
        {move || data.data.get().map(|s| {
            let svc = service.get_value();
            let current = s.services.iter().find(|a| a.spec.service() == Some(svc.as_str())).cloned();
            view! { <ServiceForm stack=s service=svc current=current on_saved=on_saved/> }
        })}
    }
}

#[component]
fn ServiceForm(
    stack: StackView,
    service: String,
    current: Option<AppView>,
    on_saved: Callback<()>,
) -> impl IntoView {
    let has_file = stack.spec.file.is_some();
    let settings = stack
        .spec
        .services
        .get(&service)
        .cloned()
        .unwrap_or_default();
    let s = |v: String| RwSignal::new(v);
    let opt = |v: Option<String>| RwSignal::new(v.unwrap_or_default());
    let source = RwSignal::new(match (&settings.image, &settings.build) {
        (Some(_), _) => "image",
        (_, Some(Build::Rust { .. })) => "rust",
        (_, Some(_)) => "dockerfile",
        _ if has_file => "file",
        _ => "image",
    });
    let image = opt(settings.image.clone());
    let (dockerfile, context, rust_bin, rust_package, features) = match &settings.build {
        Some(Build::Dockerfile {
            dockerfile,
            context,
        }) => (
            s(dockerfile.clone()),
            s(context.clone()),
            s("".into()),
            s("".into()),
            s("".into()),
        ),
        Some(Build::Rust {
            package,
            bin,
            features,
            ..
        }) => (
            s("".into()),
            s("".into()),
            s(bin.clone()),
            opt(package.clone()),
            s(features.join(", ")),
        ),
        _ => (
            s("".into()),
            s("".into()),
            s("".into()),
            s("".into()),
            s("".into()),
        ),
    };
    let command = opt(settings.command.as_deref().map(facade_core::join_command));
    let release = opt(settings.release_command.clone());
    let health = opt(settings.healthcheck.as_ref().map(HealthCheck::command));
    let http_port = opt(settings.http_port.map(|p| p.to_string()));
    let domains = opt(settings.domains.as_ref().map(|d| d.join("\n")));
    let ports = opt(settings.ports.as_ref().map(|p| {
        p.iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }));
    let volumes = opt(settings.volumes.as_ref().map(|v| {
        v.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }));
    let memory = opt(settings.limits.memory_mb.map(|m| m.to_string()));
    let cpus = opt(settings.limits.cpus.map(|c| c.to_string()));
    let pids = opt(settings.limits.pids.map(|p| p.to_string()));
    let stop_timeout = opt(settings.stop_timeout_secs.map(|t| t.to_string()));
    let stop_signal = opt(settings.stop_signal.clone());
    let depends_on = RwSignal::new(settings.depends_on.clone().unwrap_or_default());
    let database = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let saving = RwSignal::new(false);
    let others: Vec<String> = {
        // The services the stack has: its apps, and (without a file) its definitions.
        let mut names: Vec<String> = stack
            .services
            .iter()
            .filter_map(|a| a.spec.service().map(str::to_string))
            .chain(stack.spec.services.keys().filter(|_| !has_file).cloned())
            .filter(|n| *n != service)
            .collect();
        names.sort();
        names.dedup();
        names
    };
    // What the file (or the last deploy) says, as hints.
    let cur = current.as_ref().map(|a| a.spec.clone());
    let hint = move |f: &dyn Fn(&AppSpec) -> String, fallback: &str| {
        cur.as_ref()
            .map(f)
            .filter(|v| !v.is_empty())
            .map(|v| {
                if has_file {
                    format!("from the file: {v}")
                } else {
                    v
                }
            })
            .unwrap_or_else(|| fallback.to_string())
    };
    let ph_command = hint(
        &|a| facade_core::join_command(&a.command),
        "(the image's own command)",
    );
    let ph_release = hint(
        &|a| a.release_command.clone().unwrap_or_default(),
        "./migrate up",
    );
    let ph_health = hint(
        &|a| {
            a.healthcheck
                .as_ref()
                .map(HealthCheck::command)
                .unwrap_or_default()
        },
        "curl -fs localhost:8080/health",
    );
    let ph_port = hint(
        &|a| a.http_port.map(|p| p.to_string()).unwrap_or_default(),
        "8080",
    );
    let ph_domains = hint(&|a| a.domains.join(", "), "api.example.com");
    let ph_volumes = hint(
        &|a| {
            a.volumes
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        },
        "data:/var/lib/data",
    );
    let ph_memory = hint(
        &|a| {
            a.limits
                .memory_mb
                .map(|m| m.to_string())
                .unwrap_or_default()
        },
        "no limit",
    );
    let ph_stop_timeout = hint(
        &|a| {
            a.stop_timeout_secs
                .map(|t| t.to_string())
                .unwrap_or_default()
        },
        "10",
    );
    let ph_stop_signal = hint(&|a| a.stop_signal.clone().unwrap_or_default(), "SIGTERM");
    let ph_source = StoredValue::new(
        current
            .as_ref()
            .map(|a| source_summary(&a.spec))
            .unwrap_or_default(),
    );

    let stack_spec = StoredValue::new(stack.spec.clone());
    let service_name = StoredValue::new(service.clone());
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let text = |v: RwSignal<String>| {
            Some(v.get_untracked().trim().to_string()).filter(|v| !v.is_empty())
        };
        let lines = |v: RwSignal<String>| -> Vec<String> {
            v.get_untracked()
                .split(['\n', ','])
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        };
        let result = (|| -> Result<(ServiceSpec, Vec<EnvVar>), String> {
            let mut out = ServiceSpec::default();
            let svc = service_name.get_value();
            let spec = stack_spec.get_value();
            let mut db_env = vec![];
            if let Ok(kind) = database.get_untracked().parse::<DatabaseKind>() {
                let db = facade_core::templates::database_service(
                    kind, None, &spec.name, &svc, random_hex,
                )
                .map_err(|e| e.to_string())?;
                out = db.service;
                db_env = db.env;
            } else {
                match source.get_untracked() {
                    "image" => out.image = Some(text(image).ok_or("give an image")?),
                    "dockerfile" => {
                        let df = text(dockerfile).ok_or("give the Dockerfile's path")?;
                        let ctx = text(context).unwrap_or_else(|| {
                            std::path::Path::new(&df)
                                .parent()
                                .map(|p| p.to_string_lossy().to_string())
                                .filter(|p| !p.is_empty())
                                .unwrap_or_else(|| ".".into())
                        });
                        out.build = Some(Build::Dockerfile {
                            dockerfile: df,
                            context: ctx,
                        });
                    }
                    "rust" => {
                        out.build = Some(Build::Rust {
                            package: text(rust_package),
                            bin: text(rust_bin).ok_or("give the binary's name")?,
                            features: lines(features),
                            no_default_features: false,
                        })
                    }
                    _ => {}
                }
            }
            if let Some(c) = text(command) {
                out.command = Some(facade_core::split_command(&c).map_err(|e| e.to_string())?);
            }
            if let Some(r) = text(release) {
                out.release_command = Some(r);
            }
            if let Some(h) = text(health) {
                out.healthcheck = Some(HealthCheck::shell(&h));
            }
            if let Some(p) = text(http_port) {
                out.http_port = Some(p.parse().map_err(|_| format!("`{p}` is not a port"))?);
            }
            let d = lines(domains);
            if !d.is_empty() {
                out.domains = Some(d.into_iter().map(|d| d.to_lowercase()).collect());
            }
            let p = lines(ports);
            if !p.is_empty() {
                out.ports = Some(
                    p.iter()
                        .map(|p| p.parse::<PortMap>().map_err(|e| e.to_string()))
                        .collect::<Result<_, _>>()?,
                );
            }
            let v = lines(volumes);
            if !v.is_empty() {
                out.volumes = Some(
                    v.iter()
                        .map(|v| v.parse::<Volume>().map_err(|e| e.to_string()))
                        .collect::<Result<_, _>>()?,
                );
            }
            let deps = depends_on.get_untracked();
            if !deps.is_empty() {
                out.depends_on = Some(deps);
            }
            out.limits = Limits {
                memory_mb: text(memory)
                    .map(|m| {
                        m.parse()
                            .map_err(|_| "memory must be a number of MB".to_string())
                    })
                    .transpose()?,
                cpus: text(cpus)
                    .map(|c| c.parse().map_err(|_| "CPUs must be a number".to_string()))
                    .transpose()?,
                pids: text(pids)
                    .map(|p| {
                        p.parse()
                            .map_err(|_| "processes must be a whole number".to_string())
                    })
                    .transpose()?,
            };
            if let Some(t) = text(stop_timeout) {
                out.stop_timeout_secs = Some(
                    t.parse()
                        .map_err(|_| "the stop timeout must be a number of seconds".to_string())?,
                );
            }
            if let Some(signal) = text(stop_signal) {
                out.stop_signal = Some(signal.to_uppercase());
            }
            Ok((out, db_env))
        })();
        let (settings, db_env) = match result {
            Ok(r) => r,
            Err(e) => return error.set(Some(e)),
        };
        let mut spec = stack_spec.get_value();
        let svc = service_name.get_value();
        if settings.is_empty() {
            spec.services.remove(&svc);
        } else {
            spec.services.insert(svc.clone(), settings);
        }
        saving.set(true);
        error.set(None);
        spawn_local(async move {
            let saved =
                api::put::<_, StackView>(&format!("/api/stacks/{}", spec.name), &spec).await;
            let result = match saved {
                Ok(_) if !db_env.is_empty() => {
                    let set = db_env
                        .into_iter()
                        .map(|(k, value, sensitive)| {
                            (
                                k,
                                SetEnv {
                                    value,
                                    sensitive,
                                    build: None,
                                },
                            )
                        })
                        .collect();
                    api::patch(
                        &format!("/api/apps/{}.{svc}/env", spec.name),
                        &EnvPatch { set, unset: vec![] },
                    )
                    .await
                }
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => {
                    toasts().ok("Saved. It applies from the next deploy.");
                    on_saved.run(());
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            saving.try_set(false);
        });
    };
    let remove = move |_| {
        let mut spec = stack_spec.get_value();
        spec.services.remove(&service_name.get_value());
        save_spec(spec, error, saving, on_saved);
    };
    let is_new = current.is_none() && !stack.spec.services.contains_key(&service);

    view! {
        <form class="form service-form" on:submit=submit>
            <Show when=move || is_new && !has_file>
                <label class="field">
                    <span>"Start from"</span>
                    <select on:change=move |ev| database.set(event_target_value(&ev)) prop:value=move || database.get()>
                        <option value="">"Nothing (set the source below)"</option>
                        <option value="postgres">"PostgreSQL"</option>
                        <option value="mysql">"MySQL"</option>
                        <option value="mariadb">"MariaDB"</option>
                        <option value="redis">"Redis"</option>
                    </select>
                    <small>"A database: its image, a volume, a generated password and DATABASE_URL (REDIS_URL) for the other services to refer to."</small>
                </label>
            </Show>
            <Show when=move || database.with(String::is_empty)>
                <div class="field">
                    <span>"Runs"</span>
                    <div class="segmented" role="radiogroup">
                        {[("file", "As in the file"), ("image", "Image"), ("dockerfile", "Dockerfile"), ("rust", "Rust binary")]
                            .into_iter()
                            .filter(|(k, _)| has_file || *k != "file")
                            .map(|(key, label)| view! {
                                <button type="button" role="radio"
                                    class=move || if source.get() == key { "segment active" } else { "segment" }
                                    on:click=move |_| source.set(key)>{label}</button>
                            }).collect_view()}
                    </div>
                    <Show when=move || source.get() == "file">
                        <small>{move || ph_source.get_value()}</small>
                    </Show>
                </div>
                <Show when=move || source.get() == "image">
                    <label class="field">
                        <span>"Image"</span>
                        <input type="text" bind:value=image placeholder="postgres:18"/>
                    </label>
                </Show>
                <Show when=move || source.get() == "dockerfile">
                    <div class="form-row">
                        <label class="field grow">
                            <span>"Dockerfile"</span>
                            <input type="text" bind:value=dockerfile placeholder="apps/api/Dockerfile"/>
                        </label>
                        <label class="field">
                            <span>"Build context"</span>
                            <input type="text" bind:value=context placeholder="its directory"/>
                        </label>
                    </div>
                </Show>
                <Show when=move || source.get() == "rust">
                    <div class="form-row">
                        <label class="field"><span>"Binary"</span><input type="text" bind:value=rust_bin placeholder="server"/></label>
                        <label class="field"><span>"Package"</span><input type="text" bind:value=rust_package placeholder="(workspace root)"/></label>
                        <label class="field grow"><span>"Features"</span><input type="text" bind:value=features placeholder="server"/></label>
                    </div>
                </Show>
            </Show>
            <div class="form-row">
                <label class="field">
                    <span>"HTTP port"</span>
                    <input type="text" inputmode="numeric" bind:value=http_port placeholder=ph_port/>
                </label>
                <label class="field grow">
                    <span>"Domains"</span>
                    <textarea rows="1" bind:value=domains placeholder=ph_domains></textarea>
                </label>
            </div>
            <label class="field">
                <span>"Command"</span>
                <input type="text" bind:value=command placeholder=ph_command autocomplete="off"/>
            </label>
            <label class="field">
                <span>"Release command"</span>
                <input type="text" bind:value=release placeholder=ph_release autocomplete="off"/>
                <small>"Runs in a container of the new build before it replaces the running one (migrations)."</small>
            </label>
            <label class="field">
                <span>"Health check"</span>
                <input type="text" bind:value=health placeholder=ph_health autocomplete="off"/>
                <small>"Run in the container; exit 0 means ready. Services depending on this one start after it passes."</small>
            </label>
            <div class="form-row">
                <label class="field">
                    <span>"Stop timeout (seconds)"</span>
                    <input type="text" inputmode="numeric" bind:value=stop_timeout placeholder=ph_stop_timeout/>
                    <small>"Time to shut down cleanly before being killed."</small>
                </label>
                <label class="field">
                    <span>"Stop signal"</span>
                    <input type="text" bind:value=stop_signal placeholder=ph_stop_signal autocomplete="off"/>
                </label>
            </div>
            <div class="form-row">
                <label class="field grow">
                    <span>"Volumes"</span>
                    <textarea rows="1" class="mono" bind:value=volumes placeholder=ph_volumes></textarea>
                    <small>"name:/path. Services naming the same volume share it."</small>
                </label>
                <label class="field grow">
                    <span>"Published ports"</span>
                    <textarea rows="1" class="mono" bind:value=ports placeholder="3478:3478/udp"></textarea>
                </label>
            </div>
            {(!others.is_empty()).then(|| view! {
                <div class="field">
                    <span>"Starts after"</span>
                    <div class="checks">
                        {others.clone().into_iter().map(|o| {
                            let a = o.clone();
                            let b = o.clone();
                            view! {
                                <label class="check">
                                    <input type="checkbox" prop:checked=move || depends_on.with(|d| d.contains(&a))
                                        on:change=move |ev| {
                                            let on = event_target_checked(&ev);
                                            let b = b.clone();
                                            depends_on.update(|d| { d.retain(|x| *x != b); if on { d.push(b) } });
                                        }/>
                                    {o}
                                </label>
                            }
                        }).collect_view()}
                    </div>
                </div>
            })}
            <div class="form-row">
                <label class="field"><span>"Memory (MB)"</span><input type="text" inputmode="numeric" bind:value=memory placeholder=ph_memory/></label>
                <label class="field"><span>"CPUs"</span><input type="text" inputmode="decimal" bind:value=cpus placeholder="no limit"/></label>
                <label class="field"><span>"Processes"</span><input type="text" inputmode="numeric" bind:value=pids placeholder="4096"/></label>
            </div>
            <ErrorNote error=error/>
            <div class="form-actions">
                {(!is_new).then(|| view! {
                    <button type="button" class="button button-danger" on:click=remove
                        title=if has_file { "Forget the settings given here; the file's apply" } else { "Remove the service from the stack" }>
                        {if has_file { "Reset to the file" } else { "Remove service" }}
                    </button>
                })}
                <button type="submit" class="button button-primary" disabled=saving>"Save"</button>
            </div>
            <p class="muted small">"Applies from the next deploy of the stack."</p>
        </form>
    }
}

// ---------------------------------------------------------------- new stack

#[component]
pub fn NewStack() -> impl IntoView {
    let query = use_query_map();
    let param = move |k: &str| query.with_untracked(|q| q.get(k).unwrap_or_default());
    let name = RwSignal::new(String::new());
    let repo = RwSignal::new(param("repo"));
    let branch = RwSignal::new(param("branch"));
    let file = RwSignal::new(param("file"));
    let node = RwSignal::new(String::new());
    let nodes = polled(None, || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });
    Effect::new(move |_| {
        if node.with_untracked(String::is_empty)
            && let Some(local) = nodes
                .data
                .with(|n| n.iter().flatten().find(|n| n.local).map(|n| n.name.clone()))
        {
            node.set(local);
        }
    });
    let files = RwSignal::new(None::<Vec<String>>);
    let plan = RwSignal::new(None::<StackPlan>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let navigate = use_navigate();

    let spec = move || StackSpec {
        name: name.get_untracked().trim().to_string(),
        node: node.get_untracked(),
        repo: Some(repo.get_untracked().trim().to_string()).filter(|r| !r.is_empty()),
        branch: Some(branch.get_untracked().trim().to_string()).filter(|b| !b.is_empty()),
        file: Some(file.get_untracked().trim().to_string()).filter(|f| !f.is_empty()),
        services: Default::default(),
    };
    let scan = move |_| {
        let request = InspectRequest {
            repo: repo.get_untracked().trim().to_string(),
            branch: Some(branch.get_untracked().trim().to_string()).filter(|b| !b.is_empty()),
        };
        busy.set(true);
        error.set(None);
        plan.set(None);
        spawn_local(async move {
            match api::post::<_, InspectReport>("/api/inspect", &request).await {
                Ok(report) => {
                    if file.with_untracked(String::is_empty)
                        && let Some(first) = report.compose_files.first()
                    {
                        file.try_set(first.clone());
                    }
                    files.try_set(Some(report.compose_files));
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            busy.try_set(false);
        });
    };
    let preview = move |_| {
        let mut draft = spec();
        // A preview does not need the name yet.
        if draft.name.is_empty() {
            draft.name = "new-stack".into();
        }
        let body = StackPlanRequest {
            spec: draft,
            commit: None,
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::post::<_, StackPlan>("/api/stacks/plan", &body).await {
                Ok(p) => {
                    plan.try_set(Some(p));
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            busy.try_set(false);
        });
    };
    let create = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let body = spec();
        let navigate = navigate.clone();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::post::<_, StackView>("/api/stacks", &body).await {
                Ok(s) => {
                    let tab = if s.spec.file.is_some() {
                        "services"
                    } else {
                        "settings"
                    };
                    toasts().ok(format!("Stack {} created", s.spec.name));
                    navigate(
                        &format!("/stacks/{}?tab={tab}", s.spec.name),
                        Default::default(),
                    );
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    view! {
        <div class="page page-narrow">
            <header class="page-head">
                <div>
                    <A href="/apps" attr:class="crumb">"← Apps"</A>
                    <h1>"New stack"</h1>
                </div>
            </header>
            <p class="muted lead">
                "A stack deploys several services together from one repository, at one commit, on one node, where they reach each other by name. "
                "Its services come from a docker-compose file, read again at every deploy, or you define them here."
            </p>
            <form class="card form" on:submit=create>
                <div class="form-row">
                    <label class="field">
                        <span>"Name"</span>
                        <input type="text" bind:value=name required placeholder="shop" autocomplete="off"
                            pattern="[a-z0-9]([a-z0-9\\-]*[a-z0-9])?"/>
                        <small>"Its services are named shop.api, shop.db, …"</small>
                    </label>
                    <label class="field">
                        <span>"Node"</span>
                        <select on:change=move |ev| node.set(event_target_value(&ev)) prop:value=move || node.get()>
                            <For each=move || nodes.data.get().unwrap_or_default() key=|n| n.name.clone() let:n>
                                <option value=n.name.clone() selected={ let v = n.name.clone(); move || node.with(|x| *x == v) }>{n.name.clone()}</option>
                            </For>
                        </select>
                    </label>
                </div>
                <div class="form-row repo-row">
                    <label class="field grow">
                        <span>"Repository"</span>
                        <input type="text" inputmode="url" spellcheck="false" autocomplete="off" bind:value=repo
                            placeholder="https://github.com/me/shop"/>
                    </label>
                    <label class="field">
                        <span>"Branch"</span>
                        <input type="text" bind:value=branch placeholder="default branch"/>
                    </label>
                    <button type="button" class="button scan-button" on:click=scan
                        disabled=move || busy.get() || repo.with(|r| r.trim().is_empty())>"Scan"</button>
                </div>
                {move || files.get().map(|list| view! {
                    <div class="field">
                        <span>"Services"</span>
                        <div class="checks">
                            {list.into_iter().map(|f| {
                                let a = f.clone();
                                let b = f.clone();
                                view! {
                                    <label class="check">
                                        <input type="radio" name="file" prop:checked=move || file.with(|x| *x == a)
                                            on:change=move |_| { file.set(b.clone()); plan.set(None) }/>
                                        "from " <code>{f}</code>
                                    </label>
                                }
                            }).collect_view()}
                            <label class="check">
                                <input type="radio" name="file" prop:checked=move || file.with(String::is_empty)
                                    on:change=move |_| { file.set(String::new()); plan.set(None) }/>
                                "defined here (add them after creating the stack)"
                            </label>
                        </div>
                    </div>
                })}
                <ErrorNote error=error/>
                <div class="form-actions">
                    <Show when=move || !file.with(String::is_empty)>
                        <button type="button" class="button" on:click=preview disabled=busy>"Preview"</button>
                    </Show>
                    <button type="submit" class="button button-primary" disabled=busy>"Create stack"</button>
                </div>
                <p class="muted small">"Nothing runs until you deploy it."</p>
            </form>
            {move || plan.get().map(|p| view! { <PlanView plan=p/> })}
        </div>
    }
}
