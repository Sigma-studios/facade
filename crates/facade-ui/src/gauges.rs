//! Small pieces showing what agents measure beyond CPU and memory: a bar
//! against a limit, a strip of health checks, a volume's size trend, a node's
//! strain.

use facade_core::resources::{HealthCheck, Pressure, STRAIN_HELP, Strain};
use leptos::prelude::*;

use crate::util::{ago, bytes, clock};

/// Past this share of its limit a bar turns orange, then red.
const SERIOUS: f64 = 80.0;
const CRITICAL: f64 = 95.0;

/// A thin bar of `percent` of a limit, under a figure that says it: orange
/// above 80 %, red above 95 %, with an icon then so colour is not alone.
#[component]
pub fn LimitBar(label: &'static str, #[prop(into)] percent: Signal<f64>) -> impl IntoView {
    let level = move || {
        let p = percent.get();
        if p > CRITICAL {
            "limit-critical"
        } else if p > SERIOUS {
            "limit-serious"
        } else {
            ""
        }
    };
    let high = move || percent.get() > SERIOUS;
    view! {
        <div class=move || format!("limit-bar {}", level())>
            <div class="limit-track" role="meter" aria-label=label
                aria-valuenow=move || format!("{:.0}", percent.get())
                aria-valuemin="0" aria-valuemax="100">
                <div class="limit-fill" style=move || format!("width: {:.1}%", percent.get().clamp(0.0, 100.0))></div>
            </div>
            <Show when=high>
                <span class="limit-icon" title=move || format!("{:.0} % of its limit", percent.get())>"⚠"</span>
            </Show>
        </div>
    }
}

/// One square per recent health check, oldest first (green passed, red
/// failed, with a mark so colour is not alone), then how long the last took.
#[component]
pub fn HealthStrip(#[prop(into)] checks: Signal<Vec<HealthCheck>>) -> impl IntoView {
    let summary = move || {
        checks.with(|c| {
            let last = c.last()?;
            let failed = c.iter().filter(|c| !c.ok).count();
            let mut text = format!(
                "last {} {}, took {} ms",
                if last.ok { "passed" } else { "failed" },
                ago(last.at_ms / 1000),
                last.ms
            );
            if failed > 0 {
                text += &format!(" · {failed} of the last {} failed", c.len());
            }
            Some(text)
        })
    };
    view! {
        <Show when=move || checks.with(|c| !c.is_empty())>
            <div class="health-strip" role="list" aria-label="Recent health checks, oldest first">
                {move || checks.get().into_iter().map(|c| {
                    let mut tip = format!(
                        "{} · {} · {} ms",
                        clock(c.at_ms / 1000),
                        if c.ok { "passed" } else { "failed" },
                        c.ms
                    );
                    if !c.output.is_empty() {
                        tip += &format!("\n{}", c.output);
                    }
                    let class = if c.ok { "health-check health-ok" } else { "health-check health-failed" };
                    view! { <span class=class role="listitem" title=tip></span> }
                }).collect_view()}
            </div>
            <div class="muted small">{summary}</div>
        </Show>
    }
}

const TREND_W: f64 = 120.0;
const TREND_H: f64 = 24.0;

/// A volume's size over `span` (start, end) as a small line, from zero to its
/// largest size so growth is drawn to scale.
#[component]
pub fn VolumeTrend(points: Vec<(u64, u64)>, span: (u64, u64)) -> impl IntoView {
    let top = points.iter().map(|p| p.1).max().unwrap_or(0).max(1) as f64;
    let (start, end) = span;
    let width = end.saturating_sub(start).max(1) as f64;
    let x = |at: u64| (at.saturating_sub(start) as f64 / width * TREND_W).clamp(0.0, TREND_W);
    let y = |size: u64| TREND_H - 1.0 - size as f64 / top * (TREND_H - 2.0);
    let mut d: String = points
        .iter()
        .enumerate()
        .map(|(i, &(at, size))| {
            format!(
                "{}{:.1},{:.1} ",
                if i == 0 { "M" } else { "L" },
                x(at),
                y(size)
            )
        })
        .collect();
    // One measure so far: a short stroke ending where it is.
    if let [(at, size)] = points[..] {
        let (x1, y1) = (x(at), y(size));
        d = format!("M{:.1},{y1:.1} L{x1:.1},{y1:.1}", (x1 - 4.0).max(0.0));
    }
    let summary = match (points.first(), points.last()) {
        (Some(first), Some(last)) if points.len() > 1 => format!(
            "{} {} → {} now",
            bytes(first.1),
            ago(first.0),
            bytes(last.1)
        ),
        _ => "Measured once so far".into(),
    };
    let label = summary.clone();
    view! {
        <span class="volume-trend" title=label>
            <svg viewBox=format!("0 0 {TREND_W} {TREND_H}") preserveAspectRatio="none" role="img" aria-label=summary>
                <line class="trend-base" x1="0" x2=TREND_W y1=TREND_H y2=TREND_H vector-effect="non-scaling-stroke"/>
                <path class="trend-line" d=d vector-effect="non-scaling-stroke"/>
            </svg>
        </span>
    }
}

/// A node's strain, coloured, with how it is judged in its tooltip.
#[component]
pub fn StrainBadge(#[prop(into)] pressure: Signal<Option<Pressure>>) -> impl IntoView {
    let strain = move || pressure.get().and_then(|p| p.strain());
    let detail = move || {
        let p = pressure.get()?;
        let part = |name: &str, psi: Option<facade_core::resources::Psi>| {
            psi.map(|psi| format!("{name} {:.0} %", psi.avg60))
        };
        let parts: Vec<String> = [
            part("CPU", p.cpu),
            part("memory", p.memory),
            part("disk", p.io),
        ]
        .into_iter()
        .flatten()
        .collect();
        Some(format!("Last minute: {}.\n{STRAIN_HELP}", parts.join(", ")))
    };
    view! {
        {move || strain().map(|s| {
            let (class, icon) = match s {
                Strain::Low => ("badge badge-good", "●"),
                Strain::Moderate => ("badge badge-serious", "▲"),
                Strain::High => ("badge badge-critical", "✕"),
            };
            view! {
                <span class=class title=detail>
                    <span class="badge-icon" aria-hidden="true">{icon}</span>
                    {format!("strain: {s}")}
                </span>
            }
        })}
    }
}
