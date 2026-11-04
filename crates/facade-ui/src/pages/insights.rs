//! What visitors see of an app: its availability (the header's figure and the
//! 30-day bar it opens), whether its domains answer, its visitors and
//! bandwidth, and its failed requests.

use std::time::Duration;

use facade_core::insights::{
    AppInsights, Bandwidth, DayUptime, ErrorPeriod, ErrorRow, ErrorsView, NodeInsights, Reach,
    Uptime, civil, day_of,
};
use leptos::prelude::*;

use crate::api;
use crate::components::{Sort, SortKey, SortTh};
use crate::util::{Polled, ago, bytes, clock, now, polled};

/// An app's insights, polled every minute.
pub fn app_insights(name: Memo<String>) -> Polled<AppInsights> {
    polled(Some(Duration::from_secs(60)), move || {
        let name = name.get();
        async move { api::get::<AppInsights>(&format!("/api/apps/{name}/insights")).await }
    })
}

/// `99.95 %`, with the digits that tell apart what matters near 100 %.
pub fn percent_text(p: f64) -> String {
    if p >= 100.0 {
        "100 %".into()
    } else if p >= 99.995 {
        "99.99 %".into()
    } else if p >= 99.0 {
        format!("{p:.2} %")
    } else {
        format!("{p:.1} %")
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A UTC day as `Nov 4`.
fn day_label(day: u64) -> String {
    let (_, m, d) = civil(day);
    format!("{} {d}", MONTHS[(m - 1) as usize])
}

fn month_label(day: u64) -> &'static str {
    MONTHS[(civil(day).1 - 1) as usize]
}

/// Minutes of a day as `14:02`.
fn minute_label(m: u16) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

fn duration_label(minutes: u32) -> String {
    match minutes {
        0..60 => format!("{minutes} min"),
        _ if minutes.is_multiple_of(60) => format!("{} h", minutes / 60),
        _ => format!("{} h {} min", minutes / 60, minutes % 60),
    }
}

// ---------------------------------------------------------------- availability

/// The header's figure: `up 99.95 % · 30 days`, opening the day bar.
#[component]
pub fn AvailabilityFigure(insights: Polled<AppInsights>, open: RwSignal<bool>) -> impl IntoView {
    let figure = move || {
        insights.data.with(|i| {
            let a = &i.as_ref()?.availability;
            Some((a.percent?, a.days.len()))
        })
    };
    move || {
        figure().map(|(percent, days)| {
            view! {
                " · "
                <button type="button" class="link-button uptime-figure"
                    aria-expanded=move || open.get().to_string()
                    title="Availability: the share of minutes it was up, among those it was up or down"
                    on:click=move |_| open.update(|o| *o = !*o)>
                    {format!("up {} · {days} days", percent_text(percent))}
                </button>
            }
        })
    }
}

/// One day's segment: up (green), with where it was down (red) and stopped or
/// unknown (grey) cut out, by the minute.
fn day_segment(d: &DayUptime, today: bool) -> impl IntoView + use<> {
    let recorded = d.up + d.down + d.stopped + d.unknown;
    let pct = |m: u32| f64::from(m) * 100.0 / 1440.0;
    let mut tip = day_label(d.day);
    match d.percent() {
        Some(p) => tip += &format!(": up {}", percent_text(p)),
        None if d.stopped > 0 => tip += ": stopped",
        None => tip += ": no data",
    }
    let downs: Vec<String> = d
        .slices
        .iter()
        .filter(|s| s.state == Uptime::Down)
        .take(6)
        .map(|s| format!("{}–{}", minute_label(s.start), minute_label(s.end)))
        .collect();
    if d.down > 0 {
        tip += &format!(
            " · down {} ({} UTC)",
            duration_label(d.down),
            downs.join(", ")
        );
    }
    if d.stopped > 0 && d.percent().is_some() {
        tip += &format!(" · stopped {}", duration_label(d.stopped));
    }
    if today {
        tip += " · today";
    }
    let slices = d
        .slices
        .iter()
        .map(|s| {
            let class = match s.state {
                Uptime::Down => "uptime-cut uptime-down",
                _ => "uptime-cut uptime-none",
            };
            let start = pct(u32::from(s.start));
            let width = pct(u32::from(s.end - s.start)).max(0.7);
            view! { <span class=class style=format!("left: {start:.2}%; width: {width:.2}%")></span> }
        })
        .collect_view();
    view! {
        <span class="uptime-day" title=tip.clone() aria-label=tip>
            <span class="uptime-up" style=format!("width: {:.2}%", pct(recorded))></span>
            {slices}
        </span>
    }
}

/// The thin bar under the header: one segment per day over 30 days.
#[component]
pub fn AvailabilityBar(insights: Polled<AppInsights>) -> impl IntoView {
    let days = move || {
        insights
            .data
            .with(|i| i.as_ref().map(|i| i.availability.clone()))
    };
    move || {
        days().map(|a| {
            let today = day_of(now());
            let how = match &a.domain {
                Some(d) => format!(
                    "Seen from outside: down when {d} does not answer, or the app has crashed or failed."
                ),
                None => "From its container: down when crashed or failed.".to_string(),
            };
            let first = a.days.first().map(|d| day_label(d.day)).unwrap_or_default();
            view! {
                <section class="uptime" aria-label="Availability over 30 days">
                    <div class="uptime-bar">
                        {a.days.iter().map(|d| day_segment(d, d.day == today)).collect_view()}
                    </div>
                    <div class="uptime-axis">
                        <span>{first}</span>
                        <span class="uptime-key">
                            <span class="uptime-swatch uptime-up"></span>"up"
                            <span class="uptime-swatch uptime-down"></span>"down"
                            <span class="uptime-swatch uptime-none"></span>"stopped or no data"
                        </span>
                        <span>"today"</span>
                    </div>
                    <p class="muted small">{how}" Stopped on purpose is not counted."</p>
                </section>
            }
        })
    }
}

// ---------------------------------------------------------------- reachability

/// `reachable · 180 ms`, or `unreachable since 14:02` in red; nothing until
/// checked.
#[component]
pub fn ReachBadge(reach: Option<Reach>) -> impl IntoView {
    reach.map(|r| {
        if r.ok {
            let label = match r.ms {
                Some(ms) => format!("reachable · {ms} ms"),
                None => "reachable".into(),
            };
            let title = format!(
                "Loaded from the server as a visitor would, {}{}",
                ago(r.checked_at),
                r.status.map(|s| format!(" (HTTP {s})")).unwrap_or_default()
            );
            view! {
                <span class="badge badge-good reach-badge" title=title>
                    <span class="badge-icon" aria-hidden="true">"●"</span>{label}
                </span>
            }
            .into_any()
        } else {
            let label = match r.down_since {
                Some(t) if now().saturating_sub(t) < 86_400 => {
                    format!("unreachable since {}", clock(t))
                }
                Some(t) => format!("unreachable since {}", crate::util::date(t)),
                None => "unreachable".into(),
            };
            let title = format!(
                "{} (checked {})",
                r.error.clone().unwrap_or_else(|| "no answer".into()),
                ago(r.checked_at)
            );
            view! {
                <span class="badge badge-critical reach-badge" title=title>
                    <span class="badge-icon" aria-hidden="true">"✕"</span>{label}
                </span>
            }
            .into_any()
        }
    })
}

// ---------------------------------------------------------------- visitors & bandwidth

/// The Overview's tile: today's visitors, yesterday's under them.
#[component]
pub fn VisitorsTile(insights: Polled<AppInsights>) -> impl IntoView {
    let counts = move || {
        insights.data.with(|i| {
            let i = i.as_ref().filter(|i| i.routed)?;
            let today = day_of(now());
            Some((
                i.visitors_on(today).unwrap_or(0),
                i.visitors_on(today.saturating_sub(1)).unwrap_or(0),
            ))
        })
    };
    move || {
        counts().map(|(today, yesterday)| {
            view! {
                <div class="tile" title="Distinct visitors (address and browser) today, UTC; an estimate within about 2 %">
                    <span class="tile-label">"Visitors"</span>
                    <span class="tile-value">{today}</span>
                    <span class="tile-sub">{format!("{yesterday} yesterday")}</span>
                </div>
            }
        })
    }
}

/// `4.2 GB served this month · 120 MB received`.
pub fn bandwidth_line(b: &Bandwidth) -> String {
    format!(
        "{} served this month · {} received",
        bytes(b.month_out),
        bytes(b.month_in)
    )
}

/// The Info tab's Traffic section: bandwidth and 30 days of visitors.
#[component]
pub fn TrafficSection(insights: Polled<AppInsights>) -> impl IntoView {
    let data = move || insights.data.get().filter(|i| i.routed);
    let hovered = RwSignal::new(None::<usize>);
    move || {
        data().map(|i| {
            let b = i.bandwidth.clone();
            let days = i.visitors.clone();
            let max = days.iter().map(|d| d.visitors).max().unwrap_or(0);
            // Days are counted apart (a visitor of two days counts on each):
            // an average says what a sum would overstate.
            let counted: Vec<u64> = days.iter().map(|d| d.visitors).filter(|v| *v > 0).collect();
            let average = (counted.iter().sum::<u64>() as f64 / counted.len().max(1) as f64).round();
            let today = day_of(now());
            let head = {
                let days = days.clone();
                move || match hovered.get().and_then(|h| days.get(h)) {
                    Some(d) => format!("{} · {} visitors", day_label(d.day), d.visitors),
                    None => format!(
                        "today {} · yesterday {}",
                        i.visitors_on(today).unwrap_or(0),
                        i.visitors_on(today.saturating_sub(1)).unwrap_or(0)
                    ),
                }
            };
            let last_month = (b.last_month_out > 0).then(|| {
                format!(
                    " · {} in {}",
                    bytes(b.last_month_out),
                    month_label(b.month_start.saturating_sub(1))
                )
            });
            let bars = days
                .iter()
                .enumerate()
                .map(|(n, d)| {
                    let height = if max == 0 {
                        0.0
                    } else {
                        d.visitors as f64 * 100.0 / max as f64
                    };
                    let label = format!("{}: {} visitors", day_label(d.day), d.visitors);
                    view! {
                        <span class="vbar-slot" aria-label=label
                            on:mouseenter=move |_| hovered.set(Some(n))>
                            <span class="vbar" class:vbar-today=d.day == today
                                class:vbar-on=move || hovered.get() == Some(n)
                                style=format!("height: {height:.1}%")></span>
                        </span>
                    }
                })
                .collect_view();
            let first = days.first().map(|d| day_label(d.day)).unwrap_or_default();
            view! {
                <section class="card details">
                    <h2 class="section-title">"Traffic"</h2>
                    <dl class="kv">
                        <dt>"Bandwidth"</dt>
                        <dd title="Bytes through the entry point, as on the wire (TLS included): what a provider's traffic quota counts">
                            {bandwidth_line(&b)}
                            {last_month.map(|l| view! { <span class="muted small">{l}</span> })}
                        </dd>
                        <dt>"Visitors"</dt>
                        <dd>{format!("{average} a day on average")}<span class="muted small">" · distinct address and browser per day (UTC), within about 2 %"</span></dd>
                    </dl>
                    <figure class="vchart">
                        <div class="spark-head">
                            <span class="spark-title">"Visitors per day"</span>
                            <span class="spark-now">{head}</span>
                        </div>
                        <div class="vchart-plot" on:mouseleave=move |_| hovered.set(None)>
                            <span class="spark-scale">{max}</span>
                            <div class="vbars">{bars}</div>
                        </div>
                        <div class="spark-axis"><span>{first}</span><span>"today"</span></div>
                    </figure>
                </section>
            }
        })
    }
}

/// The node page's figure: what it served this calendar month.
#[component]
pub fn NodeMonth(name: String) -> impl IntoView {
    let data = polled(Some(Duration::from_secs(60)), move || {
        let name = name.clone();
        async move { api::get::<NodeInsights>(&format!("/api/nodes/{name}/insights")).await }
    });
    move || {
        data.data.get().map(|i| {
            let b = i.bandwidth;
            view! {
                <div class="metric-sub">"This month"</div>
                <div class="metric-month" title="Served to visitors through the entry point since the 1st (UTC), TLS included: what a provider's traffic quota counts">
                    <strong>{format!("{} served", bytes(b.month_out))}</strong>
                    <span class="muted">{format!(" · {} received", bytes(b.month_in))}</span>
                </div>
            }
        })
    }
}

// ---------------------------------------------------------------- failed requests

#[derive(Clone, Copy, PartialEq)]
enum Class {
    All,
    Client,
    Server,
}

/// Columns: status, method, path, count, last seen.
fn error_key(col: usize, r: &ErrorRow) -> SortKey {
    match col {
        0 => SortKey::num(Some(r.status)),
        1 => SortKey::text(&r.method),
        2 => SortKey::text(&r.path),
        3 => SortKey::num(Some(r.count as f64)),
        _ => SortKey::num(Some(r.last_seen as f64)),
    }
}

/// Rows shown before "Show all".
const FEW: usize = 10;

/// The Overview's failed requests: grouped by status, method and path,
/// sortable, filtered by class and path, over a period. Not there while the
/// app has none.
#[component]
pub fn ErrorsSection(name: Memo<String>) -> impl IntoView {
    let period = RwSignal::new(ErrorPeriod::Day);
    let class = RwSignal::new(Class::All);
    let filter = RwSignal::new(String::new());
    let all = RwSignal::new(false);
    let sort = Sort::new();
    let errors = polled(Some(Duration::from_secs(30)), move || {
        let (name, period) = (name.get(), period.get());
        async move {
            api::get::<ErrorsView>(&format!("/api/apps/{name}/errors?period={}", period.key()))
                .await
        }
    });
    // The data of the period on screen only.
    let view = move || errors.data.get().filter(|v| v.period == period.get());
    let rows = Memo::new(move |_| {
        let needle = filter.get().trim().to_lowercase();
        let mut rows: Vec<ErrorRow> = view()
            .map(|v| v.rows)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| match class.get() {
                Class::All => true,
                Class::Client => (400..500).contains(&r.status),
                Class::Server => r.status >= 500,
            })
            .filter(|r| needle.is_empty() || r.path.to_lowercase().contains(&needle))
            .collect();
        sort.apply(&mut rows, error_key);
        rows
    });
    // Hidden without any failed request, unless another period was picked.
    let shown =
        move || view().is_some_and(|v| !v.rows.is_empty()) || period.get() != ErrorPeriod::Day;
    let total = move || rows.with(|r| r.iter().map(|r| r.count).sum::<u64>());
    view! {
        <Show when=shown>
            <section class="errors">
                <div class="charts-head">
                    <h2 class="section-title">"Failed requests"</h2>
                    <div class="segmented" role="radiogroup" aria-label="Period">
                        {ErrorPeriod::ALL.into_iter().map(|p| view! {
                            <button type="button" role="radio"
                                class=move || if period.get() == p { "segment active" } else { "segment" }
                                aria-checked=move || (period.get() == p).to_string()
                                on:click=move |_| period.set(p)>
                                {match p { ErrorPeriod::Hour => "1 h", ErrorPeriod::Day => "24 h", ErrorPeriod::Week => "7 d" }}
                            </button>
                        }).collect_view()}
                    </div>
                </div>
                <div class="toolbar">
                    <div class="segmented" role="radiogroup" aria-label="Status class">
                        {[(Class::All, "All"), (Class::Client, "4xx"), (Class::Server, "5xx")].into_iter().map(|(c, label)| view! {
                            <button type="button" role="radio"
                                class=move || if class.get() == c { "segment active" } else { "segment" }
                                aria-checked=move || (class.get() == c).to_string()
                                on:click=move |_| class.set(c)>{label}</button>
                        }).collect_view()}
                    </div>
                    <input type="search" class="table-filter" placeholder="Filter by path"
                        aria-label="Filter by path" bind:value=filter/>
                    <span class="muted small push-right">
                        {move || {
                            let n = rows.with(Vec::len);
                            format!("{} request{} · {n} path{}", total(), if total() == 1 { "" } else { "s" }, if n == 1 { "" } else { "s" })
                        }}
                    </span>
                </div>
                <Show when=move || rows.with(|r| !r.is_empty())
                    fallback=move || view! {
                        <div class="empty"><p>{move || match period.get() {
                            ErrorPeriod::Hour => "No failed requests over the last hour.",
                            ErrorPeriod::Day => "No failed requests over the last 24 hours.",
                            ErrorPeriod::Week => "No failed requests over the last 7 days.",
                        }}</p></div>
                    }>
                    <div class="table-wrap errors-table">
                        <table class="table">
                            <thead><tr>
                                <SortTh sort=sort col=0 label="Status"/>
                                <SortTh sort=sort col=1 label="Method"/>
                                <SortTh sort=sort col=2 label="Path"/>
                                <SortTh sort=sort col=3 label="Count" num=true desc=true/>
                                <SortTh sort=sort col=4 label="Last seen" num=true desc=true/>
                            </tr></thead>
                            <tbody>
                                <For each=move || {
                                        let rows = rows.get();
                                        let n = if all.get() { rows.len() } else { rows.len().min(FEW) };
                                        rows.into_iter().take(n).collect::<Vec<_>>()
                                    }
                                    key=|r| (r.status, r.method.clone(), r.path.clone(), r.count, r.last_seen)
                                    let:r>
                                    <tr>
                                        <td>
                                            <span class={if r.status >= 500 { "status-code status-5xx" } else { "status-code status-4xx" }}>
                                                {r.status}
                                            </span>
                                        </td>
                                        <td class="muted">{r.method.clone()}</td>
                                        <td class="error-path"><code>{r.path.clone()}</code></td>
                                        <td class="num">{r.count}</td>
                                        <td class="num muted" title=format!("first seen {}", ago(r.first_seen))>{ago(r.last_seen)}</td>
                                    </tr>
                                </For>
                            </tbody>
                        </table>
                    </div>
                    <Show when=move || { !all.get() && rows.with(Vec::len) > FEW }>
                        <button type="button" class="link-button table-more"
                            on:click=move |_| all.set(true)>
                            {move || format!("Show all {}", rows.with(Vec::len))}
                        </button>
                    </Show>
                    {move || view().filter(|v| v.more > 0).map(|v| view! {
                        <p class="muted small table-note">{format!("{} rarer paths are not listed.", v.more)}</p>
                    })}
                </Show>
            </section>
        </Show>
    }
}
