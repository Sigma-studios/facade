//! The facade web UI: a client-side Leptos app served by `facaded` next to the API.

mod api;
mod components;
mod pages;
mod util;

use std::time::Duration;

use facade_core::api::{AlertView, LoginRequest, LoginResponse, WhoAmI};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::{A, Outlet, ParentRoute, Route, Router, Routes};
use leptos_router::hooks::use_navigate;
use leptos_router::path;

use util::polled;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    view! {
        <Router>
            <Routes fallback=|| view! { <div class="page"><h1>"Not found"</h1></div> }>
                <Route path=path!("/login") view=Login/>
                <ParentRoute path=path!("") view=Shell>
                    <Route path=path!("") view=pages::overview::Overview/>
                    <Route path=path!("apps") view=pages::apps::Apps/>
                    <Route path=path!("apps/new") view=pages::apps::NewApp/>
                    <Route path=path!("apps/:name") view=pages::app::AppPage/>
                    <Route path=path!("nodes") view=pages::nodes::Nodes/>
                    <Route path=path!("domains") view=pages::domains::Domains/>
                    <Route path=path!("alerts") view=pages::alerts::Alerts/>
                    <Route path=path!("settings") view=pages::settings::Settings/>
                </ParentRoute>
            </Routes>
        </Router>
        <ToastHost/>
    }
}

/// Sidebar layout for every page but login. Sends visitors without a session to
/// the login page.
#[component]
fn Shell() -> impl IntoView {
    let navigate = use_navigate();
    if api::token().is_none() {
        navigate("/login", Default::default());
    }
    let alerts = polled(Some(Duration::from_secs(15)), || async {
        api::get::<Vec<AlertView>>("/api/alerts").await
    });
    let whoami = polled(None, || async { api::get::<WhoAmI>("/api/whoami").await });
    let must_change_password = move || {
        whoami
            .data
            .with(|w| w.as_ref().is_some_and(|w| w.must_change_password))
    };
    let firing = move || {
        alerts.data.with(|a| {
            a.as_ref()
                .map_or(0, |a| a.iter().filter(|x| x.resolved_at.is_none()).count())
        })
    };
    let logout = move |_| {
        spawn_local(async move {
            let _ = api::act("/api/logout").await;
            api::set_token(None);
            let _ = window().location().set_href("/login");
        });
    };
    view! {
        <div class="shell">
            <nav class="sidebar" aria-label="Main">
                <A href="/" attr:class="brand">
                    <span class="brand-mark" aria-hidden="true">"▚"</span>
                    "facade"
                </A>
                <ul class="nav">
                    <li><A href="/" exact=true>"Overview"</A></li>
                    <li><A href="/apps">"Apps"</A></li>
                    <li><A href="/nodes">"Nodes"</A></li>
                    <li><A href="/domains">"Domains"</A></li>
                    <li>
                        <A href="/alerts">
                            "Alerts"
                            <Show when=move || { firing() > 0 }>
                                <span class="nav-count" title="firing alerts">{firing}</span>
                            </Show>
                        </A>
                    </li>
                    <li><A href="/settings">"Settings"</A></li>
                </ul>
                <button class="nav-logout" on:click=logout>"Log out"</button>
            </nav>
            <main class="content">
                <Show when=must_change_password>
                    <div class="shell-banner">
                        <A href="/settings" attr:class="banner banner-critical">
                            <span aria-hidden="true">"⚠"</span>
                            <span>
                                <strong>"Change the admin password."</strong>
                                " It is still the one generated at install. Set your own in Settings."
                            </span>
                        </A>
                    </div>
                </Show>
                <Outlet/>
            </main>
        </div>
    }
}

#[component]
fn Login() -> impl IntoView {
    let username = RwSignal::new("admin".to_string());
    let password = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let navigate = use_navigate();

    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        busy.set(true);
        error.set(None);
        let navigate = navigate.clone();
        spawn_local(async move {
            let request = LoginRequest {
                username: username.get_untracked(),
                password: password.get_untracked(),
            };
            match api::post::<_, LoginResponse>("/api/login", &request).await {
                Ok(LoginResponse { token }) => {
                    api::set_token(Some(&token));
                    navigate("/", Default::default());
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    view! {
        <div class="login">
            <form class="card login-card" on:submit=submit>
                <div class="brand login-brand">
                    <span class="brand-mark" aria-hidden="true">"▚"</span>
                    "facade"
                </div>
                <label class="field">
                    <span>"Username"</span>
                    <input type="text" autocomplete="username" bind:value=username required/>
                </label>
                <label class="field">
                    <span>"Password"</span>
                    <input type="password" autocomplete="current-password" bind:value=password
                        required autofocus/>
                </label>
                <components::ErrorNote error=error/>
                <button class="button button-primary" type="submit" disabled=busy>
                    {move || if busy.get() { "Signing in…" } else { "Sign in" }}
                </button>
            </form>
        </div>
    }
}

#[component]
fn ToastHost() -> impl IntoView {
    let toasts = util::toasts();
    view! {
        <div class="toasts" aria-live="polite">
            <For each=move || toasts.0.get() key=|t| t.id let:toast>
                <div class=if toast.error { "toast toast-error" } else { "toast" }>
                    <span aria-hidden="true">{if toast.error { "⚠" } else { "✓" }}</span>
                    <span class="toast-text">{toast.message.clone()}</span>
                    <button class="icon-button" aria-label="Dismiss"
                        on:click=move |_| toasts.dismiss(toast.id)>"✕"</button>
                </div>
            </For>
        </div>
    }
}
