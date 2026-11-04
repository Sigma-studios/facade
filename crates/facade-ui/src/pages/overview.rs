use std::time::Duration;

use facade_core::api::{
    AlertView, AppDiskView, AppView, NodeView, StatusView, UsagePoint, WRITTEN_WARNING,
};
use leptos::prelude::*;
use leptos_router::components::A;

use super::nodes::not_reporting;
use crate::api;
use crate::components::{
    DiskMeter, ErrorNote, Loading, Meter, OnlineBadge, Sort, SortKey, SortTh, StateBadge, Trend,
    follow_row,
};
use crate::gauges::LimitBar;
use crate::util::{Polled, ago, bytes, cpus, me, percent, polled};

#[component]
pub fn Overview() -> impl IntoView {
    let me = me();
    let status = polled(Some(Duration::from_secs(5)), || async {
        api::get::<StatusView>("/api/status").await
    });
    let alerts = polled(Some(Duration::from_secs(15)), || async {
        api::get::<Vec<AlertView>>("/api/alerts").await
    });
    let firing = move || {
        alerts.data.with(|a| {
            a.iter()
                .flatten()
                .filter(|x| x.resolved_at.is_none())
                .cloned()
                .collect::<Vec<_>>()
        })
    };

    view! {
        <div class="page">
            <header class="page-head">
                <h1>"Overview"</h1>
            </header>
            <ErrorNote error=status.error/>
            <Show when=move || me.memory_cap().is_some() || me.cpu_cap().is_some()>
                <section class="card cap-card">
                    <h2 class="section-title">"Your share"</h2>
                    <div class="meters">
                        {move || me.memory_cap().map(|(used, cap)| view! {
                            <Meter label="Memory" percent=percent(used, cap)
                                detail=format!("{used} of {cap} MB used by your apps")/>
                        })}
                        {move || me.cpu_cap().map(|(used, cap)| view! {
                            <Meter label="CPUs" percent={ if cap > 0.0 { used * 100.0 / cap } else { 0.0 } }
                                detail=format!("{} of {} CPUs used by your apps", cpus(used), cpus(cap))/>
                        })}
                    </div>
                </section>
            </Show>
            <Show when=move || !firing().is_empty()>
                <A href="/settings/alerts" attr:class="banner banner-critical">
                    <span aria-hidden="true">"⚠"</span>
                    <span>
                        <strong>{move || {
                            let n = firing().len();
                            format!("{n} alert{} firing", if n == 1 { "" } else { "s" })
                        }}</strong>
                        ": "
                        {move || firing().iter().map(|a| a.message.clone()).collect::<Vec<_>>().join(" · ")}
                    </span>
                </A>
            </Show>
            <Show when=move || status.loaded() fallback=Loading>
                <section class="nodes-section">
                    <h2 class="section-title">"Nodes"</h2>
                    <NodesTable status/>
                </section>
                <section>
                    <div class="section-head">
                        <h2 class="section-title">"Apps"</h2>
                        <Show when=move || status.data.with(|s| s.as_ref().is_some_and(|s| !s.apps.is_empty()))>
                            <A href="/apps/new" attr:class="button button-primary">"New app"</A>
                        </Show>
                    </div>
                    <AppsTable apps=Signal::derive(move || {
                        status.data.get().map(|s| s.apps).unwrap_or_default()
                    })/>
                </section>
            </Show>
        </div>
    }
}

/// Every node on one row each: its CPU and memory over the last hour (the
/// figures on hover), its disk, and badges for what needs looking at. A row
/// leads to the node's page, which has the details.
#[component]
fn NodesTable(status: Polled<StatusView>) -> impl IntoView {
    view! {
        <div class="table-wrap nodes-table-wrap">
            <table class="table nodes-table">
                <thead>
                    <tr><th>"Node"</th><th>"CPU"</th><th>"Memory"</th><th>"Disk"</th></tr>
                </thead>
                <For
                    each=move || status.data.get().map(|s| s.nodes).unwrap_or_default()
                    key=|n| (n.name.clone(), n.connected, n.stats.as_ref().map(|s| (s.cpus, s.mem_total, s.disk_total)))
                    children=move |node| view! { <NodeRow node status/> }
                />
            </table>
        </div>
    }
}

/// A node's row in [`NodesTable`]. It is not redrawn as the status refreshes:
/// the charts follow the node's usage feed, the rest the status.
#[component]
fn NodeRow(node: NodeView, status: Polled<StatusView>) -> impl IntoView {
    let name = node.name.clone();
    let current = Signal::derive({
        let name = name.clone();
        move || {
            status
                .data
                .with(|s| s.as_ref()?.nodes.iter().find(|n| n.name == name).cloned())
        }
    });
    let storage = Signal::derive(move || current.with(|n| n.as_ref()?.storage.clone()));
    let errors = Signal::derive(move || {
        current.with(|n| {
            n.as_ref()
                .filter(|n| n.connected)
                .map(|n| n.ingress_errors.join("; "))
                .unwrap_or_default()
        })
    });
    let cells = node.stats.clone().filter(|_| node.connected).map(|s| {
        let usage = polled(Some(Duration::from_secs(15)), {
            let name = name.clone();
            move || {
                let name = name.clone();
                async move {
                    api::get::<Vec<UsagePoint>>(&format!("/api/nodes/{name}/usage?range=1h")).await
                }
            }
        });
        let (cores, mem_total, disk_total) = (s.cpus, s.mem_total, s.disk_total);
        let series = move |f: fn(&UsagePoint, u64) -> f64, total: u64| {
            Signal::derive(move || {
                usage
                    .data
                    .with(|u| u.iter().flatten().map(|p| (p.at, f(p, total))).collect::<Vec<_>>())
            })
        };
        let span = Signal::derive(|| {
            let now = crate::util::now();
            (now.saturating_sub(3600), now)
        });
        let disk_used = Signal::derive(move || {
            let latest = usage
                .data
                .with(|u| u.as_ref().and_then(|u| u.last().and_then(|p| p.disk)));
            latest.unwrap_or(s.disk_used)
        });
        view! {
            <td class="node-chart" data-label="CPU">
                <Trend label="CPU" points=series(|p, _| f64::from(p.cpu), 0) span=span gap=40
                    format=Callback::new(move |v: f64| format!("{v:.0} % of {cores} core{}", if cores == 1 { "" } else { "s" }))/>
            </td>
            <td class="node-chart" data-label="Memory">
                <Trend label="Memory" points=series(|p, total| percent(p.mem, total), mem_total) span=span gap=40
                    format=Callback::new(move |v: f64| format!("{} / {}", bytes((v / 100.0 * mem_total as f64) as u64), bytes(mem_total)))/>
            </td>
            <td class="node-disk" data-label="Disk">
                <DiskMeter storage used=disk_used total=disk_total caption=true/>
            </td>
        }
    });
    let href = format!("/nodes/{name}");
    view! {
        <tbody class="row-link row-group" on:click=follow_row on:mouseup=follow_row>
            <tr>
                <td class="node-cell">
                    <div class="node-cell-head">
                        <A href=href.clone() attr:class="strong-link row-link-main">{name.clone()}</A>
                        <OnlineBadge online=node.connected/>
                        <Show when=move || errors.with(|e| !e.is_empty())>
                            <span class="badge badge-critical"
                                title=move || format!("Apps on this node are not reachable over the web: {}", errors.get())>
                                <span class="badge-icon" aria-hidden="true">"✕"</span>
                                "unreachable"
                            </span>
                        </Show>
                    </div>
                    <div class="muted small">
                        {move || current.with(|n| n.as_ref().map(|n| {
                            let mut facts = vec![n.public_ip.clone().unwrap_or_else(|| "no public IP yet".into())];
                            if let Some(s) = n.stats.as_ref().filter(|s| !s.arch.is_empty()) {
                                facts.push(s.arch.clone());
                            }
                            if n.local {
                                facts.push("this server".into());
                            }
                            facts.join(" · ")
                        }))}
                    </div>
                </td>
                {match cells {
                    Some(cells) => cells.into_any(),
                    None => view! { <td colspan="3" class="muted small node-silent">{not_reporting(&node)}</td> }.into_any(),
                }}
            </tr>
        </tbody>
    }
}

#[component]
pub fn AppsTable(#[prop(into)] apps: Signal<Vec<AppView>>) -> impl IntoView {
    let me = me();
    let sort = Sort::new();
    view! {
        <Show
            when=move || apps.with(|a| !a.is_empty())
            fallback=|| view! {
                <div class="empty">
                    <p>"No apps yet."</p>
                    <A href="/apps/new" attr:class="button button-primary">"Create your first app"</A>
                </div>
            }
        >
            <div class="table-wrap">
                <table class="table">
                    <thead>
                        <tr>
                            <SortTh sort col=0 label="App"/>
                            <SortTh sort col=1 label="State"/>
                            <SortTh sort col=2 label="Node"/>
                            <SortTh sort col=3 label="CPU" num=true desc=true/>
                            <SortTh sort col=4 label="Memory" num=true desc=true/>
                            <SortTh sort col=5 label="Disk" num=true desc=true/>
                            <SortTh sort col=6 label="Domains"/>
                        </tr>
                    </thead>
                    <tbody>
                        <For
                            each=move || {
                                let mut apps = apps.get();
                                sort.apply(&mut apps, app_key);
                                apps
                            }
                            key=|a| (a.spec.name.clone(), a.state, a.stats.as_ref().map(|s| (s.mem_used, s.cpu_percent.to_bits())), a.spec.domains.clone(), a.owner.clone(), a.disk.as_ref().map(|d| (d.total(), d.written)))
                            children=move |app| {
                                let href = format!("/apps/{}", app.spec.name);
                                let stats = app.stats.clone().filter(|s| s.mem_used > 0);
                                view! {
                                    <tr class="row-link" on:click=follow_row on:mouseup=follow_row>
                                        <td>
                                            <A href=href attr:class="strong-link row-link-main">{app.spec.name.clone()}</A>
                                            {app.owner.clone().map(|owner| view! {
                                                <Show when=move || me.is_admin()>
                                                    <div class="muted small">{format!("owned by {owner}")}</div>
                                                </Show>
                                            })}
                                        </td>
                                        <td><StateBadge state=app.state/></td>
                                        <td class="muted">{app.spec.node.clone()}</td>
                                        <td class="num">{stats.as_ref().map(|s| format!("{:.1} %", s.cpu_percent))}</td>
                                        <td class="num">{stats.as_ref().map(|s| {
                                            // Against its limit, when it has one.
                                            let share = app.spec.limits.memory_mb
                                                .map(|m| crate::util::percent(s.mem_used, m * 1024 * 1024));
                                            view! {
                                                {bytes(s.mem_used)}
                                                {share.map(|share| view! {
                                                    <div class="mem-cell-bar" title=format!("{share:.0} % of its {} MB limit", app.spec.limits.memory_mb.unwrap_or(0))>
                                                        <LimitBar label="Memory against its limit" percent=share/>
                                                    </div>
                                                })}
                                            }
                                        })}</td>
                                        <td class="num">{app.disk.clone().filter(|d| d.total() > 0).map(|disk| view! { <DiskCell disk/> })}</td>
                                        <td>
                                            <div class="domains">
                                                {app.spec.domains.iter().map(|d| view! {
                                                    <a href=format!("https://{d}") target="_blank" rel="noopener">{d.clone()}</a>
                                                }).collect_view()}
                                            </div>
                                        </td>
                                    </tr>
                                }
                            }
                        />
                    </tbody>
                </table>
            </div>
        </Show>
    }
}

/// What each [`AppsTable`] column sorts by; usage that shows blank is missing.
fn app_key(col: usize, app: &AppView) -> SortKey {
    let stats = app.stats.as_ref().filter(|s| s.mem_used > 0);
    match col {
        0 => SortKey::text(&app.spec.name),
        1 => SortKey::text(app.state.to_string()),
        2 => SortKey::text(&app.spec.node),
        3 => SortKey::num(stats.map(|s| s.cpu_percent)),
        4 => SortKey::num(stats.map(|s| s.mem_used as f64)),
        5 => SortKey::num(
            app.disk
                .as_ref()
                .map(|d| d.total() as f64)
                .filter(|t| *t > 0.0),
        ),
        _ => SortKey::text(app.spec.domains.first().map_or("", String::as_str)),
    }
}

/// An app's disk use in a table: the total, and on hover (or focus) what it is
/// made of. The tip is placed on the page, so the table's scrolling box does not
/// cut it off.
#[component]
fn DiskCell(disk: AppDiskView) -> impl IntoView {
    // Where the tip goes, as CSS for a fixed box: its right edge under the cell's
    // (so it takes the room on the left, never squeezed), below the cell, or
    // above it near the bottom of the window.
    let anchor = RwSignal::new(None::<String>);
    let show = move |ev: web_sys::Event| {
        use wasm_bindgen::JsCast;
        let Some(target) = ev.current_target() else {
            return;
        };
        let rect = target
            .unchecked_into::<web_sys::Element>()
            .get_bounding_client_rect();
        let Some(page) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.document_element())
        else {
            return;
        };
        let (width, height) = (page.client_width() as f64, page.client_height() as f64);
        let right = (width - rect.right()).max(8.0);
        anchor.set(Some(if rect.bottom() + 160.0 > height {
            format!(
                "right: {right:.0}px; bottom: {:.0}px",
                height - rect.top() + 4.0
            )
        } else {
            format!("right: {right:.0}px; top: {:.0}px", rect.bottom() + 4.0)
        }));
    };
    let warn = disk.written >= WRITTEN_WARNING;
    let mut lines = vec![("Image", bytes(disk.images))];
    if disk.rollback_images > 0 {
        lines.push((
            "Kept for rollbacks",
            format!(
                "{} image{}, included",
                disk.rollback_images,
                if disk.rollback_images == 1 { "" } else { "s" }
            ),
        ));
    }
    if disk.volumes > 0 {
        lines.push(("Volumes", bytes(disk.volumes)));
    }
    if disk.files > 0 {
        lines.push(("Static files", bytes(disk.files)));
    }
    lines.push(("Written in the container", bytes(disk.written)));
    view! {
        <span class="cell-tip-host" tabindex="0"
            on:mouseenter=move |ev| show(ev.into())
            on:focus=move |ev| show(ev.into())
            on:mouseleave=move |_| anchor.set(None)
            on:blur=move |_| anchor.set(None)>
            {warn.then(|| view! { <span class="meter-warn" title="written inside the container: lost on redeploy">"⚠ "</span> })}
            {bytes(disk.total())}
        </span>
        {move || anchor.get().map(|place| view! {
            <div class="cell-tip" role="tooltip" style=place>
                <dl>
                    {lines.iter().map(|(k, v)| view! { <dt>{*k}</dt><dd>{v.clone()}</dd> }).collect_view()}
                </dl>
                {warn.then(|| view! {
                    <div class="cell-tip-warn">"Written inside the container is lost on redeploy: keep it in a volume."</div>
                })}
                <div class="muted">{format!("Measured {}", ago(disk.at))}</div>
            </div>
        })}
    }
}
