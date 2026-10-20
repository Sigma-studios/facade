use facade_core::api::{ApplyReport, ChangePassword};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::{CopyField, ErrorNote};
use crate::util::{download, run, toasts};

#[component]
pub fn Settings() -> impl IntoView {
    view! {
        <div class="page page-narrow">
            <header class="page-head"><h1>"Settings"</h1></header>
            <Password/>
            <Export/>
            <Apply/>
        </div>
    }
}

#[component]
fn Password() -> impl IntoView {
    let password = RwSignal::new(String::new());
    let again = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if password.get_untracked() != again.get_untracked() {
            return error.set(Some("The two passwords differ".into()));
        }
        error.set(None);
        let body = ChangePassword {
            password: password.get_untracked(),
        };
        run(
            "Password changed. Other sessions were signed out.",
            async move { api::put_empty("/api/password", &body).await },
            move || {
                password.set(String::new());
                again.set(String::new());
            },
        );
    };
    view! {
        <section class="card form">
            <h2 class="section-title">"Password"</h2>
            <form class="form" on:submit=submit>
                <div class="form-row">
                    <label class="field">
                        <span>"New password"</span>
                        <input type="password" bind:value=password minlength="12" required
                            autocomplete="new-password"/>
                        <small>"At least 12 characters."</small>
                    </label>
                    <label class="field">
                        <span>"Repeat"</span>
                        <input type="password" bind:value=again required autocomplete="new-password"/>
                    </label>
                </div>
                <ErrorNote error=error/>
                <div class="form-actions">
                    <button type="submit" class="button button-primary">"Change password"</button>
                </div>
            </form>
        </section>
    }
}

#[component]
fn Export() -> impl IntoView {
    let export = move |_| {
        spawn_local(async move {
            match api::get_text("/api/export").await {
                Ok(toml) => download("facade.toml", &toml, "application/toml"),
                Err(e) => toasts().err(e),
            }
        });
    };
    view! {
        <section class="card form">
            <h2 class="section-title">"Export configuration"</h2>
            <p class="muted small">
                "Every node and app as TOML, to keep in git or restore elsewhere. Sensitive variables are "
                "listed without their values."
            </p>
            <div class="form-actions">
                <button class="button" on:click=export>"Download facade.toml"</button>
            </div>
        </section>
    }
}

#[component]
fn Apply() -> impl IntoView {
    let text = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let report = RwSignal::new(None::<ApplyReport>);
    let busy = RwSignal::new(false);
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        error.set(None);
        report.set(None);
        busy.set(true);
        let body = text.get_untracked();
        spawn_local(async move {
            match api::post_text::<ApplyReport>("/api/apply", body).await {
                Ok(r) => report.set(Some(r)),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    view! {
        <section class="card form">
            <h2 class="section-title">"Apply configuration"</h2>
            <p class="muted small">
                "Creates or updates the nodes and apps described in a facade.toml. Nothing is deleted, "
                "and new apps are not deployed."
            </p>
            <form class="form" on:submit=submit>
                <label class="field">
                    <span>"facade.toml"</span>
                    <textarea class="mono" rows="10" bind:value=text required
                        placeholder="[[app]]\nname = \"landing\"\nnode = \"main\"\n…"></textarea>
                </label>
                <ErrorNote error=error/>
                <div class="form-actions">
                    <button type="submit" class="button button-primary" disabled=busy>"Apply"</button>
                </div>
            </form>
            {move || report.get().map(|r| view! {
                <div class="report">
                    {[("Created", r.created.clone()), ("Updated", r.updated.clone()), ("Unchanged", r.unchanged.clone())]
                        .into_iter()
                        .filter(|(_, list)| !list.is_empty())
                        .map(|(label, list)| view! { <p><strong>{label} ": "</strong>{list.join(", ")}</p> })
                        .collect_view()}
                    {(!r.missing_secrets.is_empty()).then(|| view! {
                        <div class="note">
                            <strong>"Sensitive variables to set: "</strong>
                            {r.missing_secrets.join(", ")}
                        </div>
                    })}
                    {r.new_nodes.iter().map(|n| view! {
                        <p>"New node " <strong>{n.name.clone()}</strong> ", start its agent with:"</p>
                        <CopyField value=format!("facaded agent --join {}", n.join_token)/>
                    }).collect_view()}
                </div>
            })}
        </section>
    }
}
