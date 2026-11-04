use std::time::Duration;

use facade_core::api::{DnsSettings, DnsUpdate, DomainStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::api;
use crate::components::{ErrorNote, FilterSelect, Loading, Sort, SortKey, SortTh};
use crate::util::{me, polled, run, toasts};

/// Public suffixes of more than one label that people register under, so
/// `shop.example.co.uk` belongs to `example.co.uk`. Not the whole list: the
/// common ones, separated by spaces.
const SUFFIXES: &str = "\
    co.uk org.uk me.uk ltd.uk plc.uk net.uk ac.uk gov.uk com.au net.au org.au edu.au gov.au \
    co.nz net.nz org.nz co.jp ne.jp or.jp ac.jp co.kr or.kr com.br net.br org.br com.mx com.ar \
    com.tr com.cn net.cn org.cn com.tw com.hk com.sg com.my co.in net.in org.in co.id co.il \
    co.za org.za com.ua com.pl co.at or.at com.es com.pt co.it eu.org github.io gitlab.io \
    pages.dev netlify.app vercel.app fly.dev herokuapp.com duckdns.org";

/// The registered domain a host name belongs to: `www.example.com` →
/// `example.com`. Addresses stay as they are.
fn main_domain(host: &str) -> String {
    let host = host.trim_end_matches('.').to_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let n = labels.len();
    let keep = if n >= 3
        && SUFFIXES
            .split_whitespace()
            .any(|s| s == labels[n - 2..].join("."))
    {
        3
    } else {
        2
    };
    labels[n.saturating_sub(keep)..].join(".")
}

#[component]
pub fn Domains() -> impl IntoView {
    let me = me();
    let query = use_query_map();
    let navigate = use_navigate();
    let domains = polled(Some(Duration::from_secs(15)), || async {
        api::get::<Vec<DomainStatus>>("/api/domains").await
    });
    // The main domain picked, from the URL (`?domain=example.com`); empty for all.
    let picked = Memo::new(move |_| query.read().get("domain").unwrap_or_default());
    let pick = Callback::new(move |domain: String| {
        let url = if domain.is_empty() {
            "/domains".to_string()
        } else {
            format!("/domains?domain={domain}")
        };
        navigate(
            &url,
            leptos_router::NavigateOptions {
                replace: true,
                scroll: false,
                ..Default::default()
            },
        );
    });
    let mains = Signal::derive(move || {
        let mut mains = domains.data.with(|d| {
            d.iter()
                .flatten()
                .map(|d| main_domain(&d.domain))
                .collect::<Vec<_>>()
        });
        // One picked in a link but gone stays listed, so the selector says so.
        mains.extend(Some(picked.get()).filter(|p| !p.is_empty()));
        mains.sort();
        mains.dedup();
        mains
    });
    let total = move || domains.data.with(|d| d.as_ref().map_or(0, Vec::len));
    let shown = Signal::derive(move || {
        let picked = picked.get();
        domains
            .data
            .get()
            .unwrap_or_default()
            .into_iter()
            .filter(|d| picked.is_empty() || main_domain(&d.domain) == picked)
            .collect::<Vec<_>>()
    });
    let problems = move || {
        shown.with(|d| {
            d.iter()
                .filter(|d| dns_state(d) == Check::Failed || https_state(d) == Check::Failed)
                .count()
        })
    };
    view! {
        <div class="page">
            <header class="page-head"><h1>"Domains"</h1></header>
            <p class="muted lead">
                "Each domain points straight at the node running its app, which serves it over HTTPS. "
                "Add domains in an app's settings."
            </p>
            <ErrorNote error=domains.error/>
            <Show when=move || domains.loaded() fallback=Loading>
                <Show when=move || { total() > 0 }>
                    <div class="toolbar">
                        <Show when=move || { mains.with(Vec::len) > 1 || !picked.with(String::is_empty) }>
                            <FilterSelect label="Main domain" all="All domains" options=mains
                                value=picked on_change=pick/>
                        </Show>
                        <span class="muted small">
                            {move || {
                                let (n, all) = (shown.with(Vec::len), total());
                                if n == all { format!("{all} domain{}", if all == 1 { "" } else { "s" }) } else { format!("{n} of {all}") }
                            }}
                        </span>
                        <Show when=move || { problems() > 0 }>
                            <span class="badge badge-critical">
                                <span class="badge-icon" aria-hidden="true">"✕"</span>
                                {move || format!("{} with a problem", problems())}
                            </span>
                        </Show>
                    </div>
                </Show>
                <Show when=move || { total() == 0 || !shown.with(Vec::is_empty) }
                    fallback=move || view! {
                        <div class="empty"><p>{move || format!("No domain under {}.", picked.get())}</p></div>
                    }>
                    <DomainTable domains=shown show_app=true/>
                </Show>
            </Show>
            <Show when=move || me.is_admin()>
                <DnsProvider on_change=move || domains.reload()/>
            </Show>
        </div>
    }
}

/// How a check stands, worst first (the order a sort by it shows).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Check {
    Failed,
    Pending,
    Ok,
}

fn dns_state(d: &DomainStatus) -> Check {
    if d.dns_error.is_some() || (!d.dns_ok && !d.managed) {
        Check::Failed
    } else if d.dns_ok {
        Check::Ok
    } else {
        Check::Pending
    }
}

fn https_state(d: &DomainStatus) -> Check {
    if d.cert_error.is_some() {
        Check::Failed
    } else if d.certificate.is_some() {
        Check::Ok
    } else {
        Check::Pending
    }
}

fn app_label(d: &DomainStatus) -> String {
    d.app.clone().unwrap_or_else(|| "façade itself".into())
}

/// Columns: domain, app, node, points to, certificate, expires. A check sorts
/// worst first, then by its value.
fn sort_key(col: usize, d: &DomainStatus) -> SortKey {
    match col {
        0 => SortKey::text(&d.domain),
        1 => SortKey::text(app_label(d)),
        2 => SortKey::text(&d.node),
        3 => SortKey::text(format!(
            "{}{}",
            dns_state(d) as u8,
            d.expected_ip.as_deref().unwrap_or_default()
        )),
        4 => SortKey::text(format!(
            "{}{}",
            https_state(d) as u8,
            d.certificate.as_deref().unwrap_or_default()
        )),
        _ => SortKey::num(d.cert_expires.map(|t| t as f64)),
    }
}

/// Domains with their DNS and certificate status, also used on an app's page
/// (`show_app` off: app and node are the page's). A healthy row is three short
/// facts (address, certificate, expiry); only a row with a problem spells it out,
/// and what is automatic is said once, under the table.
#[component]
pub fn DomainTable(
    #[prop(into)] domains: Signal<Vec<DomainStatus>>,
    show_app: bool,
) -> impl IntoView {
    let sort = Sort::new();
    // A node column that always says the same thing is noise.
    let show_node = Memo::new(move |_| {
        show_app
            && domains.with(|d| {
                d.iter()
                    .any(|x| d.first().is_some_and(|f| f.node != x.node))
            })
    });
    let note = move || {
        domains.with(|d| {
            let managed = d.iter().any(|d| d.managed);
            let manual = d.iter().any(|d| !d.managed);
            let dns = match (managed, manual) {
                (true, false) => "façade keeps every DNS record up to date. ",
                (true, true) => "façade keeps the DNS records of its provider up to date; the others are set by hand. ",
                _ => "DNS records are set by hand at the domain provider. ",
            };
            format!("{dns}Certificates renew automatically before they expire.")
        })
    };
    view! {
        <Show
            when=move || domains.with(|d| !d.is_empty())
            fallback=|| view! { <div class="empty"><p>"No domains yet."</p></div> }
        >
            <div class="table-wrap domain-table">
                <table class="table">
                    <thead><tr>
                        <SortTh sort=sort col=0 label="Domain"/>
                        {show_app.then(|| view! { <SortTh sort=sort col=1 label="App"/> })}
                        {move || show_node.get().then(|| view! { <SortTh sort=sort col=2 label="Node"/> })}
                        <SortTh sort=sort col=3 label="Points to"/>
                        <SortTh sort=sort col=4 label="Certificate"/>
                        <SortTh sort=sort col=5 label="Expires" num=true/>
                    </tr></thead>
                    <tbody>
                        <For each=move || {
                                let mut rows = domains.get();
                                sort.apply(&mut rows, sort_key);
                                let node = show_node.get();
                                rows.into_iter().map(|d| (d, node)).collect::<Vec<_>>()
                            }
                            key=|(d, node)| (d.domain.clone(), d.app.clone(), d.dns_ok, d.certificate.clone(), d.cert_expires, d.dns_error.clone(), d.cert_error.clone(), d.resolved.clone(), d.expected_ip.clone(), d.managed, *node)
                            let:row>
                            <DomainRow d=row.0 show_app=show_app show_node=row.1/>
                        </For>
                    </tbody>
                </table>
            </div>
            <p class="muted small table-note">{note}</p>
        </Show>
    }
}

/// What to do about a DNS record that does not point where it must yet.
fn dns_problem(d: &DomainStatus) -> String {
    match (&d.dns_error, d.managed, &d.expected_ip) {
        (Some(e), ..) => e.clone(),
        (None, true, Some(ip)) => {
            format!("façade set the record to {ip}; it can take a few minutes to be visible.")
        }
        (None, false, Some(ip)) => format!(
            "Create an A record pointing to {ip} at your domain provider.{}",
            if d.resolved.is_empty() {
                String::new()
            } else {
                format!(" It points to {} now.", d.resolved.join(", "))
            }
        ),
        (None, _, None) => format!(
            "Node {} has no known public IP: set it on the Nodes page.",
            d.node
        ),
    }
}

#[component]
fn DomainRow(d: DomainStatus, show_app: bool, show_node: bool) -> impl IntoView {
    let points_to = match dns_state(&d) {
        Check::Ok => {
            let ip = d.expected_ip.clone().unwrap_or_else(|| "resolves".into());
            let how = if d.managed {
                "record kept up to date by façade"
            } else {
                "record set by hand"
            };
            view! {
                <span class="badge badge-good" title=how>
                    <span class="badge-icon" aria-hidden="true">"●"</span><code>{ip}</code>
                </span>
            }
            .into_any()
        }
        state => {
            let (class, icon, label) = match state {
                Check::Failed if d.dns_error.is_some() => ("critical", "✕", "error"),
                Check::Failed => ("critical", "✕", "missing"),
                _ => ("busy", "◐", "propagating"),
            };
            view! {
                <span class=format!("badge badge-{class}")>
                    <span class="badge-icon" aria-hidden="true">{icon}</span>{label}
                </span>
                <div class="small check-detail">{dns_problem(&d)}</div>
            }
            .into_any()
        }
    };
    let certificate = match (&d.cert_error, &d.certificate) {
        (None, Some(name)) => {
            let label = if name.starts_with("*.") {
                name.clone()
            } else {
                "its own".into()
            };
            // Where the table is too narrow for an Expires column, the date goes here.
            let until = d
                .cert_expires
                .map(|at| format!("until {}", crate::util::date(at)));
            view! {
                <span class="badge badge-good" title=format!("Certificate for {name}")>
                    <span class="badge-icon" aria-hidden="true">"●"</span>{label}
                </span>
                {until.map(|u| view! { <div class="small muted cert-until">{u}</div> })}
            }
            .into_any()
        }
        (error, _) => {
            let (class, icon, label, detail) = match error {
                Some(e) => ("critical", "✕", "failed", e.clone()),
                None => (
                    "busy",
                    "◐",
                    "pending",
                    "Requested once the DNS record points to the node.".to_string(),
                ),
            };
            view! {
                <span class=format!("badge badge-{class}")>
                    <span class="badge-icon" aria-hidden="true">{icon}</span>{label}
                </span>
                <div class="small check-detail">{detail}</div>
            }
            .into_any()
        }
    };
    let expires = match (d.cert_error.is_none(), d.cert_expires) {
        (true, Some(at)) => Some(crate::util::date(at)),
        _ => None,
    };
    let expires_class = if expires.is_some() {
        "num expires-cell"
    } else {
        "num expires-cell expires-none"
    };
    let expires = expires.unwrap_or_else(|| "—".into());
    let app = d.app.clone();
    view! {
        <tr>
            <td class="domain-cell">
                <a class="domain-name" href=format!("https://{}", d.domain) target="_blank" rel="noopener">{d.domain.clone()}</a>
            </td>
            {show_app.then(|| view! {
                <td class="app-cell">
                    {match app.clone() {
                        Some(app) => view! { <A href=format!("/apps/{app}")>{app.clone()}</A> }.into_any(),
                        None => view! { <span class="muted">"façade itself"</span> }.into_any(),
                    }}
                </td>
            })}
            {show_node.then(|| view! { <td class="muted node-cell">{d.node.clone()}</td> })}
            // On a phone the row is lines: the name, its app, then the checks.
            <td class="row-break" aria-hidden="true"></td>
            <td class="check-cell">{points_to}</td>
            <td class="check-cell">{certificate}</td>
            <td class=expires_class>{expires}</td>
        </tr>
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
                        "per domain by itself. Without it, create the records listed above by hand."
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
