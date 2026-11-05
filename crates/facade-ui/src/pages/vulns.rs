//! A node's Vulnerabilities tab: what affects what it runs, worst first.

use std::time::Duration;

use facade_core::vulns::{AckRequest, Finding, FindingStatus, NodeVulns, VulnSeverity};
use leptos::prelude::*;

use crate::api;
use crate::components::{ErrorNote, Loading, Modal};
use crate::util::{ago, me, polled, run};

#[component]
pub fn VulnsTab(node: String) -> impl IntoView {
    let me = me();
    let name = StoredValue::new(node.clone());
    let data = polled(Some(Duration::from_secs(60)), move || {
        let node = name.get_value();
        async move { api::get::<NodeVulns>(&format!("/api/nodes/{node}/vulns")).await }
    });
    let all = RwSignal::new(false);
    let rows = Memo::new(move |_| {
        let mut rows = data
            .data
            .with(|d| d.as_ref().map(|d| d.findings.clone()).unwrap_or_default());
        if !all.get() {
            rows.retain(|f| f.severity.at_least(VulnSeverity::High));
        }
        rows
    });
    let hidden = move || {
        data.data.with(|d| {
            d.as_ref().map_or(0, |d| {
                d.findings
                    .iter()
                    .filter(|f| !f.severity.at_least(VulnSeverity::High))
                    .count()
            })
        })
    };

    // Acknowledging: which CVE, and why.
    let ack_open = RwSignal::new(false);
    let ack_cve = RwSignal::new(String::new());
    let ack_note = RwSignal::new(String::new());
    let save_ack = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let cve = ack_cve.get_untracked();
        let body = AckRequest {
            node: Some(name.get_value()),
            note: ack_note.get_untracked(),
        };
        run(
            format!("{cve} acknowledged"),
            async move {
                api::post::<_, usize>(&format!("/api/vulns/{cve}/ack"), &body)
                    .await
                    .map(|_| ())
            },
            move || {
                ack_open.set(false);
                data.reload();
            },
        );
    };
    let unack = move |cve: String| {
        let node = name.get_value();
        run(
            format!("{cve} alerts again"),
            async move { api::delete(&format!("/api/vulns/{cve}/ack?node={node}")).await },
            move || data.reload(),
        );
    };

    let header = move || {
        data.data.with(|d| {
            let d = d.as_ref()?;
            let s = &d.summary;
            let mut line = format!(
                "{} critical, {} high, {} medium or low.",
                s.critical, s.high, s.other
            );
            if let Some(src) = &d.source {
                line.push_str(&format!(" Matched against {src}"));
                if let Some(at) = s.data_as_of {
                    line.push_str(&format!(" (published {})", ago(at)));
                }
                line.push('.');
            }
            Some(line)
        })
    };
    let unknown = move || data.data.with(|d| d.as_ref()?.summary.unknown.clone());

    view! {
        <div class="toolbar">
            <span class="muted small">{header}</span>
            <label class="push-right small">
                <input type="checkbox" bind:checked=all/>
                {move || format!(" Medium and low too ({})", hidden())}
            </label>
        </div>
        <ErrorNote error=data.error/>
        {move || unknown().map(|why| view! { <div class="note">{format!("Not checked: {why}.")}</div> })}
        <Show when=move || data.loaded() fallback=Loading>
            <Show when=move || rows.with(|r| !r.is_empty())
                fallback=move || view! {
                    <div class="empty"><p>{move || if unknown().is_some() {
                        "No findings to show."
                    } else if all.get() {
                        "Nothing known affects what this node runs."
                    } else {
                        "No high or critical vulnerability in what this node runs."
                    }}</p></div>
                }>
                <div class="table-wrap">
                    <table class="table">
                        <thead>
                            <tr>
                                <th>"Severity"</th>
                                <th>"CVE"</th>
                                <th>"Package"</th>
                                <th>"Status"</th>
                                <th>"Found"</th>
                                <th>"What"</th>
                                <Show when=move || me.is_admin()><th></th></Show>
                            </tr>
                        </thead>
                        <tbody>
                            <For each=move || rows.get()
                                key=|f| (f.cve.clone(), f.package.clone(), f.status, f.ack.is_some())
                                let:f>
                                <Row f=f.clone() admin=me.is_admin()
                                    on_ack=move |cve: String| {
                                        ack_cve.set(cve);
                                        ack_note.set(String::new());
                                        ack_open.set(true);
                                    }
                                    on_unack=unack/>
                            </For>
                        </tbody>
                    </table>
                </div>
            </Show>
        </Show>
        <Modal open=ack_open title="Acknowledge a vulnerability">
            <form class="form" on:submit=save_ack>
                <p class="muted small">
                    <strong>{move || ack_cve.get()}</strong>
                    " stops alerting on this node until its status changes: a fix comes out, or a reboot applies it."
                </p>
                <label class="field">
                    <span>"Why"</span>
                    <input type="text" bind:value=ack_note placeholder="mptcp disabled" autocomplete="off"/>
                    <small>"What mitigates it, for whoever looks next."</small>
                </label>
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| ack_open.set(false)>"Cancel"</button>
                    <button type="submit" class="button button-primary">"Acknowledge"</button>
                </div>
            </form>
        </Modal>
    }
}

#[component]
fn Row(
    f: Finding,
    admin: bool,
    on_ack: impl Fn(String) + Send + Sync + Copy + 'static,
    on_unack: impl Fn(String) + Send + Sync + Copy + 'static,
) -> impl IntoView {
    let class = match f.severity {
        VulnSeverity::Critical => "badge badge-critical",
        VulnSeverity::High => "badge badge-warn",
        _ => "badge badge-neutral",
    };
    let status = match (f.status, &f.fixed) {
        (FindingStatus::FixAvailable, Some(v)) => format!("fix available ({v})"),
        (FindingStatus::FixedAfterReboot, Some(v)) => format!("fixed after reboot ({v})"),
        (s, _) => s.label().to_string(),
    };
    let ack = f.ack.clone().map(|a| {
        let note = if a.note.is_empty() {
            String::new()
        } else {
            format!(": {}", a.note)
        };
        format!("acknowledged by {}{note}", a.by)
    });
    let cve = f.cve.clone();
    let acked = f.ack.is_some();
    let action = admin.then(|| {
        let cve = cve.clone();
        view! {
            <td class="num">
                {if acked {
                    view! { <button class="button button-small" on:click=move |_| on_unack(cve.clone())>"Unacknowledge"</button> }.into_any()
                } else if f.status == FindingStatus::NoFix {
                    view! { <button class="button button-small" on:click=move |_| on_ack(cve.clone())>"Acknowledge…"</button> }.into_any()
                } else {
                    ().into_any()
                }}
            </td>
        }
    });
    view! {
        <tr>
            <td><span class=class>{f.severity.as_str()}</span></td>
            <td><a href=f.url.clone() target="_blank" rel="noopener">{f.cve.clone()}</a></td>
            <td>{f.package.clone()}<div class="muted small">{f.installed.clone()}</div></td>
            <td>{status}{ack.map(|a| view! { <div class="muted small">{a}</div> })}</td>
            <td class="muted small">{ago(f.first_seen)}</td>
            <td class="small">{f.title.clone()}</td>
            {action}
        </tr>
    }
}
