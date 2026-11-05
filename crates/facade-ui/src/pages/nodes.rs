use std::time::Duration;

use facade_core::api::{CreateNode, CreatedNode, NodeView, SetNodeIp, VolumeView};
use facade_core::proto::{Listener, NodeStats, StorageReport};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};

use crate::api;
use crate::components::{
    CopyField, DiskMeter, ErrorNote, Loading, Menu, Meter, Modal, OnlineBadge, Range, RangePicker,
    Sort, SortKey, SortTh, Sparkline, TabBar, UsageFeed, byte_rate as rate, latest as last,
    per_second, usage_feed,
};
use crate::gauges::StrainBadge;
use crate::util::{Polled, ago, bytes, me, percent, polled, run, toasts};

const TABS: &[(&str, &str)] = &[
    ("overview", "Overview"),
    ("activity", "Activity"),
    ("ports", "Ports"),
    ("volumes", "Volumes"),
];

/// One node at a time, laid out as an app's page: the one in the URL
/// (`/nodes/eu-2`), else the first, switched from its title. Its facts and
/// actions in the header; its usage, ports and volumes in tabs (`?tab=ports`).
#[component]
pub fn Nodes() -> impl IntoView {
    // Members see the nodes but cannot change them.
    let me = me();
    let params = use_params_map();
    let query = use_query_map();
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
    // Volumes were a section of this page once: `#volumes` links land on the tab.
    let tab = Memo::new(move |_| {
        query.read().get("tab").unwrap_or_else(|| {
            let hash = window().location().hash().unwrap_or_default();
            if hash == "#volumes" {
                "volumes"
            } else {
                "overview"
            }
            .into()
        })
    });
    let node = Signal::derive(move || {
        let name = current.get()?;
        nodes
            .data
            .with(|n| n.as_ref()?.iter().find(|n| n.name == name).cloned())
    });
    // What the tabs are drawn from; the rest follows the polls.
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
    let connected = move || node.with(|n| n.as_ref().is_some_and(|n| n.connected));
    let local = move || node.with(|n| n.as_ref().is_some_and(|n| n.local));
    let go = move |path: &str, replace: bool| {
        navigate.with_value(|n| {
            n(
                path,
                leptos_router::NavigateOptions {
                    replace,
                    scroll: !replace,
                    ..Default::default()
                },
            )
        })
    };
    // The tab in a node's URL; the overview needs none.
    let href = move |name: &str, tab: &str| match tab {
        "overview" => format!("/nodes/{name}"),
        tab => format!("/nodes/{name}?tab={tab}"),
    };
    let set_tab = Callback::new(move |t: &'static str| {
        if let Some(name) = current.get_untracked() {
            go(&href(&name, t), true);
        }
    });

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
            go(&format!("/nodes/{}", j.name), false);
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
                go("/nodes", false);
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
    let this = move || current.get_untracked().unwrap_or_default();

    let errors = Signal::derive(move || {
        node.with(|n| {
            n.as_ref()
                .filter(|n| n.connected)
                .map(|n| n.ingress_errors.clone())
                .unwrap_or_default()
        })
    });
    let never_seen = move || {
        node.with(|n| {
            n.as_ref()
                .is_some_and(|n| !n.connected && n.last_seen.is_none())
        })
    };

    // The tab's content, rebuilt only when the node or its shape changes.
    let content = move || {
        let (name, connected, _) = shape.get()?;
        let n = node.get_untracked()?;
        Some(match tab.get().as_str() {
            "ports" => view! { <Ports node=name/> }.into_any(),
            "activity" => view! { <super::activity::ActivityTab node=name/> }.into_any(),
            "volumes" => view! { <NodeVolumes node=name volumes/> }.into_any(),
            _ => {
                let storage = Signal::derive(move || node.with(|n| n.as_ref()?.storage.clone()));
                let live = Signal::derive(move || node.with(|n| n.as_ref()?.stats.clone()));
                let orphaned = orphaned_on(volumes, name.clone());
                let has_notes = storage_notes_shown(storage, orphaned);
                let volumes_href = href(&name, "volumes");
                view! {
                    {n.stats.clone().map(|totals| view! {
                        <NodeMetrics name=name.clone() totals live connected storage/>
                    })}
                    <p class="node-notes small muted">
                        <Show when=move || has_notes.get()>
                            <StorageNotes storage orphaned volumes_href=volumes_href.clone()/>
                            " "
                        </Show>
                        "Nodes clean up after builds on their own: every 6 hours, and harder when the disk is 85 % full."
                    </p>
                }
                .into_any()
            }
        })
    };

    view! {
        <div class="page">
            <header class="page-head">
                <div>
                    <span class="crumb">"Node"</span>
                    <div class="title-row">
                        <Show when=move || current.with(Option::is_some) fallback=|| view! { <h1>"Nodes"</h1> }>
                            <Menu label=Signal::derive(move || current.get().unwrap_or_default()) heading=true start=true
                                title="Switch to another node">
                                <For each=move || nodes.data.get().unwrap_or_default()
                                    key=|n| (n.name.clone(), n.connected)
                                    children=move |n| {
                                        let (to, here) = (n.name.clone(), n.name.clone());
                                        let link = move || href(&to, &tab.get());
                                        let class = move || if current.with(|c| c.as_ref() == Some(&here)) { "menu-item active" } else { "menu-item" };
                                        let dot = if n.connected { "dot dot-good" } else { "dot dot-critical" };
                                        view! {
                                            <A href=link attr:class=class attr:role="menuitem">
                                                <span class=dot aria-hidden="true"></span>
                                                {n.name}
                                                {(!n.connected).then(|| view! { <span class="muted small">"offline"</span> })}
                                            </A>
                                        }
                                    }/>
                                <Show when=move || me.is_admin()>
                                    <div class="menu-sep" role="separator"></div>
                                    <button class="menu-item" role="menuitem" on:click=move |_| add_open.set(true)>"Add a node…"</button>
                                </Show>
                            </Menu>
                        </Show>
                        <Show when=move || node.with(Option::is_some)>
                            <OnlineBadge online=Signal::derive(connected)/>
                            <super::activity::NodeBadges node/>
                        </Show>
                        <StrainBadge pressure=Signal::derive(move || node.with(|n| {
                            n.as_ref().filter(|n| n.connected)?.stats.as_ref()?.resources.pressure
                        }))/>
                    </div>
                    <div class="muted small subtitle">
                        {move || node.with(|n| n.as_ref().map(node_facts))}
                        <super::activity::NodeUptime node=Signal::derive(move || current.get().unwrap_or_default())
                            text=Signal::derive(move || node.with(|n| {
                                let n = n.as_ref().filter(|n| n.connected)?;
                                Some(format!("up {}", uptime(n.stats.as_ref()?.uptime)))
                            }))/>
                    </div>
                </div>
                <Show when=move || me.is_admin() && node.with(Option::is_some)>
                    <div class="actions">
                        <Show when=connected>
                            <button class="button" title="Remove dangling images and build cache unused for a week"
                                on:click=move |_| prune(&this(), false)>"Clean up"</button>
                            <button class="button" title="Also drop the whole build cache (next builds start from scratch) and the images kept for rollbacks"
                                on:click=move |_| prune(&this(), true)>"Deep clean"</button>
                        </Show>
                        <button class="button" title="The address this node's domains point to" on:click=move |_| {
                            ip_value.set(node.with_untracked(|n| {
                                n.as_ref().and_then(|n| n.public_ip.clone()).unwrap_or_default()
                            }));
                            ip_open.set(true);
                        }>"Public IP"</button>
                        <Show when=move || !local()>
                            <Menu label="More">
                                <button class="menu-item" role="menuitem" on:click=move |_| token_for(this())>"New join token…"</button>
                                <button class="menu-item danger" role="menuitem" on:click=move |_| remove_open.set(true)>"Remove node…"</button>
                            </Menu>
                        </Show>
                    </div>
                </Show>
            </header>
            <ErrorNote error=nodes.error/>
            <IngressNote errors/>
            <Show when=never_seen>
                <div class="note">"Not connected yet: start its agent with the join command (More, New join token)."</div>
            </Show>
            <Show when=move || nodes.loaded() fallback=Loading>
                <Show when=move || wanted.get().is_some() && node.with(Option::is_none)>
                    <div class="empty">
                        <p>{move || format!("No node is named {}.", wanted.get().unwrap_or_default())}</p>
                        <p><A href="/nodes">"See the nodes"</A></p>
                    </div>
                </Show>
                <Show when=move || node.with(Option::is_some)>
                    <TabBar tabs=Signal::stored(TABS.to_vec()) active=tab on_pick=set_tab/>
                </Show>
                {content}
            </Show>
        </div>

        <Modal open=add_open title="Add a node">
            <Show
                when=move || joined.with(Option::is_some)
                fallback=move || view! {
                    <form class="form" on:submit=add>
                        <p class="muted small">
                            "A node is a server that runs apps. Its agent connects to this server and serves its own apps' domains: it needs Docker, and ports 80 and 443 open."
                        </p>
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

/// What the node is, on one line under its name, as an app's page says what
/// the app runs: hardware, address, uptime (or when it was last seen), agent.
fn node_facts(node: &NodeView) -> String {
    let mut facts = vec![];
    if let Some(s) = &node.stats {
        if !s.arch.is_empty() {
            facts.push(s.arch.clone());
        }
        facts.push(format!(
            "{} core{}",
            s.cpus,
            if s.cpus == 1 { "" } else { "s" }
        ));
        facts.push(format!("{} memory", bytes(s.mem_total)));
        facts.push(format!("{} disk", bytes(s.disk_total)));
    }
    facts.push(match (&node.public_ip, node.public_ip_manual) {
        (Some(ip), true) => format!("{ip} (set by hand)"),
        (Some(ip), false) => ip.clone(),
        (None, _) => "no public IP yet".into(),
    });
    match (&node.stats, node.connected, node.last_seen) {
        // Uptime follows, leading to the last reboot.
        (Some(_), true, _) => {}
        (_, false, Some(at)) => facts.push(format!("last seen {}", ago(at))),
        _ => {}
    }
    if node.local {
        facts.push("this server".into());
    }
    if let Some(v) = &node.agent_version {
        facts.push(format!("agent {v}"));
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

/// The node's CPU, memory and disk, one panel each: the figure for now (from
/// `live`, else the `totals` snapshot; none while offline), its bar, then its
/// chart over the period picked above them. The disk bar says what takes the
/// space. Offline, the period starts long enough to show when it was last there.
#[component]
fn NodeMetrics(
    name: String,
    totals: NodeStats,
    live: Signal<Option<NodeStats>>,
    connected: bool,
    storage: Signal<Option<StorageReport>>,
) -> impl IntoView {
    let range = RwSignal::new(if connected { Range::Hour } else { Range::Day });
    let feed = usage_feed(
        Signal::stored(format!("/api/nodes/{name}/usage")),
        range.into(),
    );
    let UsageFeed {
        cpu,
        mem,
        disk,
        net_in,
        net_out,
        requests,
        errors,
        disk_read,
        disk_write,
        pressure_cpu,
        pressure_mem,
        pressure_io,
        io_wait,
        span,
        gap,
        ..
    } = feed;
    let traffic = feed.has_traffic();
    let (cores, mem_total, disk_total) = (totals.cpus, totals.mem_total, totals.disk_total);
    let now = Signal::derive(move || live.get().unwrap_or_else(|| totals.clone()));
    let known = |s: Signal<Vec<crate::components::Sample>>| {
        s.with(|s| s.iter().any(crate::components::Sample::known))
    };
    // Kernels without pressure information (or with it off) show no panel.
    let pressure = Signal::derive(move || now.get().resources.pressure);
    let has_pressure = Signal::derive(move || {
        pressure.get().is_some() || known(pressure_cpu) || known(pressure_mem) || known(pressure_io)
    });
    let has_disk_io = Signal::derive(move || known(disk_read) || known(disk_write));
    // Time waiting on disk: what disk pressure says over the last minute, else
    // the share of CPU time idle waiting on it.
    let disk_wait = move || {
        let psi = pressure.get().and_then(|p| p.io).map(|io| io.avg60);
        psi.map(|v| (v, "Share of the last minute some tasks waited on disk"))
            .or_else(|| last(io_wait).map(|v| (v, "Share of CPU time idle waiting on disk")))
    };
    let cpu_share = Signal::derive(move || now.get().cpu_percent as f64);
    let mem_share = Signal::derive(move || percent(now.get().mem_used, mem_total));
    let disk_used = Signal::derive(move || now.get().disk_used);
    // Offline, nothing is current: a dash, and the history under it.
    let figure = move |value: Signal<String>, of: String| {
        if connected {
            view! {
                <div class="metric-figure">
                    <span class="metric-value">{move || value.get()}</span>
                    <span class="metric-of">{of}</span>
                </div>
            }
            .into_any()
        } else {
            view! {
                <div class="metric-figure">
                    <span class="metric-value muted">"—"</span>
                    <span class="metric-of">"offline"</span>
                </div>
            }
            .into_any()
        }
    };
    let warn = |share: Signal<f64>| {
        let high = move || connected && share.get() >= 90.0;
        view! {
            <Show when=high>
                <span class="meter-warn" title="above 90 %">"⚠"</span>
            </Show>
        }
    };
    view! {
        <div class="charts-head">
            <h2 class="section-title">"Usage"</h2>
            <RangePicker range/>
        </div>
        <div class="metric-grid">
            <section class="metric-panel">
                <div class="metric-label">"CPU" {warn(cpu_share)}</div>
                {figure(
                    Signal::derive(move || format!("{:.0} %", cpu_share.get())),
                    format!("of {cores} core{}", if cores == 1 { "" } else { "s" }),
                )}
                {connected.then(|| view! {
                    <Meter label="CPU" bare=true percent=cpu_share detail=Signal::stored(String::new())/>
                })}
                <Sparkline title="CPU" points=cpu span=span gap=gap max=Signal::stored(Some(100.0))
                    format=|v| format!("{v:.1} %") bare=true axis=true/>
            </section>
            <section class="metric-panel">
                <div class="metric-label">"Memory" {warn(mem_share)}</div>
                {figure(
                    Signal::derive(move || bytes(now.get().mem_used)),
                    format!("of {}", bytes(mem_total)),
                )}
                {connected.then(|| view! {
                    <Meter label="Memory" bare=true percent=mem_share detail=Signal::stored(String::new())/>
                })}
                <Sparkline title="Memory" points=mem span=span gap=gap max=Signal::stored(Some(mem_total as f64))
                    format=|v| bytes(v as u64) bare=true axis=true/>
            </section>
            <section class="metric-panel">
                <div class="metric-label">"Disk" {warn(Signal::derive(move || percent(disk_used.get(), disk_total)))}</div>
                {figure(
                    Signal::derive(move || bytes(disk_used.get())),
                    format!("of {}", bytes(disk_total)),
                )}
                {connected.then(|| view! { <DiskMeter storage used=disk_used total=disk_total bare=true/> })}
                <Sparkline title="Disk" points=disk span=span gap=gap max=Signal::stored(Some(disk_total as f64))
                    format=|v| bytes(v as u64) bare=true axis=true/>
            </section>
            // Its own card: under the space chart it made the row twice as tall.
            <Show when=move || has_disk_io.get()>
                <section class="metric-panel metric-long">
                    <div class="metric-label">"Disk activity"</div>
                    {figure(
                        Signal::derive(move || match (last(disk_read), last(disk_write)) {
                            (Some(r), Some(w)) => format!("↓ {} · ↑ {}", rate(r), rate(w)),
                            _ => "—".into(),
                        }),
                        String::new(),
                    )}
                    <div class="metric-sub">
                        "read · write"
                        {move || disk_wait().filter(|_| connected).map(|(v, why)| view! {
                            <span title=format!("{why} (time spent waiting on the disk)")>{format!(" · waiting {}", share(v / 100.0))}</span>
                        })}
                    </div>
                    <Sparkline title="Disk activity" points=disk_read span=span gap=gap max=Signal::stored(None)
                        format=rate bare=true axis=true extra=disk_write extra_label="write" calm=true/>
                </section>
            </Show>
            <section class="metric-panel metric-long">
                <div class="metric-label">"Network"</div>
                {figure(
                    Signal::derive(move || match (last(net_in), last(net_out)) {
                        (Some(i), Some(o)) => format!("↓ {} · ↑ {}", rate(i), rate(o)),
                        _ => "—".into(),
                    }),
                    String::new(),
                )}
                <div class="metric-sub">"in · out · "<super::insights::NodeMonth name=name.clone()/></div>
                <Sparkline title="Received" points=net_in span=span gap=gap max=Signal::stored(None)
                    format=rate bare=true axis=true extra=net_out extra_label="out" calm=true/>
            </section>
            // How often tasks waited for CPU, memory or disk (Linux PSI).
            <Show when=move || has_pressure.get()>
                <section class="metric-panel pressure-panel">
                    <div class="metric-label" title=facade_core::resources::STRAIN_HELP>"Pressure"</div>
                    {figure(
                        Signal::derive(move || pressure.get().and_then(|p| p.strain()).map(|s| s.to_string()).unwrap_or_else(|| "—".into())),
                        "strain".into(),
                    )}
                    // The last minute's, as the strain is judged; else the chart's last.
                    <div class="metric-sub">{move || {
                        let now = |series: Signal<Vec<crate::components::Sample>>, pick: fn(&facade_core::resources::Pressure) -> Option<facade_core::resources::Psi>| {
                            pressure.get().as_ref().and_then(pick).map(|p| p.avg60).filter(|_| connected)
                                .or_else(|| last(series))
                                .map(|v| format!("{v:.1} %"))
                                .unwrap_or_else(|| "—".into())
                        };
                        format!(
                            "mem {} · CPU {} · disk {}",
                            now(pressure_mem, |p| p.memory),
                            now(pressure_cpu, |p| p.cpu),
                            now(pressure_io, |p| p.io),
                        )
                    }}</div>
                    // Memory and disk waits hurt first; CPU waits are in the line above.
                    <Sparkline title="Memory waiting" points=pressure_mem span=span gap=gap max=Signal::stored(Some(10.0))
                        format=|v| format!("{v:.1} %") bare=true axis=true extra=pressure_io extra_label="disk" calm=true/>
                </section>
            </Show>
            // Its apps' requests through its entry point, added up.
            <Show when=move || traffic.get()>
                <section class="metric-panel">
                    <div class="metric-label">"Traffic"</div>
                    {figure(
                        Signal::derive(move || last(requests).map(per_second).unwrap_or_else(|| "—".into())),
                        "requests".into(),
                    )}
                    <div class="metric-sub">{move || {
                        // Points are evenly spaced: their rates add up as their counts do.
                        let total = |s: Signal<Vec<crate::components::Sample>>| {
                            s.with(|s| s.iter().map(|s| s.value).filter(|v| !v.is_nan()).sum::<f64>())
                        };
                        let (r, e) = (total(requests), total(errors));
                        if r > 0.0 {
                            format!("{} answered 5xx over the period", share(e / r))
                        } else {
                            "No requests over the period".into()
                        }
                    }}</div>
                    <Sparkline title="Requests" points=requests span=span gap=gap max=Signal::stored(None)
                        format=per_second bare=true axis=true extra=errors extra_label="5xx"/>
                </section>
            </Show>
        </div>
    }
}

/// A share (0 to 1) in percent, with a decimal when small.
fn share(v: f64) -> String {
    let percent = v * 100.0;
    if percent > 0.0 && percent < 1.0 {
        format!("{percent:.1} %")
    } else {
        format!("{percent:.0} %")
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
                "The Ports tab shows which."
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
        <div class="toolbar">
            <span class="muted small">"Every TCP and UDP port open on the node, façade's or not. A port held by another program cannot be published by an app."</span>
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
        <ErrorNote error=volumes.error/>
        <Show when=move || volumes.loaded() fallback=Loading>
            <Show when=move || here.with(|v| !v.is_empty())
                fallback=|| view! { <div class="empty"><p>"No volumes on this node. Apps get one when their settings list it (databases have one)."</p></div> }>
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
                <p class="muted small after-cards">"Orphaned volumes belong to deleted apps, or were removed from an app's settings. Their data stays until you delete them."</p>
            </Show>
        </Show>

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
                <A href=volumes_href.clone() attr:class="disk-orphans">
                    {format!("{count} orphaned volume{} ({}) no app declares.", if count == 1 { "" } else { "s" }, bytes(size))}
                </A>
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
