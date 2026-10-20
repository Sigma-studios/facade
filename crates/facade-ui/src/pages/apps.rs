use std::time::Duration;

use facade_core::api::{AppView, NodeView};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;

use super::overview::AppsTable;
use super::spec_form::{SpecFields, SpecForm};
use crate::api;
use crate::components::{ErrorNote, Loading};
use crate::util::{polled, toasts};

#[component]
pub fn Apps() -> impl IntoView {
    let apps = polled(Some(Duration::from_secs(5)), || async {
        api::get::<Vec<AppView>>("/api/apps").await
    });
    view! {
        <div class="page">
            <header class="page-head">
                <h1>"Apps"</h1>
                <A href="/apps/new" attr:class="button button-primary">"New app"</A>
            </header>
            <ErrorNote error=apps.error/>
            <Show when=move || apps.loaded() fallback=Loading>
                <AppsTable apps=Signal::derive(move || apps.data.get().unwrap_or_default())/>
            </Show>
        </div>
    }
}

#[component]
pub fn NewApp() -> impl IntoView {
    let nodes = polled(None, || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
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
