use facade_core::api::UsagePoint;
use facade_core::proto::AppState;
use leptos::prelude::*;

use crate::util::{bytes, copy_to_clipboard, cpus, toasts};

/// App state as icon + label; the colour only reinforces it.
#[component]
pub fn StateBadge(#[prop(into)] state: Signal<AppState>) -> impl IntoView {
    let parts = move || match state.get() {
        AppState::Running => ("good", "●", "running"),
        AppState::Building => ("busy", "◐", "building"),
        AppState::Pending => ("busy", "○", "pending"),
        AppState::Stopped => ("neutral", "■", "stopped"),
        AppState::Crashed => ("critical", "✕", "crashed"),
        AppState::Failed => ("critical", "✕", "failed"),
        AppState::Unknown => ("muted", "?", "unknown"),
    };
    view! {
        <span class=move || format!("badge badge-{}", parts().0)>
            <span class="badge-icon" aria-hidden="true">{move || parts().1}</span>
            {move || parts().2}
        </span>
    }
}

#[component]
pub fn OnlineBadge(#[prop(into)] online: Signal<bool>) -> impl IntoView {
    view! {
        <span class=move || if online.get() { "badge badge-good" } else { "badge badge-critical" }>
            <span class="badge-icon" aria-hidden="true">
                {move || if online.get() { "●" } else { "✕" }}
            </span>
            {move || if online.get() { "online" } else { "offline" }}
        </span>
    }
}

/// A usage bar: label, value text, and a fill that turns critical (with an icon)
/// from 90 %.
#[component]
pub fn Meter(
    label: &'static str,
    #[prop(into)] percent: Signal<f64>,
    #[prop(into)] detail: Signal<String>,
) -> impl IntoView {
    let high = move || percent.get() >= 90.0;
    view! {
        <div class="meter">
            <div class="meter-head">
                <span class="meter-label">{label}</span>
                <span class="meter-value">
                    <Show when=high>
                        <span class="meter-warn" title="above 90 %">"⚠ "</span>
                    </Show>
                    {move || detail.get()}
                </span>
            </div>
            <div class="meter-track" role="meter" aria-label=label
                aria-valuenow=move || format!("{:.0}", percent.get())
                aria-valuemin="0" aria-valuemax="100">
                <div
                    class=move || if high() { "meter-fill meter-fill-high" } else { "meter-fill" }
                    style=move || format!("width: {:.1}%", percent.get().clamp(0.0, 100.0))
                ></div>
            </div>
        </div>
    }
}

const TREND_W: f64 = 120.0;
const TREND_H: f64 = 28.0;

/// A small last-hour line beside a meter, on the meter's 0–100 % scale: points
/// are `(time, percent)`, drawn over `span`, unjoined across gaps over `gap`
/// seconds. Hovering shows `format(percent)` and the time.
#[component]
pub fn Trend(
    label: &'static str,
    #[prop(into)] points: Signal<Vec<(u64, f64)>>,
    #[prop(into)] span: Signal<(u64, u64)>,
    gap: u64,
    format: Callback<f64, String>,
) -> impl IntoView {
    let hover = RwSignal::new(None::<(u64, f64)>);
    let x_of = move |at: u64| {
        let (start, end) = span.get();
        let width = end.saturating_sub(start).max(1) as f64;
        (at.saturating_sub(start) as f64 / width * TREND_W).clamp(0.0, TREND_W)
    };
    let y_of = |v: f64| TREND_H - 1.5 - v.clamp(0.0, 100.0) / 100.0 * (TREND_H - 3.0);
    let line = move || {
        points.with(|p| {
            let mut d = String::new();
            let mut last: Option<u64> = None;
            for &(at, v) in p {
                let join = last.is_some_and(|l| at.saturating_sub(l) <= gap);
                d.push_str(&format!(
                    "{}{:.1},{:.1} ",
                    if join { "L" } else { "M" },
                    x_of(at),
                    y_of(v)
                ));
                last = Some(at);
            }
            d
        })
    };
    let on_move = move |ev: leptos::ev::MouseEvent| {
        use wasm_bindgen::JsCast;
        let Some(target) = ev.current_target() else {
            return;
        };
        let el: web_sys::Element = target.unchecked_into();
        let width = el.get_bounding_client_rect().width();
        if width <= 0.0 {
            return;
        }
        let x = (ev.offset_x() as f64 / width).clamp(0.0, 1.0) * TREND_W;
        hover.set(points.with(|p| {
            p.iter()
                .copied()
                .min_by(|a, b| (x_of(a.0) - x).abs().total_cmp(&(x_of(b.0) - x).abs()))
        }));
    };

    view! {
        <div class="trend" on:mousemove=on_move on:mouseleave=move |_| hover.set(None)>
            <svg viewBox=format!("0 0 {TREND_W} {TREND_H}") preserveAspectRatio="none"
                role="img" aria-label=format!("{label}, last hour")>
                <line class="trend-base" x1="0" x2=TREND_W y1=TREND_H y2=TREND_H
                    vector-effect="non-scaling-stroke"/>
                <path class="trend-line" d=line vector-effect="non-scaling-stroke"/>
                {move || hover.get().map(|(at, v)| {
                    let x = x_of(at);
                    view! {
                        <line class="spark-cross" x1=x x2=x y1="0" y2=TREND_H
                            vector-effect="non-scaling-stroke"/>
                        <circle class="spark-dot" cx=x cy=y_of(v) r="3"
                            vector-effect="non-scaling-stroke"/>
                    }
                })}
            </svg>
            {move || hover.get().map(|(at, v)| {
                let left = (x_of(at) / TREND_W * 100.0).clamp(20.0, 80.0);
                view! {
                    <div class="spark-tip" style=format!("left: {left:.1}%")>
                        <strong>{format.run(v)}</strong>
                        <span>{crate::util::clock(at)}</span>
                    </div>
                }
            })}
        </div>
    }
}

const SPARK_W: f64 = 600.0;
const SPARK_H: f64 = 120.0;

/// One point of a [`Sparkline`]: the average over its span and the peak.
#[derive(Clone, Copy, PartialEq)]
pub struct Sample {
    pub at: u64,
    pub value: f64,
    pub peak: f64,
}

/// One series over `span` (start, end), with a hover crosshair and tooltip. Points
/// further apart than `gap` seconds are not joined (the app was not running). A
/// faint line shows the peaks when they differ from the averages. `max` fixes the
/// top of the scale (a memory limit, a CPU limit); the data can raise it.
#[component]
pub fn Sparkline(
    title: &'static str,
    #[prop(into)] points: Signal<Vec<Sample>>,
    #[prop(into)] span: Signal<(u64, u64)>,
    #[prop(into)] gap: Signal<u64>,
    #[prop(into)] max: Signal<Option<f64>>,
    format: fn(f64) -> String,
) -> impl IntoView {
    let hover = RwSignal::new(None::<usize>);
    let scale = move || {
        let data_max = points.with(|p| {
            p.iter()
                .map(|s| s.peak.max(s.value))
                .fold(0.0_f64, f64::max)
        });
        max.get().unwrap_or(0.0).max(data_max).max(1e-9)
    };
    let x_of = move |at: u64| {
        let (start, end) = span.get();
        let width = end.saturating_sub(start).max(1) as f64;
        (at.saturating_sub(start) as f64 / width * SPARK_W).min(SPARK_W)
    };
    let y_of = move |v: f64| SPARK_H - (v / scale()) * (SPARK_H - 8.0) - 2.0;
    let has_peaks = move || points.with(|p| p.iter().any(|s| s.peak > s.value * 1.05 + 1e-9));
    // Runs of points close enough in time to be joined.
    let runs = move || {
        let gap = gap.get();
        points.with(|p| {
            let mut runs: Vec<Vec<Sample>> = Vec::new();
            for s in p {
                match runs.last_mut() {
                    Some(run) if run.last().is_some_and(|l| s.at.saturating_sub(l.at) <= gap) => {
                        run.push(*s)
                    }
                    _ => runs.push(vec![*s]),
                }
            }
            runs
        })
    };
    let path = move |pick: fn(&Sample) -> f64, close: bool| {
        let mut d = String::new();
        for run in runs() {
            for (i, s) in run.iter().enumerate() {
                let (x, y) = (x_of(s.at), y_of(pick(s)));
                d.push_str(&format!("{}{x:.1},{y:.1} ", if i == 0 { "M" } else { "L" }));
            }
            let (first, last) = (x_of(run[0].at), x_of(run[run.len() - 1].at));
            if close {
                d.push_str(&format!("L{last:.1},{SPARK_H} L{first:.1},{SPARK_H} Z "));
            } else if run.len() == 1 {
                // A lone point: a zero-length segment, drawn as a dot by the round cap.
                d.push_str("l0.01,0 ");
            }
        }
        d
    };
    let current = move || points.with(|p| p.last().map(|s| format(s.value)));
    let long = move || {
        let (start, end) = span.get();
        end.saturating_sub(start) >= 86400
    };
    let stamp = move |at: u64| {
        if long() {
            format!("{} {}", crate::util::day(at), crate::util::clock(at))
        } else {
            crate::util::clock(at)
        }
    };
    let on_move = move |ev: leptos::ev::MouseEvent| {
        use wasm_bindgen::JsCast;
        let Some(target) = ev.current_target() else {
            return;
        };
        let el: web_sys::Element = target.unchecked_into();
        let width = el.get_bounding_client_rect().width();
        if width <= 0.0 {
            return;
        }
        let x = (ev.offset_x() as f64 / width).clamp(0.0, 1.0) * SPARK_W;
        let nearest = points.with(|p| {
            p.iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| (x_of(a.at) - x).abs().total_cmp(&(x_of(b.at) - x).abs()))
                .map(|(i, _)| i)
        });
        hover.set(nearest);
    };
    let hovered = move || {
        let i = hover.get()?;
        points.with(|p| p.get(i).copied())
    };

    view! {
        <figure class="spark">
            <figcaption class="spark-head">
                <span class="spark-title">
                    {title}
                    <Show when=has_peaks>
                        <span class="spark-key">
                            <span class="spark-key-avg"></span>"average"
                            <span class="spark-key-peak"></span>"peak"
                        </span>
                    </Show>
                </span>
                <span class="spark-now">{move || current().unwrap_or_else(|| "—".into())}</span>
            </figcaption>
            <Show
                when=move || points.with(|p| !p.is_empty())
                fallback=|| view! { <div class="spark-empty">"No samples over this period yet"</div> }
            >
                <div class="spark-plot" on:mousemove=on_move on:mouseleave=move |_| hover.set(None)>
                    <svg viewBox=format!("0 0 {SPARK_W} {SPARK_H}") preserveAspectRatio="none"
                        role="img" aria-label=title>
                        <line class="spark-base" x1="0" x2=SPARK_W y1=SPARK_H y2=SPARK_H
                            vector-effect="non-scaling-stroke"/>
                        <path class="spark-area" d=move || path(|s| s.value, true)/>
                        <Show when=has_peaks>
                            <path class="spark-peak" d=move || path(|s| s.peak, false)
                                vector-effect="non-scaling-stroke"/>
                        </Show>
                        <path class="spark-line" d=move || path(|s| s.value, false)
                            vector-effect="non-scaling-stroke"/>
                        {move || hovered().map(|s| {
                            let (x, y) = (x_of(s.at), y_of(s.value));
                            view! {
                                <line class="spark-cross" x1=x x2=x y1="0" y2=SPARK_H
                                    vector-effect="non-scaling-stroke"/>
                                <circle class="spark-dot" cx=x cy=y r="4"
                                    vector-effect="non-scaling-stroke"/>
                            }
                        })}
                    </svg>
                    {move || hovered().map(|s| {
                        let left = (x_of(s.at) / SPARK_W * 100.0).clamp(12.0, 88.0);
                        let peak = (s.peak > s.value * 1.05 + 1e-9).then(|| format!("peak {}", format(s.peak)));
                        view! {
                            <div class="spark-tip" style=format!("left: {left:.1}%")>
                                <strong>{format(s.value)}</strong>
                                {peak.map(|p| view! { <span>{p}</span> })}
                                <span>{stamp(s.at)}</span>
                            </div>
                        }
                    })}
                    <div class="spark-scale">{move || format(scale())}</div>
                </div>
                <div class="spark-axis">
                    <span>{move || stamp(span.get().0)}</span>
                    <span>"now"</span>
                </div>
            </Show>
        </figure>
    }
}

/// The periods the usage charts cover.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Range {
    Hour,
    Day,
    Week,
    Month,
}

impl Range {
    const ALL: [Range; 4] = [Range::Hour, Range::Day, Range::Week, Range::Month];

    fn key(self) -> &'static str {
        match self {
            Range::Hour => "1h",
            Range::Day => "1d",
            Range::Week => "7d",
            Range::Month => "30d",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Range::Hour => "1 h",
            Range::Day => "24 h",
            Range::Week => "7 d",
            Range::Month => "30 d",
        }
    }

    fn secs(self) -> u64 {
        match self {
            Range::Hour => 3600,
            Range::Day => 86400,
            Range::Week => 7 * 86400,
            Range::Month => 30 * 86400,
        }
    }

    /// Seconds between points: the agents' reports, then the server's buckets.
    fn step(self) -> u64 {
        match self {
            Range::Hour => 15,
            Range::Day => 300,
            Range::Week => 1800,
            Range::Month => 7200,
        }
    }
}

/// CPU and memory (and disk, for a node) charts over a chosen period, from a
/// `…/usage` endpoint. The maxima fix the top of each scale (limits, totals).
#[component]
pub fn UsageCharts(
    #[prop(into)] url: Signal<String>,
    #[prop(into)] cpu_max: Signal<Option<f64>>,
    #[prop(into)] mem_max: Signal<Option<f64>>,
    #[prop(optional, into)] disk_max: Option<Signal<Option<f64>>>,
) -> impl IntoView {
    let range = RwSignal::new(Range::Hour);
    let usage = crate::util::polled(Some(std::time::Duration::from_secs(15)), move || {
        let url = url.get();
        let range = range.get();
        async move {
            crate::api::get::<Vec<UsagePoint>>(&format!("{url}?range={}", range.key()))
                .await
                .map(|points| (url, range, points))
        }
    });
    // Only the data of what is on screen: a slow answer for the previous range
    // (or app) must not draw over it.
    let series = move |f: fn(&UsagePoint) -> (f64, f64)| {
        Signal::derive(move || {
            usage.data.with(|u| match u {
                Some((u, r, points)) if *r == range.get() && *u == url.get() => points
                    .iter()
                    .map(|p| {
                        let (value, peak) = f(p);
                        Sample {
                            at: p.at,
                            value,
                            peak,
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            })
        })
    };
    let cpu = series(|p| (f64::from(p.cpu), f64::from(p.cpu_peak)));
    let mem = series(|p| (p.mem as f64, p.mem_peak as f64));
    let disk = series(|p| {
        let d = p.disk.unwrap_or(0) as f64;
        (d, d)
    });
    let span = Signal::derive(move || {
        let now = crate::util::now();
        (now.saturating_sub(range.get().secs()), now)
    });
    let gap = Signal::derive(move || range.get().step() * 5 / 2);

    view! {
        <div class="charts-head">
            <h2 class="section-title">"Usage"</h2>
            <div class="segmented" role="radiogroup" aria-label="Period">
                {Range::ALL.into_iter().map(|r| view! {
                    <button type="button" role="radio"
                        class=move || if range.get() == r { "segment active" } else { "segment" }
                        aria-checked=move || (range.get() == r).to_string()
                        on:click=move |_| range.set(r)>{r.label()}</button>
                }).collect_view()}
            </div>
        </div>
        <div class="charts">
            <Sparkline title="CPU" points=cpu span=span gap=gap max=cpu_max format=|v| format!("{v:.1} %")/>
            <Sparkline title="Memory" points=mem span=span gap=gap max=mem_max format=|v| bytes(v as u64)/>
            {disk_max.map(|disk_max| view! {
                <Sparkline title="Disk" points=disk span=span gap=gap max=disk_max format=|v| bytes(v as u64)/>
            })}
        </div>
    }
}

#[component]
pub fn Modal(
    #[prop(into)] open: RwSignal<bool>,
    title: &'static str,
    children: ChildrenFn,
) -> impl IntoView {
    let children = StoredValue::new(children);
    view! {
        <Show when=move || open.get()>
            <div class="modal-backdrop" on:click=move |_| open.set(false)>
                <div class="modal" role="dialog" aria-modal="true" aria-label=title
                    on:click=|ev| ev.stop_propagation()>
                    <div class="modal-head">
                        <h2>{title}</h2>
                        <button class="icon-button" aria-label="Close"
                            on:click=move |_| open.set(false)>"✕"</button>
                    </div>
                    {children.read_value()()}
                </div>
            </div>
        </Show>
    }
}

/// Read-only text with a copy button, for join commands and the like.
#[component]
pub fn CopyField(#[prop(into)] value: Signal<String>) -> impl IntoView {
    view! {
        <div class="copy-field">
            <code>{move || value.get()}</code>
            <button class="button" on:click=move |_| {
                copy_to_clipboard(&value.get());
                toasts().ok("Copied");
            }>"Copy"</button>
        </div>
    }
}

#[component]
pub fn ErrorNote(#[prop(into)] error: Signal<Option<String>>) -> impl IntoView {
    view! {
        <Show when=move || error.with(Option::is_some)>
            <div class="note note-error" role="alert">
                <span aria-hidden="true">"⚠ "</span>
                {move || error.get().unwrap_or_default()}
            </div>
        </Show>
    }
}

#[component]
pub fn Loading() -> impl IntoView {
    view! { <div class="loading">"Loading…"</div> }
}

const MB: u64 = 1024 * 1024;

/// What an [`AmountSlider`] chooses. Values stay absolute (MB, cores): a share
/// of the machine would drift when it is resized.
#[derive(Clone, Copy, PartialEq)]
pub enum Amount {
    /// Megabytes; the slider moves in percent of the total.
    Memory,
    /// Cores, in quarters.
    Cpus,
}

impl Amount {
    /// Slider `(min, max, step)` for a total.
    fn range(self, total: f64) -> (f64, f64, f64) {
        match self {
            Amount::Memory => (1.0, 100.0, 1.0),
            Amount::Cpus => (0.25, total.max(0.25), 0.25),
        }
    }

    fn to_slider(self, value: f64, total: f64) -> f64 {
        match self {
            Amount::Memory => value * 100.0 / total,
            Amount::Cpus => value,
        }
    }

    fn of_slider(self, slider: f64, total: f64) -> f64 {
        match self {
            Amount::Memory => (slider * total / 100.0).round(),
            Amount::Cpus => slider,
        }
    }

    /// The text the form stores and parses.
    fn format(self, value: f64) -> String {
        match self {
            Amount::Memory => (value.round().max(0.0) as u64).to_string(),
            Amount::Cpus => cpus(((value * 4.0).round() / 4.0).max(0.25)),
        }
    }

    pub fn describe(self, value: f64, total: f64) -> String {
        match self {
            Amount::Memory => format!(
                "{:.0} % · {} of {}",
                value * 100.0 / total,
                bytes(value as u64 * MB),
                bytes(total as u64 * MB)
            ),
            Amount::Cpus => format!("{} of {} cores", cpus(value), cpus(total)),
        }
    }

    fn left(self, value: f64) -> String {
        match self {
            Amount::Memory => format!("{} MB left in your share", value.max(0.0) as u64),
            Amount::Cpus => format!("{} CPUs left in your share", cpus(value.max(0.0))),
        }
    }
}

/// A memory or CPU amount as a slider over `total` (MB or cores) with a number
/// input kept in sync. `value` holds the number as text, empty for none. Without
/// a known total only the number input is shown.
#[component]
pub fn AmountSlider(
    label: &'static str,
    amount: Amount,
    value: RwSignal<String>,
    #[prop(into)] total: Signal<Option<f64>>,
    /// Offer a "none" checkbox labelled `none_label`; otherwise a value is required.
    #[prop(into)]
    optional: Signal<bool>,
    none_label: &'static str,
    /// What is left of a shared cap, marked on the slider.
    #[prop(into)]
    available: Signal<Option<f64>>,
) -> impl IntoView {
    let current = move || value.with(|v| v.trim().parse::<f64>().ok());
    let empty = move || value.with(|v| v.trim().is_empty());
    let (unit, number_min, number_step) = match amount {
        Amount::Memory => ("MB", "16", "1"),
        Amount::Cpus => ("CPUs", "0.25", "0.25"),
    };
    let over = move || match (current(), available.get()) {
        (Some(v), Some(a)) => v > a + 1e-9,
        _ => false,
    };
    let number = move || {
        view! {
            <input type="number" class="slider-number" min=number_min step=number_step
                bind:value=value required=move || !optional.get()
                placeholder=move || if optional.get() { "no limit" } else { "required" }/>
        }
    };
    view! {
        <div class="field">
            <span>{label}</span>
            {move || match total.get() {
                None => view! {
                    <div class="slider-row">{number()}<span class="muted small">{unit}</span></div>
                }.into_any(),
                Some(total) => {
                    let (min, max, step) = amount.range(total);
                    // A starting point when a limit is switched on: a quarter, or what is left.
                    let start = move || {
                        let quarter = amount.of_slider(amount.to_slider(total, total) / 4.0, total);
                        let start = available.get().map_or(quarter, |a| a.min(quarter));
                        amount.format(start.max(amount.of_slider(min, total)))
                    };
                    view! {
                        <Show when=move || optional.get()>
                            <label class="check slider-none">
                                <input type="checkbox" prop:checked=empty
                                    on:change=move |ev| value.set(if event_target_checked(&ev) { String::new() } else { start() })/>
                                {none_label}
                            </label>
                        </Show>
                        <Show when=move || !optional.get() || !empty()>
                            <div class="slider-row">
                                <div class="slider-track">
                                    <input type="range" min=min max=max step=step aria-label=label
                                        prop:value=move || amount.to_slider(current().unwrap_or(0.0), total).clamp(min, max).to_string()
                                        on:input=move |ev| {
                                            let slider = event_target_value(&ev).parse().unwrap_or(min);
                                            value.set(amount.format(amount.of_slider(slider, total)));
                                        }/>
                                    {move || available.get().map(|a| {
                                        let at = ((amount.to_slider(a, total) - min) / (max - min)).clamp(0.0, 1.0);
                                        view! {
                                            <span class="slider-mark" title="what is left in your share"
                                                style=format!("left: {:.1}%", at * 100.0)></span>
                                        }
                                    })}
                                </div>
                                {number()}
                                <span class="muted small">{unit}</span>
                            </div>
                            <small class=move || if over() { "slider-label over" } else { "slider-label" }>
                                {move || match current() {
                                    Some(v) => amount.describe(v, total),
                                    None => "Pick a value.".into(),
                                }}
                                {move || available.get().map(|a| format!(" · {}", amount.left(a)))}
                            </small>
                        </Show>
                    }.into_any()
                }
            }}
        </div>
    }
}
