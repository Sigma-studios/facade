use std::time::Duration;

use facade_core::api::{AlertView, AppView, NodeView, StatusView};
use leptos::prelude::*;
use leptos_router::components::A;

use crate::api;
use crate::components::{ErrorNote, Loading, Meter, OnlineBadge, StateBadge};
use crate::util::{ago, bytes, cpus, me, percent, polled};

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
                <A href="/apps/new" attr:class="button button-primary">"New app"</A>
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
                            children=move |node| view! { <NodeCard node/> }
                        />
                    </div>
                </section>
                <section>
                    <h2 class="section-title">"Apps"</h2>
                    <AppsTable apps=Signal::derive(move || {
                        status.data.get().map(|s| s.apps).unwrap_or_default()
                    })/>
                </section>
            </Show>
        </div>
    }
}

#[component]
pub fn NodeCard(node: NodeView) -> impl IntoView {
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
                Some(s) => view! {
                    <div class="meters">
                        <Meter label="CPU" percent=s.cpu_percent as f64
                            detail=format!("{:.0} % of {} cores", s.cpu_percent, s.cpus)/>
                        <Meter label="Memory" percent=percent(s.mem_used, s.mem_total)
                            detail=format!("{} / {}", bytes(s.mem_used), bytes(s.mem_total))/>
                        <Meter label="Disk" percent=percent(s.disk_used, s.disk_total)
                            detail=format!("{} / {}", bytes(s.disk_used), bytes(s.disk_total))/>
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
                            <th>"Domains"</th>
                        </tr>
                    </thead>
                    <tbody>
                        <For
                            each=move || apps.get()
                            key=|a| (a.spec.name.clone(), a.state, a.stats.as_ref().map(|s| (s.mem_used, s.cpu_percent.to_bits())), a.spec.domains.clone(), a.owner.clone())
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
