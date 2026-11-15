//! How a reboot went, as its node's agent measured it: one row per app on a
//! shared time axis (stopping, down, starting until healthy), the machine's
//! off time behind them, and the same numbers in a table.

use facade_core::events::{EventView, EventsPage, RebootApp, RebootReport};
use facade_core::reboot_report::duration;
use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::use_query_map;
use std::time::Duration;

use crate::api;
use crate::components::{Ago, ErrorNote, Loading};
use crate::util::polled;

/// Local `HH:MM:SS` of a time in milliseconds.
fn clock_s(ms: u64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    format!(
        "{:02}:{:02}:{:02}",
        d.get_hours(),
        d.get_minutes(),
        d.get_seconds()
    )
}

/// One segment of a row: `(class, from, to, what it says on hover)`.
type Segment = (&'static str, u64, u64, String);

fn segments(r: &RebootReport, a: &RebootApp) -> Vec<Segment> {
    let mut out = vec![];
    if let (Some(from), Some(to)) = (r.shutdown_at, a.stopped_at) {
        let how = if a.killed {
            format!(", then killed (its stop timeout is {} s)", a.stop_timeout)
        } else {
            String::new()
        };
        out.push((
            "rb-stopping",
            from,
            to,
            format!(
                "{}: stopping for {}{how}",
                a.name,
                duration(to - from.min(to))
            ),
        ));
    }
    let down_from = a.stopped_at.or(r.shutdown_at).or(r.off_at);
    let down_to = a.started_at.unwrap_or(r.at);
    if let Some(from) = down_from {
        let what = if a.started_at.is_some() {
            format!(
                "{}: down for {}",
                a.name,
                duration(down_to.saturating_sub(from))
            )
        } else {
            format!("{}: not running again", a.name)
        };
        out.push(("rb-down", from, down_to, what));
    }
    if a.health
        && let Some(started) = a.started_at
    {
        let to = a.healthy_at.unwrap_or(r.at);
        let what = match a.healthy_at {
            Some(h) => format!(
                "{}: running, healthy {} later",
                a.name,
                duration(h.saturating_sub(started))
            ),
            None => format!("{}: running, never healthy", a.name),
        };
        out.push(("rb-starting", started, to, what));
    }
    out
}

/// The timeline and table of a reboot report.
#[component]
pub fn RebootTimeline(report: RebootReport) -> impl IntoView {
    let r = report;
    let rows: Vec<(RebootApp, Vec<Segment>)> = r
        .apps
        .iter()
        .map(|a| (a.clone(), segments(&r, a)))
        .collect();
    // The axis: from the shutdown (or the first thing known) to the last app back.
    let start = r
        .shutdown_at
        .into_iter()
        .chain(r.off_at)
        .chain(rows.iter().flat_map(|(_, s)| s.iter().map(|s| s.1)))
        .min()
        .unwrap_or(r.boot_at)
        .min(r.boot_at);
    let end = rows
        .iter()
        .flat_map(|(_, s)| s.iter().map(|s| s.2))
        .chain([r.boot_at, r.docker_up_at.unwrap_or(r.boot_at)])
        .max()
        .unwrap_or(r.boot_at)
        .max(start + 1000);
    let span = (end - start) as f64;
    let pos = move |t: u64| (t.saturating_sub(start)) as f64 / span * 100.0;
    let off = r
        .off_at
        .map(|o| (pos(o), pos(r.boot_at) - pos(o), r.off_for().unwrap_or(0)));
    let boot_left = pos(r.boot_at);
    let boot_at = r.boot_at;
    let mut facts = vec![];
    if let Some(at) = r.shutdown_at {
        let slowest = r
            .slowest_stop()
            .map(|(a, t)| {
                format!(
                    "; apps stopped within {} (slowest: {})",
                    duration(t),
                    a.name
                )
            })
            .unwrap_or_default();
        facts.push(format!("Shutdown began at {}{slowest}.", clock_s(at)));
    } else if !r.clean {
        facts.push("No shutdown in Docker's journal: the machine went down under the apps (a crash or power cut?).".to_string());
    }
    facts.push(match (r.off_at, r.off_for()) {
        (Some(o), Some(d)) => format!(
            "Off from {} to {} ({}).",
            clock_s(o),
            clock_s(r.boot_at),
            duration(d)
        ),
        _ => format!("Started again at {}.", clock_s(r.boot_at)),
    });
    if let Some(up) = r.docker_up_at {
        facts.push(format!(
            "Docker running {} after the start.",
            duration(up.saturating_sub(r.boot_at))
        ));
    }
    let longest = r.longest_down().map(|(a, d)| (a.name.clone(), d));
    let for_table = r.clone();
    let table = r
        .apps
        .clone()
        .into_iter()
        .map(move |a| {
            let r = &for_table;
            let stop = match (a.stop_took(r), a.killed) {
                (Some(t), true) => view! {
                    <span class="rb-killed">
                        <span aria-hidden="true">"⚠ "</span>
                        {format!("killed after {}", duration(t))}
                    </span>
                }
                .into_any(),
                (Some(t), false) => duration(t).into_any(),
                (None, _) => "—".into_any(),
            };
            let back = match (a.back_after_boot(r), a.started_at) {
                (Some(t), _) => format!(
                    "+{}{}",
                    duration(t),
                    if a.health { " (healthy)" } else { "" }
                ),
                (None, Some(_)) => "running, never healthy".into(),
                (None, None) => "not running again".into(),
            };
            let down = a.down(r).map(duration).unwrap_or_else(|| "—".into());
            let href = format!("/apps/{}", a.app);
            view! {
                <tr>
                    <td><A href=href>{a.name.clone()}</A></td>
                    <td class="num">{stop}</td>
                    <td class="num">{down}</td>
                    <td class="num">{back}</td>
                </tr>
            }
        })
        .collect_view();
    view! {
        <div class="rb">
            {longest.map(|(name, d)| view! {
                <p class="rb-headline">
                    <strong>{duration(d)}</strong>
                    <span class="muted small">{format!(" longest down ({name})")}</span>
                </p>
            })}
            <div class="rb-key small muted" aria-hidden="true">
                <span class="rb-swatch rb-stopping"></span>"stopping"
                <span class="rb-swatch rb-down"></span>"down"
                <span class="rb-swatch rb-starting"></span>"starting until healthy"
                <span class="rb-swatch rb-off"></span>"machine off"
            </div>
            // A column for "killed" on every row when one has it, so the tracks align.
            <div class=if r.apps.iter().any(|a| a.killed) { "rb-chart rb-with-notes" } else { "rb-chart" }
                role="img" aria-label=r.summary()>
                {rows.into_iter().map(|(a, segs)| view! {
                    <div class="rb-row">
                        <span class="rb-name" title=a.name.clone()>{a.name.clone()}</span>
                        <div class="rb-track">
                            {segs.into_iter().map(|(class, from, to, what)| {
                                let left = pos(from);
                                let width = (pos(to) - left).max(0.4);
                                view! {
                                    <span class=format!("rb-seg {class}")
                                        style=format!("left: {left:.2}%; width: {width:.2}%")
                                        title=what></span>
                                }
                            }).collect_view()}
                            {off.map(|(left, width, d)| view! {
                                <span class="rb-off-band" style=format!("left: {left:.2}%; width: {width:.2}%")
                                    aria-hidden="true" data-off=duration(d)></span>
                            })}
                            <span class="rb-boot" style=format!("left: {boot_left:.2}%")
                                title=format!("System started at {}", clock_s(boot_at))></span>
                        </div>
                        {a.killed.then(|| view! {
                            <span class="rb-killed small" title=format!("Did not stop within its {} s stop timeout", a.stop_timeout)>
                                <span aria-hidden="true">"⚠ "</span>"killed"
                            </span>
                        })}
                    </div>
                }).collect_view()}
            </div>
            <div class="rb-axis small muted">
                <span>{clock_s(start)}</span>
                <span>{format!("{} in all", duration(end - start))}</span>
                <span>{clock_s(end)}</span>
            </div>
            <ul class="rb-facts small">
                {facts.into_iter().map(|f| view! { <li>{f}</li> }).collect_view()}
            </ul>
            {(!r.apps.is_empty()).then(|| view! {
                <table class="table rb-table small">
                    <thead><tr>
                        <th>"App"</th>
                        <th class="num">"Stopped in"</th>
                        <th class="num">"Down"</th>
                        <th class="num">"Back after the start"</th>
                    </tr></thead>
                    <tbody>{table}</tbody>
                </table>
            })}
        </div>
    }
}

/// A node's last reboots, each leading to its entry and timeline.
#[component]
pub fn PastReboots(#[prop(into)] node: Signal<String>) -> impl IntoView {
    let list = polled(Some(Duration::from_secs(120)), move || {
        let node = node.get();
        async move {
            api::get::<EventsPage>(&format!("/api/events?node={node}&kind=reboot&limit=5")).await
        }
    });
    move || {
        let events = list.data.get()?.events;
        if events.is_empty() {
            return None;
        }
        let node = node.get_untracked();
        Some(view! {
            <div class="rb-past">
                <h4>"Past reboots"</h4>
                <ul class="small">
                    {events.into_iter().map(|e| view! {
                        <li>
                            <A href=format!("/nodes/{node}?tab=reboots&event={}", e.id)>
                                <Ago at=e.at/>
                            </A>
                            {format!(": {}", e.summary.trim_start_matches("Rebooted: "))}
                        </li>
                    }).collect_view()}
                </ul>
            </div>
        })
    }
}

/// A node's Reboots tab: one row per reboot (when, how long the machine was
/// off, the app down the longest, what went wrong), each opening to its
/// timeline: every app's stop, downtime and return. `?event=` opens one.
#[component]
pub fn RebootsTab(#[prop(into)] node: Signal<String>) -> impl IntoView {
    const PAGE: u32 = 20;
    let query = use_query_map();
    let wanted = query
        .read_untracked()
        .get("event")
        .and_then(|e| e.parse::<i64>().ok());
    let shown = RwSignal::new(PAGE);
    let list = polled(Some(Duration::from_secs(60)), move || {
        let (node, limit) = (node.get(), shown.get());
        async move {
            api::get::<EventsPage>(&format!(
                "/api/events?node={node}&kind=reboot&limit={limit}"
            ))
            .await
        }
    });
    let events = move || list.data.get().map(|p| p.events).unwrap_or_default();
    let more = move || list.data.with(|p| p.as_ref().is_some_and(|p| p.more));
    view! {
        <ErrorNote error=list.error/>
        <Show when=move || list.loaded() fallback=Loading>
            <Show when=move || list.data.with(|p| p.as_ref().is_some_and(|p| !p.events.is_empty()))
                fallback=|| view! {
                    <div class="empty"><p>
                        "No reboot recorded yet. After the next one (asked by façade or not), each shows here: "
                        "when each app stopped and whether it had to be killed, how long the machine was off, "
                        "and when each app ran and passed its health check again."
                    </p></div>
                }>
                <div class="table-wrap">
                    <table class="table rb-list">
                        <thead><tr>
                            <th>"Rebooted"</th>
                            <th class="num">"Off"</th>
                            <th class="num">"Longest down"</th>
                            <th>"Apps"</th>
                            <th>"Problems"</th>
                        </tr></thead>
                        <For each=events key=|e| (e.id, e.summary.clone()) let:e>
                            {
                                let open = wanted == Some(e.id);
                                view! { <RebootRow e open node=node.get_untracked()/> }
                            }
                        </For>
                    </table>
                </div>
                <Show when=more>
                    <div class="table-more">
                        <button class="button" on:click=move |_| shown.update(|s| *s += PAGE)>"Show more"</button>
                    </div>
                </Show>
            </Show>
        </Show>
    }
}

/// One reboot: its figures on a line, and its timeline under it when open.
#[component]
fn RebootRow(e: EventView, open: bool, node: String) -> impl IntoView {
    let open = RwSignal::new(open);
    let id = e.id;
    let report = e.details.reboot.clone();
    let cells = match &report {
        None => view! {
            <td colspan="4" class="muted small">
                "No report: façade measures reboots since November 2026."
            </td>
        }
        .into_any(),
        Some(r) => {
            let off = r.off_for().map(duration).unwrap_or_else(|| "—".into());
            let longest = match r.longest_down() {
                Some((a, d)) => view! {
                    {duration(d)}
                    <span class="muted">{format!(" ({})", a.name)}</span>
                }
                .into_any(),
                None => "—".into_any(),
            };
            let n = r.apps.len();
            let not_back = r.not_back();
            let apps = match (n, not_back.len()) {
                (0, _) => "none ran there".to_string(),
                (1, 0) => format!("{} back", r.apps[0].name),
                (n, 0) => format!("all {n} back"),
                (n, missing) => format!("{} of {n} back", n - missing),
            };
            let mut problems = vec![];
            if !r.clean {
                problems.push(("badge badge-critical", "not shut down cleanly".to_string(),
                    "No shutdown in Docker's journal: the machine went down under the apps (a crash or power cut?)".to_string()));
            }
            if !not_back.is_empty() {
                let names: Vec<_> = not_back.iter().map(|a| a.name.as_str()).collect();
                problems.push((
                    "badge badge-critical",
                    format!("{} not back", not_back.len()),
                    format!("Not running again: {}", names.join(", ")),
                ));
            }
            let killed = r.killed();
            if !killed.is_empty() {
                let names: Vec<_> = killed.iter().map(|a| a.name.as_str()).collect();
                problems.push((
                    "badge badge-warn",
                    format!("{} killed", killed.len()),
                    format!(
                        "Did not stop within their stop timeout: {}",
                        names.join(", ")
                    ),
                ));
            }
            let problems = if problems.is_empty() {
                view! { <span class="muted">"—"</span> }.into_any()
            } else {
                problems
                    .into_iter()
                    .map(|(class, label, tip)| view! { <span class=class title=tip>{label}</span> " " })
                    .collect_view()
                    .into_any()
            };
            view! {
                <td class="num">{off}</td>
                <td class="num">{longest}</td>
                <td>{apps}</td>
                <td>{problems}</td>
            }
            .into_any()
        }
    };
    let entry = format!("/nodes/{node}?tab=activity&event={id}");
    let target = open.get_untracked();
    let row = NodeRef::<leptos::html::Tr>::new();
    if target {
        row.on_load(|el| {
            let el = el.clone();
            request_animation_frame(move || el.scroll_into_view());
        });
    }
    view! {
        <tbody class=if target { "rb-group rb-target" } else { "rb-group" }>
            <tr class="rb-line" node_ref=row on:click=move |ev| {
                // A link in the row goes where it says.
                let on_link = ev
                    .target()
                    .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
                    .is_some_and(|t| t.closest("a").ok().flatten().is_some());
                if !on_link {
                    open.update(|o| *o = !*o);
                }
            }>
                <td>
                    <button type="button" class="rb-toggle" aria-expanded=move || open.get().to_string()
                        title="Show each app's stop, downtime and return">
                        <span class="rb-chevron" aria-hidden="true">"▸"</span>
                        <Ago at=e.at/>
                    </button>
                </td>
                {cells}
            </tr>
            <Show when=move || open.get()>
                <tr class="rb-detail">
                    <td colspan="5">
                        {report.clone().map(|report| view! { <RebootTimeline report/> })}
                        <p class="small muted">
                            <A href=entry.clone()>"Its Activity entry"</A>
                        </p>
                    </td>
                </tr>
            </Show>
        </tbody>
    }
}
