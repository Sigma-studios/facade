use std::time::Duration;

use facade_core::api::{AppView, CreateDatabase, DatabaseKind, NodeView, StackView};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;

use super::overview::AppsTable;
use super::spec_form::{SpecFields, SpecForm};
use super::stack::StacksList;
use crate::api;
use crate::components::{ErrorNote, Loading, Modal};
use crate::util::{me, polled, toasts};

#[component]
pub fn Apps() -> impl IntoView {
    // Stacks and databases are for admins; members see their apps in one table.
    let me = me();
    let apps = polled(Some(Duration::from_secs(5)), || async {
        api::get::<Vec<AppView>>("/api/apps").await
    });
    let stacks = polled(Some(Duration::from_secs(5)), move || {
        let admin = me.is_admin();
        async move {
            if admin {
                api::get::<Vec<StackView>>("/api/stacks").await
            } else {
                Ok(vec![])
            }
        }
    });
    let standalone = Signal::derive(move || {
        let grouped = me.is_admin();
        apps.data
            .get()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !grouped || a.spec.stack.is_none())
            .collect::<Vec<_>>()
    });
    let db_open = RwSignal::new(false);
    view! {
        <div class="page">
            <header class="page-head">
                <h1>"Apps"</h1>
                <div class="actions">
                    <Show when=move || me.is_admin()>
                        <A href="/stacks/new" attr:class="button">"New stack"</A>
                        <button class="button" on:click=move |_| db_open.set(true)>"New database"</button>
                    </Show>
                    <A href="/apps/new" attr:class="button button-primary">"New app"</A>
                </div>
            </header>
            <ErrorNote error=apps.error/>
            <Show when=move || apps.loaded() fallback=Loading>
                // With stacks only, no "create your first app" invitation.
                <Show when=move || !standalone.with(Vec::is_empty) || stacks.data.with(|s| s.as_ref().is_none_or(Vec::is_empty))>
                    <AppsTable apps=standalone/>
                </Show>
            </Show>
            <Show when=move || stacks.data.with(|s| s.as_ref().is_some_and(|s| !s.is_empty()))>
                <h2 class="section-title stacks-title">"Stacks"</h2>
                <StacksList stacks=Signal::derive(move || stacks.data.get().unwrap_or_default())/>
            </Show>
            <Show when=move || me.is_admin()>
                <NewDatabase open=db_open apps=Signal::derive(move || apps.data.get().unwrap_or_default())/>
            </Show>
        </div>
    }
}

#[component]
pub fn NewApp() -> impl IntoView {
    let nodes = polled(None, || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });
    let apps = polled(None, || async {
        api::get::<Vec<AppView>>("/api/apps").await
    });
    let form = SpecForm::new(String::new());
    // Default to the server's own node once the list arrives.
    Effect::new(move |_| {
        if let Some(local) = nodes
            .data
            .with(|n| n.iter().flatten().find(|n| n.local).map(|n| n.name.clone()))
        {
            form.default_node(local);
        }
    });
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let navigate = use_navigate();

    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let spec = match form.to_spec() {
            Ok(spec) => spec,
            Err(e) => return error.set(Some(e)),
        };
        error.set(None);
        busy.set(true);
        let navigate = navigate.clone();
        spawn_local(async move {
            match api::post::<_, AppView>("/api/apps", &spec).await {
                Ok(app) => {
                    toasts().ok(format!(
                        "{} created. Add its environment variables, then deploy.",
                        app.spec.name
                    ));
                    navigate(
                        &format!("/apps/{}?tab=env", app.spec.name),
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
                    <h1>"New app"</h1>
                </div>
            </header>
            <form class="card form" on:submit=submit>
                <SpecFields form nodes=Signal::derive(move || nodes.data.get().unwrap_or_default())
                    apps=Signal::derive(move || apps.data.get().unwrap_or_default())
                    editing=false/>
                <ErrorNote error=error/>
                <div class="form-actions">
                    <A href="/apps" attr:class="button">"Cancel"</A>
                    <button class="button button-primary" type="submit" disabled=busy>
                        {move || if busy.get() { "Creating…" } else { "Create app" }}
                    </button>
                </div>
                <p class="muted small">"Nothing runs until you deploy it."</p>
            </form>
        </div>
    }
}

const KINDS: &[(&str, &str)] = &[
    ("postgres", "PostgreSQL"),
    ("mysql", "MySQL"),
    ("mariadb", "MariaDB"),
    ("redis", "Redis"),
];

/// A database app: official image, a volume, a generated password, and a
/// connection URL handed to the apps it is linked to.
#[component]
fn NewDatabase(open: RwSignal<bool>, #[prop(into)] apps: Signal<Vec<AppView>>) -> impl IntoView {
    let nodes = polled(None, || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });
    let name = RwSignal::new(String::new());
    let kind = RwSignal::new("postgres".to_string());
    let version = RwSignal::new(String::new());
    let node = RwSignal::new(String::new());
    let memory = RwSignal::new(String::new());
    let linked = RwSignal::new(Vec::<String>::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let navigate = use_navigate();
    Effect::new(move |_| {
        if node.with_untracked(String::is_empty)
            && let Some(local) = nodes
                .data
                .with(|n| n.iter().flatten().find(|n| n.local).map(|n| n.name.clone()))
        {
            node.set(local);
        }
    });
    let parsed_kind = move || {
        kind.get()
            .parse::<DatabaseKind>()
            .unwrap_or(DatabaseKind::Postgres)
    };
    let candidates = move || {
        let node = node.get();
        apps.get()
            .into_iter()
            .filter(|a| a.spec.node == node && !a.spec.host_network && !a.spec.is_static())
            .map(|a| a.spec.name)
            .collect::<Vec<_>>()
    };
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let memory_mb = match memory.get_untracked().trim() {
            "" => None,
            m => match m.parse::<u64>() {
                Ok(m) => Some(m),
                Err(_) => return error.set(Some("memory must be a number of MB".into())),
            },
        };
        let body = CreateDatabase {
            name: name.get_untracked().trim().to_string(),
            kind: parsed_kind(),
            version: Some(version.get_untracked().trim().to_string()).filter(|v| !v.is_empty()),
            node: Some(node.get_untracked()).filter(|n| !n.is_empty()),
            memory_mb,
            link: linked.get_untracked(),
            env_var: None,
        };
        busy.set(true);
        error.set(None);
        let navigate = navigate.clone();
        spawn_local(async move {
            match api::post::<_, AppView>("/api/databases", &body).await {
                Ok(app) => {
                    toasts().ok(format!("{} created and starting", app.spec.name));
                    open.set(false);
                    navigate(&format!("/apps/{}", app.spec.name), Default::default());
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <Modal open=open title="New database">
            <form class="form" on:submit=submit.clone()>
                <div class="form-row">
                    <label class="field">
                        <span>"Name"</span>
                        <input type="text" bind:value=name required placeholder="db"
                            pattern="[a-z0-9]([a-z0-9\\-]*[a-z0-9])?" autocomplete="off"/>
                        <small>"Also its host name for linked apps."</small>
                    </label>
                    <label class="field">
                        <span>"Database"</span>
                        <select on:change=move |ev| kind.set(event_target_value(&ev)) prop:value=move || kind.get()>
                            {KINDS.iter().map(|(k, label)| view! { <option value=*k>{*label}</option> }).collect_view()}
                        </select>
                    </label>
                </div>
                <div class="form-row">
                    <label class="field">
                        <span>"Version"</span>
                        <input type="text" bind:value=version autocomplete="off"
                            placeholder=move || parsed_kind().default_version()/>
                    </label>
                    <label class="field">
                        <span>"Node"</span>
                        <select on:change=move |ev| { node.set(event_target_value(&ev)); linked.set(vec![]) }
                            prop:value=move || node.get()>
                            <For each=move || nodes.data.get().unwrap_or_default() key=|n| n.name.clone() let:n>
                                <option value=n.name.clone()>{n.name.clone()}</option>
                            </For>
                        </select>
                    </label>
                    <label class="field">
                        <span>"Memory (MB)"</span>
                        <input type="number" min="64" bind:value=memory placeholder="no limit"/>
                    </label>
                </div>
                <div class="field">
                    <span>"Connect these apps"</span>
                    <Show when=move || !candidates().is_empty()
                        fallback=|| view! { <small>"No app on this node yet; link apps to it later from their settings."</small> }>
                        <div class="checks">
                            {move || candidates().into_iter().map(|app| {
                                let a1 = app.clone();
                                let a2 = app.clone();
                                view! {
                                    <label class="check">
                                        <input type="checkbox"
                                            prop:checked=move || linked.with(|l| l.contains(&a1))
                                            on:change=move |ev| {
                                                let on = event_target_checked(&ev);
                                                let app = a2.clone();
                                                linked.update(|l| { l.retain(|x| *x != app); if on { l.push(app) } });
                                            }/>
                                        {app}
                                    </label>
                                }
                            }).collect_view()}
                        </div>
                        <small>{move || format!(
                            "Each gets {} (a reference to the database's own, so the password lives in one place) and restarts.",
                            parsed_kind().url_var()
                        )}</small>
                    </Show>
                </div>
                <p class="muted small">
                    "Its data lives in a volume on the node. The password is generated and kept encrypted; "
                    "the database is only reachable from the apps linked to it."
                </p>
                <ErrorNote error=error/>
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| open.set(false)>"Cancel"</button>
                    <button type="submit" class="button button-primary" disabled=busy>
                        {move || if busy.get() { "Creating…" } else { "Create and start" }}
                    </button>
                </div>
            </form>
        </Modal>
    }
}
