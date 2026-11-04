use std::time::Duration;

use facade_core::api::{
    AppDiskView, AppView, AutoDeployMethod, AutoDeployView, BackupReport, BackupRequest,
    BackupView, CommitView, DeployRequest, DeploymentStatus, DeploymentView, DomainStatus,
    EnvPatch, EnvView, ImageUpdateCheck, NodeView, SetEnv, SetOwner, UpdateCheck, UserView,
    VolumeView, WRITTEN_WARNING,
};
use facade_core::proto::AppState;
use facade_core::{Build, Source};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};
use uuid::Uuid;

use super::domains::DomainTable;
use super::spec_form::{SpecFields, SpecForm};
use crate::api;
use crate::components::{ErrorNote, Loading, Menu, Modal, StateBadge, TabBar, UsageCharts};
use crate::util::{
    Polled, ago, bytes, clock, cpus, date, me, percent, polled, run, short_hash, strip_ansi, toasts,
};

const TABS: &[(&str, &str)] = &[
    ("overview", "Overview"),
    ("info", "Info"),
    ("activity", "Activity"),
    ("logs", "Logs"),
    ("deployments", "Deployments"),
    ("env", "Environment"),
    ("volumes", "Volumes"),
    ("settings", "Settings"),
];

#[derive(Clone, Copy, PartialEq)]
enum SourceKind {
    Git,
    Image,
    Executable,
}

/// What the header's main button does: deploy where the app needs it, else
/// look for something new first, then deploy what was found.
#[derive(Clone, Copy, PartialEq)]
enum Primary {
    Stack,
    First,
    Deploy,
    Check,
    Update,
}

/// What the header's update check found that a deploy would bring.
#[derive(Clone)]
enum Found {
    Commits(UpdateCheck),
    Image(ImageUpdateCheck),
}

impl Found {
    /// `Some(false)`: up to date; `None`: what runs is unknown.
    fn news(&self) -> Option<bool> {
        match self {
            Found::Commits(c) => c.behind.map(|n| n > 0),
            Found::Image(i) => i.outdated(),
        }
    }

    fn deploy_label(&self) -> String {
        match self {
            Found::Commits(c) => match c.behind {
                Some(n) => format!("Deploy {n} new commit{}", if n == 1 { "" } else { "s" }),
                None => format!("Deploy {}", short_hash(&c.latest.hash)),
            },
            Found::Image(i) if i.outdated() == Some(true) => "Deploy new image".into(),
            Found::Image(_) => "Pull and deploy".into(),
        }
    }

    /// The commit to deploy: the one the check saw, not whatever the branch has
    /// by the time the button is pressed. Images are pulled by their tag.
    fn commit(&self) -> Option<String> {
        match self {
            Found::Commits(c) => Some(c.latest.hash.clone()),
            Found::Image(_) => None,
        }
    }

    fn up_to_date(&self) -> String {
        match self {
            Found::Commits(c) => format!(
                "Up to date with {} ({})",
                c.branch.as_deref().unwrap_or("the default branch"),
                short_hash(&c.latest.hash)
            ),
            Found::Image(i) => format!("Up to date: {} has not changed", i.image),
        }
    }
}

/// A commit's 7 characters, or a digest's first 12 hex digits.
fn short_digest(digest: &str) -> String {
    match digest.strip_prefix("sha256:") {
        Some(hex) => format!("sha256:{}", hex.get(..12).unwrap_or(hex)),
        None => short_hash(digest).to_string(),
    }
}

#[component]
pub fn AppPage() -> impl IntoView {
    let me = me();
    let params = use_params_map();
    let query = use_query_map();
    let navigate = use_navigate();
    let name = Memo::new(move |_| params.read().get("name").unwrap_or_default());
    let tab = Memo::new(move |_| query.read().get("tab").unwrap_or_else(|| "overview".into()));
    let selected_deployment = RwSignal::new(None::<Uuid>);

    let app = polled(Some(Duration::from_secs(3)), move || {
        let name = name.get();
        async move { api::get::<AppView>(&format!("/api/apps/{name}")).await }
    });
    // For the overview's latest deployment and the header's rollback.
    let deployments = polled(Some(Duration::from_secs(10)), move || {
        let name = name.get();
        async move { api::get::<Vec<DeploymentView>>(&format!("/api/apps/{name}/deployments")).await }
    });
    let set_tab = {
        let navigate = navigate.clone();
        move |t: &str| {
            navigate(
                &format!("/apps/{}?tab={t}", name.get_untracked()),
                leptos_router::NavigateOptions {
                    replace: true,
                    scroll: false,
                    ..Default::default()
                },
            )
        }
    };

    let busy = RwSignal::new(false);
    let checking = RwSignal::new(false);
    let found = RwSignal::new(None::<Found>);
    // The stack a service belongs to, which deploys it.
    let stack = move || {
        app.data.with(|a| {
            a.as_ref()
                .and_then(|a| a.spec.stack.as_ref().map(|s| s.stack.clone()))
        })
    };
    let kind = Memo::new(move |_| {
        app.data.with(|a| {
            a.as_ref().map(|a| match a.spec.source {
                Source::Git { .. } => SourceKind::Git,
                Source::Image { .. } => SourceKind::Image,
                Source::Executable { .. } => SourceKind::Executable,
            })
        })
    });
    let commit = Memo::new(move |_| app.data.with(|a| a.as_ref().and_then(|a| a.commit.clone())));
    // What was found is stale once another commit runs.
    Effect::new(move |previous: Option<Option<String>>| {
        let now = commit.get();
        if previous.is_some_and(|p| p != now) {
            found.set(None);
        }
        now
    });
    // `commit`: a specific one to deploy (the update check's, a rollback's), or
    // the branch's latest.
    let deploy = Callback::new({
        let set_tab = set_tab.clone();
        let navigate = navigate.clone();
        move |commit: Option<String>| {
            busy.set(true);
            let set_tab = set_tab.clone();
            if let Some(stack) = stack() {
                let navigate = navigate.clone();
                spawn_local(async move {
                    match api::act_json::<facade_core::api::StackDeploymentView>(&format!(
                        "/api/stacks/{stack}/deploy"
                    ))
                    .await
                    {
                        Ok(_) => navigate(
                            &format!("/stacks/{stack}?tab=deployments"),
                            Default::default(),
                        ),
                        Err(e) => toasts().err(e),
                    }
                    busy.set(false);
                });
                return;
            }
            let path = format!("/api/apps/{}/deploy", name.get_untracked());
            let body = DeployRequest {
                commit,
                rebuild: false,
            };
            spawn_local(async move {
                match api::post::<_, DeploymentView>(&path, &body).await {
                    Ok(d) => {
                        found.set(None);
                        selected_deployment.set(Some(d.id));
                        set_tab("deployments");
                        app.reload();
                        deployments.reload();
                    }
                    Err(e) => toasts().err(e),
                }
                busy.set(false);
            });
        }
    });
    // Fetching the branch (or asking the registry) takes a few seconds: only
    // when asked.
    let check = move || {
        checking.set(true);
        let name = name.get_untracked();
        let image = kind.get_untracked() == Some(SourceKind::Image);
        spawn_local(async move {
            let result = if image {
                api::act_json::<ImageUpdateCheck>(&format!("/api/apps/{name}/image-updates"))
                    .await
                    .map(Found::Image)
            } else {
                api::act_json::<UpdateCheck>(&format!("/api/apps/{name}/updates"))
                    .await
                    .map(Found::Commits)
            };
            match result {
                Ok(f) if f.news() == Some(false) => {
                    toasts().ok(f.up_to_date());
                    found.try_set(None);
                }
                Ok(f) => {
                    found.try_set(Some(f));
                }
                Err(e) => toasts().err(e),
            }
            checking.try_set(false);
        });
    };
    let simple = move |action: &'static str, done: &'static str| {
        let path = format!("/api/apps/{}/{action}", name.get_untracked());
        run(done, async move { api::act(&path).await }, move || {
            app.reload()
        });
    };

    // A restart without downtime waits for the new container to be ready:
    // seconds, longer with a slow health check.
    let restarting = RwSignal::new(false);
    let restart = move |_| {
        restarting.set(true);
        let path = format!("/api/apps/{}/restart", name.get_untracked());
        run(
            "Restarted",
            async move {
                let result = api::act(&path).await;
                restarting.try_set(false);
                result
            },
            move || app.reload(),
        );
    };

    let confirm_delete = RwSignal::new(false);
    let typed = RwSignal::new(String::new());
    let delete_volumes = RwSignal::new(false);
    let delete = {
        let navigate = navigate.clone();
        move |_| {
            let navigate = navigate.clone();
            let n = name.get_untracked();
            let volumes = delete_volumes.get_untracked();
            run(
                format!("{n} deleted"),
                async move { api::delete(&format!("/api/apps/{n}?volumes={volumes}")).await },
                move || navigate("/apps", Default::default()),
            );
        }
    };

    let view_of = move || app.data.get();
    let state = Signal::derive(move || view_of().map(|a| a.state).unwrap_or(AppState::Unknown));
    let enabled = move || view_of().is_some_and(|a| a.spec.enabled);
    let deployed = move || view_of().is_some_and(|a| a.revision > 0);
    let primary = move || {
        if stack().is_some() {
            return Primary::Stack;
        }
        if !deployed() {
            return Primary::First;
        }
        if found.with(Option::is_some) {
            return Primary::Update;
        }
        // Stopped, failed or crashed, a deploy is what it needs; an uploaded
        // executable has nothing new to look for.
        let settled = enabled()
            && matches!(
                state.get(),
                AppState::Running | AppState::Building | AppState::Pending
            );
        match kind.get() {
            Some(SourceKind::Git | SourceKind::Image) if settled => Primary::Check,
            _ => Primary::Deploy,
        }
    };
    let primary_label = move || {
        if busy.get() {
            return "Starting…".to_string();
        }
        if checking.get() {
            return "Checking…".to_string();
        }
        match primary() {
            Primary::Stack => "Deploy stack".into(),
            Primary::First => "First deploy".into(),
            Primary::Deploy => "Deploy".into(),
            Primary::Check => "Check for updates".into(),
            Primary::Update => found
                .with(|f| f.as_ref().map(Found::deploy_label))
                .unwrap_or_default(),
        }
    };
    let primary_title = move || match (primary(), kind.get()) {
        (Primary::Check, Some(SourceKind::Image)) => {
            "Ask the registry whether the tag points to a new image"
        }
        (Primary::Check, _) => "Fetch the branch and compare it with the deployed commit",
        (Primary::Update, _) => "Deploy what the check found",
        _ => "Fetch the latest code or image, rebuild and restart",
    };
    let on_primary = move |_| match primary() {
        Primary::Check => check(),
        Primary::Update => deploy.run(found.with_untracked(|f| f.as_ref().and_then(Found::commit))),
        _ => deploy.run(None),
    };
    // The previous successfully deployed commit, as `facade rollback` takes.
    let rollback = Memo::new(move |_| {
        let current = commit.get()?;
        deployments.data.with(|l| {
            l.iter()
                .flatten()
                .find(|d| {
                    d.status == DeploymentStatus::Succeeded
                        && d.commit.as_ref().is_some_and(|c| *c != current)
                })
                .and_then(|d| d.commit.clone())
        })
    });
    // Deploys besides the main button's: the same commit again, the previous one,
    // or the latest without looking first. A stack's services deploy with it.
    let more = Memo::new(move |_| {
        let mut items: Vec<(String, &'static str, Option<String>)> = vec![];
        if stack().is_some() || !deployed() {
            return items;
        }
        let checks = matches!(primary(), Primary::Check | Primary::Update);
        match kind.get() {
            Some(SourceKind::Git) => {
                if checks {
                    items.push((
                        "Deploy latest commit".into(),
                        "Fetch the branch and deploy its latest commit, without checking first",
                        None,
                    ));
                }
                if let Some(c) = commit.get() {
                    items.push((
                        format!("Redeploy {}", short_hash(&c)),
                        "Deploy the running commit again; its image is reused if it is still on the node",
                        Some(c),
                    ));
                }
                if let Some(c) = rollback.get() {
                    items.push((
                        format!("Roll back to {}", short_hash(&c)),
                        "Deploy the previous successfully deployed commit",
                        Some(c),
                    ));
                }
            }
            Some(SourceKind::Image) if checks => items.push((
                "Pull and redeploy".into(),
                "Pull the tag again and restart on it, new image or not",
                None,
            )),
            _ => {}
        }
        items
    });
    let has_volumes = Memo::new(move |_| {
        app.data
            .with(|a| a.as_ref().is_some_and(|a| !a.spec.volumes.is_empty()))
    });
    let tabs = Signal::derive(move || {
        TABS.iter()
            .copied()
            .filter(|(key, _)| *key != "volumes" || has_volumes.get())
            .collect::<Vec<_>>()
    });
    // A memo, so the tabs are only rebuilt when this changes, not at every poll
    // (which would reset the settings form while it is being edited).
    let stack_service = Memo::new(move |_| {
        app.data.with(|a| {
            let spec = &a.as_ref()?.spec;
            Some((
                spec.stack.as_ref()?.stack.clone(),
                spec.service()?.to_string(),
            ))
        })
    });

    view! {
        <div class="page">
            <header class="page-head">
                <div>
                    {move || match stack() {
                        Some(stack) => view! {
                            <A href=format!("/stacks/{stack}") attr:class="crumb">{format!("← Stack {stack}")}</A>
                        }.into_any(),
                        None => view! { <A href="/apps" attr:class="crumb">"← Apps"</A> }.into_any(),
                    }}
                    <h1 class="title-row">
                        {move || name.get()}
                        <Show when=move || app.loaded()>
                            <StateBadge state=state/>
                        </Show>
                    </h1>
                    <div class="muted small subtitle">
                        {move || view_of().map(|a| subtitle(&a))}
                        {move || {
                            view_of()
                                .and_then(|a| a.owner)
                                .filter(|_| me.is_admin())
                                .map(|o| format!(" · owned by {o}"))
                        }}
                    </div>
                </div>
                <div class="actions">
                    <button class="button button-primary"
                        disabled=move || busy.get() || checking.get() || !app.loaded()
                        on:click=on_primary title=primary_title>
                        {primary_label}
                    </button>
                    <button class="button" disabled=move || !deployed() || restarting.get()
                        on:click=restart>
                        {move || if restarting.get() { "Restarting…" } else { "Restart" }}
                    </button>
                    <Show
                        when=enabled
                        fallback=move || view! {
                            <button class="button" on:click=move |_| simple("start", "Started")>"Start"</button>
                        }
                    >
                        <button class="button" disabled=move || !deployed()
                            on:click=move |_| simple("stop", "Stopped")>"Stop"</button>
                    </Show>
                    <Show when=move || more.with(|m| !m.is_empty())>
                        <Menu label="More" title="Other deploys">
                            {move || more.get().into_iter().map(|(label, title, commit)| view! {
                                <button class="menu-item" role="menuitem" title=title
                                    disabled=move || busy.get()
                                    on:click=move |_| deploy.run(commit.clone())>{label}</button>
                            }).collect_view()}
                        </Menu>
                    </Show>
                </div>
            </header>
            <ErrorNote error=app.error/>
            {move || view_of().and_then(|a| {
                let stack = a.spec.stack.clone()?;
                let service = a.spec.service()?.to_string();
                Some(view! {
                    <div class="note">
                        "Service " <strong>{service}</strong> " of stack "
                        <A href=format!("/stacks/{}", stack.stack)>{stack.stack.clone()}</A>
                        ", reached by the other services as " <code>{stack.host.clone()}</code>
                        ". It is deployed with its stack; its settings come from the stack (and its compose file)."
                    </div>
                })
            })}
            <Show when=move || state.get() == AppState::Crashed && view_of().is_some_and(|a| a.message.is_none())>
                <div class="note note-error">
                    "The container keeps stopping. Its "
                    <a href=move || format!("/apps/{}?tab=logs", name.get())>"logs"</a>
                    " usually say why."
                </div>
            </Show>
            {move || view_of().and_then(|a| a.message).map(|m| view! {
                <div class=move || if state.get() == AppState::Failed { "note note-error" } else { "note" }>
                    {m}
                </div>
            })}
            <UpdateFound found/>

            <TabBar tabs active=tab on_pick=Callback::new({
                let set_tab = set_tab.clone();
                move |t: &'static str| set_tab(t)
            })/>

            <Show when=move || app.loaded() fallback=Loading>
                {move || match tab.get().as_str() {
                    "logs" => view! { <LogsTab name=name/> }.into_any(),
                    "activity" => view! { <super::activity::ActivityTab app=name/> }.into_any(),
                    "deployments" => view! {
                        <DeploymentsTab name=name selected=selected_deployment
                            current=Signal::derive(move || view_of().and_then(|a| a.commit))
                            on_deployed=move || {
                                app.reload();
                                deployments.reload();
                            }/>
                    }.into_any(),
                    "env" => view! {
                        <EnvTab builds=true
                            path=Signal::derive(move || format!("/api/apps/{}/env", name.get()))
                            saved=Signal::derive(move || if deployed() {
                                "Saved. The app restarts with the new values.".to_string()
                            } else {
                                "Saved. It will be used from the first deploy.".to_string()
                            })/>
                    }.into_any(),
                    "volumes" => view! { <VolumesTab name=name app=Signal::derive(move || app.data.get())/> }.into_any(),
                    "info" => view! { <InfoTab app=Signal::derive(move || app.data.get())/> }.into_any(),
                    // Stacks are managed by admins; members edit a service as a plain app.
                    "settings" => match stack_service.get().filter(|_| me.is_admin()) {
                        Some((stack, service)) => view! {
                            <super::stack::ServiceSettings stack=stack service=service
                                on_saved=Callback::new(move |_| app.reload())/>
                            <super::git::AppGitAccess app=Signal::derive(move || app.data.get())/>
                        }.into_any(),
                        None => view! {
                            <SettingsTab app=app.data.get_untracked().expect("loaded")
                                live=Signal::derive(move || app.data.get())
                                on_saved=move || app.reload() on_delete=move || confirm_delete.set(true)/>
                        }.into_any(),
                    },
                    _ => view! {
                        <OverviewTab name=name app=Signal::derive(move || app.data.get()) deployments/>
                    }.into_any(),
                }}
            </Show>

            <Modal open=confirm_delete title="Delete app">
                <p>"This removes the container, its image and all its environment variables. It cannot be undone."</p>
                <Show when=move || has_volumes.get()>
                    <label class="check">
                        <input type="checkbox" bind:checked=delete_volumes/>
                        "Also delete its volumes and the data in them"
                    </label>
                    <p class="muted small">
                        {move || if delete_volumes.get() {
                            "The volumes and their files are deleted with the app. Backups are kept.".to_string()
                        } else {
                            format!(
                                "Otherwise they stay on {} (Nodes page), and an app created with the same name gets them back.",
                                view_of().map(|a| a.spec.node).unwrap_or_default()
                            )
                        }}
                    </p>
                </Show>
                <label class="field">
                    <span>"Type " <code>{move || name.get()}</code> " to confirm"</span>
                    <input type="text" bind:value=typed autocomplete="off"/>
                </label>
                <div class="form-actions">
                    <button class="button" on:click=move |_| confirm_delete.set(false)>"Cancel"</button>
                    <button class="button button-danger"
                        disabled=move || typed.get() != name.get()
                        on:click=delete.clone()>"Delete"</button>
                </div>
            </Modal>
        </div>
    }
}

fn subtitle(app: &AppView) -> String {
    let source = match &app.spec.source {
        Source::Image { image } => format!("image {image}"),
        Source::Executable { filename, arch, .. } => format!("executable {filename} ({arch})"),
        Source::Git { repo, build, .. } => {
            format!(
                "{} · {}",
                repo.trim_start_matches("https://"),
                build_kind(build)
            )
        }
    };
    let deployed = match (&app.commit, app.revision) {
        (_, 0) => "never deployed".to_string(),
        (Some(commit), _) => format!("commit {}", short_hash(commit)),
        (None, revision) => format!("revision {revision}"),
    };
    format!("on {} · {source} · {deployed}", app.spec.node)
}

fn build_kind(build: &Build) -> &'static str {
    match build {
        Build::Dockerfile { .. } => "Dockerfile",
        Build::Rust { .. } => "Rust",
        Build::Python { .. } => "Python",
        Build::Static { .. } => "static site",
        Build::Trunk { .. } => "Rust web app (trunk)",
    }
}

/// What the update check found, under the header until deployed or dismissed:
/// the new commits, or the new image.
#[component]
fn UpdateFound(found: RwSignal<Option<Found>>) -> impl IntoView {
    /// Commits listed at most; the rest are counted.
    const SHOWN: usize = 5;
    move || {
        let f = found.get()?;
        let (headline, commits, more) = match f {
            Found::Commits(c) => {
                let branch = c
                    .branch
                    .clone()
                    .unwrap_or_else(|| "the default branch".into());
                let headline = match (c.behind, &c.deployed) {
                    (Some(n), _) => format!(
                        "{n} new commit{} on {branch}:",
                        if n == 1 { "" } else { "s" }
                    ),
                    (None, Some(d)) => format!(
                        "The deployed commit {} is not on {branch}. Its latest commits:",
                        short_hash(d)
                    ),
                    (None, None) => format!("Latest commits on {branch}:"),
                };
                let total = c.behind.map_or(c.commits.len(), |n| n as usize);
                let more = total.saturating_sub(SHOWN);
                let commits: Vec<CommitView> = c.commits.into_iter().take(SHOWN).collect();
                (headline, commits, more)
            }
            Found::Image(i) => {
                let headline = match &i.deployed {
                    Some(d) => format!(
                        "{} points to a new image: {} (running {}).",
                        i.image,
                        short_digest(&i.latest),
                        short_digest(d)
                    ),
                    None => format!(
                        "{} points to {}; which image runs is not known (its last deploy did not pull one).",
                        i.image,
                        short_digest(&i.latest)
                    ),
                };
                (headline, vec![], 0)
            }
        };
        Some(view! {
            <div class="note note-info update-found" role="status">
                <div class="update-found-head">
                    <span>{headline}</span>
                    <button class="icon-button push-right" aria-label="Dismiss" title="Dismiss"
                        on:click=move |_| found.set(None)>"✕"</button>
                </div>
                {(!commits.is_empty()).then(|| view! {
                    <ul class="commit-list">
                        {commits.into_iter().map(|c| view! { <CommitRow c=c/> }).collect_view()}
                    </ul>
                })}
                {(more > 0).then(|| view! {
                    <div class="muted small">{format!("and {more} more")}</div>
                })}
            </div>
        })
    }
}

// ---------------------------------------------------------------- overview

/// The essentials, on one screen: what it uses, its last deployment (and
/// whether it deploys by itself), its domains. The rest is in Info.
#[component]
fn OverviewTab(
    name: Memo<String>,
    #[prop(into)] app: Signal<Option<AppView>>,
    deployments: Polled<Vec<DeploymentView>>,
) -> impl IntoView {
    let spec = move || app.get().map(|a| a.spec);
    let stats = move || app.get().and_then(|a| a.stats);
    let disk = move || app.get().and_then(|a| a.disk);
    // Counted by the entry point: nothing for an app without a domain.
    let open = move || stats().and_then(|s| s.traffic).map(|t| t.open);
    let cpu_max = Signal::derive(move || spec().and_then(|s| s.limits.cpus).map(|c| c * 100.0));
    let mem_max = Signal::derive(move || {
        spec()
            .and_then(|s| s.limits.memory_mb)
            .map(|m| (m * 1024 * 1024) as f64)
    });
    let is_static = move || spec().is_some_and(|s| s.is_static());
    let domains = polled(Some(Duration::from_secs(30)), || async {
        api::get::<Vec<DomainStatus>>("/api/domains").await
    });
    let info_href = move || format!("/apps/{}?tab=info", name.get());

    view! {
        <Show when=move || !is_static() fallback=move || view! {
            <p class="muted">
                "Static sites are served by the agent directly; they have no container to measure."
                {move || disk().filter(|d| d.files > 0).map(|d| format!(" Its files take {}.", bytes(d.files)))}
            </p>
        }>
            <div class="tiles">
                <div class="tile">
                    <span class="tile-label">"CPU"</span>
                    <span class="tile-value">{move || stats().map(|s| format!("{:.1} %", s.cpu_percent)).unwrap_or_else(|| "—".into())}</span>
                    <span class="tile-sub">{move || spec().and_then(|s| s.limits.cpus).map(|c| format!("limit {} CPU", cpus(c))).unwrap_or_else(|| "no limit".into())}</span>
                </div>
                <div class="tile">
                    <span class="tile-label">"Memory"</span>
                    <span class="tile-value">{move || stats().filter(|s| s.mem_used > 0).map(|s| bytes(s.mem_used)).unwrap_or_else(|| "—".into())}</span>
                    <span class="tile-sub">{move || spec().and_then(|s| s.limits.memory_mb).map(|m| format!("limit {m} MB")).unwrap_or_else(|| "no limit".into())}</span>
                </div>
                <div class="tile">
                    <span class="tile-label">"Restarts"</span>
                    <span class="tile-value">{move || stats().map(|s| s.restarts.to_string()).unwrap_or_else(|| "—".into())}</span>
                    <super::activity::LastRestart app=name restarts=Signal::derive(move || stats().map(|s| s.restarts))/>
                </div>
                <A href=info_href attr:class="tile tile-link" attr:title=move || disk().map(disk_detail).unwrap_or_default()>
                    <span class="tile-label">"Disk"</span>
                    <span class="tile-value">{move || disk().map(|d| bytes(d.total())).unwrap_or_else(|| "—".into())}</span>
                    {move || disk().map(|d| if d.written >= WRITTEN_WARNING {
                        view! {
                            <span class="tile-sub tile-warn">
                                <span aria-hidden="true">"⚠ "</span>
                                {format!("{} written in the container: lost on redeploy, keep it in a volume", bytes(d.written))}
                            </span>
                        }.into_any()
                    } else {
                        view! {
                            <span class="tile-sub">{if d.volumes > 0 {
                                format!("image {} · volumes {}", bytes(d.images), bytes(d.volumes))
                            } else {
                                format!("image {}", bytes(d.images))
                            }}</span>
                        }.into_any()
                    })}
                </A>
                <div class="tile">
                    <span class="tile-label">"Network"</span>
                    <span class="tile-value">{move || stats().map(|s| format!("↓ {}", bytes(s.net_rx))).unwrap_or_else(|| "—".into())}</span>
                    <span class="tile-sub">{move || stats().map(|s| format!("↑ {} sent", bytes(s.net_tx))).unwrap_or_default()}</span>
                </div>
                {move || open().map(|open| view! {
                    <div class="tile" title="Kept-alive HTTP, WebSockets and TLS ports, through the entry point">
                        <span class="tile-label">"Connections"</span>
                        <span class="tile-value">{open}</span>
                        <span class="tile-sub">"open now"</span>
                    </div>
                })}
            </div>
        </Show>
        <LatestDeployment name=name deployments
            auto_deploy=Signal::derive(move || spec().is_some_and(|s| s.stack.is_none()))/>
        <super::activity::RecentActivity app=name/>
        <UsageCharts url=Signal::derive(move || format!("/api/apps/{}/usage", name.get()))
            cpu_max=cpu_max mem_max=mem_max resources=Signal::derive(move || !is_static())/>
        <Show when=move || spec().is_some_and(|s| !s.domains.is_empty())>
            <section class="app-domains">
                <h2 class="section-title">"Domains"</h2>
                <DomainTable domains=Signal::derive(move || {
                    let name = name.get();
                    domains.data.get().unwrap_or_default().into_iter()
                        .filter(|d| d.app.as_deref() == Some(name.as_str()))
                        .collect::<Vec<_>>()
                }) show_app=false/>
            </section>
        </Show>
    }
}

/// The last deployment on one line, leading to the Deployments tab, and
/// whether the app deploys by itself (`auto_deploy`: its own switch, not its
/// stack's), leading to the switch in Settings.
#[component]
fn LatestDeployment(
    name: Memo<String>,
    deployments: Polled<Vec<DeploymentView>>,
    #[prop(into)] auto_deploy: Signal<bool>,
) -> impl IntoView {
    let status = polled(Some(Duration::from_secs(30)), move || {
        let (name, wanted) = (name.get(), auto_deploy.get());
        async move {
            if !wanted {
                return Ok(None);
            }
            api::get::<AutoDeployView>(&format!("/api/apps/{name}/auto-deploy"))
                .await
                .map(Some)
        }
    });
    let auto = move || {
        let s = status
            .data
            .get()
            .flatten()
            .filter(|s| s.watching.is_some())?;
        let (text, class) = match (s.enabled, &s.error, s.method) {
            (false, _, _) => ("Auto-deploy off".to_string(), "muted"),
            (true, Some(_), _) => (
                "Auto-deploy on · last check failed".to_string(),
                "tile-warn",
            ),
            (true, None, AutoDeployMethod::Webhook) => ("Auto-deploy on · webhook".into(), ""),
            (true, None, _) => ("Auto-deploy on · polling".into(), ""),
        };
        Some(view! {
            <A href=move || format!("/apps/{}?tab=settings", name.get()) attr:class=format!("small {class}")
                attr:title="Its switch and how changes are noticed, in Settings">
                {text}
            </A>
        })
    };
    let latest = move || {
        deployments
            .data
            .with(|l| l.as_ref().and_then(|l| l.first().cloned()))
    };
    view! {
        <Show when=move || deployments.data.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))>
            <section class="deploy-strip">
                {move || latest().map(|d| {
                    let took = match (d.started_at, d.finished_at) {
                        (Some(s), Some(f)) => format!(" · took {}s", f.saturating_sub(s)),
                        (None, None) => " · waiting for the build slot".into(),
                        _ => String::new(),
                    };
                    view! {
                        <span class="deploy-strip-label">"Latest deployment"</span>
                        <DeployStatus status=d.status/>
                        <A href=move || format!("/apps/{}?tab=deployments", name.get()) attr:class="deploy-strip-what">
                            <code>{d.commit.as_deref().map(|c| short_hash(c).to_string()).unwrap_or_else(|| format!("rev {}", d.revision))}</code>
                            {d.commit_title.clone().map(|t| view! { <span class="deploy-strip-title">{t}</span> })}
                        </A>
                        <span class="muted small">{format!("{}{took}", ago(d.created_at))}</span>
                    }
                })}
                <span class="push-right">{auto}</span>
            </section>
        </Show>
    }
}

// ---------------------------------------------------------------- info

/// What the app is and what it takes: its configuration as it runs, and its
/// disk with the largest layers of its image.
#[component]
fn InfoTab(#[prop(into)] app: Signal<Option<AppView>>) -> impl IntoView {
    let spec = move || app.get().map(|a| a.spec);
    let commit = move || app.get().and_then(|a| a.commit);
    let disk = move || app.get().and_then(|a| a.disk);
    view! {
        <section class="card details">
            <h2 class="section-title">"Configuration"</h2>
            {move || spec().map(|s| {
                let node = s.node.clone();
                let node_href = format!("/nodes/{node}");
                view! {
                <dl class="kv">
                    <dt>"Node"</dt>
                    <dd><A href=node_href>{node}</A></dd>
                    <dt>"Source"</dt>
                    <dd>{match &s.source {
                        Source::Image { image } => format!("Docker image {image}").into_any(),
                        Source::Executable { filename, arch, args, .. } => format!(
                            "Executable {filename} ({arch}){}",
                            if args.is_empty() { String::new() } else { format!(", run with {}", facade_core::join_command(args)) }
                        ).into_any(),
                        Source::Git { repo, branch, build } => {
                            let branch = format!(
                                " ({}) · {}",
                                branch.clone().unwrap_or_else(|| "default branch".into()),
                                build_kind(build)
                            );
                            match facade_core::git::parse(repo) {
                                Some(location) => view! {
                                    <a href=location.web_url() target="_blank" rel="noopener">{repo.clone()}</a>
                                    {branch}
                                }.into_any(),
                                None => format!("{repo}{branch}").into_any(),
                            }
                        }
                    }}</dd>
                    {matches!(s.source, Source::Git { .. }).then(|| view! {
                        <dt>"Deployed commit"</dt>
                        <dd>{move || match commit() {
                            Some(c) => view! { <code title=c.clone()>{short_hash(&c).to_string()}</code> }.into_any(),
                            None => view! { <span>"—"</span> }.into_any(),
                        }}</dd>
                    })}
                    <dt>"Limits"</dt>
                    <dd>{limits(&s.limits)}</dd>
                    <dt>"HTTP port"</dt>
                    <dd>{s.http_port.map(|p| p.to_string()).unwrap_or_else(|| "—".into())}</dd>
                    <dt>"Published ports"</dt>
                    <dd>{if s.host_network { "node network".to_string() } else if s.ports.is_empty() { "—".into() } else {
                        s.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
                    }}</dd>
                    {(!s.tls_ports.is_empty()).then(|| view! {
                        <dt>"TLS ports"</dt>
                        <dd>{s.tls_ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")}</dd>
                    })}
                    {(!s.command.is_empty()).then(|| view! {
                        <dt>"Command"</dt><dd><code>{facade_core::join_command(&s.command)}</code></dd>
                    })}
                    {s.release_command.clone().map(|r| view! {
                        <dt>"Release command"</dt><dd><code>{r}</code></dd>
                    })}
                    {s.healthcheck.clone().map(|h| view! {
                        <dt>"Health check"</dt>
                        <dd>{if h.is_disabled() {
                            view! { <span>"off (the image's own too)"</span> }.into_any()
                        } else {
                            view! { <code>{h.command()}</code> }.into_any()
                        }}</dd>
                    })}
                    {(!s.redirects.is_empty()).then(|| view! {
                        <dt>"Redirects"</dt>
                        <dd>{format!("{} → {}", s.redirects.join(", "), s.domains.first().cloned().unwrap_or_default())}</dd>
                    })}
                    {(!s.volumes.is_empty()).then(|| view! {
                        <dt>"Volumes"</dt>
                        <dd>{s.volumes.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")}</dd>
                    })}
                    {(!s.links.is_empty()).then(|| view! {
                        <dt>"Links"</dt>
                        <dd>{s.links.iter().map(|l| if l.host() == l.app {
                            l.app.clone()
                        } else {
                            format!("{} (as {})", l.app, l.host())
                        }).collect::<Vec<_>>().join(", ")}</dd>
                    })}
                    {(!s.is_static()).then(|| {
                        let (how, why) = match s.overlap() {
                            Ok(why) => ("no downtime: the new version starts next to the running one", why),
                            Err(why) => ("the old version stops before the new one starts", why),
                        };
                        view! { <dt>"Deploys"</dt><dd title=why>{how}</dd> }
                    })}
                </dl>
                }
            })}
        </section>
        {move || disk().map(|d| {
            let written_class = if d.written >= WRITTEN_WARNING { "tile-warn" } else { "" };
            view! {
            <section class="card details">
                <h2 class="section-title">"Disk"</h2>
                <dl class="kv">
                    <dt>"Images"</dt>
                    <dd>{bytes(d.images)}{(d.rollback_images > 0).then(|| view! {
                        <span class="muted small">{format!(" · with {} kept for rollbacks", d.rollback_images)}</span>
                    })}</dd>
                    <dt>"Volumes"</dt>
                    <dd>{bytes(d.volumes)}</dd>
                    <dt>"Written in the container"</dt>
                    <dd class=written_class>
                        {bytes(d.written)}
                        <span class="muted small">" · lost on redeploy"</span>
                    </dd>
                    {(d.files > 0).then(|| view! { <dt>"Static files"</dt><dd>{bytes(d.files)}</dd> })}
                    {(d.backups > 0).then(|| view! {
                        <dt>"Backups"</dt>
                        <dd>{bytes(d.backups)}<span class="muted small">" · on the server's disk, chunks shared with other apps split"</span></dd>
                    })}
                </dl>
                <p class="muted small kv-foot">{format!("Measured {}.", ago(d.at))}</p>
                <ImageLayers disk=Signal::stored(Some(d))/>
            </section>
            }
        })}
    }
}

fn limits(l: &facade_core::Limits) -> String {
    let mut parts = vec![];
    if let Some(m) = l.memory_mb {
        parts.push(format!("{m} MB memory"));
    }
    if let Some(c) = l.cpus {
        parts.push(format!("{} CPU", cpus(c)));
    }
    if let Some(p) = l.pids {
        parts.push(format!("{p} processes"));
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(" · ")
    }
}

#[component]
pub fn CommitRow(c: CommitView) -> impl IntoView {
    view! {
        <li class="commit">
            <code title=c.hash.clone()>{short_hash(&c.hash).to_string()}</code>
            <span class="commit-subject">{c.subject}</span>
            <span class="muted small">{format!("{} · {}", c.author, ago(c.time))}</span>
        </li>
    }
}

/// What takes the space in the image the app runs: its largest layers, each
/// as a bar against the whole image, with the Dockerfile step that made it.
#[component]
fn ImageLayers(#[prop(into)] disk: Signal<Option<AppDiskView>>) -> impl IntoView {
    move || {
        let d = disk.get().filter(|d| !d.layers.is_empty())?;
        let image = d.layers.iter().map(|l| l.size).sum::<u64>() + d.smaller_layers_size;
        let bar = move |size: u64| {
            view! {
                <span class="layer-bar">
                    <span class="layer-fill" style=format!("width: {:.1}%", percent(size, image).max(0.5))></span>
                </span>
            }
        };
        let smaller = (d.smaller_layers > 0).then(|| {
            view! {
                <li class="layer">
                    {bar(d.smaller_layers_size)}
                    <span class="layer-size">{bytes(d.smaller_layers_size)}</span>
                    <span class="layer-step muted">{format!(
                        "{} smaller layer{}",
                        d.smaller_layers,
                        if d.smaller_layers == 1 { "" } else { "s" }
                    )}</span>
                </li>
            }
        });
        Some(view! {
            <section class="image-layers">
                <h3 class="subsection-title">"What its image holds"</h3>
                <p class="muted small">{format!(
                    "{} in all, largest layers first; each one is a step of its Dockerfile.",
                    bytes(image),
                )}</p>
                <ul class="layers">
                    {d.layers.into_iter().map(|l| view! {
                        <li class="layer">
                            {bar(l.size)}
                            <span class="layer-size">{bytes(l.size)}</span>
                            <code class="layer-step" title=l.command.clone()>{l.command.clone()}</code>
                        </li>
                    }).collect_view()}
                    {smaller}
                </ul>
            </section>
        })
    }
}

/// Every part of an app's disk use, for the tile's tooltip.
fn disk_detail(d: AppDiskView) -> String {
    let mut lines = vec![format!(
        "Images: {}{}",
        bytes(d.images),
        if d.rollback_images > 0 {
            format!(" (with {} kept for rollbacks)", d.rollback_images)
        } else {
            String::new()
        }
    )];
    lines.push(format!("Volumes: {}", bytes(d.volumes)));
    lines.push(format!("Written in the container: {}", bytes(d.written)));
    if d.files > 0 {
        lines.push(format!("Static files: {}", bytes(d.files)));
    }
    if d.backups > 0 {
        lines.push(format!(
            "Backups, on the server's disk: {} (chunks shared with other apps split)",
            bytes(d.backups)
        ));
    }
    lines.push(format!("Measured {}", ago(d.at)));
    lines.join("\n")
}

// ---------------------------------------------------------------- logs

const MAX_LINES: usize = 5000;

#[component]
fn LogsTab(name: Memo<String>) -> impl IntoView {
    let lines = RwSignal::new(Vec::<(usize, String)>::new());
    let follow = RwSignal::new(true);
    let tail = RwSignal::new(500u32);
    let streaming = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let controller = StoredValue::new_local(None::<web_sys::AbortController>);
    let pane = NodeRef::<leptos::html::Div>::new();

    Effect::new(move |_| {
        let (name, follow, tail) = (name.get(), follow.get(), tail.get());
        if let Some(previous) = controller.get_value() {
            previous.abort();
        }
        let Ok(abort) = web_sys::AbortController::new() else {
            return;
        };
        controller.set_value(Some(abort.clone()));
        lines.set(vec![]);
        error.set(None);
        streaming.set(true);
        spawn_local(async move {
            let mut partial = String::new();
            let mut counter = 0usize;
            let path = format!("/api/apps/{name}/logs?tail={tail}&follow={follow}");
            let result = api::stream_text(&path, &abort.signal(), |chunk| {
                partial.push_str(&chunk);
                let mut fresh = vec![];
                while let Some(i) = partial.find('\n') {
                    let line: String = partial.drain(..=i).collect();
                    counter += 1;
                    fresh.push((counter, strip_ansi(line.trim_end())));
                }
                if !fresh.is_empty() {
                    lines.try_update(|l| {
                        l.extend(fresh);
                        if l.len() > MAX_LINES {
                            l.drain(..l.len() - MAX_LINES);
                        }
                    });
                }
            })
            .await;
            if !abort.signal().aborted() {
                streaming.try_set(false);
                if let Err(e) = result {
                    error.try_set(Some(e));
                }
            }
        });
    });
    on_cleanup(move || {
        if let Some(c) = controller.try_get_value().flatten() {
            c.abort();
        }
    });
    // Keep the newest line in view while following.
    Effect::new(move |_| {
        lines.track();
        if follow.get_untracked()
            && let Some(el) = pane.get()
        {
            el.set_scroll_top(el.scroll_height());
        }
    });

    view! {
        <div class="toolbar">
            <label class="check">
                <input type="checkbox" bind:checked=follow/>
                "Follow"
            </label>
            <label class="inline-field">
                "Last"
                <select on:change=move |ev| tail.set(event_target_value(&ev).parse().unwrap_or(500))
                    prop:value=move || tail.get().to_string()>
                    <option value="100">"100"</option>
                    <option value="500">"500"</option>
                    <option value="2000">"2000"</option>
                </select>
                "lines"
            </label>
            <span class="muted small">
                {move || if streaming.get() && follow.get() { "● live" } else { "" }}
            </span>
        </div>
        <ErrorNote error=error/>
        <div class="logs" node_ref=pane>
            <Show when=move || lines.with(|l| !l.is_empty()) fallback=move || view! {
                <div class="logs-empty">{move || if streaming.get() { "Waiting for output…" } else { "No logs." }}</div>
            }>
                <For each=move || lines.get() key=|(i, _)| *i let:line>
                    <LogLine text=line.1/>
                </For>
            </Show>
        </div>
    }
}

#[component]
fn LogLine(text: String) -> impl IntoView {
    // Docker prefixes each line with an RFC 3339 timestamp.
    let (time, rest) = match text.split_once(' ') {
        Some((ts, rest)) if ts.len() > 19 && ts.as_bytes().get(10) == Some(&b'T') => (
            Some((ts[11..19].to_string(), ts.to_string())),
            rest.to_string(),
        ),
        _ => (None, text),
    };
    view! {
        <div class="log-line">
            {time.map(|(short, full)| view! { <span class="log-time" title=full>{short}</span> })}
            <span class="log-text">{rest}</span>
        </div>
    }
}

// ---------------------------------------------------------------- deployments

#[component]
fn DeploymentsTab(
    name: Memo<String>,
    selected: RwSignal<Option<Uuid>>,
    /// The commit the app runs now.
    #[prop(into)]
    current: Signal<Option<String>>,
    on_deployed: impl Fn() + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let list = polled(Some(Duration::from_secs(3)), move || {
        let name = name.get();
        async move { api::get::<Vec<DeploymentView>>(&format!("/api/apps/{name}/deployments")).await }
    });
    // Show the latest deployment until one is picked.
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
                Some(id) => api::get::<DeploymentView>(&format!("/api/deployments/{id}"))
                    .await
                    .map(Some),
                None => Ok(None),
            }
        }
    });
    let log_pane = NodeRef::<leptos::html::Pre>::new();
    Effect::new(move |_| {
        let running = detail.data.with(|d| {
            d.as_ref()
                .and_then(|d| d.as_ref())
                .is_some_and(|d| !d.status.is_finished())
        });
        if running && let Some(el) = log_pane.get() {
            el.set_scroll_top(el.scroll_height());
        }
    });
    let cancel = move |id: Uuid| {
        run(
            "Deployment cancelled",
            async move { api::act(&format!("/api/deployments/{id}/cancel")).await },
            move || {
                list.reload();
                detail.reload();
            },
        );
    };
    let redeploy = Callback::new(move |commit: String| {
        let path = format!("/api/apps/{}/deploy", name.get_untracked());
        let on_deployed = on_deployed.clone();
        spawn_local(async move {
            let body = DeployRequest {
                commit: Some(commit),
                rebuild: false,
            };
            match api::post::<_, DeploymentView>(&path, &body).await {
                Ok(d) => {
                    selected.set(Some(d.id));
                    list.reload();
                    on_deployed();
                }
                Err(e) => toasts().err(e),
            }
        });
    });

    view! {
        <ErrorNote error=list.error/>
        <Show when=move || list.data.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))
            fallback=|| view! { <div class="empty"><p>"No deployments yet. Press Deploy to build and start the app."</p></div> }>
            <div class="split">
                <ul class="deploy-list">
                    <For each=move || list.data.get().unwrap_or_default()
                        key=|d| (d.id, d.status, d.finished_at, d.commit_title.clone())
                        let:d>
                        <li>
                            <button class=move || if selected.get() == Some(d.id) { "deploy-item active" } else { "deploy-item" }
                                on:click=move |_| selected.set(Some(d.id))>
                                <DeployStatus status=d.status/>
                                <span class="deploy-rev">
                                    {match &d.commit {
                                        Some(c) => short_hash(c).to_string(),
                                        None => format!("rev {}", d.revision),
                                    }}
                                </span>
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
                        // A stack's services are redeployed with their stack.
                        let member = name.get_untracked().contains('.');
                        let can_redeploy = status == DeploymentStatus::Succeeded
                            && commit.is_some()
                            && !is_current
                            && !member;
                        view! {
                            <div class="deploy-head">
                                <DeployStatus status=status/>
                                <span>{format!("Revision {}", d.revision)}</span>
                                {d.commit.clone().map(|c| view! {
                                    <code title=c.clone()>{c[..c.len().min(12)].to_string()}</code>
                                })}
                                <span class="muted small">
                                    {format!("requested {}", ago(d.created_at))}
                                    {match (d.started_at, d.finished_at) {
                                        (Some(s), Some(f)) => Some(format!(" · took {}s", f.saturating_sub(s))),
                                        (None, None) => Some(" · waiting for the build slot".to_string()),
                                        _ => None,
                                    }}
                                </span>
                                <span class="push-right">
                                    {(!status.is_finished()).then(|| view! {
                                        <button class="button" on:click=move |_| cancel(id)>"Cancel"</button>
                                    })}
                                    {can_redeploy.then(|| view! {
                                        <button class="button"
                                            title="Deploy this commit again (a rollback). Its image is reused if it is still on the node."
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
                        }
                    })}
                    <pre class="deploy-log" node_ref=log_pane>
                        {move || detail.data.get().flatten().map(|d| strip_ansi(&d.log)).unwrap_or_default()}
                    </pre>
                </div>
            </div>
        </Show>
    }
}

#[component]
pub fn DeployStatus(status: DeploymentStatus) -> impl IntoView {
    let (class, icon, label) = match status {
        DeploymentStatus::Queued => ("busy", "○", "queued"),
        DeploymentStatus::Running => ("busy", "◐", "running"),
        DeploymentStatus::Succeeded => ("good", "✓", "succeeded"),
        DeploymentStatus::Failed => ("critical", "✕", "failed"),
        DeploymentStatus::Cancelled => ("neutral", "■", "cancelled"),
    };
    view! {
        <span class=format!("badge badge-{class}")>
            <span class="badge-icon" aria-hidden="true">{icon}</span>{label}
        </span>
    }
}

// ---------------------------------------------------------------- environment

/// Variables at `path` (an app's, or a stack's); `saved` says what saving does.
/// `builds`: an app's, which can be handed to its builds.
#[component]
pub fn EnvTab(
    #[prop(into)] path: Signal<String>,
    #[prop(into)] saved: Signal<String>,
    #[prop(optional)] builds: bool,
) -> impl IntoView {
    let reveal = RwSignal::new(false);
    // An app's variables answer with the rebuilds the change started.
    let send = move |path: String, patch: EnvPatch| async move {
        if !builds {
            return api::patch(&path, &patch).await;
        }
        let rebuilding: Vec<DeploymentView> = api::patch_json(&path, &patch).await?;
        if !rebuilding.is_empty() {
            toasts().ok("Rebuilding for the new build variables: see Deployments");
        }
        Ok(())
    };
    let vars = polled(None, move || {
        let (path, reveal) = (path.get(), reveal.get());
        async move { api::get::<Vec<EnvView>>(&format!("{path}?reveal={reveal}")).await }
    });

    // One modal for adding and editing a variable.
    let editor_open = RwSignal::new(false);
    let editing_existing = RwSignal::new(false);
    let key = RwSignal::new(String::new());
    let value = RwSignal::new(String::new());
    let sensitive = RwSignal::new(true);
    let build = RwSignal::new(false);
    let open_editor = move |var: Option<EnvView>| {
        match var {
            Some(v) => {
                key.set(v.key);
                value.set(v.value.unwrap_or_default());
                sensitive.set(v.sensitive);
                build.set(v.build);
                editing_existing.set(true);
            }
            None => {
                key.set(String::new());
                value.set(String::new());
                sensitive.set(true);
                build.set(false);
                editing_existing.set(false);
            }
        }
        editor_open.set(true);
    };
    let saved_message = move || saved.get_untracked();
    let save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let patch = EnvPatch {
            set: [(
                key.get_untracked().trim().to_string(),
                SetEnv {
                    value: value.get_untracked(),
                    sensitive: sensitive.get_untracked(),
                    build: builds.then(|| build.get_untracked()),
                },
            )]
            .into(),
            unset: vec![],
        };
        let path = path.get_untracked();
        run(saved_message(), send(path, patch), move || {
            editor_open.set(false);
            vars.reload();
        });
    };
    let remove = move |k: String| {
        let path = path.get_untracked();
        let patch = EnvPatch {
            set: Default::default(),
            unset: vec![k.clone()],
        };
        run(format!("{k} removed"), send(path, patch), move || {
            vars.reload()
        });
    };

    // Bulk import from .env text.
    let import_open = RwSignal::new(false);
    let import_text = RwSignal::new(String::new());
    let import_sensitive = RwSignal::new(true);
    let import = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let mut patch = EnvPatch::default();
        for line in import_text.get_untracked().lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            if let Some((k, v)) = line.split_once('=') {
                let v = v.trim();
                let v = v
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                    .unwrap_or(v);
                patch.set.insert(
                    k.trim().to_string(),
                    SetEnv {
                        value: v.to_string(),
                        sensitive: import_sensitive.get_untracked(),
                        build: None,
                    },
                );
            }
        }
        if patch.set.is_empty() {
            return toasts().err("No KEY=VALUE lines found");
        }
        let count = patch.set.len();
        let path = path.get_untracked();
        run(
            format!("{count} variables saved"),
            send(path, patch),
            move || {
                import_open.set(false);
                import_text.set(String::new());
                vars.reload();
            },
        );
    };

    view! {
        <div class="toolbar">
            <button class="button button-primary" on:click=move |_| open_editor(None)>"Add variable"</button>
            <button class="button" on:click=move |_| import_open.set(true)>"Import .env"</button>
            <label class="check push-right">
                <input type="checkbox" bind:checked=reveal/>
                "Reveal sensitive values"
            </label>
        </div>
        <ErrorNote error=vars.error/>
        <Show when=move || vars.loaded() fallback=Loading>
            <Show when=move || vars.data.with(|v| v.as_ref().is_some_and(|v| !v.is_empty()))
                fallback=|| view! { <div class="empty"><p>"No variables yet."</p></div> }>
                <div class="table-wrap">
                    <table class="table env-table">
                        <thead><tr>
                            <th>"Name"</th><th>"Value"</th><th>"Sensitive"</th>
                            {builds.then(|| view! { <th>"Build"</th> })}
                            <th></th>
                        </tr></thead>
                        <tbody>
                            <For each=move || vars.data.get().unwrap_or_default()
                                key=|v| (v.key.clone(), v.value.clone(), v.sensitive, v.build)
                                let:var>
                                {
                                    let edit_var = var.clone();
                                    let key_for_remove = var.key.clone();
                                    view! {
                                        <tr>
                                            <td><code>{var.key.clone()}</code></td>
                                            <td class="env-value">
                                                {match var.value.clone() {
                                                    Some(v) => view! { <code>{v}</code> }.into_any(),
                                                    None => view! { <span class="muted" title="hidden">"••••••••"</span> }.into_any(),
                                                }}
                                            </td>
                                            <td>{if var.sensitive { "yes" } else { "no" }}</td>
                                            {builds.then(|| view! { <td>{if var.build { "yes" } else { "no" }}</td> })}
                                            <td class="row-actions">
                                                <button class="link-button" on:click=move |_| open_editor(Some(edit_var.clone()))>"Edit"</button>
                                                <button class="link-button danger" on:click=move |_| remove(key_for_remove.clone())>"Remove"</button>
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
        <p class="muted small">
            "Sensitive values are encrypted on the server, hidden unless revealed, and left out of exports."
        </p>

        <Modal open=editor_open title="Environment variable">
            <form class="form" on:submit=save>
                <label class="field">
                    <span>"Name"</span>
                    <input type="text" bind:value=key disabled=editing_existing required
                        placeholder="DATABASE_URL" pattern="[A-Za-z_][A-Za-z0-9_]*" autocomplete="off"/>
                </label>
                <label class="field">
                    <span>"Value"</span>
                    <textarea rows="3" bind:value=value
                        placeholder=move || if editing_existing.get() && sensitive.get() && !reveal.get() {
                            "Hidden. Type a new value to replace it."
                        } else { "" }></textarea>
                </label>
                <label class="check">
                    <input type="checkbox" bind:checked=sensitive/>
                    "Sensitive (hidden, encrypted, not exported)"
                </label>
                {builds.then(|| view! {
                    <label class="check">
                        <input type="checkbox" bind:checked=build/>
                        "Hand to builds too (a build secret named like the variable)"
                    </label>
                })}
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| editor_open.set(false)>"Cancel"</button>
                    <button type="submit" class="button button-primary">"Save"</button>
                </div>
            </form>
        </Modal>

        <Modal open=import_open title="Import .env">
            <form class="form" on:submit=import>
                <label class="field">
                    <span>"Paste KEY=VALUE lines"</span>
                    <textarea rows="10" class="mono" bind:value=import_text
                        placeholder="SIGNALLING_ADDR=0.0.0.0:9090\nTURN_PASSWORD=…"></textarea>
                </label>
                <label class="check">
                    <input type="checkbox" bind:checked=import_sensitive/>
                    "Mark all as sensitive"
                </label>
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| import_open.set(false)>"Cancel"</button>
                    <button type="submit" class="button button-primary">"Import"</button>
                </div>
            </form>
        </Modal>
    }
}

// ---------------------------------------------------------------- volumes

#[component]
fn VolumesTab(name: Memo<String>, #[prop(into)] app: Signal<Option<AppView>>) -> impl IntoView {
    // Where the app mounts a volume, if it still declares it.
    let mount = move |volume: &str| {
        app.with(|a| {
            a.as_ref()?
                .spec
                .volumes
                .iter()
                .find(|v| v.name == volume)
                .map(|v| v.path.clone())
        })
    };
    let volumes = polled(Some(Duration::from_secs(30)), move || {
        let name = name.get();
        async move {
            let all = api::get::<Vec<VolumeView>>("/api/volumes").await?;
            Ok(all
                .into_iter()
                .filter(|v| v.app == name)
                .collect::<Vec<_>>())
        }
    });
    let backups = polled(None, move || {
        let name = name.get();
        async move { api::get::<Vec<BackupView>>(&format!("/api/backups?app={name}")).await }
    });

    let backing_up = RwSignal::new(false);
    let back_up = move |_| {
        backing_up.set(true);
        let body = BackupRequest {
            app: Some(name.get_untracked()),
        };
        spawn_local(async move {
            match api::post::<_, BackupReport>("/api/backups", &body).await {
                Ok(report) => {
                    if !report.done.is_empty() {
                        let n = report.done.len();
                        toasts().ok(format!(
                            "{n} volume{} backed up",
                            if n == 1 { "" } else { "s" }
                        ));
                    }
                    for failure in report.failed {
                        toasts().err(failure);
                    }
                    backups.reload();
                    volumes.reload();
                }
                Err(e) => toasts().err(e),
            }
            backing_up.try_set(false);
        });
    };
    let download = move |path: String, fallback: String| {
        toasts().ok("Download starting…");
        spawn_local(async move {
            if let Err(e) = api::download(&path, &fallback).await {
                toasts().err(e);
            }
        });
    };

    // Replacing a volume's contents from an uploaded archive.
    let replace_open = RwSignal::new(false);
    let replace_target = RwSignal::new(None::<VolumeView>);
    let replace_file = StoredValue::new_local(None::<web_sys::File>);
    let replace_name = RwSignal::new(None::<String>);
    let replacing = RwSignal::new(false);
    let file_input = NodeRef::<leptos::html::Input>::new();
    let open_replace = move |v: VolumeView| {
        replace_target.set(Some(v));
        replace_file.set_value(None);
        replace_name.set(None);
        replace_open.set(true);
    };
    let replace = move |_| {
        let (Some(v), Some(file)) = (replace_target.get_untracked(), replace_file.get_value())
        else {
            return;
        };
        replacing.set(true);
        let path = format!("/api/apps/{}/volumes/{}/archive", v.app, v.volume);
        spawn_local(async move {
            match api::put_file(&path, &file).await {
                Ok(()) => {
                    toasts().ok(format!("{} replaced; the app is starting again", v.volume));
                    replace_open.try_set(false);
                    volumes.reload();
                }
                Err(e) => toasts().err(e),
            }
            replacing.try_set(false);
        });
    };

    let restore_open = RwSignal::new(false);
    let restore_target = RwSignal::new(None::<BackupView>);
    let restore = move |_| {
        let Some(b) = restore_target.get_untracked() else {
            return;
        };
        run(
            format!("{} restored; the app is starting again", b.volume),
            async move {
                api::act(&format!(
                    "/api/backups/{}/{}/{}/restore",
                    b.app, b.volume, b.id
                ))
                .await
            },
            move || {
                restore_open.set(false);
                volumes.reload();
            },
        );
    };

    view! {
        <div class="toolbar">
            <button class="button button-primary" disabled=backing_up on:click=back_up>
                {move || if backing_up.get() { "Backing up…" } else { "Back up now" }}
            </button>
            <span class="muted small">"The server also backs volumes up on a schedule (daily by default) and keeps the copies on its disk."</span>
        </div>
        <ErrorNote error=volumes.error/>
        <Show when=move || volumes.loaded() fallback=Loading>
            <div class="table-wrap">
                <table class="table">
                    <thead>
                        <tr>
                            <th>"Volume"</th>
                            <th>"Mounted at"</th>
                            <th class="num">"Size"</th>
                            <th>"Last backup"</th>
                            <th></th>
                        </tr>
                    </thead>
                    <tbody>
                        <For each=move || volumes.data.get().unwrap_or_default()
                            key=|v| (v.node.clone(), v.name.clone(), v.size, v.last_backup, v.declared)
                            let:v>
                            {
                                let export = format!("/api/apps/{}/volumes/{}/archive", v.app, v.volume);
                                let fallback = format!("{}-{}.tar.gz", v.app, v.volume);
                                let target = v.clone();
                                let mounted = mount(&v.volume).filter(|_| v.declared);
                                view! {
                                    <tr>
                                        <td><code>{v.volume.clone()}</code></td>
                                        <td>{match mounted {
                                            Some(p) => view! { <code>{p}</code> }.into_any(),
                                            None => view! { <span class="muted">"not used any more"</span> }.into_any(),
                                        }}</td>
                                        <td class="num">{v.size.map(bytes).unwrap_or_else(|| "—".into())}</td>
                                        <td class="muted">{v.last_backup.map(ago).unwrap_or_else(|| "never".into())}</td>
                                        <td class="row-actions">
                                            <button class="link-button"
                                                on:click=move |_| download(export.clone(), fallback.clone())>"Download"</button>
                                            <button class="link-button danger"
                                                on:click=move |_| open_replace(target.clone())>"Replace…"</button>
                                        </td>
                                    </tr>
                                }
                            }
                        </For>
                    </tbody>
                </table>
            </div>
        </Show>

        <section class="app-backups">
            <h2 class="section-title">"Backups"</h2>
            <ErrorNote error=backups.error/>
            <Show when=move || backups.data.with(|b| b.as_ref().is_some_and(|b| !b.is_empty()))
                fallback=|| view! { <div class="empty"><p>"No backups yet."</p></div> }>
                <div class="table-wrap">
                    <table class="table">
                        <thead>
                            <tr>
                                <th>"Taken"</th>
                                <th>"Volume"</th>
                                <th class="num" title="What a full copy of the volume takes, compressed">"Size"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            <For each=move || backups.data.get().unwrap_or_default()
                                key=|b| (b.volume.clone(), b.id)
                                let:b>
                                {
                                    let path = format!("/api/backups/{}/{}/{}", b.app, b.volume, b.id);
                                    let fallback = format!("{}-{}-{}.tar.gz", b.app, b.volume, b.id);
                                    let target = b.clone();
                                    view! {
                                        <tr>
                                            <td>
                                                {format!("{} {}", date(b.id), clock(b.id))}
                                                <span class="muted small">{format!(" · {}", ago(b.id))}</span>
                                            </td>
                                            <td><code>{b.volume.clone()}</code></td>
                                            <td class="num">
                                                {bytes(b.size)}
                                                {match b.added {
                                                    Some(added) => view! {
                                                        <span class="muted small"
                                                            title="What this backup added to the store; the rest it shares with other backups.">
                                                            {format!(" · {} new", bytes(added))}
                                                        </span>
                                                    }.into_any(),
                                                    None => view! {
                                                        <span class="muted small"
                                                            title="A full copy, from before incremental backups.">
                                                            " · full copy"
                                                        </span>
                                                    }.into_any(),
                                                }}
                                            </td>
                                            <td class="row-actions">
                                                <button class="link-button"
                                                    on:click=move |_| download(path.clone(), fallback.clone())>"Download"</button>
                                                <button class="link-button danger" on:click=move |_| {
                                                    restore_target.set(Some(target.clone()));
                                                    restore_open.set(true);
                                                }>"Restore…"</button>
                                            </td>
                                        </tr>
                                    }
                                }
                            </For>
                        </tbody>
                    </table>
                </div>
            </Show>
        </section>

        <Modal open=replace_open title="Replace volume contents">
            <p>
                "The app is stopped, everything in "
                <code>{move || replace_target.get().map(|v| mount(&v.volume).unwrap_or(v.volume)).unwrap_or_default()}</code>
                " is replaced by the archive's files, then the app starts again. Download a copy first if you may need the current files."
            </p>
            <label class="field">
                <span>"Archive (.tar or .tar.gz)"</span>
                <input type="file" node_ref=file_input accept=".tar,.tar.gz,.tgz,application/gzip,application/x-tar"
                    on:change=move |_| {
                        let file = file_input.get().and_then(|el| el.files()).and_then(|f| f.get(0));
                        replace_name.set(file.as_ref().map(|f| f.name()));
                        replace_file.set_value(file);
                    }/>
            </label>
            <div class="form-actions">
                <button class="button" on:click=move |_| replace_open.set(false)>"Cancel"</button>
                <button class="button button-danger"
                    disabled=move || replacing.get() || replace_name.with(Option::is_none)
                    on:click=replace>
                    {move || if replacing.get() { "Replacing…" } else { "Replace" }}
                </button>
            </div>
        </Modal>

        <Modal open=restore_open title="Restore backup">
            <p>
                {move || restore_target.get().map(|b| format!(
                    "Restore {} to its state of {} {}? The app is stopped and the volume's current files are replaced by this backup.",
                    b.volume, date(b.id), clock(b.id)
                ))}
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| restore_open.set(false)>"Cancel"</button>
                <button class="button button-danger" on:click=restore>"Restore"</button>
            </div>
        </Modal>
    }
}

// ---------------------------------------------------------------- settings

#[component]
fn SettingsTab(
    app: AppView,
    /// The app as it is now, for the access section (saved on its own).
    #[prop(into)]
    live: Signal<Option<AppView>>,
    on_saved: impl Fn() + Clone + Send + Sync + 'static,
    on_delete: impl Fn() + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let me = me();
    let nodes = polled(None, || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });
    let apps = polled(None, || async {
        api::get::<Vec<AppView>>("/api/apps").await
    });
    let form = SpecForm::from_spec(&app.spec);
    let error = RwSignal::new(None::<String>);
    let name = app.spec.name.clone();
    let auto_deploy = format!("/api/apps/{name}/auto-deploy");
    // Volumes only follow the app to another node when asked to.
    let old_node = StoredValue::new(app.spec.node.clone());
    let had_volumes = !app.spec.volumes.is_empty();
    let move_data = RwSignal::new(false);
    let moving = move || had_volumes && form.node.with(|n| old_node.with_value(|o| n != o));
    let on_access = on_saved.clone();
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let spec = match form.to_spec() {
            Ok(spec) => spec,
            Err(e) => return error.set(Some(e)),
        };
        error.set(None);
        let path = if moving() && move_data.get_untracked() {
            format!("/api/apps/{name}?move_data=true")
        } else {
            format!("/api/apps/{name}")
        };
        let on_saved = on_saved.clone();
        run(
            "Settings saved",
            async move { api::put_empty(&path, &spec).await },
            on_saved,
        );
    };
    view! {
        // A stack's services deploy with it: the switch is the stack's.
        {app.spec.stack.is_none().then(|| view! {
            <super::auto_deploy::AutoDeployPanel path=auto_deploy/>
        })}
        <form class="card form" on:submit=submit>
            <SpecFields form nodes=Signal::derive(move || nodes.data.get().unwrap_or_default())
                apps=Signal::derive(move || apps.data.get().unwrap_or_default()) editing=true/>
            <Show when=moving>
                <div class="note">
                    <label class="check">
                        <input type="checkbox" bind:checked=move_data/>
                        "Copy its volumes to the new node"
                    </label>
                    <div class="small">
                        {move || format!(
                            "The app keeps data on {}. Its volumes are copied over, then deleted from {0}. Without this, the move is refused.",
                            old_node.get_value()
                        )}
                    </div>
                </div>
            </Show>
            <ErrorNote error=error/>
            <div class="form-actions">
                <button class="button button-primary" type="submit">"Save changes"</button>
            </div>
            <p class="muted small">
                "Saving applies right away: the app restarts, and is rebuilt first if its source changed."
            </p>
        </form>
        <super::git::AppGitAccess app=live/>
        <Show when=move || me.is_admin()>
            <AccessSection live=live on_change=on_access.clone()/>
        </Show>
        <section class="card danger-zone">
            <div>
                <h2 class="section-title">"Delete this app"</h2>
                <p class="muted small">"Removes its container, image and variables. Its volumes are kept unless you choose otherwise."</p>
            </div>
            <button class="button button-danger" on:click=move |_| on_delete()>"Delete app…"</button>
        </section>
    }
}

/// Who owns the app and which members may manage it. Admins only.
#[component]
fn AccessSection(
    #[prop(into)] live: Signal<Option<AppView>>,
    on_change: impl Fn() + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let users = polled(None, || async {
        api::get::<Vec<UserView>>("/api/users").await
    });
    let members = move || {
        users.data.with(|u| {
            u.iter()
                .flatten()
                .filter(|u| u.role != "admin")
                .map(|u| u.username.clone())
                .collect::<Vec<_>>()
        })
    };
    let name = move || live.with(|a| a.as_ref().map(|a| a.spec.name.clone()).unwrap_or_default());
    let owner = move || live.with(|a| a.as_ref().and_then(|a| a.owner.clone()));
    let access = move || live.with(|a| a.as_ref().map(|a| a.access.clone()).unwrap_or_default());
    let set_owner = {
        let on_change = on_change.clone();
        move |value: String| {
            let body = SetOwner {
                owner: Some(value).filter(|v| !v.is_empty()),
            };
            let done = match &body.owner {
                Some(o) => format!("Now owned by {o}"),
                None => "Now owned by the admins".to_string(),
            };
            let path = format!("/api/apps/{}/owner", name());
            run(
                done,
                async move { api::put_empty(&path, &body).await },
                on_change.clone(),
            );
        }
    };
    let grantee = RwSignal::new(String::new());
    let grant = {
        let on_change = on_change.clone();
        move |ev: leptos::ev::SubmitEvent| {
            ev.prevent_default();
            let user = grantee.get_untracked();
            if user.is_empty() {
                return;
            }
            let path = format!("/api/apps/{}/access/{user}", name());
            let on_change = on_change.clone();
            run(
                format!("{user} can now manage it"),
                async move { api::put_empty(&path, &()).await },
                move || {
                    grantee.set(String::new());
                    on_change();
                },
            );
        }
    };
    let revoke = Callback::new(move |user: String| {
        let path = format!("/api/apps/{}/access/{user}", name());
        run(
            format!("{user} no longer has access"),
            async move { api::delete(&path).await },
            on_change.clone(),
        );
    });
    // Members who could still be given access.
    let candidates = move || {
        let (owner, access) = (owner(), access());
        members()
            .into_iter()
            .filter(|m| Some(m) != owner.as_ref() && !access.contains(m))
            .collect::<Vec<_>>()
    };

    view! {
        <section class="card form">
            <h2 class="section-title">"Access"</h2>
            <ErrorNote error=users.error/>
            <label class="field">
                <span>"Owner"</span>
                <select on:change=move |ev| set_owner(event_target_value(&ev))
                    prop:value=move || owner().unwrap_or_default()>
                    <option value="" selected=move || owner().is_none()>"The admins"</option>
                    <For each=members key=|m| m.clone() let:m>
                        <option value=m.clone()
                            selected={
                                let name = m.clone();
                                move || owner().as_deref() == Some(name.as_str())
                            }>{m.clone()}</option>
                    </For>
                </select>
                <small>"The owner's memory and CPU caps count this app, and its builds use the owner's registry and git logins."</small>
            </label>
            <div class="field">
                <span>"Members with access"</span>
                <Show when=move || !access().is_empty()
                    fallback=|| view! { <p class="muted small">"Only the owner and the admins."</p> }>
                    <ul class="access-list">
                        <For each=access key=|u| u.clone() let:user>
                            <li>
                                {user.clone()}
                                <button class="link-button danger" on:click={
                                    let user = user.clone();
                                    move |_| revoke.run(user.clone())
                                }>"Remove"</button>
                            </li>
                        </For>
                    </ul>
                </Show>
            </div>
            <Show when=move || !candidates().is_empty()>
                <form class="form-row" on:submit=grant.clone()>
                    <select on:change=move |ev| grantee.set(event_target_value(&ev))
                        prop:value=move || grantee.get()>
                        <option value="">"Choose a member…"</option>
                        <For each=candidates key=|m| m.clone() let:m>
                            <option value=m.clone()>{m.clone()}</option>
                        </For>
                    </select>
                    <button type="submit" class="button" disabled=move || grantee.with(String::is_empty)>
                        "Give access"
                    </button>
                </form>
            </Show>
        </section>
    }
}
