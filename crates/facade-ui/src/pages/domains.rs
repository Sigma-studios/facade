use std::time::Duration;

use facade_core::api::{DnsSettings, DnsUpdate, DomainStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;

use crate::api;
use crate::components::{ErrorNote, Loading};
use crate::util::{me, polled, run, toasts};

#[component]
pub fn Domains() -> impl IntoView {
    let me = me();
    let domains = polled(Some(Duration::from_secs(15)), || async {
        api::get::<Vec<DomainStatus>>("/api/domains").await
    });
    view! {
        <div class="page">
            <header class="page-head"><h1>"Domains"</h1></header>
            <p class="muted lead">
                "Each domain points straight at the node running its app, which serves it over HTTPS. "
                "Add domains in an app's settings."
            </p>
            <Show when=move || me.is_admin()>
                <DnsProvider on_change=move || domains.reload()/>
            </Show>
            <ErrorNote error=domains.error/>
            <Show when=move || domains.loaded() fallback=Loading>
                <DomainTable domains=Signal::derive(move || domains.data.get().unwrap_or_default()) show_app=true/>
            </Show>
        </div>
    }
}

/// Status rows, also used on an app's page.
#[component]
pub fn DomainTable(
    #[prop(into)] domains: Signal<Vec<DomainStatus>>,
    show_app: bool,
) -> impl IntoView {
    view! {
        <Show
            when=move || domains.with(|d| !d.is_empty())
            fallback=|| view! { <div class="empty"><p>"No domains yet."</p></div> }
        >
            <div class="domain-list">
                <For each=move || domains.get()
                    key=|d| (d.domain.clone(), d.dns_ok, d.certificate.clone(), d.dns_error.clone(), d.cert_error.clone(), d.resolved.clone())
                    let:d>
                    <DomainRow d=d show_app=show_app/>
                </For>
            </div>
        </Show>
    }
}

#[component]
fn DomainRow(d: DomainStatus, show_app: bool) -> impl IntoView {
    let ip = d.expected_ip.clone();
    let (dns_class, dns_icon, dns_label) = if d.dns_error.is_some() {
        ("critical", "✕", "DNS error")
    } else if d.dns_ok {
        ("good", "●", "DNS ok")
    } else if d.managed {
        ("busy", "◐", "DNS propagating")
    } else {
        ("critical", "✕", "DNS missing")
    };
    let (cert_class, cert_icon, cert_label) = if d.cert_error.is_some() {
        ("critical", "✕", "HTTPS failed")
    } else if d.certificate.is_some() {
        ("good", "●", "HTTPS")
    } else {
        ("busy", "◐", "HTTPS pending")
    };
    let dns_detail = match (&d.dns_error, d.dns_ok, d.managed, &ip) {
        (Some(e), ..) => e.clone(),
        (None, true, true, Some(ip)) => {
            format!("Points to {ip}. Record kept up to date by façade.")
        }
        (None, true, false, Some(ip)) => format!("Points to {ip}."),
        (None, true, _, None) => "Resolves.".into(),
        (None, false, true, Some(ip)) => {
            format!("façade set the record to {ip}; it can take a few minutes to be visible.")
        }
        (None, false, false, Some(ip)) => format!(
            "Create an A record for {} pointing to {ip} at your domain provider.{}",
            d.domain,
            if d.resolved.is_empty() {
                String::new()
            } else {
                format!(" It points to {} now.", d.resolved.join(", "))
            }
        ),
        (None, false, _, None) => format!(
            "Node {} has no known public IP: set it on the Nodes page.",
            d.node
        ),
    };
    let cert_detail = match (&d.cert_error, &d.certificate) {
        (Some(e), _) => e.clone(),
        (None, Some(name)) => {
            let until = d.cert_expires.map(crate::util::date).unwrap_or_default();
            if name.starts_with("*.") {
                format!("Wildcard certificate {name}, valid until {until}, renewed automatically.")
            } else {
                format!("Valid until {until}, renewed automatically.")
            }
        }
        (None, None) => "Requested once the DNS record points to the node.".into(),
    };
    let app = d.app.clone();
    view! {
        <article class="domain-row">
            <div class="domain-main">
                <a class="domain-name" href=format!("https://{}", d.domain) target="_blank" rel="noopener">{d.domain.clone()}</a>
                <span class="muted small">
                    {show_app.then(|| match app.clone() {
                        Some(app) => view! {
                            <A href=format!("/apps/{app}")>{app.clone()}</A>
                        }.into_any(),
                        None => view! { <span>"façade itself"</span> }.into_any(),
                    })}
                    {show_app.then_some(" · ")}
                    {format!("node {}", d.node)}
                </span>
            </div>
            <div class="domain-check">
                <span class=format!("badge badge-{dns_class}")>
                    <span class="badge-icon" aria-hidden="true">{dns_icon}</span>{dns_label}
                </span>
                <span class="small">{dns_detail}</span>
            </div>
            <div class="domain-check">
                <span class=format!("badge badge-{cert_class}")>
                    <span class="badge-icon" aria-hidden="true">{cert_icon}</span>{cert_label}
                </span>
                <span class="small">{cert_detail}</span>
            </div>
        </article>
    }
}

#[component]
fn DnsProvider(on_change: impl Fn() + Copy + Send + Sync + 'static) -> impl IntoView {
    let settings = polled(None, || async { api::get::<DnsSettings>("/api/dns").await });
    let editing = RwSignal::new(false);
    let api_key = RwSignal::new(String::new());
    let secret_key = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);

    let connect = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        busy.set(true);
        error.set(None);
        let update = DnsUpdate {
            provider: "porkbun".into(),
            api_key: api_key.get_untracked(),
            secret_key: secret_key.get_untracked(),
        };
        spawn_local(async move {
            match api::put::<_, DnsSettings>("/api/dns", &update).await {
                Ok(s) => {
                    toasts().ok(format!("Porkbun connected: {}", s.zones.join(", ")));
                    settings.data.try_set(Some(s));
                    editing.try_set(false);
                    api_key.try_set(String::new());
                    secret_key.try_set(String::new());
                    on_change();
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            busy.try_set(false);
        });
    };
    let disconnect = move |_| {
        run(
            "DNS records are now created by hand",
            async move { api::put_empty("/api/dns", &DnsUpdate::default()).await },
            move || {
                settings.reload();
                on_change();
            },
        );
    };
    let connected = move || {
        settings
            .data
            .with(|s| s.as_ref().is_some_and(|s| s.provider.is_some()))
    };

    view! {
        <section class="card form">
            <h2 class="section-title">"DNS provider"</h2>
            <Show
                when=move || connected() && !editing.get()
                fallback=move || view! {
                    <p class="muted small">
                        "Connect Porkbun and façade creates each domain's DNS record and a wildcard certificate "
                        "per domain by itself. Without it, create the records shown below by hand."
                    </p>
                    <form class="form" on:submit=connect>
                        <div class="form-row">
                            <label class="field">
                                <span>"Porkbun API key"</span>
                                <input type="text" bind:value=api_key placeholder="pk1_…" autocomplete="off" required/>
                            </label>
                            <label class="field">
                                <span>"Secret API key"</span>
                                <input type="password" bind:value=secret_key placeholder="sk1_…" autocomplete="off" required/>
                            </label>
                        </div>
                        <p class="muted small">
                            "Create the keys at porkbun.com → Account → API Access, then turn on “API Access” in each domain's details."
                        </p>
                        <ErrorNote error=error/>
                        <div class="form-actions">
                            <Show when=connected>
                                <button type="button" class="button" on:click=move |_| editing.set(false)>"Cancel"</button>
                            </Show>
                            <button type="submit" class="button button-primary" disabled=busy>
                                {move || if busy.get() { "Checking…" } else { "Connect Porkbun" }}
                            </button>
                        </div>
                    </form>
                }
            >
                <div class="provider-row">
                    <div>
                        <strong>{move || settings.data.get().and_then(|s| s.provider).map(capitalize)}</strong>
                        <span class="muted small">
                            {move || settings.data.get().map(|s| {
                                if s.zones.is_empty() {
                                    " · no domains with API access".to_string()
                                } else {
                                    format!(" · manages {}", s.zones.join(", "))
                                }
                            })}
                        </span>
                    </div>
                    <div class="actions">
                        <button class="button" on:click=move |_| editing.set(true)>"Change keys"</button>
                        <button class="button button-danger-quiet" on:click=disconnect>"Disconnect"</button>
                    </div>
                </div>
                <ErrorNote error=Signal::derive(move || settings.data.get().and_then(|s| s.error))/>
            </Show>
        </section>
    }
}

fn capitalize(s: String) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => s,
    }
}
