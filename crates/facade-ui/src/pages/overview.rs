use std::time::Duration;

use facade_core::api::{
    AlertView, AppDiskView, AppView, NodeView, StatusView, UsagePoint, VolumeView, WRITTEN_WARNING,
};
use facade_core::proto::StorageReport;
use leptos::prelude::*;
use leptos_router::components::A;

use super::nodes::{NodeDisk, orphaned_on};
use crate::api;
use crate::components::{DiskMeter, ErrorNote, Loading, Meter, OnlineBadge, StateBadge, Trend};
use crate::util::{ago, bytes, cpus, me, percent, polled};

#[component]
pub fn Overview() -> impl IntoView {
    let me = me();
    let status = polled(Some(Duration::from_secs(5)), || async {
        api::get::<StatusView>("/api/status").await
    });
    let volumes = polled(Some(Duration::from_secs(30)), || async {
        api::get::<Vec<VolumeView>>("/api/volumes").await
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
                <A href="/alerts" attr:class="banner banner-critical">
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
                <section>
                    <h2 class="section-title">"Nodes"</h2>
                    <div class="grid-cards">
                        <For
                            each=move || status.data.get().map(|s| s.nodes).unwrap_or_default()
                            key=|n| (n.name.clone(), n.ingress_errors.clone())
                            children=move |node| {
                                // The card stays; its disk part follows the status.
                                let name = node.name.clone();
                                let storage = Signal::derive({
                                    let name = name.clone();
                                    move || status.data.with(|s| {
                                        s.as_ref()?.nodes.iter().find(|n| n.name == name)?.storage.clone()
                                    })
                                });
                                let orphaned = orphaned_on(volumes, name.clone());
                                let connected = node.connected;
                                view! {
                                    <div class="node-wrap">
                                        <NodeCard node trends=true storage/>
                                        <NodeDisk name connected storage orphaned/>
                                    </div>
                                }
                            }
                        />
                    </div>
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

#[component]
/// `trends` adds each meter's last hour beside it.
pub fn NodeCard(
    node: NodeView,
    #[prop(optional)] trends: bool,
    /// What takes the disk, followed as reports arrive; else the node's own.
    #[prop(optional, into)]
    storage: Option<Signal<Option<StorageReport>>>,
) -> impl IntoView {
    let storage = storage.unwrap_or_else({
        let storage = node.storage.clone();
        move || Signal::derive(move || storage.clone())
    });
    let stats = node.stats.clone();
    let online = node.connected;
    view! {
        <article class="card node-card">
            <header class="card-head">
                <div>
                    <h3>{node.name.clone()}</h3>
                    <div class="muted small">
                        {node.public_ip.clone().unwrap_or_else(|| "no public IP yet".into())}
                        {node.stats.as_ref().filter(|s| !s.arch.is_empty()).map(|s| format!(" · {}", s.arch))}
                        {node.local.then_some(" · this server")}
                    </div>
                </div>
                <OnlineBadge online=online/>
            </header>
            {(online && !node.ingress_errors.is_empty()).then(|| view! {
                <div class="note note-error" role="alert">
                    <span aria-hidden="true">"⚠ "</span>
                    "Apps on this node are not reachable over the web: "
                    {node.ingress_errors.join("; ")}
                    ". Another program (a web server?) holds the port; façade keeps retrying. "
                    "The Ports list on the Nodes page shows which."
                </div>
            })}
            {match stats.filter(|_| online) {
                Some(s) if trends => {
                    let name = node.name.clone();
                    let usage = polled(Some(Duration::from_secs(15)), move || {
                        let name = name.clone();
                        async move {
                            api::get::<Vec<UsagePoint>>(&format!("/api/nodes/{name}/usage?range=1h")).await
                        }
                    });
                    let (mem_total, disk_total, cores) = (s.mem_total, s.disk_total, s.cpus);
                    // The card is not redrawn as the status refreshes: the meters
                    // follow the usage feed instead.
                    let latest = move || usage.data.with(|u| u.as_ref().and_then(|u| u.last().cloned()));
                    let mem_now = move || latest().map_or(s.mem_used, |p| p.mem);
                    let disk_now = move || latest().and_then(|p| p.disk).unwrap_or(s.disk_used);
                    let series = move |f: Box<dyn Fn(&UsagePoint) -> f64 + Send + Sync>| {
                        Signal::derive(move || {
                            usage.data.with(|u| u.iter().flatten().map(|p| (p.at, f(p))).collect::<Vec<_>>())
                        })
                    };
                    let cpu = series(Box::new(|p| f64::from(p.cpu)));
                    let mem = series(Box::new(move |p| percent(p.mem, mem_total)));
                    let disk = series(Box::new(move |p| percent(p.disk.unwrap_or(0), disk_total)));
                    let span = Signal::derive(|| {
                        let now = crate::util::now();
                        (now.saturating_sub(3600), now)
                    });
                    view! {
                        <div class="meters">
                            <div class="meter-row">
                                <Meter label="CPU" percent=Signal::derive(move || latest().map_or(s.cpu_percent, |p| p.cpu) as f64)
                                    detail=Signal::derive(move || format!("{:.0} % of {cores} cores", latest().map_or(s.cpu_percent, |p| p.cpu)))/>
                                <Trend label="CPU" points=cpu span=span gap=40
                                    format=Callback::new(|v: f64| format!("{v:.0} %"))/>
                            </div>
                            <div class="meter-row">
                                <Meter label="Memory" percent=Signal::derive(move || percent(mem_now(), mem_total))
                                    detail=Signal::derive(move || format!("{} / {}", bytes(mem_now()), bytes(mem_total)))/>
                                <Trend label="Memory" points=mem span=span gap=40
                                    format=Callback::new(move |v: f64| bytes((v / 100.0 * mem_total as f64) as u64))/>
                            </div>
                            <div class="meter-row">
                                <DiskMeter storage used=Signal::derive(disk_now) total=disk_total/>
                                <Trend label="Disk" points=disk span=span gap=40
                                    format=Callback::new(move |v: f64| bytes((v / 100.0 * disk_total as f64) as u64))/>
                            </div>
                        </div>
                    }.into_any()
                }
                Some(s) => view! {
                    <div class="meters">
                        <Meter label="CPU" percent=s.cpu_percent as f64
                            detail=format!("{:.0} % of {} cores", s.cpu_percent, s.cpus)/>
                        <Meter label="Memory" percent=percent(s.mem_used, s.mem_total)
                            detail=format!("{} / {}", bytes(s.mem_used), bytes(s.mem_total))/>
                        <DiskMeter storage used=s.disk_used total=s.disk_total/>
                    </div>
                }.into_any(),
                None => view! {
                    <p class="muted small">
                        {match node.last_seen {
                            Some(at) => format!("Last seen {}", ago(at)),
                            None => "Start its agent with the join command from the Nodes page.".into(),
                        }}
                    </p>
                }.into_any(),
            }}
        </article>
    }
}

#[component]
pub fn AppsTable(#[prop(into)] apps: Signal<Vec<AppView>>) -> impl IntoView {
    let me = me();
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
                            <th>"App"</th>
                            <th>"State"</th>
                            <th>"Node"</th>
                            <th class="num">"CPU"</th>
                            <th class="num">"Memory"</th>
                            <th class="num">"Disk"</th>
                            <th>"Domains"</th>
                        </tr>
                    </thead>
                    <tbody>
                        <For
                            each=move || apps.get()
                            key=|a| (a.spec.name.clone(), a.state, a.stats.as_ref().map(|s| (s.mem_used, s.cpu_percent.to_bits())), a.spec.domains.clone(), a.owner.clone(), a.disk.as_ref().map(|d| (d.total(), d.written)))
                            children=move |app| {
                                let href = format!("/apps/{}", app.spec.name);
                                let stats = app.stats.clone().filter(|s| s.mem_used > 0);
                                view! {
                                    <tr>
                                        <td>
                                            <A href=href attr:class="strong-link">{app.spec.name.clone()}</A>
                                            {app.owner.clone().map(|owner| view! {
                                                <Show when=move || me.is_admin()>
                                                    <div class="muted small">{format!("owned by {owner}")}</div>
                                                </Show>
                                            })}
                                        </td>
                                        <td><StateBadge state=app.state/></td>
                                        <td class="muted">{app.spec.node.clone()}</td>
                                        <td class="num">{stats.as_ref().map(|s| format!("{:.1} %", s.cpu_percent))}</td>
                                        <td class="num">{stats.as_ref().map(|s| bytes(s.mem_used))}</td>
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

/// An app's disk use in a table: the total, and on hover (or focus) what it is
/// made of. The tip is placed on the page, so the table's scrolling box does not
/// cut it off.
#[component]
fn DiskCell(disk: AppDiskView) -> impl IntoView {
    let anchor = RwSignal::new(None::<(f64, f64)>);
    let show = move |ev: web_sys::Event| {
        use wasm_bindgen::JsCast;
        if let Some(target) = ev.current_target() {
            let rect = target
                .unchecked_into::<web_sys::Element>()
                .get_bounding_client_rect();
            anchor.set(Some((rect.right(), rect.bottom())));
        }
    };
    let warn = disk.written >= WRITTEN_WARNING;
    let mut lines = vec![(
        "Image",
        if disk.rollback_images > 0 {
            format!(
                "{} (+{} for rollbacks)",
                bytes(disk.images),
                disk.rollback_images
            )
        } else {
            bytes(disk.images)
        },
    )];
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
        {move || anchor.get().map(|(right, bottom)| view! {
            <div class="cell-tip" role="tooltip"
                style=format!("left: {right:.0}px; top: {:.0}px", bottom + 4.0)>
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
