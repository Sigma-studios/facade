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

const SPARK_W: f64 = 600.0;
const SPARK_H: f64 = 120.0;

/// One series over time, with a hover crosshair and tooltip. `max` fixes the top of
/// the scale (a memory limit, 100 % CPU); otherwise the data's maximum is used.
#[component]
pub fn Sparkline(
    title: &'static str,
    #[prop(into)] points: Signal<Vec<(u64, f64)>>,
    #[prop(into)] max: Signal<Option<f64>>,
    format: fn(f64) -> String,
) -> impl IntoView {
    let hover = RwSignal::new(None::<usize>);
    let scale = move || {
        let data_max = points.with(|p| p.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max));
        max.get().unwrap_or(0.0).max(data_max).max(1e-9)
    };
    let xy = move |i: usize, v: f64, n: usize| {
        let x = if n <= 1 {
            SPARK_W
        } else {
            i as f64 * SPARK_W / (n - 1) as f64
        };
        let y = SPARK_H - (v / scale()) * (SPARK_H - 8.0) - 2.0;
        (x, y)
    };
    let line = move || {
        points.with(|p| {
            let n = p.len();
            p.iter()
                .enumerate()
                .map(|(i, (_, v))| {
                    let (x, y) = xy(i, *v, n);
                    format!("{}{x:.1},{y:.1}", if i == 0 { "M" } else { "L" })
                })
                .collect::<String>()
        })
    };
    let area = move || {
        let l = line();
        if l.is_empty() {
            return String::new();
        }
        format!("{l} L{SPARK_W},{SPARK_H} L0,{SPARK_H} Z")
    };
    let current = move || points.with(|p| p.last().map(|(_, v)| format(*v)));
    let on_move = move |ev: leptos::ev::MouseEvent| {
        use wasm_bindgen::JsCast;
        let Some(target) = ev.current_target() else {
            return;
        };
        let el: web_sys::Element = target.unchecked_into();
        let width = el.get_bounding_client_rect().width();
        let n = points.with(Vec::len);
        if n == 0 || width <= 0.0 {
            return;
        }
        let fraction = (ev.offset_x() as f64 / width).clamp(0.0, 1.0);
        hover.set(Some(((fraction * (n - 1) as f64).round()) as usize));
    };
    let hovered = move || {
        let i = hover.get()?;
        points.with(|p| {
            let n = p.len();
            p.get(i).map(|(at, v)| {
                let (x, y) = xy(i, *v, n);
                (x, y, *at, *v)
            })
        })
    };

    view! {
        <figure class="spark">
            <figcaption class="spark-head">
                <span class="spark-title">{title}</span>
                <span class="spark-now">{move || current().unwrap_or_else(|| "—".into())}</span>
            </figcaption>
            <Show
                when=move || points.with(|p| p.len() > 1)
                fallback=|| view! { <div class="spark-empty">"Collecting samples…"</div> }
            >
                <div class="spark-plot" on:mousemove=on_move on:mouseleave=move |_| hover.set(None)>
                    <svg viewBox=format!("0 0 {SPARK_W} {SPARK_H}") preserveAspectRatio="none"
                        role="img" aria-label=title>
                        <line class="spark-base" x1="0" x2=SPARK_W y1=SPARK_H y2=SPARK_H
                            vector-effect="non-scaling-stroke"/>
                        <path class="spark-area" d=area/>
                        <path class="spark-line" d=line vector-effect="non-scaling-stroke"/>
                        {move || hovered().map(|(x, y, _, _)| view! {
                            <line class="spark-cross" x1=x x2=x y1="0" y2=SPARK_H
                                vector-effect="non-scaling-stroke"/>
                            <circle class="spark-dot" cx=x cy=y r="4"
                                vector-effect="non-scaling-stroke"/>
                        })}
                    </svg>
                    {move || hovered().map(|(x, _, at, v)| {
                        let left = (x / SPARK_W * 100.0).clamp(8.0, 92.0);
                        view! {
                            <div class="spark-tip" style=format!("left: {left:.1}%")>
                                <strong>{format(v)}</strong>
                                <span>{crate::util::clock(at)}</span>
                            </div>
                        }
                    })}
                    <div class="spark-scale">{move || format(scale())}</div>
                </div>
            </Show>
        </figure>
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
