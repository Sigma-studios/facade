use std::time::Duration;

use facade_core::api::{
    ApplyReport, ChangePassword, CredentialView, ServerSettings, ServerSettingsUpdate,
    SetCredential,
};
use facade_core::proto::CredentialKind;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::components::{CopyField, ErrorNote, Loading, Modal};
use crate::util::{download, polled, run, toasts};

#[component]
pub fn Settings() -> impl IntoView {
    view! {
        <div class="page page-narrow">
            <header class="page-head"><h1>"Settings"</h1></header>
            <Password/>
            <ServerAccess/>
            <Credentials/>
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
    let navigate = use_navigate();
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if password.get_untracked() != again.get_untracked() {
            return error.set(Some("The two passwords differ".into()));
        }
        error.set(None);
        let body = ChangePassword {
            password: password.get_untracked(),
        };
        // Every session ends with the change, this one included.
        let navigate = navigate.clone();
        run(
            "Password changed. Sign in with the new one.",
            async move { api::put_empty("/api/password", &body).await },
            move || {
                api::set_token(None);
                navigate("/login", Default::default());
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

/// The web UI domain, the Let's Encrypt contact and who may use the direct API port.
#[component]
fn ServerAccess() -> impl IntoView {
    let settings = polled(Some(Duration::from_secs(30)), || async {
        api::get::<ServerSettings>("/api/server-settings").await
    });
    let domain = RwSignal::new(String::new());
    let email = RwSignal::new(String::new());
    // The inputs are filled once, so a refresh does not overwrite what is being typed.
    let filled = StoredValue::new(false);
    let fill = move |s: &ServerSettings| {
        domain.set(s.api_domain.clone().unwrap_or_default());
        email.set(s.acme_email.clone().unwrap_or_default());
    };
    Effect::new(move |_| {
        if !filled.get_value()
            && let Some(s) = settings.data.get()
        {
            fill(&s);
            filled.set_value(true);
        }
    });
    let busy = RwSignal::new(false);
    let confirm_open = RwSignal::new(false);
    let save = move |update: ServerSettingsUpdate, done: &'static str| {
        busy.set(true);
        spawn_local(async move {
            match api::put::<_, ServerSettings>("/api/server-settings", &update).await {
                Ok(s) => {
                    fill(&s);
                    settings.data.try_set(Some(s));
                    confirm_open.try_set(false);
                    toasts().ok(done);
                }
                Err(e) => toasts().err(e),
            }
            busy.try_set(false);
        });
    };
    let save_domain = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let update = ServerSettingsUpdate {
            api_domain: Some(domain.get_untracked().trim().to_lowercase()),
            ..Default::default()
        };
        save(update, "Web UI domain saved");
    };
    let save_email = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let update = ServerSettingsUpdate {
            acme_email: Some(email.get_untracked().trim().to_string()),
            ..Default::default()
        };
        save(update, "Contact email saved");
    };
    let local_only = move |on: bool| {
        let update = ServerSettingsUpdate {
            direct_api_local_only: Some(on),
            ..Default::default()
        };
        save(
            update,
            if on {
                "The direct API port now only accepts connections from the server"
            } else {
                "The direct API port accepts connections from anywhere again"
            },
        );
    };
    let current = move || settings.data.get();
    let saved_domain = move || current().and_then(|s| s.api_domain);
    let ready = move || current().is_some_and(|s| s.api_domain_ready);
    let restricted = move || current().is_some_and(|s| s.direct_api_local_only);

    view! {
        <section class="card form">
            <h2 class="section-title">"Server access"</h2>
            <ErrorNote error=settings.error/>
            <Show when=move || settings.loaded() fallback=Loading>
                <form class="form" on:submit=save_domain>
                    <label class="field">
                        <span>"Web UI domain"</span>
                        <div class="form-row">
                            <input type="text" class="grow" bind:value=domain
                                placeholder="facade.example.com" autocomplete="off"/>
                            <button type="submit" class="button" disabled=busy>"Save"</button>
                        </div>
                        <small>
                            {move || match (saved_domain(), ready()) {
                                (None, _) => view! {
                                    "Reach facade at its own domain with a real certificate: point an A record for the domain at this server first."
                                }.into_any(),
                                (Some(d), false) => view! {
                                    {format!("Waiting for the certificate of {d} (check its DNS on the ")}
                                    <A href="/domains">"Domains"</A>
                                    " page)."
                                }.into_any(),
                                (Some(d), true) => view! {
                                    "Live at "
                                    <a href=format!("https://{d}")>{format!("https://{d}")}</a>
                                    "."
                                }.into_any(),
                            }}
                        </small>
                    </label>
                </form>
                <form class="form" on:submit=save_email>
                    <label class="field">
                        <span>"Let's Encrypt contact email"</span>
                        <div class="form-row">
                            <input type="text" class="grow" bind:value=email
                                placeholder="admin@example.com" autocomplete="email"/>
                            <button type="submit" class="button" disabled=busy>"Save"</button>
                        </div>
                        <small>"Told about certificates that fail to renew. Optional."</small>
                    </label>
                </form>
                <div class="field">
                    <span>"Direct API port"</span>
                    <p class="small">
                        <code>{move || current().map(|s| s.api_listen).unwrap_or_default()}</code>
                        {move || if restricted() {
                            " only accepts connections from the server itself (SSH tunnels)."
                        } else {
                            " (self-signed certificate) accepts connections from anywhere."
                        }}
                    </p>
                    <Show
                        when=restricted
                        fallback=move || view! {
                            <div class="form-actions push-left">
                                <button class="button" disabled=move || busy.get() || !ready()
                                    on:click=move |_| confirm_open.set(true)>"Restrict to this server…"</button>
                            </div>
                            <Show when=move || !ready()>
                                <small>"Possible once the web UI domain has its certificate, so facade stays reachable."</small>
                            </Show>
                        }
                    >
                        <div class="form-actions push-left">
                            <button class="button" disabled=busy
                                on:click=move |_| local_only(false)>"Open to everyone again"</button>
                        </div>
                    </Show>
                </div>
            </Show>
        </section>

        <Modal open=confirm_open title="Restrict the direct API port">
            <p>
                {move || {
                    let d = saved_domain().unwrap_or_default();
                    format!(
                        "Afterwards facade is only reachable at https://{d} (or through an SSH tunnel to the port). \
                         The CLI must sign in again with `facade login https://{d}`."
                    )
                }}
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| confirm_open.set(false)>"Cancel"</button>
                <button class="button button-danger" disabled=busy
                    on:click=move |_| local_only(true)>"Restrict"</button>
            </div>
        </Modal>
    }
}

#[component]
fn Credentials() -> impl IntoView {
    let list = polled(None, || async {
        api::get::<Vec<CredentialView>>("/api/credentials").await
    });
    let kind = RwSignal::new("registry".to_string());
    let host = RwSignal::new(String::new());
    let username = RwSignal::new(String::new());
    let secret = RwSignal::new(String::new());
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let path = format!(
            "/api/credentials/{}/{}",
            kind.get_untracked(),
            host.get_untracked().trim().to_lowercase()
        );
        let body = SetCredential {
            username: username.get_untracked().trim().to_string(),
            secret: secret.get_untracked(),
        };
        run(
            "Login saved",
            async move { api::put_empty(&path, &body).await },
            move || {
                host.set(String::new());
                username.set(String::new());
                secret.set(String::new());
                list.reload();
            },
        );
    };
    let remove_open = RwSignal::new(false);
    let remove_target = RwSignal::new(None::<CredentialView>);
    let remove = move |_| {
        let Some(c) = remove_target.get_untracked() else {
            return;
        };
        run(
            format!("Login for {} removed", c.host),
            async move { api::delete(&format!("/api/credentials/{}/{}", c.kind, c.host)).await },
            move || {
                remove_open.set(false);
                list.reload();
            },
        );
    };
    view! {
        <section class="card form">
            <h2 class="section-title">"Registry & git logins"</h2>
            <p class="muted small">
                "Registry logins are used to pull private images and the images a Dockerfile's FROM names "
                "(e.g. ghcr.io, with a token that can read packages). Git logins are used to clone private "
                "repositories over HTTPS (e.g. github.com, with a token that can read the repository)."
            </p>
            <ErrorNote error=list.error/>
            <Show when=move || list.loaded() fallback=Loading>
                <Show when=move || list.data.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))
                    fallback=|| view! { <p class="muted small">"No logins yet."</p> }>
                    <div class="table-wrap">
                        <table class="table">
                            <thead><tr><th>"Kind"</th><th>"Host"</th><th>"Username"</th><th></th></tr></thead>
                            <tbody>
                                <For each=move || list.data.get().unwrap_or_default()
                                    key=|c| (c.kind, c.host.clone(), c.username.clone())
                                    let:c>
                                    {
                                        let target = c.clone();
                                        view! {
                                            <tr>
                                                <td>{c.kind.to_string()}</td>
                                                <td><code>{c.host.clone()}</code></td>
                                                <td>{c.username.clone()}</td>
                                                <td class="row-actions">
                                                    <button class="link-button danger" on:click=move |_| {
                                                        remove_target.set(Some(target.clone()));
                                                        remove_open.set(true);
                                                    }>"Remove"</button>
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
            <form class="form" on:submit=submit>
                <div class="form-row">
                    <label class="field">
                        <span>"Kind"</span>
                        <select on:change=move |ev| kind.set(event_target_value(&ev)) prop:value=move || kind.get()>
                            <option value=CredentialKind::Registry.to_string()>"Registry"</option>
                            <option value=CredentialKind::Git.to_string()>"Git"</option>
                        </select>
                    </label>
                    <label class="field">
                        <span>"Host"</span>
                        <input type="text" bind:value=host required autocomplete="off"
                            placeholder=move || if kind.get() == "git" { "github.com" } else { "ghcr.io" }/>
                    </label>
                </div>
                <div class="form-row">
                    <label class="field">
                        <span>"Username"</span>
                        <input type="text" bind:value=username required autocomplete="off"/>
                    </label>
                    <label class="field">
                        <span>"Token"</span>
                        <input type="password" bind:value=secret required autocomplete="new-password"/>
                        <small>"Or password. Stored encrypted, never shown again."</small>
                    </label>
                </div>
                <div class="form-actions">
                    <button type="submit" class="button button-primary">"Save login"</button>
                </div>
            </form>
        </section>

        <Modal open=remove_open title="Remove login">
            <p>
                {move || remove_target.get().map(|c| format!(
                    "Remove the {} login for {}? Pulls and clones that need it will fail.",
                    c.kind, c.host
                ))}
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| remove_open.set(false)>"Cancel"</button>
                <button class="button button-danger" on:click=remove>"Remove"</button>
            </div>
        </Modal>
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
