use std::time::Duration;

use facade_core::api::{CreateNode, CreatedNode, NodeView, SetNodeIp, VolumeView};
use facade_core::proto::{Listener, NodeStats, StorageReport};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map};

use crate::api;
use crate::components::{
    CopyField, DiskMeter, ErrorNote, Loading, Meter, Modal, OnlineBadge, Range, RangePicker, Sort,
    SortKey, SortTh, Sparkline, UsageFeed, usage_feed,
};
use crate::util::{Polled, ago, bytes, me, percent, polled, run, toasts};

/// One node at a time: the one in the URL (`/nodes/eu-2`), else the first. Its
/// card with the actions, its usage, its ports and its volumes.
#[component]
pub fn Nodes() -> impl IntoView {
    // Members see the nodes but cannot change them.
    let me = me();
    let params = use_params_map();
    // Stored, so the closures using it can be copied into the views.
    let navigate = StoredValue::new_local(use_navigate());
    let nodes = polled(Some(Duration::from_secs(5)), || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });
    let volumes = polled(Some(Duration::from_secs(30)), || async {
        api::get::<Vec<VolumeView>>("/api/volumes").await
    });
    let wanted = Memo::new(move |_| params.read().get("name"));
    let current = Memo::new(move |_| {
        wanted.get().or_else(|| {
            nodes
                .data
                .with(|n| n.as_ref()?.first().map(|n| n.name.clone()))
        })
    });
    let node = Signal::derive(move || {
        let name = current.get()?;
        nodes
            .data
            .with(|n| n.as_ref()?.iter().find(|n| n.name == name).cloned())
    });
    // What the node's part of the page is drawn from; the rest follows the polls.
    let shape = Memo::new(move |_| {
        node.with(|n| {
            n.as_ref().map(|n| {
                (
                    n.name.clone(),
                    n.connected,
                    n.stats
                        .as_ref()
                        .map(|s| (s.cpus, s.mem_total, s.disk_total)),
                )
            })
        })
    });
    let go = move |path: &str| navigate.with_value(|n| n(path, Default::default()));
    let pick = move |name: String| go(&format!("/nodes/{name}"));

    let add_open = RwSignal::new(false);
    let new_name = RwSignal::new(String::new());
    let add_error = RwSignal::new(None::<String>);
    let joined = RwSignal::new(None::<CreatedNode>);
    let add = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        add_error.set(None);
        let body = CreateNode {
            name: new_name.get_untracked().trim().to_string(),
        };
        spawn_local(async move {
            match api::post::<_, CreatedNode>("/api/nodes", &body).await {
                Ok(created) => {
                    joined.set(Some(created));
                    nodes.reload();
                }
                Err(e) => add_error.set(Some(e)),
            }
        });
    };
    let close_add = move || {
        add_open.set(false);
        // To the node just added (or given a new token).
        if let Some(j) = joined.get_untracked() {
            pick(j.name);
        }
        joined.set(None);
        new_name.set(String::new());
        add_error.set(None);
    };

    let token_for = move |name: String| {
        spawn_local(async move {
            match api::act_json::<CreatedNode>(&format!("/api/nodes/{name}/token")).await {
                Ok(created) => {
                    joined.set(Some(created));
                    add_open.set(true);
                }
                Err(e) => toasts().err(e),
            }
        });
    };
    let remove_open = RwSignal::new(false);
    let remove = move |_| {
        let Some(name) = current.get_untracked() else {
            return;
        };
        run(
            format!("Node {name} removed"),
            async move { api::delete(&format!("/api/nodes/{name}")).await },
            move || {
                remove_open.set(false);
                nodes.reload();
                go("/nodes");
            },
        );
    };
    let ip_open = RwSignal::new(false);
    let ip_value = RwSignal::new(String::new());
    let save_ip = move |auto: bool| {
        let Some(name) = current.get_untracked() else {
            return;
        };
        let body = SetNodeIp {
            public_ip: (!auto).then(|| ip_value.get_untracked().trim().to_string()),
        };
        run(
            "Public IP saved; DNS records follow",
            async move { api::put_empty(&format!("/api/nodes/{name}/ip"), &body).await },
            move || {
                ip_open.set(false);
                nodes.reload();
            },
        );
    };
    let join_command = Signal::derive(move || {
        joined
            .get()
            .map(|j| format!("facaded agent --join {}", j.join_token))
            .unwrap_or_default()
    });
    let install_command = Signal::derive(move || {
        joined
            .get()
            .map(|j| format!("./install.sh agent {}", j.join_token))
            .unwrap_or_default()
    });

    let detail = move || {
        let (name, connected, _) = shape.get()?;
        let n = node.get_untracked()?;
        let local = n.local;
        let storage = Signal::derive(move || node.with(|n| n.as_ref()?.storage.clone()));
        let orphaned = orphaned_on(volumes, name.clone());
        let live = Signal::derive(move || node.with(|n| n.as_ref()?.stats.clone()));
        // The period the charts cover, picked in the card's head; for a node
        // that is away, long enough to show when it was last there.
        let range = RwSignal::new(if connected { Range::Hour } else { Range::Day });
        let metrics = n.stats.clone().map(|s| {
            view! { <NodeMetrics name=name.clone() totals=s live connected storage range/> }
        });
        let errors = Signal::derive(move || {
            node.with(|n| {
                n.as_ref()
                    .filter(|n| n.connected)
                    .map(|n| n.ingress_errors.clone())
                    .unwrap_or_default()
            })
        });
        let has_notes = storage_notes_shown(storage, orphaned);
        let this = StoredValue::new(name.clone());
        Some(view! {
            <section class="node-detail">
                <div class="node-wrap">
                    <article class="card node-card">
                        <header class="card-head">
                            <div>
                                <div class="node-title-row">
                                    <h2 class="node-title">{name.clone()}</h2>
                                    <OnlineBadge online=connected/>
                                </div>
                                <div class="muted small">{move || node.with(|n| n.as_ref().map(node_facts))}</div>
                            </div>
                            {metrics.is_some().then(|| view! { <RangePicker range/> })}
                        </header>
                        <IngressNote errors/>
                        {(!connected).then(|| view! { <p class="muted small">{not_reporting(&n)}</p> })}
                        {metrics}
                        <Show when=move || has_notes.get()>
                            <p class="node-notes small muted">
                                <StorageNotes storage orphaned volumes_href="#volumes"/>
                            </p>
                        </Show>
                    </article>
                    <Show when=move || me.is_admin()>
                        <div class="card-actions">
                            <Show when=move || connected>
                                <button class="link-button" title="Remove dangling images and build cache unused for a week"
                                    on:click=move |_| prune(&this.get_value(), false)>"Clean up"</button>
                                <button class="link-button" title="Also drop the whole build cache (next builds start from scratch) and the images kept for rollbacks"
                                    on:click=move |_| prune(&this.get_value(), true)>"Deep clean"</button>
                            </Show>
                            <button class="link-button" on:click=move |_| {
                                ip_value.set(node.with_untracked(|n| {
                                    n.as_ref().and_then(|n| n.public_ip.clone()).unwrap_or_default()
                                }));
                                ip_open.set(true);
                            }>"Public IP"</button>
                            <Show when=move || !local>
                                <button class="link-button" on:click=move |_| token_for(this.get_value())>"New join token"</button>
                                <button class="link-button danger push-right" on:click=move |_| remove_open.set(true)>"Remove"</button>
                            </Show>
                        </div>
                    </Show>
                </div>
                <p class="muted small after-cards">
                    "Nodes clean up after builds on their own: every 6 hours, and harder when the disk is 85 % full."
                </p>
            </section>
            <Ports node=name.clone()/>
            <NodeVolumes node=name volumes/>
        })
    };

    view! {
        <div class="page">
            <header class="page-head">
                <div class="title-row">
                    <h1>"Nodes"</h1>
                    <Show when=move || nodes.data.with(|n| n.as_ref().is_some_and(|n| !n.is_empty()))>
                        <select class="node-picker" aria-label="Node"
                            on:change=move |ev| pick(event_target_value(&ev))
                            prop:value=move || current.get().unwrap_or_default()>
                            <For each=move || nodes.data.get().unwrap_or_default() key=|n| (n.name.clone(), n.connected) let:n>
                                <option value=n.name.clone()
                                    selected={
                                        let name = n.name.clone();
                                        move || current.with(|c| c.as_ref() == Some(&name))
                                    }>
                                    {if n.connected { n.name.clone() } else { format!("{} (offline)", n.name) }}
                                </option>
                            </For>
                        </select>
                    </Show>
                </div>
                <Show when=move || me.is_admin()>
                    <button class="button button-primary" on:click=move |_| add_open.set(true)>"Add node"</button>
                </Show>
            </header>
            <p class="muted lead">
                "Servers that run apps. Each one runs an agent that connects to this server and serves its own apps' domains: it needs Docker, and ports 80 and 443 open."
            </p>
            <ErrorNote error=nodes.error/>
            <Show when=move || nodes.loaded() fallback=Loading>
                <Show when=move || wanted.get().is_some() && node.with(Option::is_none)>
                    <div class="empty">
                        <p>{move || format!("No node is named {}.", wanted.get().unwrap_or_default())}</p>
                        <p><A href="/nodes">"See the nodes"</A></p>
                    </div>
                </Show>
                {detail}
            </Show>
        </div>

        <Modal open=add_open title="Add a node">
            <Show
                when=move || joined.with(Option::is_some)
                fallback=move || view! {
                    <form class="form" on:submit=add>
                        <label class="field">
                            <span>"Name"</span>
                            <input type="text" bind:value=new_name required placeholder="eu-2"
                                pattern="[a-z0-9]([a-z0-9\\-]*[a-z0-9])?" autocomplete="off"/>
                            <small>"How apps refer to this server."</small>
                        </label>
                        <ErrorNote error=add_error/>
                        <div class="form-actions">
                            <button type="button" class="button" on:click=move |_| close_add()>"Cancel"</button>
                            <button type="submit" class="button button-primary">"Create join token"</button>
                        </div>
                    </form>
                }
            >
                <p>"On the new server (with the façade release folder), install the agent as a service:"</p>
                <CopyField value=install_command/>
                <p class="muted small">"Or run it directly:"</p>
                <CopyField value=join_command/>
                <p class="muted small">
                    "The token is shown once and lets that machine join as this node. Issue a new one if it leaks."
                </p>
                <div class="form-actions">
                    <button class="button button-primary" on:click=move |_| close_add()>"Done"</button>
                </div>
            </Show>
        </Modal>

        <Modal open=ip_open title="Public IP">
            <form class="form" on:submit=move |ev: leptos::ev::SubmitEvent| { ev.prevent_default(); save_ip(false) }>
                <p class="muted small">
                    "Domains of apps on " <strong>{move || current.get().unwrap_or_default()}</strong>
                    " point here. It is detected from where the agent connects (or the server's public_host); "
                    "set it when that is not the address the internet reaches the node at."
                </p>
                <label class="field">
                    <span>"IP address"</span>
                    <input type="text" bind:value=ip_value placeholder="203.0.113.10" autocomplete="off" required/>
                </label>
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| save_ip(true)>"Detect automatically"</button>
                    <button type="submit" class="button button-primary">"Save"</button>
                </div>
            </form>
        </Modal>

        <Modal open=remove_open title="Remove node">
            <p>
                "Remove " <strong>{move || current.get().unwrap_or_default()}</strong>
                "? Its apps must be moved or deleted first. Stop its agent afterwards ("
                <code>"systemctl disable --now facaded-agent"</code>")."
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| remove_open.set(false)>"Cancel"</button>
                <button class="button button-danger" on:click=remove>"Remove"</button>
            </div>
        </Modal>
    }
}

/// Public IP, CPU, cores, agent and uptime, on one line.
fn node_facts(node: &NodeView) -> String {
    let mut facts = vec![
        node.public_ip
            .clone()
            .unwrap_or_else(|| "no public IP yet".into()),
    ];
    if node.public_ip_manual {
        facts[0].push_str(" (set by hand)");
    }
    if let Some(s) = &node.stats {
        if !s.arch.is_empty() {
            facts.push(s.arch.clone());
        }
        facts.push(format!(
            "{} core{}",
            s.cpus,
            if s.cpus == 1 { "" } else { "s" }
        ));
    }
    if node.local {
        facts.push("this server".into());
    }
    if let Some(v) = &node.agent_version {
        facts.push(format!("agent {v}"));
    }
    if let Some(s) = node.stats.as_ref().filter(|_| node.connected) {
        facts.push(format!("up {}", uptime(s.uptime)));
    }
    facts.join(" · ")
}

fn uptime(secs: u64) -> String {
    let (days, hours, minutes) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, h) => format!("{h}h {minutes}m"),
        (d, h) => format!("{d}d {h}h"),
    }
}

/// What stands in for the meters of a node that does not report.
pub fn not_reporting(node: &NodeView) -> String {
    match node.last_seen {
        Some(at) => format!("Last seen {}", ago(at)),
        None => "Not connected yet: start its agent with the join command.".into(),
    }
}

/// The node's CPU, memory and disk, one row each: the bar for now (from `live`,
/// else the `totals` snapshot; none while offline) beside the chart over the
/// `range`. The disk bar says what takes the space.
#[component]
fn NodeMetrics(
    name: String,
    totals: NodeStats,
    live: Signal<Option<NodeStats>>,
    connected: bool,
    storage: Signal<Option<StorageReport>>,
    range: RwSignal<Range>,
) -> impl IntoView {
    let UsageFeed {
        cpu,
        mem,
        disk,
        span,
        gap,
    } = usage_feed(
        Signal::stored(format!("/api/nodes/{name}/usage")),
        range.into(),
    );
    let (cores, mem_total, disk_total) = (totals.cpus, totals.mem_total, totals.disk_total);
    let now = Signal::derive(move || live.get().unwrap_or_else(|| totals.clone()));
    // Offline, nothing is current: the name only, its history beside it.
    let offline = |label: &'static str| {
        view! {
            <div class="meter">
                <div class="meter-head"><span class="meter-label">{label}</span></div>
            </div>
        }
    };
    let (cpu_now, mem_now, disk_now) = if connected {
        (
            view! {
                <Meter label="CPU" percent=Signal::derive(move || now.get().cpu_percent as f64)
                    detail=Signal::derive(move || format!("{:.0} % of {cores} core{}", now.get().cpu_percent, if cores == 1 { "" } else { "s" }))/>
            }
            .into_any(),
            view! {
                <Meter label="Memory" percent=Signal::derive(move || percent(now.get().mem_used, mem_total))
                    detail=Signal::derive(move || format!("{} / {}", bytes(now.get().mem_used), bytes(mem_total)))/>
            }
            .into_any(),
            view! {
                <DiskMeter storage used=Signal::derive(move || now.get().disk_used) total=disk_total/>
            }
            .into_any(),
        )
    } else {
        (
            offline("CPU").into_any(),
            offline("Memory").into_any(),
            offline("Disk").into_any(),
        )
    };
    view! {
        <div class="node-metrics">
            <div class="metric-row">
                <div class="metric-now">{cpu_now}</div>
                <Sparkline title="CPU" points=cpu span=span gap=gap max=Signal::stored(Some(100.0))
                    format=|v| format!("{v:.1} %") bare=true/>
            </div>
            <div class="metric-row">
                <div class="metric-now">{mem_now}</div>
                <Sparkline title="Memory" points=mem span=span gap=gap max=Signal::stored(Some(mem_total as f64))
                    format=|v| bytes(v as u64) bare=true/>
            </div>
            <div class="metric-row">
                <div class="metric-now">{disk_now}</div>
                <Sparkline title="Disk" points=disk span=span gap=gap max=Signal::stored(Some(disk_total as f64))
                    format=|v| bytes(v as u64) bare=true axis=true/>
            </div>
        </div>
    }
}

/// Why apps on the node cannot be reached over the web, if they cannot.
#[component]
fn IngressNote(#[prop(into)] errors: Signal<Vec<String>>) -> impl IntoView {
    view! {
        <Show when=move || errors.with(|e| !e.is_empty())>
            <div class="note note-error" role="alert">
                <span aria-hidden="true">"⚠ "</span>
                "Apps on this node are not reachable over the web: "
                {move || errors.get().join("; ")}
                ". Another program (a web server?) holds the port; façade keeps retrying. "
                "The Ports list below shows which."
            </div>
        </Show>
    }
}

/// Who holds a port, as the "Used by" column reads.
fn used_by(l: &Listener) -> String {
    match (l.app.as_deref(), l.description.as_deref()) {
        (Some("facade"), Some(what)) => what.into(),
        (Some("facade"), None) => "façade".into(),
        (Some(app), Some(what)) => format!("{app} · {what}"),
        (Some(app), None) => app.into(),
        (None, Some(what)) => what.into(),
        (None, None) => "another program".into(),
    }
}

/// The process column's text.
fn process(l: &Listener) -> String {
    match (l.process.clone(), l.pid) {
        (Some(p), Some(pid)) => format!("{p} ({pid})"),
        (Some(p), None) => p,
        (None, Some(pid)) => format!("pid {pid}"),
        (None, None) => String::new(),
    }
}

/// What the filter looks in: the program, who holds the port, the port itself.
fn listener_matches(l: &Listener, needle: &str) -> bool {
    [
        l.process.clone().unwrap_or_default(),
        used_by(l),
        l.port.to_string(),
    ]
    .iter()
    .any(|s| s.to_lowercase().contains(needle))
}

fn listener_key(col: usize, l: &Listener) -> SortKey {
    match col {
        0 => SortKey::num(Some(l.port)),
        1 => SortKey::text(l.protocol.to_string()),
        2 => SortKey::text(&l.address),
        3 => SortKey::text(l.process.as_deref().unwrap_or("")),
        _ => SortKey::text(used_by(l)),
    }
}

/// What listens on a node, and which app (or facade itself) holds each port.
#[component]
fn Ports(node: String) -> impl IntoView {
    let listeners = polled(Some(Duration::from_secs(10)), move || {
        let node = node.clone();
        async move { api::get::<Vec<Listener>>(&format!("/api/nodes/{node}/ports")).await }
    });
    let sort = Sort::new();
    let filter = RwSignal::new(String::new());
    let total = move || listeners.data.with(|l| l.as_ref().map_or(0, Vec::len));
    let rows = Memo::new(move |_| {
        let needle = filter.get().trim().to_lowercase();
        let mut rows = listeners
            .data
            .get()
            .unwrap_or_default()
            .into_iter()
            .filter(|l| needle.is_empty() || listener_matches(l, &needle))
            .collect::<Vec<_>>();
        sort.apply(&mut rows, listener_key);
        rows
    });
    view! {
        <section class="node-ports">
            <div class="toolbar">
                <h2 class="section-title">"Ports"</h2>
                <Show when=move || { total() > 0 }>
                    <input type="search" class="table-filter push-right" placeholder="Filter by program, app or port"
                        aria-label="Filter ports" autocomplete="off" bind:value=filter/>
                    <span class="muted small" aria-live="polite">
                        {move || {
                            let (shown, total) = (rows.with(Vec::len), total());
                            if shown == total { format!("{total} open") } else { format!("{shown} of {total}") }
                        }}
                    </span>
                </Show>
            </div>
            <p class="muted small">"Every TCP and UDP port open on the node, façade's or not. A port held by another program cannot be published by an app."</p>
            <ErrorNote error=listeners.error/>
            <Show when=move || listeners.loaded() fallback=Loading>
                <Show when=move || { total() > 0 }
                    fallback=|| view! { <div class="empty"><p>"Nothing is listening."</p></div> }>
                    <div class="table-wrap">
                        <table class="table">
                            <thead>
                                <tr>
                                    <SortTh sort col=0 label="Port" num=true/>
                                    <SortTh sort col=1 label="Protocol"/>
                                    <SortTh sort col=2 label="Address"/>
                                    <SortTh sort col=3 label="Process"/>
                                    <SortTh sort col=4 label="Used by"/>
                                </tr>
                            </thead>
                            <tbody>
                                <For each=move || rows.get()
                                    key=|l| (l.protocol, l.address.clone(), l.port, l.pid, l.app.clone())
                                    let:l>
                                    <tr>
                                        <td class="num">{l.port}</td>
                                        <td>{l.protocol.to_string()}</td>
                                        <td><code>{l.address.clone()}</code></td>
                                        <td class="muted">{Some(process(&l)).filter(|p| !p.is_empty()).unwrap_or_else(|| "—".into())}</td>
                                        <td>
                                            {match (l.app.clone(), l.description.clone()) {
                                                (Some(app), what) if app == "facade" => view! {
                                                    <span>{what.unwrap_or_else(|| "façade".into())}</span>
                                                }.into_any(),
                                                (Some(app), what) => view! {
                                                    <A href=format!("/apps/{app}") attr:class="strong-link">{app.clone()}</A>
                                                    {what.map(|w| view! { <span class="muted small">{format!(" · {w}")}</span> })}
                                                }.into_any(),
                                                (None, what) => view! {
                                                    <span class="muted">{what.unwrap_or_else(|| "another program".into())}</span>
                                                }.into_any(),
                                            }}
                                        </td>
                                    </tr>
                                </For>
                                <Show when=move || rows.with(Vec::is_empty)>
                                    <tr><td colspan="5" class="muted small">"No port matches."</td></tr>
                                </Show>
                            </tbody>
                        </table>
                    </div>
                </Show>
            </Show>
        </section>
    }
}

/// The volumes on a node, orphaned ones deletable.
#[component]
fn NodeVolumes(node: String, volumes: Polled<Vec<VolumeView>>) -> impl IntoView {
    let me = me();
    let here = Signal::derive(move || {
        volumes.data.with(|v| {
            v.iter()
                .flatten()
                .filter(|v| v.node == node)
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    // Arriving at `#volumes` from another page: the section only now has its
    // rows, after the router looked for it.
    let section = NodeRef::<leptos::html::Section>::new();
    Effect::new(move |done: Option<bool>| {
        if done == Some(true) || !volumes.loaded() {
            return done.unwrap_or(false);
        }
        if window().location().hash().is_ok_and(|h| h != "#volumes") {
            return true;
        }
        let Some(el) = section.get() else {
            return false;
        };
        // Once the sections above (ports, charts) have had their data too.
        set_timeout(move || el.scroll_into_view(), Duration::from_millis(400));
        true
    });
    let volume_target = RwSignal::new(None::<VolumeView>);
    let volume_open = RwSignal::new(false);
    let delete_volume = move |_| {
        let Some(v) = volume_target.get_untracked() else {
            return;
        };
        let path = format!("/api/nodes/{}/volumes/{}", v.node, v.name);
        run(
            format!("{} deleted", v.name),
            async move { api::delete(&path).await },
            move || {
                volume_open.set(false);
                volumes.reload();
            },
        );
    };
    view! {
        <section id="volumes" node_ref=section>
            <h2 class="section-title">"Volumes"</h2>
            <Show when=move || here.with(|v| !v.is_empty())
                fallback=|| view! { <p class="muted small">"No volumes on this node. Apps get one when their settings list it (databases have one)."</p> }>
                <div class="table-wrap">
                    <table class="table">
                        <thead><tr><th>"Volume"</th><th>"App"</th><th class="num">"Size"</th><th>"State"</th><th></th></tr></thead>
                        <tbody>
                            <For each=move || here.get()
                                key=|v| (v.node.clone(), v.name.clone(), v.size, v.in_use, v.declared)
                                let:v>
                                {
                                    let target = v.clone();
                                    view! {
                                        <tr>
                                            <td><code>{v.name.clone()}</code></td>
                                            <td>{v.app.clone()}</td>
                                            <td class="num">
                                                {v.size.map(bytes).unwrap_or_else(|| "—".into())}
                                                {v.limit.map(|l| view! { <span class="muted small">{format!(" of {}", bytes(l))}</span> })}
                                            </td>
                                            <td>{if !v.declared {
                                                view! { <span class="badge badge-neutral">"orphaned"</span> }.into_any()
                                            } else if v.in_use {
                                                view! { <span class="badge badge-good">"in use"</span> }.into_any()
                                            } else {
                                                view! { <span class="muted small">"not mounted"</span> }.into_any()
                                            }}</td>
                                            <td class="row-actions">
                                                <Show when=move || !v.declared && me.is_admin()>
                                                    <button class="link-button danger" on:click={
                                                        let target = target.clone();
                                                        move |_| {
                                                            volume_target.set(Some(target.clone()));
                                                            volume_open.set(true);
                                                        }
                                                    }>"Delete"</button>
                                                </Show>
                                            </td>
                                        </tr>
                                    }
                                }
                            </For>
                        </tbody>
                    </table>
                </div>
                <p class="muted small">"Orphaned volumes belong to deleted apps, or were removed from an app's settings. Their data stays until you delete them."</p>
            </Show>
        </section>

        <Modal open=volume_open title="Delete volume">
            <p>
                "Delete " <code>{move || volume_target.get().map(|v| v.name).unwrap_or_default()}</code>
                " and all the data in it? This cannot be undone."
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| volume_open.set(false)>"Cancel"</button>
                <button class="button button-danger" on:click=delete_volume>"Delete"</button>
            </div>
        </Modal>
    }
}

/// Whether [`StorageNotes`] has anything to say.
fn storage_notes_shown(
    storage: Signal<Option<StorageReport>>,
    orphaned: Signal<(usize, u64)>,
) -> Signal<bool> {
    Signal::derive(move || {
        orphaned.get().0 > 0
            || storage.with(|s| {
                s.as_ref()
                    .is_some_and(|s| s.build_cache_reclaimable > 0 || s.last_cleanup.is_some())
            })
    })
}

/// What a node's disk meter does not say: unused build cache, orphaned volumes
/// (linking to `volumes_href`), the last cleanup.
#[component]
fn StorageNotes(
    #[prop(into)] storage: Signal<Option<StorageReport>>,
    #[prop(into)] orphaned: Signal<(usize, u64)>,
    #[prop(into)] volumes_href: String,
) -> impl IntoView {
    let reclaimable = move || {
        storage.with(|s| {
            s.as_ref()
                .filter(|s| s.build_cache_reclaimable > 0)
                .map(|s| {
                    format!(
                        "{} of build cache unused for now. ",
                        bytes(s.build_cache_reclaimable)
                    )
                })
        })
    };
    let cleanup = move || {
        storage.with(|s| {
            s.as_ref()?.last_cleanup.as_ref().map(|c| {
                format!(
                    "{}leaned {}, freed {}.",
                    if c.aggressive { "Deep-c" } else { "C" },
                    ago(c.at),
                    bytes(c.freed)
                )
            })
        })
    };
    view! {
        {reclaimable}
        {move || {
            let (count, size) = orphaned.get();
            (count > 0).then(|| view! {
                <a href=volumes_href.clone() class="disk-orphans">
                    {format!("{count} orphaned volume{} ({}) no app declares.", if count == 1 { "" } else { "s" }, bytes(size))}
                </a>
                " "
            })
        }}
        {cleanup}
    }
}

/// How many volumes on `node` no app declares, and their size.
fn orphaned_on(volumes: Polled<Vec<VolumeView>>, node: String) -> Signal<(usize, u64)> {
    Signal::derive(move || {
        volumes.data.with(|v| {
            v.iter()
                .flatten()
                .filter(|v| v.node == node && !v.declared)
                .fold((0, 0), |(n, size), v| (n + 1, size + v.size.unwrap_or(0)))
        })
    })
}

fn prune(name: &str, aggressive: bool) {
    let path = format!("/api/nodes/{name}/prune?aggressive={aggressive}");
    run(
        format!("Cleaning up {name}; the figures update in a minute"),
        async move { api::act(&path).await },
        move || {},
    );
}
