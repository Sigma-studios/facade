//! Activity: one list of what happened, newest first, with an icon, a line
//! and a time per entry. The same list is the Activity page (every app and
//! node, with filters in the URL), an app's and a node's Activity tab, and
//! the last few entries on an app's overview.

use std::time::Duration;

use facade_core::api::{AppView, NodeView};
use facade_core::events::{Category, EventKind, EventView, EventsPage, Scope, Severity};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::api;
use crate::components::{ErrorNote, FilterSelect, Loading};
use crate::util::{ago, clock, date, day, me, now, polled, strip_ansi};

/// Entries per page.
const PAGE: u32 = 50;

/// What a list shows.
#[derive(Debug, Clone, Default, PartialEq)]
struct Filter {
    app: String,
    node: String,
    /// A category (`restarts`) or a kind (`reboot`).
    kind: String,
    problems: bool,
}

impl Filter {
    fn api(&self, limit: u32, before: Option<i64>) -> String {
        let mut q = vec![format!("limit={limit}")];
        for (key, value) in [
            ("app", &self.app),
            ("node", &self.node),
            ("kind", &self.kind),
        ] {
            if !value.is_empty() {
                q.push(format!("{key}={value}"));
            }
        }
        if self.problems {
            q.push("problems=true".into());
        }
        if let Some(before) = before {
            q.push(format!("before={before}"));
        }
        format!("/api/events?{}", q.join("&"))
    }
}

/// Up to `limit` entries, a page of the API at a time.
async fn fetch(filter: Filter, limit: u32) -> Result<EventsPage, String> {
    let mut out = EventsPage::default();
    loop {
        let want = (limit as usize).saturating_sub(out.events.len()).min(500) as u32;
        let before = out.events.last().map(|e| e.id);
        let page = api::get::<EventsPage>(&filter.api(want.max(1), before)).await?;
        out.events.extend(page.events);
        out.more = page.more;
        if !out.more || out.events.len() >= limit as usize {
            return Ok(out);
        }
    }
}

// ---------------------------------------------------------------- the page

/// The sidebar's Activity page: every entry the caller may see, filtered by
/// app, node, kind and problems (all in the URL).
#[component]
pub fn ActivityPage() -> impl IntoView {
    let me = me();
    let query = use_query_map();
    let navigate = use_navigate();
    let filter = Memo::new(move |_| {
        let q = query.read();
        Filter {
            app: q.get("app").unwrap_or_default(),
            node: q.get("node").unwrap_or_default(),
            kind: q.get("kind").unwrap_or_default(),
            problems: q.get("problems").is_some_and(|p| p == "1" || p == "true"),
        }
    });
    let linked = Memo::new(move |_| {
        query
            .read()
            .get("event")
            .and_then(|e| e.parse::<i64>().ok())
    });
    let set = move |f: Filter| {
        let mut q = vec![];
        for (key, value) in [("app", &f.app), ("node", &f.node), ("kind", &f.kind)] {
            if !value.is_empty() {
                q.push(format!("{key}={value}"));
            }
        }
        if f.problems {
            q.push("problems=1".into());
        }
        let url = if q.is_empty() {
            "/activity".to_string()
        } else {
            format!("/activity?{}", q.join("&"))
        };
        navigate(
            &url,
            leptos_router::NavigateOptions {
                replace: true,
                scroll: false,
                ..Default::default()
            },
        );
    };
    let set = StoredValue::new(set);
    let change = move |edit: fn(&mut Filter, String)| {
        Callback::new(move |value: String| {
            let mut f = filter.get_untracked();
            edit(&mut f, value);
            set.with_value(|set| set(f));
        })
    };
    let apps = polled(Some(Duration::from_secs(60)), || async {
        api::get::<Vec<AppView>>("/api/apps").await
    });
    let nodes = polled(Some(Duration::from_secs(60)), || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });
    let app_names = Signal::derive(move || {
        let mut names: Vec<String> = apps
            .data
            .with(|a| a.iter().flatten().map(|a| a.spec.name.clone()).collect());
        let picked = filter.with(|f| f.app.clone());
        if !picked.is_empty() && !names.contains(&picked) {
            names.push(picked);
        }
        names.sort();
        names
    });
    let node_names = Signal::derive(move || {
        nodes.data.with(|n| {
            n.iter()
                .flatten()
                .map(|n| n.name.clone())
                .collect::<Vec<_>>()
        })
    });
    let filtered = move || filter.with(|f| *f != Filter::default());

    view! {
        <div class="page">
            <header class="page-head"><h1>"Activity"</h1></header>
            <p class="muted lead">
                "What happened to apps and nodes, and who did it: deploys, restarts and crashes, health, changes, alerts, logins, certificates. Kept 90 days."
            </p>
            <div class="toolbar filters" role="group" aria-label="Filters">
                <FilterSelect label="App" all="All apps" options=app_names
                    value=Signal::derive(move || filter.with(|f| f.app.clone()))
                    on_change=change(|f, v| f.app = v)/>
                <Show when=move || me.is_admin()>
                    <FilterSelect label="Node" all="All nodes" options=node_names
                        value=Signal::derive(move || filter.with(|f| f.node.clone()))
                        on_change=change(|f, v| f.node = v)/>
                </Show>
                <KindSelect value=Signal::derive(move || filter.with(|f| f.kind.clone()))
                    on_change=change(|f, v| f.kind = v)/>
                <label class="check">
                    <input type="checkbox"
                        prop:checked=move || filter.with(|f| f.problems)
                        on:change=move |ev| {
                            let mut f = filter.get_untracked();
                            f.problems = event_target_checked(&ev);
                            set.with_value(|set| set(f));
                        }/>
                    "Problems only"
                </label>
                <Show when=filtered>
                    <button type="button" class="link-button" on:click=move |_| {
                        set.with_value(|set| set(Filter::default()))
                    }>"Clear filters"</button>
                </Show>
            </div>
            <LinkedEntry id=linked fixed_app=false fixed_node=false/>
            <EventList filter=filter page=PAGE
                fixed_app=Signal::derive(move || filter.with(|f| !f.app.is_empty()))
                fixed_node=Signal::derive(move || filter.with(|f| !f.node.is_empty()))
                selected=linked
                empty="Nothing matches these filters."/>
        </div>
    }
}

/// The kind filter: categories, with their names.
#[component]
fn KindSelect(#[prop(into)] value: Signal<String>, on_change: Callback<String>) -> impl IntoView {
    view! {
        <span class="filter-select-wrap">
            <select class="filter-control filter-select" aria-label="Kind"
                class:filter-active=move || value.with(|v| !v.is_empty())
                on:change=move |ev| on_change.run(event_target_value(&ev))
                prop:value=move || value.get()>
                <option value="">"All kinds"</option>
                {Category::ALL.into_iter().map(|c| view! {
                    <option value=c.as_str() selected=move || value.with(|v| v == c.as_str())>{c.label()}</option>
                }).collect_view()}
                // Reached from a link (a node's uptime): a kind of its own.
                {move || {
                    let v = value.get();
                    (!v.is_empty() && Category::parse(&v).is_none()).then(|| view! {
                        <option value=v.clone() selected=true>{v.replace('_', " ")}</option>
                    })
                }}
            </select>
        </span>
    }
}

// ---------------------------------------------------------------- tabs

/// An app's or a node's Activity tab, with its "problems only" switch (in the
/// URL, like the entry a link points to).
#[component]
pub fn ActivityTab(
    #[prop(optional, into)] app: Option<Signal<String>>,
    #[prop(optional, into)] node: Option<Signal<String>>,
) -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let problems = Memo::new(move |_| {
        query
            .read()
            .get("problems")
            .is_some_and(|p| p == "1" || p == "true")
    });
    let linked = Memo::new(move |_| {
        query
            .read()
            .get("event")
            .and_then(|e| e.parse::<i64>().ok())
    });
    let base = move || match (app, node) {
        (Some(app), _) => format!("/apps/{}?tab=activity", app.get_untracked()),
        (_, Some(node)) => format!("/nodes/{}?tab=activity", node.get_untracked()),
        _ => "/activity".into(),
    };
    let filter = Signal::derive(move || Filter {
        app: app.map(|a| a.get()).unwrap_or_default(),
        node: node.map(|n| n.get()).unwrap_or_default(),
        kind: String::new(),
        problems: problems.get(),
    });
    let fixed_app = app.is_some();
    let fixed_node = node.is_some();
    view! {
        <div class="toolbar">
            <label class="check">
                <input type="checkbox" prop:checked=move || problems.get()
                    on:change=move |ev| {
                        let url = if event_target_checked(&ev) {
                            format!("{}&problems=1", base())
                        } else {
                            base()
                        };
                        navigate(&url, leptos_router::NavigateOptions {
                            replace: true,
                            scroll: false,
                            ..Default::default()
                        });
                    }/>
                "Problems only"
            </label>
            <span class="muted small">"Crashes, memory kills, failing health checks, failed deploys and renewals, alerts, disk errors, reboots."</span>
        </div>
        <LinkedEntry id=linked fixed_app fixed_node/>
        <EventList filter page=PAGE
            fixed_app=Signal::stored(fixed_app) fixed_node=Signal::stored(fixed_node)
            selected=linked
            empty=if node.is_some() {
                "Nothing yet: reboots, facade restarts and updates, memory kills, SSH logins, disk errors and system updates show here."
            } else {
                "Nothing yet: deploys, restarts and crashes, health changes and who changed what show here."
            }/>
    }
}

/// The app overview's last entries, under its latest deployment.
#[component]
pub fn RecentActivity(#[prop(into)] app: Signal<String>) -> impl IntoView {
    let filter = Signal::derive(move || Filter {
        app: app.get(),
        ..Default::default()
    });
    let recent = polled(Some(Duration::from_secs(20)), move || {
        let f = filter.get();
        async move { fetch(f, 3).await }
    });
    let events = Signal::derive(move || recent.data.get().map(|p| p.events).unwrap_or_default());
    view! {
        <Show when=move || events.with(|e| !e.is_empty())>
            <section class="ev-recent">
                <div class="ev-recent-head">
                    <span class="deploy-strip-label">"Activity"</span>
                    <A href=move || format!("/apps/{}?tab=activity", app.get()) attr:class="small">"All activity"</A>
                </div>
                <ul class="ev-list">
                    <For each=move || events.get() key=|e| (e.id, e.count, e.at) let:e>
                        <EventRow e fixed_app=true fixed_node=false open=false target=false/>
                    </For>
                </ul>
            </section>
        </Show>
    }
}

/// The Restarts tile's line: why the app last stopped by itself, leading to
/// its entry (or since when restarts are counted).
#[component]
pub fn LastRestart(
    #[prop(into)] app: Signal<String>,
    #[prop(into)] restarts: Signal<Option<u64>>,
) -> impl IntoView {
    let last = polled(Some(Duration::from_secs(30)), move || {
        let app = app.get();
        async move {
            api::get::<EventsPage>(&format!(
                "/api/events?app={app}&kind=restarts&problems=true&limit=1"
            ))
            .await
        }
    });
    let entry = move || last.data.get().and_then(|p| p.events.into_iter().next());
    move || {
        match (restarts.get(), entry()) {
        (Some(n), Some(e)) if n > 0 => {
            let reason = e
                .details
                .reason
                .clone()
                .unwrap_or_else(|| e.summary.clone());
            let href = format!("/apps/{}?tab=activity&event={}", app.get_untracked(), e.id);
            view! {
                <A href=href attr:class="tile-sub ev-tile-link" attr:title=format!("{} · {}", e.summary, ago(e.at))>
                    {format!("last: {reason}")}
                </A>
            }
            .into_any()
        }
        _ => view! { <span class="tile-sub" title="Since the container was created">"since created"</span> }
            .into_any(),
    }
    }
}

// ---------------------------------------------------------------- nodes

/// A node's header badges: pending system upkeep, disk errors lately.
#[component]
pub fn NodeBadges(#[prop(into)] node: Signal<Option<NodeView>>) -> impl IntoView {
    let upkeep = move || {
        let n = node.get()?;
        let u = n.upkeep?;
        let badge = u.badge()?;
        let mut tip = vec![];
        if !u.packages.is_empty() {
            let more = u.updates.unwrap_or(0) as usize > u.packages.len();
            tip.push(format!(
                "Updates: {}{}",
                u.packages.join(", "),
                if more { ", …" } else { "" }
            ));
        }
        if u.reboot_required {
            tip.push(if u.reboot_packages.is_empty() {
                "A reboot is required.".to_string()
            } else {
                format!("Reboot required by {}.", u.reboot_packages.join(", "))
            });
        }
        tip.push(format!("Read {}; façade never installs them.", ago(u.at)));
        Some(view! {
            <span class="badge badge-warn" title=tip.join("\n")>
                <span class="badge-icon" aria-hidden="true">"▲"</span>{badge}
            </span>
        })
    };
    let disk = move || {
        let n = node.get()?;
        let d = n.disk_errors?;
        Some(view! {
            <A href=format!("/nodes/{}?tab=activity&event={}", n.name, d.event)
                attr:class="badge badge-critical" attr:title=format!("{} ({})", d.summary, ago(d.at))>
                <span class="badge-icon" aria-hidden="true">"✕"</span>"disk errors"
            </A>
        })
    };
    view! { {upkeep} {disk} }
}

/// A node's uptime, leading to its last reboot's entry when there is one.
#[component]
pub fn NodeUptime(
    #[prop(into)] node: Signal<String>,
    #[prop(into)] text: Signal<Option<String>>,
) -> impl IntoView {
    let last = polled(Some(Duration::from_secs(120)), move || {
        let node = node.get();
        async move {
            api::get::<EventsPage>(&format!("/api/events?node={node}&kind=reboot&limit=1")).await
        }
    });
    move || {
        let text = text.get()?;
        let reboot = last.data.get().and_then(|p| p.events.into_iter().next());
        Some(match reboot {
            Some(e) => view! {
                " · "
                <A href=format!("/nodes/{}?tab=activity&event={}", node.get_untracked(), e.id)
                    attr:title=format!("Last reboot {}", ago(e.at))>{text}</A>
            }
            .into_any(),
            None => view! { {format!(" · {text}")} }.into_any(),
        })
    }
}

// ---------------------------------------------------------------- the list

/// Entries matching `filter`, a page at a time, refreshed now and then.
#[component]
fn EventList(
    #[prop(into)] filter: Signal<Filter>,
    page: u32,
    /// The list belongs to one app or node: entries do not name it.
    #[prop(into)]
    fixed_app: Signal<bool>,
    #[prop(into)] fixed_node: Signal<bool>,
    /// The entry a link points to, highlighted.
    #[prop(into)]
    selected: Signal<Option<i64>>,
    empty: &'static str,
) -> impl IntoView {
    let shown = RwSignal::new(page);
    // Back to one page when the filters change.
    Effect::new(move |_| {
        filter.track();
        shown.set(page);
    });
    let data = polled(Some(Duration::from_secs(15)), move || {
        let (f, limit) = (filter.get(), shown.get());
        async move { fetch(f, limit).await }
    });
    let events = Signal::derive(move || data.data.get().map(|p| p.events).unwrap_or_default());
    let more = move || data.data.with(|p| p.as_ref().is_some_and(|p| p.more));
    view! {
        <ErrorNote error=data.error/>
        <Show when=move || data.loaded() fallback=Loading>
            <Show when=move || events.with(|e| !e.is_empty())
                fallback=move || view! { <div class="empty"><p>{empty}</p></div> }>
                <ul class="ev-list ev-list-full">
                    <For each=move || events.get() key=|e| (e.id, e.count, e.at) let:e>
                        {
                            let target = selected.get_untracked() == Some(e.id);
                            view! { <EventRow e fixed_app=fixed_app.get_untracked() fixed_node=fixed_node.get_untracked() open=false target/> }
                        }
                    </For>
                </ul>
                <Show when=more>
                    <div class="ev-more">
                        <button class="button" on:click=move |_| shown.update(|s| *s += page)>"Show more"</button>
                    </div>
                </Show>
            </Show>
        </Show>
    }
}

/// The entry a link (a Discord alert, a tile, a badge) points to, open at the
/// top, whatever page of the list it is on.
#[component]
fn LinkedEntry(
    #[prop(into)] id: Signal<Option<i64>>,
    fixed_app: bool,
    fixed_node: bool,
) -> impl IntoView {
    let entry = RwSignal::new(None::<Result<EventView, String>>);
    Effect::new(move |_| {
        let Some(id) = id.get() else {
            entry.set(None);
            return;
        };
        spawn_local(async move {
            entry.try_set(Some(
                api::get::<EventView>(&format!("/api/events/{id}")).await,
            ));
        });
    });
    move || match entry.get() {
        None => None,
        Some(Err(e)) => Some(
            view! { <div class="note note-error">{format!("The linked entry: {e}")}</div> }
                .into_any(),
        ),
        Some(Ok(e)) => Some(
            view! {
                <section class="ev-linked" aria-label="Linked entry">
                    <span class="deploy-strip-label">"Linked entry"</span>
                    <ul class="ev-list">
                        <EventRow e fixed_app fixed_node open=true target=true/>
                    </ul>
                </section>
            }
            .into_any(),
        ),
    }
}

fn icon(kind: EventKind) -> &'static str {
    use EventKind::*;
    match kind {
        DeployStarted => "↑",
        Deployed => "✓",
        DeployFailed => "✕",
        DeployCancelled => "⊘",
        Rollback => "↶",
        AutoDeploy | AutoDeploySwitch => "⟳",
        Crash => "✕",
        OomKill => "▼",
        Restart | AgentRestart | ServerStart => "↻",
        Stop => "■",
        Start => "▶",
        Unhealthy | Healthy => "♥",
        Created => "+",
        Deleted => "−",
        Settings | Env => "✎",
        Domain => "◎",
        Access => "◉",
        AlertFired => "⚠",
        AlertResolved => "✓",
        SshLogin => "→",
        CertIssued | CertFailed => "◆",
        DnsRecord => "◇",
        Reboot => "↺",
        FacadeUpdate => "⇡",
        Upkeep => "▲",
        DiskError => "▣",
        Other => "•",
    }
}

fn severity_class(s: Severity) -> &'static str {
    match s {
        Severity::Info => "ev ev-info",
        Severity::Ok => "ev ev-ok",
        Severity::Warn => "ev ev-warn",
        Severity::Error => "ev ev-error",
    }
}

/// `03:12` today, `18/01 03:12` before.
fn when(at: u64) -> String {
    if date(at) == date(now()) {
        clock(at)
    } else {
        format!("{} {}", day(at), clock(at))
    }
}

/// One entry: its line, and what it says beyond (when, who, log lines…) once open.
#[component]
fn EventRow(
    e: EventView,
    fixed_app: bool,
    fixed_node: bool,
    open: bool,
    target: bool,
) -> impl IntoView {
    let open = RwSignal::new(open);
    let logs = RwSignal::new(None::<Result<Vec<String>, String>>);
    let id = e.id;
    let has_logs = e.has_logs;
    let load_logs = move || {
        if has_logs && logs.get_untracked().is_none() {
            logs.set(Some(Ok(vec![])));
            spawn_local(async move {
                let result = api::get::<EventView>(&format!("/api/events/{id}"))
                    .await
                    .map(|e| e.details.logs);
                logs.try_set(Some(result));
            });
        }
    };
    // A linked entry comes with its lines.
    if !e.details.logs.is_empty() {
        logs.set(Some(Ok(e.details.logs.clone())));
    } else if open.get_untracked() {
        load_logs();
    }
    let tag = if !fixed_app && e.app.is_some() {
        e.app.clone()
    } else if !fixed_node && e.scope == Scope::Node {
        e.node.clone()
    } else {
        None
    };
    let full_time = format!("{} {}", date(e.at), clock(e.at));
    let mut facts = vec![full_time.clone()];
    if e.count > 1 {
        facts.push(format!("{} times since {}", e.count, when(e.first_at)));
    }
    if let Some(actor) = &e.actor {
        facts.push(format!("by {actor}"));
    }
    let app_link = e.app.clone();
    let node_link = e.node.clone().filter(|_| e.scope == Scope::Node);
    let deployment = e
        .details
        .deployment
        .is_some()
        .then(|| e.app.clone())
        .flatten();
    let items = e.details.items.clone();
    let class = format!(
        "{}{}",
        severity_class(e.severity),
        if target { " ev-target" } else { "" }
    );
    view! {
        <li class=class id=format!("event-{id}")>
            <button type="button" class="ev-line" aria-expanded=move || open.get().to_string()
                on:click=move |_| {
                    open.update(|o| *o = !*o);
                    if open.get_untracked() {
                        load_logs();
                    }
                }>
                <span class="ev-icon" aria-hidden="true">{icon(e.kind)}</span>
                <span class="ev-summary">{e.summary.clone()}</span>
                {(e.count > 1).then(|| view! {
                    <span class="ev-count" title=format!("{} times since {}", e.count, when(e.first_at))>{format!("×{}", e.count)}</span>
                })}
                {tag.map(|t| view! { <span class="ev-tag">{t}</span> })}
                <time class="ev-time" title=full_time>{when(e.at)}</time>
            </button>
            <Show when=move || open.get()>
                <div class="ev-details">
                    <div class="small muted">
                        {facts.join(" · ")}
                        {app_link.clone().map(|a| view! { " · " <A href=format!("/apps/{a}?tab=activity")>{format!("app {a}")}</A> })}
                        {node_link.clone().map(|n| view! { " · " <A href=format!("/nodes/{n}?tab=activity")>{format!("node {n}")}</A> })}
                        {deployment.clone().map(|a| view! { " · " <A href=format!("/apps/{a}?tab=deployments")>"deployment log"</A> })}
                    </div>
                    {(!items.is_empty()).then(|| view! { <div class="small ev-items">{items.join(", ")}</div> })}
                    {move || logs.get().map(|l| match l {
                        Ok(lines) if lines.is_empty() => view! { <div class="small muted">"Loading its lines…"</div> }.into_any(),
                        Ok(lines) => {
                            // The last lines (before it died) are the ones to read first.
                            let pane = NodeRef::<leptos::html::Div>::new();
                            pane.on_load(|el| {
                                let el = el.clone();
                                request_animation_frame(move || el.set_scroll_top(el.scroll_height()));
                            });
                            view! {
                                <div class="logs ev-logs" node_ref=pane>
                                    {lines.into_iter().map(|l| view! { <SnapshotLine text=l/> }).collect_view()}
                                </div>
                            }.into_any()
                        }
                        Err(e) => view! { <div class="note note-error">{e}</div> }.into_any(),
                    })}
                </div>
            </Show>
        </li>
    }
}

/// A line of a crash snapshot, drawn like the Logs tab's.
#[component]
fn SnapshotLine(text: String) -> impl IntoView {
    let text = strip_ansi(&text);
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
