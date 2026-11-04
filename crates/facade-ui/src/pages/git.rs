//! Read access to private repositories: account-wide credentials (Settings → App
//! access), and the deploy key of one app's repository (on the app's page).

use facade_core::Source;
use facade_core::api::{
    AppView, CreateGitCredential, GitCredentialKind, GitCredentialView, NewGitSecret,
};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;

use crate::api;
use crate::components::{CopyField, ErrorNote, Loading, Modal};
use crate::util::{ago, copy_to_clipboard, me, polled, run, toasts};

/// A scope naming exactly one repository (`github.com/owner/repo`), as opposed
/// to an account, an organisation or a whole host.
pub fn single_repo(scope: &str) -> bool {
    scope
        .split_once('/')
        .is_some_and(|(_, path)| path.split('/').filter(|p| !p.is_empty()).count() >= 2)
}

fn kind_label(kind: GitCredentialKind) -> &'static str {
    match kind {
        GitCredentialKind::SshKey => "deploy key",
        GitCredentialKind::Token => "token",
    }
}

/// A credential name from free text: lowercase letters, digits and inner dashes.
fn slug(s: &str, max: usize) -> String {
    let s: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(max)
        .collect();
    s.trim_matches('-').to_string()
}

/// Creates a credential named `base`, or `base-2`, `base-3`… when taken (names
/// are unique across users, some of which this user cannot see).
async fn create_unique(
    base: String,
    taken: Vec<String>,
    scope: String,
    secret: NewGitSecret,
) -> Result<GitCredentialView, String> {
    for n in 1..20 {
        let name = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        if taken.contains(&name) {
            continue;
        }
        let body = CreateGitCredential {
            name,
            scope: scope.clone(),
            secret: secret.clone(),
        };
        match api::post::<_, GitCredentialView>("/api/git/credentials", &body).await {
            Err(e) if e.contains("exists") => continue,
            other => return other,
        }
    }
    Err("no free name for the credential".into())
}

/// `shop-deploy-key-3` → `shop-deploy-key`: what a replacement is named after.
fn base_name(name: &str) -> String {
    match name.rsplit_once('-') {
        Some((base, n)) if !base.is_empty() && n.parse::<u32>().is_ok() => base.to_string(),
        _ => name.to_string(),
    }
}

/// Tokens (and keys) covering many repositories: an account, an organisation, a
/// host. Keys for one repository are managed on their app's page.
#[component]
pub fn GitAccess() -> impl IntoView {
    let me = me();
    let list = polled(None, || async {
        api::get::<Vec<GitCredentialView>>("/api/git/credentials").await
    });
    let wide = move || {
        list.data
            .get()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| !single_repo(&c.scope))
            .collect::<Vec<_>>()
    };
    let narrow = move || {
        list.data
            .get()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| single_repo(&c.scope))
            .collect::<Vec<_>>()
    };
    let name = RwSignal::new(String::new());
    let scope = RwSignal::new(String::new());
    let username = RwSignal::new(String::new());
    let secret = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let add_open = RwSignal::new(false);
    let open = move |_| {
        name.set(String::new());
        scope.set(String::new());
        username.set(String::new());
        secret.set(String::new());
        error.set(None);
        add_open.set(true);
    };
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let body = CreateGitCredential {
            name: name.get_untracked().trim().to_string(),
            scope: scope.get_untracked().trim().to_string(),
            secret: NewGitSecret::Token {
                username: Some(username.get_untracked()).filter(|u| !u.trim().is_empty()),
                token: secret.get_untracked(),
            },
        };
        error.set(None);
        spawn_local(async move {
            match api::post::<_, GitCredentialView>("/api/git/credentials", &body).await {
                Ok(c) => {
                    list.reload();
                    toasts().ok(format!("Token {} saved", c.name));
                    add_open.try_set(false);
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
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
    // The scope typed names one repository: a deploy key on its app's page fits better.
    let one_repo =
        move || scope.with(|s| facade_core::git::normalize_scope(s).is_ok_and(|s| single_repo(&s)));
    view! {
        <section class="card form">
            <h2 class="section-title">"Git access"</h2>
            <p class="muted small">
                "Public repositories need nothing. For private ones, an " <strong>"access token"</strong>
                " gives façade read access to every repository of an account or organisation (on GitHub, a fine-grained token with read access to Contents). "
                "A single repository is better served by a " <strong>"deploy key"</strong> ", made from its app's Settings tab. "
                "Credentials are encrypted on the server, used for scans and builds (submodules and private dependencies included), and never exported."
            </p>
            <ErrorNote error=list.error/>
            <Show when=move || list.loaded() fallback=Loading>
                <Show when=move || !wide().is_empty()
                    fallback=|| view! { <p class="muted small">"No access tokens yet."</p> }>
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
                                <For each=wide key=|c| (c.name.clone(), c.apps.clone(), c.owner.clone()) let:c>
                                    {
                                        let n = c.name.clone();
                                        let owner = c.owner.clone();
                                        let public_key = c.public_key.clone();
                                        view! {
                                            <tr>
                                                <td><strong>{c.name.clone()}</strong><div class="muted small">{format!("added {}", ago(c.created_at))}</div></td>
                                                <td>{kind_label(c.kind)}</td>
                                                <td><code>{c.scope.clone()}</code></td>
                                                <td class="small">{if c.apps.is_empty() { "—".to_string() } else { c.apps.join(", ") }}</td>
                                                <Show when=move || me.is_admin()>
                                                    <td class="muted">{owner.clone().unwrap_or_else(|| "admins".into())}</td>
                                                </Show>
                                                <td class="row-actions">
                                                    {public_key.map(|k| view! {
                                                        <button type="button" class="link-button" on:click=move |_| {
                                                            copy_to_clipboard(&k);
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
                // Single-repository credentials live on their apps' pages; the ones
                // no app uses any more stay reachable here.
                <Show when=move || !narrow().is_empty()>
                    <div class="field">
                        <span>"For one repository"</span>
                        <ul class="access-list repo-keys small">
                            <For each=narrow key=|c| (c.name.clone(), c.apps.clone()) let:c>
                                {
                                    let n = c.name.clone();
                                    let unused = c.apps.is_empty();
                                    view! {
                                        <li>
                                            <span>
                                                <strong>{c.name.clone()}</strong>
                                                <span class="muted">{format!(" {} · {}", kind_label(c.kind), c.scope)}</span>
                                            </span>
                                            <span>
                                                {if unused {
                                                    view! {
                                                        <span class="muted">"no app uses it · "</span>
                                                        <button type="button" class="link-button danger"
                                                            on:click=move |_| remove(n.clone())>"Remove"</button>
                                                    }.into_any()
                                                } else {
                                                    c.apps.clone().into_iter().enumerate().map(|(i, a)| view! {
                                                        {(i > 0).then_some(", ")}
                                                        <A href=format!("/apps/{a}?tab=settings")>{a.clone()}</A>
                                                    }).collect_view().into_any()
                                                }}
                                            </span>
                                        </li>
                                    }
                                }
                            </For>
                        </ul>
                        <small>"Managed from each app's Settings tab."</small>
                    </div>
                </Show>
            </Show>
            <div class="form-actions">
                <button type="button" class="button button-primary" on:click=open>"Add access token"</button>
            </div>
        </section>

        <Modal open=add_open title="Add access token">
            <form class="form" on:submit=submit>
                <label class="field">
                    <span>"Name"</span>
                    <input type="text" bind:value=name required autocomplete="off" placeholder="github-my-org"
                        pattern="[a-z0-9]([a-z0-9\\-]*[a-z0-9])?"/>
                </label>
                <label class="field">
                    <span>"Repositories it covers"</span>
                    <input type="text" bind:value=scope required autocomplete="off" placeholder="github.com/my-org"/>
                    <small>"A host and owner (github.com/my-org), or a whole host."</small>
                </label>
                <Show when=one_repo>
                    <div class="note">"That is one repository: a deploy key from its app's Settings tab is enough, and reads nothing else."</div>
                </Show>
                <label class="field">
                    <span>"Token"</span>
                    <input type="password" bind:value=secret required autocomplete="off"/>
                </label>
                <label class="field">
                    <span>"User name"</span>
                    <input type="text" bind:value=username autocomplete="off" placeholder="x-access-token"/>
                    <small>"Sent with the token over HTTPS. The default works for GitHub and GitLab."</small>
                </label>
                <ErrorNote error=error/>
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| add_open.set(false)>"Cancel"</button>
                    <button type="submit" class="button button-primary">"Save token"</button>
                </div>
            </form>
        </Modal>
    }
}

/// What the app page's dialog does.
#[derive(Clone)]
enum Dialog {
    /// A token for this repository.
    NewToken,
    /// A new token in place of this one.
    ReplaceToken(GitCredentialView),
    /// A key was made (or replaced): where to add it.
    KeyMade {
        key: GitCredentialView,
        replaced: bool,
    },
    /// Asks before replacing this key: builds fail until the new one is added.
    ConfirmRotate(GitCredentialView),
}

/// The credential an app's builds read its repository with: its deploy key (or a
/// token for that repository alone), made, replaced and removed here.
#[component]
pub fn AppGitAccess(#[prop(into)] app: Signal<Option<AppView>>) -> impl IntoView {
    let me = me();
    let list = polled(None, || async {
        api::get::<Vec<GitCredentialView>>("/api/git/credentials").await
    });
    let name = move || app.with(|a| a.as_ref().map(|a| a.spec.name.clone()).unwrap_or_default());
    let repo = move || {
        app.with(|a| match a.as_ref().map(|a| &a.spec.source) {
            Some(Source::Git { repo, .. }) => Some(repo.clone()),
            _ => None,
        })
    };
    let owner = move || app.with(|a| a.as_ref().and_then(|a| a.owner.clone()));
    // Builds use their owner's credentials, and a credential is made for whoever
    // creates it: only the owner (the admins for theirs) can give this app one.
    let can_manage = move || match owner() {
        None => me.is_admin(),
        Some(o) => !me.is_admin() && me.username().as_deref() == Some(o.as_str()),
    };
    // Credentials of the owner covering the repository, the one builds use first.
    let covering = move || {
        let name = name();
        let mut found: Vec<GitCredentialView> = list
            .data
            .get()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.apps.contains(&name))
            .collect();
        // The server picks the most specific scope (the last one of a tie, by name).
        found.sort_by_key(|c| std::cmp::Reverse(c.scope.len()));
        found
    };
    let in_use = move || {
        let found = covering();
        let longest = found.first()?.scope.len();
        found.into_iter().rfind(|c| c.scope.len() == longest)
    };
    let own = move || {
        covering()
            .into_iter()
            .filter(|c| single_repo(&c.scope))
            .collect::<Vec<_>>()
    };
    let wide = move || covering().into_iter().find(|c| !single_repo(&c.scope));
    let taken = move || {
        list.data
            .get_untracked()
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.name)
            .collect::<Vec<_>>()
    };

    let dialog = RwSignal::new(None::<Dialog>);
    let dialog_open = RwSignal::new(false);
    Effect::new(move |_| {
        if !dialog_open.get() {
            dialog.set(None);
        }
    });
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let show = move |d: Dialog| {
        error.set(None);
        dialog.set(Some(d));
        dialog_open.set(true);
    };
    let token = RwSignal::new(String::new());
    let username = RwSignal::new(String::new());

    // Makes the new credential, then removes `old` (a rotation): builds never go
    // without one because of a failed step.
    let create = move |base: String, scope: String, secret: NewGitSecret, old: Option<String>| {
        busy.set(true);
        error.set(None);
        let taken = taken();
        spawn_local(async move {
            let made = create_unique(base, taken, scope, secret).await;
            let result = match made {
                Ok(c) => {
                    let removed = match &old {
                        Some(old) => api::delete(&format!("/api/git/credentials/{old}")).await,
                        None => Ok(()),
                    };
                    if let Err(e) = removed {
                        toasts().err(format!(
                            "The new credential is saved, but the old one is still there: {e}"
                        ));
                    }
                    Ok(c)
                }
                Err(e) => Err(e),
            };
            list.reload();
            busy.try_set(false);
            match result {
                Ok(c) if c.kind == GitCredentialKind::SshKey => {
                    show(Dialog::KeyMade {
                        key: c,
                        replaced: old.is_some(),
                    });
                }
                Ok(c) => {
                    toasts().ok(if old.is_some() {
                        format!("Token replaced by {}", c.name)
                    } else {
                        format!("Token {} saved", c.name)
                    });
                    dialog_open.try_set(false);
                }
                Err(e) => {
                    if dialog_open.get_untracked() {
                        error.try_set(Some(e));
                    } else {
                        toasts().err(e);
                    }
                }
            }
        });
    };
    let new_key = move |_| {
        let Some(repo) = repo() else { return };
        create(
            format!("{}-deploy-key", slug(&name(), 28)),
            repo,
            NewGitSecret::SshKey { private_key: None },
            None,
        );
    };
    let rotate_key = move |c: GitCredentialView| {
        create(
            base_name(&c.name),
            c.scope.clone(),
            NewGitSecret::SshKey { private_key: None },
            Some(c.name),
        );
    };
    let open_token = move |d: Dialog| {
        token.set(String::new());
        username.set(match &d {
            Dialog::ReplaceToken(c) => c.username.clone().unwrap_or_default(),
            _ => String::new(),
        });
        error.set(None);
        show(d);
    };
    let submit_token = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let secret = NewGitSecret::Token {
            username: Some(username.get_untracked()).filter(|u| !u.trim().is_empty()),
            token: token.get_untracked(),
        };
        match dialog.get_untracked() {
            Some(Dialog::ReplaceToken(c)) => {
                create(base_name(&c.name), c.scope.clone(), secret, Some(c.name))
            }
            _ => {
                let Some(repo) = repo() else { return };
                create(format!("{}-token", slug(&name(), 32)), repo, secret, None)
            }
        }
    };
    let remove_target = RwSignal::new(None::<GitCredentialView>);
    let remove_open = RwSignal::new(false);
    let remove = move |_| {
        let Some(c) = remove_target.get_untracked() else {
            return;
        };
        run(
            format!("{} removed", c.name),
            async move { api::delete(&format!("/api/git/credentials/{}", c.name)).await },
            move || {
                remove_open.set(false);
                list.reload();
            },
        );
    };

    view! {
        <Show when=move || repo().is_some()>
            <section class="card form">
                <div class="section-head">
                    <h2 class="section-title">"Git access"</h2>
                    {move || list.loaded().then(|| match in_use() {
                        Some(c) => view! {
                            <span class="badge badge-good"><span class="badge-icon" aria-hidden="true">"●"</span>{kind_label(c.kind)}</span>
                        }.into_any(),
                        None => view! {
                            <span class="badge badge-muted"><span class="badge-icon" aria-hidden="true">"○"</span>"none"</span>
                        }.into_any(),
                    })}
                </div>
                <ErrorNote error=list.error/>
                <Show when=move || list.loaded() fallback=Loading>
                    <Show when=move || !own().is_empty()
                        fallback=move || view! {
                            <Show when=move || wide().is_none()>
                                <p class="muted small">
                                    "No credential: façade reads the repository as anyone would, which works if it is public. \
                                     For a private one, give façade a deploy key: façade makes it, you add it to the repository (read-only)."
                                </p>
                            </Show>
                        }>
                        <p class="muted small">
                            {move || repo().map(|r| format!(
                                "Builds read {} with this credential. A key replaced here must be replaced on the repository too.",
                                r.trim_start_matches("https://")
                            ))}
                        </p>
                        <div class="table-wrap">
                            <table class="table">
                                <thead>
                                    <tr><th>"Name"</th><th>"Kind"</th><th>"Also used by"</th><th></th></tr>
                                </thead>
                                <tbody>
                                    <For each=own key=|c| (c.name.clone(), c.apps.clone(), c.created_at) let:c>
                                        {
                                            let this = name();
                                            let others: Vec<String> = c.apps.iter().filter(|a| **a != this).cloned().collect();
                                            let used = in_use().is_some_and(|u| u.name == c.name);
                                            let public_key = c.public_key.clone();
                                            let target = c.clone();
                                            let rotate = c.clone();
                                            let is_key = c.kind == GitCredentialKind::SshKey;
                                            view! {
                                                <tr>
                                                    <td>
                                                        <strong>{c.name.clone()}</strong>
                                                        <div class="muted small">
                                                            {format!("added {}", ago(c.created_at))}
                                                            {(!used).then_some(" · not used: another covers the repository")}
                                                        </div>
                                                    </td>
                                                    <td>{kind_label(c.kind)}</td>
                                                    <td class="small">
                                                        {if others.is_empty() {
                                                            view! { <span class="muted">"—"</span> }.into_any()
                                                        } else {
                                                            others.into_iter().enumerate().map(|(i, a)| view! {
                                                                {(i > 0).then_some(", ")}
                                                                <A href=format!("/apps/{a}?tab=settings")>{a.clone()}</A>
                                                            }).collect_view().into_any()
                                                        }}
                                                    </td>
                                                    <td class="row-actions">
                                                        {public_key.map(|k| view! {
                                                            <button type="button" class="link-button" on:click=move |_| {
                                                                copy_to_clipboard(&k);
                                                                toasts().ok("Public key copied");
                                                            }>"Copy public key"</button>
                                                        })}
                                                        <Show when=can_manage>
                                                            {
                                                                let rotate = rotate.clone();
                                                                let target = target.clone();
                                                                view! {
                                                                    <button type="button" class="link-button" disabled=busy
                                                                        on:click=move |_| if is_key {
                                                                            show(Dialog::ConfirmRotate(rotate.clone()))
                                                                        } else {
                                                                            open_token(Dialog::ReplaceToken(rotate.clone()))
                                                                        }>
                                                                        {if is_key { "Replace key" } else { "Replace token" }}
                                                                    </button>
                                                                    <button type="button" class="link-button danger" on:click=move |_| {
                                                                        remove_target.set(Some(target.clone()));
                                                                        remove_open.set(true);
                                                                    }>"Remove"</button>
                                                                }
                                                            }
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
                    {move || wide().map(|c| view! {
                        <p class="small muted">
                            {if own().is_empty() { "Builds read the repository with " } else { "Also covered by " }}
                            {kind_label(c.kind)} " " <strong>{c.name.clone()}</strong>
                            {format!(" ({}), from ", c.scope)}
                            <A href="/settings/app-access">"Settings → App access"</A>
                            {if own().is_empty() {
                                ". For this repository alone, a deploy key is enough."
                            } else {
                                ", used if the key above goes."
                            }}
                        </p>
                    })}
                    <Show when=move || !can_manage()>
                        <p class="muted small">
                            {move || match owner() {
                                Some(o) => format!("Builds use {o}'s git access: {o} manages it from this page."),
                                None => "Builds use the admins' git access: an admin manages it from this page.".into(),
                            }}
                        </p>
                    </Show>
                    <Show when=move || can_manage() && own().is_empty()>
                        <div class="form-actions push-left">
                            <button type="button" class="button" disabled=busy on:click=new_key>
                                {move || if busy.get() { "Creating…" } else { "Create a deploy key" }}
                            </button>
                            <button type="button" class="link-button" on:click=move |_| open_token(Dialog::NewToken)>
                                "Use a token instead"
                            </button>
                        </div>
                    </Show>
                </Show>
            </section>
        </Show>

        <Modal open=dialog_open title="Git access">
            {move || match dialog.get() {
                Some(Dialog::ConfirmRotate(c)) => {
                    let apps = c.apps.join(", ");
                    let target = c.clone();
                    view! {
                        <p>
                            {format!(
                                "Replace {}? façade makes a new key and forgets this one: builds of {apps} fail until the new key is added to the repository.",
                                c.name
                            )}
                        </p>
                        <ErrorNote error=error/>
                        <div class="form-actions">
                            <button class="button" on:click=move |_| dialog_open.set(false)>"Cancel"</button>
                            <button class="button button-primary" disabled=busy
                                on:click=move |_| rotate_key(target.clone())>"Replace key"</button>
                        </div>
                    }.into_any()
                }
                Some(Dialog::KeyMade { key, replaced }) => view! {
                    <p>
                        {if replaced {
                            "Replace the repository's deploy key with this one (read-only access is enough). Builds fail until it is there. façade forgot the old key: delete it on the repository too."
                        } else {
                            "Add this public key to the repository as a deploy key (read-only access is enough):"
                        }}
                    </p>
                    <CopyField value=key.public_key.clone().unwrap_or_default()/>
                    {key.add_key_url.clone().map(|url| view! {
                        <p><a href=url target="_blank" rel="noopener">"Open the repository's deploy key page ↗"</a></p>
                    })}
                    <div class="form-actions">
                        <button class="button button-primary" on:click=move |_| dialog_open.set(false)>"Done"</button>
                    </div>
                }.into_any(),
                Some(d) => {
                    let replacing = matches!(d, Dialog::ReplaceToken(_));
                    view! {
                        <form class="form" on:submit=submit_token>
                            <p class="muted small">
                                {move || if replacing {
                                    "The new token takes over; the old one is removed from façade (revoke it at your provider).".to_string()
                                } else {
                                    format!(
                                        "A token for {} alone (on GitHub, a fine-grained token with read access to its Contents).",
                                        repo().unwrap_or_default().trim_start_matches("https://")
                                    )
                                }}
                            </p>
                            <label class="field">
                                <span>"Token"</span>
                                <input type="password" bind:value=token required autocomplete="off"/>
                            </label>
                            <label class="field">
                                <span>"User name"</span>
                                <input type="text" bind:value=username autocomplete="off" placeholder="x-access-token"/>
                                <small>"Sent with the token over HTTPS. The default works for GitHub and GitLab."</small>
                            </label>
                            <ErrorNote error=error/>
                            <div class="form-actions">
                                <button type="button" class="button" on:click=move |_| dialog_open.set(false)>"Cancel"</button>
                                <button type="submit" class="button button-primary" disabled=busy>
                                    {if replacing { "Replace token" } else { "Save token" }}
                                </button>
                            </div>
                        </form>
                    }.into_any()
                }
                None => ().into_any(),
            }}
        </Modal>

        <Modal open=remove_open title="Remove credential">
            <p>
                {move || remove_target.get().map(|c| {
                    let apps = c.apps.join(", ");
                    let then = match wide() {
                        Some(w) => format!("use {} {} instead", kind_label(w.kind), w.name),
                        None => "read the repository without it (and fail if it is private)".into(),
                    };
                    let forge = match c.kind {
                        GitCredentialKind::SshKey => " Delete the key on the repository too.",
                        GitCredentialKind::Token => " Revoke the token at your provider too.",
                    };
                    format!("Remove {}? Builds of {apps} {then}.{forge}", c.name)
                })}
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| remove_open.set(false)>"Cancel"</button>
                <button class="button button-danger" on:click=remove>"Remove"</button>
            </div>
        </Modal>
    }
}
