use std::time::Duration;

use facade_core::api::{
    ApplyReport, ChangePassword, CreateGitCredential, GitCredentialKind, GitCredentialView,
    NewGitSecret, RegistryLoginView, ServerSettings, ServerSettingsUpdate, SetRegistryLogin,
};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::components::{CopyField, ErrorNote, Loading, Modal};
use crate::util::{ago, download, me, polled, run, toasts};

#[component]
pub fn Settings() -> impl IntoView {
    // Members only manage their password and their own logins.
    let me = me();
    view! {
        <div class="page page-narrow">
            <header class="page-head"><h1>"Settings"</h1></header>
            <Password/>
            <Show when=move || me.is_admin()>
                <ServerAccess/>
            </Show>
            <GitAccess/>
            <RegistryLogins/>
            <Show when=move || me.is_admin()>
                <Export/>
                <Apply/>
            </Show>
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
                                    "Reach façade at its own domain with a real certificate: point an A record for the domain at this server first."
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
                                <small>"Possible once the web UI domain has its certificate, so façade stays reachable."</small>
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
                        "Afterwards façade is only reachable at https://{d} (or through an SSH tunnel to the port). \
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

/// Logins for private container registries.
#[component]
fn RegistryLogins() -> impl IntoView {
    let me = me();
    let list = polled(None, || async {
        api::get::<Vec<RegistryLoginView>>("/api/registries").await
    });
    let host = RwSignal::new(String::new());
    let username = RwSignal::new(String::new());
    let secret = RwSignal::new(String::new());
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let path = format!(
            "/api/registries/{}",
            host.get_untracked().trim().to_lowercase()
        );
        let body = SetRegistryLogin {
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
    let remove_target = RwSignal::new(None::<RegistryLoginView>);
    let remove = move |_| {
        let Some(l) = remove_target.get_untracked() else {
            return;
        };
        run(
            format!("Login for {} removed", l.host),
            async move { api::delete(&format!("/api/registries/{}", l.host)).await },
            move || {
                remove_open.set(false);
                list.reload();
            },
        );
    };
    view! {
        <section class="card form">
            <h2 class="section-title">"Registry logins"</h2>
            <p class="muted small">
                "Used to pull private images and the images a Dockerfile's FROM names "
                "(e.g. ghcr.io, with a token that can read packages)."
                {move || if me.is_admin() {
                    " An app pulls with its owner's logins: the admins' ones below, or a member's own."
                } else {
                    " Apps you own pull with yours."
                }}
            </p>
            <ErrorNote error=list.error/>
            <Show when=move || list.loaded() fallback=Loading>
                <Show when=move || list.data.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))
                    fallback=|| view! { <p class="muted small">"No logins yet."</p> }>
                    <div class="table-wrap">
                        <table class="table">
                            <thead>
                                <tr>
                                    <th>"Registry"</th><th>"Username"</th>
                                    <Show when=move || me.is_admin()><th>"Owner"</th></Show>
                                    <th></th>
                                </tr>
                            </thead>
                            <tbody>
                                <For each=move || list.data.get().unwrap_or_default()
                                    key=|l| (l.host.clone(), l.username.clone(), l.owner.clone())
                                    let:l>
                                    {
                                        let target = l.clone();
                                        let owner = l.owner.clone();
                                        // Admins manage the admins' logins; a member's are theirs to change.
                                        let admins_own = owner.is_none();
                                        view! {
                                            <tr>
                                                <td><code>{l.host.clone()}</code></td>
                                                <td>{l.username.clone()}</td>
                                                <Show when=move || me.is_admin()>
                                                    <td class="muted">{owner.clone().unwrap_or_else(|| "admins".into())}</td>
                                                </Show>
                                                <td class="row-actions">
                                                    <Show when=move || admins_own || !me.is_admin()>
                                                        <button class="link-button danger" on:click={
                                                            let target = target.clone();
                                                            move |_| {
                                                                remove_target.set(Some(target.clone()));
                                                                remove_open.set(true);
                                                            }
                                                        }>"Remove"</button>
                                                    </Show>
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
                        <span>"Registry"</span>
                        <input type="text" bind:value=host required autocomplete="off" placeholder="ghcr.io"/>
                    </label>
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
                {move || remove_target.get().map(|l| format!(
                    "Remove the login for {}? Pulls that need it will fail.",
                    l.host
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

/// Deploy keys and access tokens for private repositories.
#[component]
fn GitAccess() -> impl IntoView {
    let me = me();
    let list = polled(None, || async {
        api::get::<Vec<GitCredentialView>>("/api/git/credentials").await
    });
    let kind = RwSignal::new("token");
    let name = RwSignal::new(String::new());
    let scope = RwSignal::new(String::new());
    let username = RwSignal::new(String::new());
    let secret = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let created = RwSignal::new(None::<GitCredentialView>);
    let add_open = RwSignal::new(false);
    let open = move |k: &'static str| {
        kind.set(k);
        name.set(String::new());
        scope.set(String::new());
        username.set(String::new());
        secret.set(String::new());
        error.set(None);
        created.set(None);
        add_open.set(true);
    };
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let token = kind.get_untracked() == "token";
        let body = CreateGitCredential {
            name: name.get_untracked().trim().to_string(),
            scope: scope.get_untracked().trim().to_string(),
            secret: if token {
                NewGitSecret::Token {
                    username: Some(username.get_untracked()).filter(|u| !u.trim().is_empty()),
                    token: secret.get_untracked(),
                }
            } else {
                NewGitSecret::SshKey {
                    private_key: Some(secret.get_untracked()).filter(|k| !k.trim().is_empty()),
                }
            },
        };
        error.set(None);
        spawn_local(async move {
            match api::post::<_, GitCredentialView>("/api/git/credentials", &body).await {
                Ok(c) => {
                    list.reload();
                    if c.kind == GitCredentialKind::Token {
                        toasts().ok(format!("Token {} saved", c.name));
                        add_open.set(false);
                    } else {
                        created.set(Some(c));
                    }
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let remove = move |n: String| {
        run(
            format!("{n} removed"),
            async move { api::delete(&format!("/api/git/credentials/{n}")).await },
            move || list.reload(),
        );
    };
    view! {
        <section class="card form">
            <h2 class="section-title">"Git access"</h2>
            <p class="muted small">
                "Public repositories need nothing. For private ones, give façade read access: a "
                <strong>"deploy key"</strong> " for one repository, or an " <strong>"access token"</strong>
                " for every repository of an account or organisation (on GitHub, a fine-grained token with read access to Contents). "
                "Credentials are encrypted on the server, used for scans and builds (submodules and private dependencies included), and never exported."
            </p>
            <ErrorNote error=list.error/>
            <Show when=move || list.data.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))>
                <div class="table-wrap">
                    <table class="table">
                        <thead>
                            <tr>
                                <th>"Name"</th><th>"Kind"</th><th>"Covers"</th><th>"Apps"</th>
                                <Show when=move || me.is_admin()><th>"Owner"</th></Show>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            <For each=move || list.data.get().unwrap_or_default() key=|c| (c.name.clone(), c.apps.clone(), c.owner.clone()) let:c>
                                {
                                    let n = c.name.clone();
                                    let owner = c.owner.clone();
                                    let public_key = c.public_key.clone();
                                    view! {
                                        <tr>
                                            <td><strong>{c.name.clone()}</strong><div class="muted small">{format!("added {}", ago(c.created_at))}</div></td>
                                            <td>{match c.kind { GitCredentialKind::SshKey => "deploy key", GitCredentialKind::Token => "token" }}</td>
                                            <td><code>{c.scope.clone()}</code></td>
                                            <td class="small">{if c.apps.is_empty() { "—".to_string() } else { c.apps.join(", ") }}</td>
                                            <Show when=move || me.is_admin()>
                                                <td class="muted">{owner.clone().unwrap_or_else(|| "admins".into())}</td>
                                            </Show>
                                            <td class="row-actions">
                                                {public_key.map(|k| view! {
                                                    <button type="button" class="link-button" on:click=move |_| {
                                                        crate::util::copy_to_clipboard(&k);
                                                        toasts().ok("Public key copied");
                                                    }>"Copy public key"</button>
                                                })}
                                                <button type="button" class="link-button danger" on:click=move |_| remove(n.clone())>"Remove"</button>
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
                <button type="button" class="button" on:click=move |_| open("key")>"New deploy key"</button>
                <button type="button" class="button button-primary" on:click=move |_| open("token")>"Add access token"</button>
            </div>
        </section>

        <Modal open=add_open title="Git access">
            <Show when=move || created.with(Option::is_none) fallback=move || view! {
                <p>"Add this public key to the repository as a read-only deploy key:"</p>
                <CopyField value=Signal::derive(move || created.get().and_then(|c| c.public_key).unwrap_or_default())/>
                {move || created.get().and_then(|c| c.add_key_url).map(|url| view! {
                    <p><a href=url target="_blank" rel="noopener">"Open the repository's deploy key page ↗"</a></p>
                })}
                <div class="form-actions">
                    <button class="button button-primary" on:click=move |_| add_open.set(false)>"Done"</button>
                </div>
            }>
                <form class="form" on:submit=submit>
                    <label class="field">
                        <span>"Name"</span>
                        <input type="text" bind:value=name required autocomplete="off"
                            placeholder=move || if kind.get() == "token" { "github-my-org" } else { "shop-deploy-key" }
                            pattern="[a-z0-9]([a-z0-9\\-]*[a-z0-9])?"/>
                    </label>
                    <label class="field">
                        <span>{move || if kind.get() == "token" { "Repositories it covers" } else { "Repository" }}</span>
                        <input type="text" bind:value=scope required autocomplete="off"
                            placeholder=move || if kind.get() == "token" { "github.com/my-org" } else { "https://github.com/me/app" }/>
                        <small>{move || if kind.get() == "token" {
                            "A host and owner (github.com/my-org), one repository, or a whole host."
                        } else {
                            "Deploy keys work for one repository."
                        }}</small>
                    </label>
                    <Show when=move || kind.get() == "token" fallback=move || view! {
                        <label class="field">
                            <span>"Private key (optional)"</span>
                            <textarea rows="4" class="mono" bind:value=secret
                                placeholder="Leave empty to have façade generate one (recommended)"></textarea>
                        </label>
                    }>
                        <label class="field">
                            <span>"Token"</span>
                            <input type="password" bind:value=secret required autocomplete="off"/>
                        </label>
                        <label class="field">
                            <span>"User name"</span>
                            <input type="text" bind:value=username autocomplete="off" placeholder="x-access-token"/>
                            <small>"Sent with the token over HTTPS. The default works for GitHub and GitLab."</small>
                        </label>
                    </Show>
                    <ErrorNote error=error/>
                    <div class="form-actions">
                        <button type="button" class="button" on:click=move |_| add_open.set(false)>"Cancel"</button>
                        <button type="submit" class="button button-primary">
                            {move || if kind.get() == "token" { "Save token" } else if secret.with(|s| s.trim().is_empty()) { "Generate key" } else { "Save key" }}
                        </button>
                    </div>
                </form>
            </Show>
        </Modal>
    }
}
