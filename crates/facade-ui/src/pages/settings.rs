use facade_core::api::{
    ApplyReport, ChangePassword, CreateGitCredential, GitCredentialKind, GitCredentialView,
    NewGitSecret,
};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::{CopyField, ErrorNote, Modal};
use crate::util::{ago, download, polled, run, toasts};

#[component]
pub fn Settings() -> impl IntoView {
    view! {
        <div class="page page-narrow">
            <header class="page-head"><h1>"Settings"</h1></header>
            <GitAccess/>
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

/// Deploy keys and access tokens for private repositories.
#[component]
fn GitAccess() -> impl IntoView {
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
                "Public repositories need nothing. For private ones, give facade read access: a "
                <strong>"deploy key"</strong> " for one repository, or an " <strong>"access token"</strong>
                " for every repository of an account or organisation (on GitHub, a fine-grained token with read access to Contents). "
                "Credentials are encrypted on the server, used for scans and builds (submodules and private dependencies included), and never exported."
            </p>
            <ErrorNote error=list.error/>
            <Show when=move || list.data.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))>
                <div class="table-wrap">
                    <table class="table">
                        <thead><tr><th>"Name"</th><th>"Kind"</th><th>"Covers"</th><th>"Apps"</th><th></th></tr></thead>
                        <tbody>
                            <For each=move || list.data.get().unwrap_or_default() key=|c| (c.name.clone(), c.apps.clone()) let:c>
                                {
                                    let n = c.name.clone();
                                    let public_key = c.public_key.clone();
                                    view! {
                                        <tr>
                                            <td><strong>{c.name.clone()}</strong><div class="muted small">{format!("added {}", ago(c.created_at))}</div></td>
                                            <td>{match c.kind { GitCredentialKind::SshKey => "deploy key", GitCredentialKind::Token => "token" }}</td>
                                            <td><code>{c.scope.clone()}</code></td>
                                            <td class="small">{if c.apps.is_empty() { "—".to_string() } else { c.apps.join(", ") }}</td>
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
                                placeholder="Leave empty to have facade generate one (recommended)"></textarea>
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
