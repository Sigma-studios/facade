use facade_core::proto::AppState;
use leptos::prelude::*;

use crate::util::{copy_to_clipboard, toasts};

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
