use std::time::Duration;

use facade_core::api::{
    AppChange, ApplyReport, ChangePassword, NodeBuildState, RegistryLoginView, ServerSettings,
    ServerSettingsUpdate, ServerUpdate, SetRegistryLogin, UpdateRun, UpdateState, UpdateTarget,
};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map};

use super::alerts::AlertsTab;
use super::app::CommitRow;
use super::git::GitAccess;
use crate::api;
use crate::components::{CopyField, ErrorNote, Loading, Modal};
use crate::util::{ago, alerts, download, me, polled, run, short_hash, toasts};

/// Settings tabs: `(key, label, admins only)`.
const TABS: &[(&str, &str, bool)] = &[
    ("general", "General", true),
    ("alerts", "Alerts", false),
    ("app-access", "App access", false),
    ("password", "Password", false),
];

#[component]
pub fn Settings() -> impl IntoView {
    // Members only see the alerts, their own logins and their password.
    let me = me();
    let params = use_params_map();
    let firing = move || alerts().firing();
    let visible = move |key: &str| {
        TABS.iter()
            .any(|(k, _, admin)| *k == key && (!admin || me.is_admin()))
    };
    // `/settings` and unknown tabs open the first one this user has.
    let tab = Memo::new(move |_| {
        let asked = params.read().get("tab").unwrap_or_default();
        if visible(&asked) {
            asked
        } else if me.is_admin() {
            "general".to_string()
        } else {
            "alerts".to_string()
        }
    });
    view! {
        <div class="page page-narrow">
            <header class="page-head"><h1>"Settings"</h1></header>
            <Show when=move || me.0.loaded() fallback=Loading>
                <nav class="tabs" aria-label="Settings">
                    {move || TABS.iter().filter(|(k, ..)| visible(k)).map(|(key, label, _)| {
                        let key = *key;
                        view! {
                            <A href=format!("/settings/{key}")
                                attr:class=move || if tab.get() == key { "tab active" } else { "tab" }>
                                {*label}
                                {(key == "alerts").then(|| view! {
                                    <Show when=move || { firing() > 0 }>
                                        <span class="nav-count tab-count" title="firing alerts">{firing}</span>
                                    </Show>
                                })}
                            </A>
                        }
                    }).collect_view()}
                </nav>
                {move || match tab.get().as_str() {
                    "general" => view! {
                        <ServerAccess/>
                        <Updates/>
                        <Configuration/>
                    }.into_any(),
                    "app-access" => view! {
                        <GitAccess/>
                        <RegistryLogins/>
                    }.into_any(),
                    "password" => view! { <Password/> }.into_any(),
                    _ => view! { <AlertsTab/> }.into_any(),
                }}
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

/// façade's own build, and updating it to the latest build of its main branch.
#[component]
fn Updates() -> impl IntoView {
    let status = polled(Some(Duration::from_secs(5)), || async {
        api::get::<ServerUpdate>("/api/server-update").await
    });
    // The build this page was loaded from: after an update, a reload brings the
    // new web UI.
    let first_seen = StoredValue::new(None::<Option<String>>);
    Effect::new(move |_| {
        if first_seen.get_value().is_none()
            && let Some(s) = status.data.get()
        {
            first_seen.set_value(Some(s.current.commit));
        }
    });
    let current = move || status.data.get();
    let updating = move || {
        current()
            .and_then(|s| s.run)
            .is_some_and(|r| r.state == UpdateState::Running)
    };
    let reload_needed = move || {
        let now = current().map(|s| s.current.commit);
        matches!((first_seen.get_value(), now), (Some(a), Some(b)) if a != b)
    };
    let repo = move || current().map(|s| s.repository).unwrap_or_default();
    let commit_link = move |hash: String| {
        let short = short_hash(&hash).to_string();
        view! {
            <a href=format!("https://github.com/{}/commit/{hash}", repo()) target="_blank"
                rel="noreferrer"><code>{short}</code></a>
        }
    };

    let checking = RwSignal::new(false);
    let check = move |_| {
        checking.set(true);
        spawn_local(async move {
            match api::act_json::<ServerUpdate>("/api/server-update/check").await {
                Ok(s) => {
                    if let Some(e) = &s.check_error {
                        toasts().err(e.clone());
                    } else if !s.available {
                        toasts().ok("façade is up to date");
                    }
                    status.data.try_set(Some(s));
                }
                Err(e) => toasts().err(e),
            }
            checking.try_set(false);
        });
    };

    // Preparing (download and check) happens when the dialog opens.
    let dialog = RwSignal::new(false);
    let preparing = RwSignal::new(false);
    let prepare_error = RwSignal::new(None::<String>);
    let target = move || current().and_then(|s| s.latest).map(|l| l.commit);
    let open_dialog = move |_| {
        let Some(commit) = target() else { return };
        dialog.set(true);
        preparing.set(true);
        prepare_error.set(None);
        spawn_local(async move {
            let body = UpdateTarget { commit };
            match api::post::<_, ServerUpdate>("/api/server-update/prepare", &body).await {
                Ok(s) => {
                    status.data.try_set(Some(s));
                }
                Err(e) => {
                    prepare_error.try_set(Some(e));
                }
            }
            preparing.try_set(false);
        });
    };
    let prepared = move || {
        current()
            .and_then(|s| s.prepared)
            .filter(|p| Some(&p.commit) == target().as_ref())
    };
    let installing = RwSignal::new(false);
    let install = move |_| {
        let Some(commit) = target() else { return };
        installing.set(true);
        spawn_local(async move {
            let body = UpdateTarget { commit };
            match api::post::<_, ServerUpdate>("/api/server-update/install", &body).await {
                Ok(s) => {
                    status.data.try_set(Some(s));
                    dialog.try_set(false);
                    toasts().ok("Updating: façade restarts in a moment");
                }
                Err(e) => {
                    prepare_error.try_set(Some(e));
                }
            }
            installing.try_set(false);
        });
    };

    view! {
        <section class="card form">
            <h2 class="section-title">"Updates"</h2>
            // The server is away for a few seconds while it restarts.
            <Show when=move || !updating()>
                <ErrorNote error=status.error/>
            </Show>
            <Show when=reload_needed>
                <div class="note">
                    "façade was updated. "
                    <button class="button" on:click=|_| {
                        if let Some(w) = web_sys::window() {
                            let _ = w.location().reload();
                        }
                    }>"Reload the page"</button>
                    " for the new web UI."
                </div>
            </Show>
            <Show when=move || status.loaded() fallback=Loading>
                // Behind the latest build: said up front, not only by the Update button.
                {move || current().filter(|s| s.available && !updating()).map(|s| {
                    let behind = match s.commits.len() {
                        0 => "A newer build of main is available.".to_string(),
                        1 => "façade is 1 commit behind main.".to_string(),
                        n => format!("façade is {n} commits behind main."),
                    };
                    let since = s.latest.map(|l| format!(" Latest build {}.", ago(l.time))).unwrap_or_default();
                    view! {
                        <div class="note note-warning" role="status">
                            <span aria-hidden="true">"⚠ "</span>
                            <strong>{behind}</strong>
                            {since}
                        </div>
                    }
                })}
                <div class="field">
                    <span>"Running"</span>
                    <p class="small">
                        {move || current().map(|s| {
                            let build = format!(" · façade {} on {}", s.current.version, s.current.arch);
                            match s.current.commit {
                                Some(c) => view! { {commit_link(c)} {build} }.into_any(),
                                None => view! { "a build without a commit (built by hand)" {build} }.into_any(),
                            }
                        })}
                    </p>
                </div>
                <div class="field">
                    <span>"Latest build of main"</span>
                    {move || current().map(|s| match (s.latest, s.check_error) {
                        (_, Some(e)) => view! { <div class="note note-error">{e}</div> }.into_any(),
                        (None, None) => view! { <p class="small muted">"Not checked yet."</p> }.into_any(),
                        (Some(l), None) => view! {
                            <p class="small">
                                {commit_link(l.commit.clone())}
                                {format!(" {} · {}", l.subject, ago(l.time))}
                                {(!s.available).then_some(" (running)")}
                            </p>
                        }.into_any(),
                    })}
                    <small>
                        {move || current().and_then(|s| s.checked_at)
                            .map(|t| format!("Checked {} on github.com/{}.", ago(t), repo()))}
                    </small>
                    {move || current().filter(|s| s.available && !s.commits.is_empty()).map(|s| view! {
                        <details class="build-details commit-details">
                            <summary>
                                <span>{format!("{} new commit{}", s.commits.len(), if s.commits.len() == 1 { "" } else { "s" })}</span>
                            </summary>
                            <div class="build-details-body">
                                <ul class="commit-list">
                                    {s.commits.into_iter().map(|c| view! { <CommitRow c=c/> }).collect_view()}
                                </ul>
                            </div>
                        </details>
                    })}
                </div>
                {move || current().and_then(|s| s.run).map(|r| view! { <RunNote run=r/> })}
                {move || {
                    let nodes: Vec<_> = current()
                        .map(|s| s.nodes.into_iter().filter(|n| n.state != NodeBuildState::Current).collect())
                        .unwrap_or_default();
                    (!nodes.is_empty()).then(|| view! {
                        <div class="field">
                            <span>"Nodes"</span>
                            <ul class="access-list small">
                                {nodes.into_iter().map(|n| {
                                    let what = match n.state {
                                        NodeBuildState::Updating => "runs another build; updates itself when it connects".to_string(),
                                        NodeBuildState::Manual => format!(
                                            "runs another build, and the server has none for its CPU ({}): update it by hand",
                                            n.arch.as_deref().unwrap_or("unknown")
                                        ),
                                        NodeBuildState::Managed => "installed by Nix: bump its facade input and rebuild".to_string(),
                                        _ => "has not said which build it runs".to_string(),
                                    };
                                    view! { <li><span>{n.name}</span><span class="muted">{what}</span></li> }
                                }).collect_view()}
                            </ul>
                        </div>
                    })
                }}
                <div class="form-actions push-left">
                    <button class="button" disabled=move || checking.get() || updating()
                        on:click=check>
                        {move || if checking.get() { "Checking…" } else { "Check now" }}
                    </button>
                    {move || current().and_then(|s| s.managed.clone().filter(|_| s.available)).map(|hint| view! {
                        <span class="muted small">{format!("façade is {hint}.")}</span>
                    })}
                    <Show when=move || current().is_some_and(|s| s.available && s.managed.is_none())>
                        <button class="button button-primary" disabled=updating on:click=open_dialog>
                            {move || format!("Update to {}…", target().map(|c| short_hash(&c).to_string()).unwrap_or_default())}
                        </button>
                    </Show>
                </div>
            </Show>
        </section>

        <Modal open=dialog title="Update façade">
            <Show when=move || !preparing.get()
                fallback=|| view! { <p>"Downloading the new build and checking what it changes…"</p> }>
                <ErrorNote error=prepare_error/>
                {move || prepared().map(|p| {
                    let affected: Vec<_> = p.plan.into_iter()
                        .filter(|c| matches!(c.change, AppChange::Restart | AppChange::Rebuild))
                        .collect();
                    if affected.is_empty() {
                        view! { <p>"No running app is restarted by this update."</p> }.into_any()
                    } else {
                        view! {
                            <p>"These apps on the server's node restart with it:"</p>
                            <ul class="access-list small">
                                {affected.into_iter().map(|c| view! {
                                    <li>
                                        <span>{c.app}</span>
                                        <span class="muted">{match c.change {
                                            AppChange::Rebuild => "rebuilt at its deployed commit, then restarted",
                                            _ => "container recreated (a few seconds down)",
                                        }}</span>
                                    </li>
                                }).collect_view()}
                            </ul>
                        }.into_any()
                    }
                })}
                <p class="small muted">
                    "façade restarts and the web UI is away for a few seconds; apps keep running. \
                     If the new version is not healthy within 90 seconds, the previous one comes back. \
                     Other nodes update themselves."
                </p>
            </Show>
            <div class="form-actions">
                <button class="button" on:click=move |_| dialog.set(false)>"Cancel"</button>
                <button class="button button-primary"
                    disabled=move || preparing.get() || installing.get() || prepared().is_none()
                    on:click=install>"Install"</button>
            </div>
        </Modal>
    }
}

/// The update in progress, or how the last one ended.
#[component]
fn RunNote(run: UpdateRun) -> impl IntoView {
    let to = short_hash(&run.to).to_string();
    let when = run.finished.map(ago).unwrap_or_default();
    let (class, text) = match run.state {
        UpdateState::Running => ("note", format!("Updating to {to}: {}…", run.message)),
        UpdateState::Done => ("note", format!("Updated to {to} {when}.")),
        UpdateState::RolledBack => (
            "note note-error",
            format!(
                "The update to {to} ({when}) was rolled back: {}",
                run.message
            ),
        ),
        UpdateState::Failed => (
            "note note-error",
            format!("The update to {to} ({when}) failed: {}", run.message),
        ),
    };
    view! { <div class=class>{text}</div> }
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

/// Export to and apply from a facade.toml.
#[component]
fn Configuration() -> impl IntoView {
    let export = move |_| {
        spawn_local(async move {
            match api::get_text("/api/export").await {
                Ok(toml) => download("facade.toml", &toml, "application/toml"),
                Err(e) => toasts().err(e),
            }
        });
    };
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
            <h2 class="section-title">"Configuration"</h2>
            <div class="field">
                <span>"Export"</span>
                <p class="muted small">
                    "Every node and app as TOML, to keep in git or restore elsewhere. Sensitive variables are "
                    "listed without their values."
                </p>
                <div class="form-actions push-left">
                    <button class="button" on:click=export>"Download facade.toml"</button>
                </div>
            </div>
            <form class="form config-apply" on:submit=submit>
                <div class="field">
                    <span>"Apply"</span>
                    <p class="muted small">
                        "Creates or updates the nodes and apps described in a facade.toml. Nothing is deleted, "
                        "and new apps are not deployed."
                    </p>
                </div>
                <label class="field">
                    <span class="visually-hidden">"facade.toml"</span>
                    <textarea class="mono" rows="8" bind:value=text required
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
