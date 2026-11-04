//! The outside check, and each app's availability.
//!
//! Every minute the server loads each domain it routes (apps' main and
//! redirecting domains, and its own API domain) the way a visitor would: the
//! name resolved by the system's resolver (public DNS), a fresh connection to
//! whatever it resolves to (the node's entry point, through its public
//! address), the certificate checked against the public roots, `GET /`.
//! Any HTTP answer counts, a redirect included and not followed: this is about
//! DNS, certificates and routing; what the app answers is the traffic
//! counters' business. One HTTP client, a few domains at a time, nothing kept
//! between rounds, so every round resolves and shakes hands again.
//!
//! Each minute also records how every deployed app stood: down when its main
//! domain does not answer or its container is crashed or failed, stopped when
//! stopped on purpose (not counted), up otherwise; without a domain, from its
//! container alone. Availability is the share of minutes up among those up or
//! down, over 30 days.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use facade_core::api::DomainStatus;
use facade_core::insights::{
    Availability, DAY, DayUptime, DomainReach, Reach as ReachView, Slice, Uptime, day_of,
    uptime_percent,
};
use facade_core::proto::AppState;
use futures_util::StreamExt;
use parking_lot::{Mutex, RwLock};
use tracing::{info, warn};

use super::Server;
use crate::config::ChecksConfig;
use crate::ingress::edge::CHECK_AGENT;

/// Consecutive failed rounds before a domain that answered before raises an
/// alert (which the monitor confirms once more).
const ALERT_AFTER: u32 = 3;
/// Days availability covers.
pub const UPTIME_DAYS: u64 = 30;
/// Stored data is pruned this often (in minutes).
const PRUNE_EVERY: u64 = 60;

#[derive(Default)]
pub struct Reach {
    domains: RwLock<HashMap<String, DomainState>>,
    /// Today's minutes of each app, as stored.
    minutes: Mutex<HashMap<String, (u64, Vec<u8>)>>,
}

#[derive(Clone, Default)]
struct DomainState {
    view: ReachView,
    /// Rounds failed in a row.
    failures: u32,
}

impl Reach {
    pub fn get(&self, domain: &str) -> Option<ReachView> {
        let domains = self.domains.read();
        domains
            .get(domain)
            .filter(|s| s.view.checked_at > 0)
            .map(|s| s.view.clone())
    }

    pub fn of_domains<'a>(&self, domains: impl Iterator<Item = &'a String>) -> Vec<DomainReach> {
        domains
            .filter_map(|d| {
                Some(DomainReach {
                    domain: d.clone(),
                    reach: self.get(d)?,
                })
            })
            .collect()
    }

    /// Adds the last check of each domain.
    pub fn annotate(&self, statuses: &mut [DomainStatus]) {
        for status in statuses {
            status.reach = self.get(&status.domain);
        }
    }

    /// Takes a round's outcome for `domain`; returns `(last_ok, down_since)`
    /// when that changed and should be kept.
    fn take(&self, domain: &str, outcome: Outcome, now: u64) -> Option<(Option<u64>, Option<u64>)> {
        let mut domains = self.domains.write();
        let state = domains.entry(domain.to_string()).or_default();
        let before = (state.view.last_ok, state.view.down_since);
        let view = &mut state.view;
        view.checked_at = now;
        view.ok = outcome.ok;
        view.ms = outcome.ms;
        view.status = outcome.status;
        view.error = outcome.error;
        if outcome.ok {
            state.failures = 0;
            view.down_since = None;
            view.last_ok = Some(now);
        } else {
            state.failures += 1;
            view.down_since.get_or_insert(now);
        }
        let after = (view.last_ok, view.down_since);
        // `last_ok` moves every round while up: kept when it starts or stops.
        (before.1 != after.1 || before.0.is_none() != after.0.is_none()).then_some(after)
    }
}

/// What loading a URL gave.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    pub ms: Option<u32>,
    pub status: Option<u16>,
    pub error: Option<String>,
}

pub struct Checker {
    client: reqwest::Client,
}

impl Checker {
    pub fn new(cfg: &ChecksConfig) -> anyhow::Result<Self> {
        let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(5)))
            // A new connection each time: a kept one would skip DNS and TLS.
            .pool_max_idle_per_host(0)
            .user_agent(format!("{CHECK_AGENT}/1 (facade outside check)"));
        for (name, ip) in &cfg.resolve {
            builder = builder.resolve(name, std::net::SocketAddr::new(*ip, 0));
        }
        if let Some(ca) = &cfg.root_ca {
            let pem = std::fs::read(ca)?;
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
        }
        Ok(Self {
            client: builder.build()?,
        })
    }

    /// Loads `url` up to its response's headers.
    pub async fn check(&self, url: &str) -> Outcome {
        let started = Instant::now();
        match self.client.get(url).send().await {
            Ok(response) => Outcome {
                ok: true,
                ms: Some(started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32),
                status: Some(response.status().as_u16()),
                error: None,
            },
            Err(e) => Outcome {
                ok: false,
                ms: None,
                status: None,
                error: Some(describe(&e)),
            },
        }
    }
}

/// Why a check failed, in a few words.
fn describe(e: &reqwest::Error) -> String {
    let mut chain = Vec::new();
    let mut source: Option<&dyn std::error::Error> = Some(e);
    while let Some(s) = source {
        chain.push(s.to_string());
        source = s.source();
    }
    let all = chain.join(": ").to_lowercase();
    if e.is_timeout() || all.contains("timed out") || all.contains("deadline") {
        return "timed out".into();
    }
    if all.contains("dns error") || all.contains("failed to lookup") || all.contains("no address") {
        return "DNS: the name does not resolve".into();
    }
    if let Some(tls) = chain
        .iter()
        .find(|c| c.contains("certificate") || c.to_lowercase().contains("tls"))
    {
        let what = if tls.contains("Expired") {
            "certificate expired".to_string()
        } else if tls.contains("NotValidForName") || tls.contains("not valid for name") {
            "certificate not valid for this name".to_string()
        } else if tls.contains("UnknownIssuer") {
            "certificate not from a trusted authority".to_string()
        } else {
            tls.trim_start_matches("invalid peer certificate: ")
                .to_string()
        };
        return format!("TLS: {what}");
    }
    if all.contains("refused") {
        return "connection refused".into();
    }
    if all.contains("reset") || all.contains("closed") {
        return "connection closed without an answer".into();
    }
    chain.last().cloned().unwrap_or_else(|| e.to_string())
}

pub async fn run(server: Arc<Server>) -> anyhow::Result<()> {
    let cfg = server.cfg.checks.clone();
    match server.store.reach_marks() {
        Ok(marks) => {
            let mut domains = server.reach.domains.write();
            for (domain, last_ok, down_since) in marks {
                domains.insert(
                    domain,
                    DomainState {
                        view: ReachView {
                            ok: down_since.is_none(),
                            last_ok,
                            down_since,
                            ..Default::default()
                        },
                        failures: 0,
                    },
                );
            }
        }
        Err(e) => warn!("reading domain checks: {e:#}"),
    }
    let checker = if cfg.enabled {
        Some(Checker::new(&cfg)?)
    } else {
        info!("outside checks are off ([checks] enabled = false)");
        None
    };
    let every = Duration::from_secs(cfg.every_secs.max(5));
    // Apart from the rounds, which can take longer than a minute (many
    // domains timing out), so that no minute goes unrecorded.
    let minutes = async {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ticks = 0u64;
        loop {
            tick.tick().await;
            let now = facade_core::now();
            record_minute(&server, now, every.as_secs());
            if ticks.is_multiple_of(PRUNE_EVERY)
                && let Err(e) = server.store.prune_edge(now)
            {
                warn!("pruning traffic history: {e:#}");
            }
            ticks += 1;
        }
    };
    let rounds = async {
        let Some(checker) = &checker else {
            return std::future::pending().await;
        };
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            round(&server, checker, cfg.concurrency.max(1)).await;
        }
    };
    tokio::join!(minutes, rounds).0
}

/// Checks every domain once.
async fn round(server: &Server, checker: &Checker, concurrency: usize) {
    let certs: Vec<String> = server
        .store
        .list_certs()
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.domain)
        .collect();
    let targets: Vec<(String, String)> = server
        .desired_domains()
        .into_iter()
        .map(|d| {
            // Served over HTTPS once it has a certificate (plain HTTP then
            // redirects there), over HTTP until then.
            let covered = certs.contains(&super::acme::covering_name(server, &d.domain));
            let scheme = if covered { "https" } else { "http" };
            let url = format!("{scheme}://{}/", d.domain);
            (d.domain, url)
        })
        .collect();
    {
        let mut domains = server.reach.domains.write();
        let gone: Vec<String> = domains
            .keys()
            .filter(|d| !targets.iter().any(|(t, _)| t == *d))
            .cloned()
            .collect();
        for domain in gone {
            domains.remove(&domain);
            let _ = server.store.delete_reach_mark(&domain);
        }
    }
    let outcomes: Vec<(String, Outcome)> = futures_util::stream::iter(targets)
        .map(|(domain, url)| async move { (domain, checker.check(&url).await) })
        .buffer_unordered(concurrency)
        .collect()
        .await;
    let now = facade_core::now();
    for (domain, outcome) in outcomes {
        if let Some((last_ok, down_since)) = server.reach.take(&domain, outcome, now)
            && let Err(e) = server.store.put_reach_mark(&domain, last_ok, down_since)
        {
            warn!("saving the check of {domain}: {e:#}");
        }
    }
}

/// Domains failing long enough to say so, as `(alert key, message)`. Only
/// those that answered before: a domain whose DNS was never set is the
/// Domains page's business.
pub fn alerts(server: &Server) -> Vec<(String, String)> {
    let routes = server.routes.read();
    server
        .reach
        .domains
        .read()
        .iter()
        .filter(|(_, s)| s.failures >= ALERT_AFTER && s.view.last_ok.is_some())
        .map(|(domain, s)| {
            let whose = routes
                .domains
                .get(domain)
                .map(|app| format!(" (app {app})"))
                .unwrap_or_default();
            let since = s
                .view
                .down_since
                .map(|t| format!(" since {} UTC", clock(t)))
                .unwrap_or_default();
            let why = s
                .view
                .error
                .as_deref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default();
            (
                format!("domain:{domain}:unreachable"),
                format!("site {domain}{whose} is unreachable from outside{since}{why}"),
            )
        })
        .collect()
}

/// `HH:MM` of a time, UTC.
fn clock(at: u64) -> String {
    let secs = at % DAY;
    format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

const UNKNOWN: u8 = 0;
const UP: u8 = 1;
const DOWN: u8 = 2;
const STOPPED: u8 = 3;

fn uptime_of(byte: u8) -> Uptime {
    match byte {
        UP => Uptime::Up,
        DOWN => Uptime::Down,
        STOPPED => Uptime::Stopped,
        _ => Uptime::Unknown,
    }
}

/// How an app stands this minute.
fn minute_state(
    enabled: bool,
    state: Option<AppState>,
    node_connected: bool,
    main_domain: Option<&ReachView>,
    unhealthy: bool,
) -> u8 {
    if !enabled {
        return STOPPED;
    }
    let state = state.filter(|_| node_connected);
    let broken = matches!(state, Some(AppState::Crashed | AppState::Failed))
        || (unhealthy && node_connected);
    if let Some(reach) = main_domain {
        return if !reach.ok || broken { DOWN } else { UP };
    }
    match state {
        Some(AppState::Running | AppState::Building) if broken => DOWN,
        Some(AppState::Running | AppState::Building) => UP,
        Some(AppState::Crashed | AppState::Failed) => DOWN,
        Some(AppState::Stopped) => STOPPED,
        _ => UNKNOWN,
    }
}

/// Whether the app's health check says it does not work: its last two results
/// failed (or its only one), so one slow check does not mark a minute down.
fn failing_health(results: &[facade_core::resources::HealthCheck]) -> bool {
    let last: Vec<_> = results.iter().rev().take(2).collect();
    !last.is_empty() && last.iter().all(|r| !r.ok)
}

fn record_minute(server: &Server, now: u64, every: u64) {
    let apps = match server.store.list_apps() {
        Ok(apps) => apps,
        Err(e) => {
            warn!("recording availability: {e:#}");
            return;
        }
    };
    let day = day_of(now);
    let minute = ((now % DAY) / 60) as usize;
    let fresh_after = now.saturating_sub(every * 3);
    let states: Vec<(String, u8)> = {
        let live = server.live.read();
        apps.iter()
            .filter(|a| a.revision > 0)
            .map(|a| {
                let spec = &a.spec;
                let reach = spec
                    .domains
                    .first()
                    .and_then(|d| server.reach.get(d))
                    .filter(|r| r.checked_at >= fresh_after);
                let app = live.apps.get(&spec.name);
                let state = minute_state(
                    spec.enabled,
                    app.map(|l| l.state),
                    server.hub.is_connected(&spec.node),
                    reach.as_ref(),
                    app.and_then(|l| l.stats.as_ref())
                        .is_some_and(|s| failing_health(&s.resources.health)),
                );
                (spec.name.clone(), state)
            })
            .collect()
    };
    let mut cache = server.reach.minutes.lock();
    cache.retain(|app, _| states.iter().any(|(a, _)| a == app));
    for (app, state) in states {
        let entry = cache
            .entry(app.clone())
            .or_insert_with(|| (u64::MAX, Vec::new()));
        if entry.0 != day {
            let stored = server.store.uptime_day(&app, day).ok().flatten();
            *entry = (
                day,
                stored
                    .filter(|m| m.len() == 1440)
                    .unwrap_or_else(|| vec![UNKNOWN; 1440]),
            );
        }
        if entry.1[minute] == state {
            continue;
        }
        entry.1[minute] = state;
        if let Err(e) = server.store.put_uptime_day(&app, day, &entry.1) {
            warn!("recording availability of {app}: {e:#}");
        }
    }
}

/// A day's minutes, up to `len` of them, added up and cut into runs.
pub fn summarize(day: u64, minutes: Option<&[u8]>, len: usize) -> DayUptime {
    let mut d = DayUptime {
        day,
        up: 0,
        down: 0,
        stopped: 0,
        unknown: 0,
        slices: Vec::new(),
    };
    let at = |i: usize| minutes.and_then(|m| m.get(i)).copied().unwrap_or(UNKNOWN);
    let mut i = 0;
    while i < len {
        let byte = at(i);
        let start = i;
        while i < len && at(i) == byte {
            i += 1;
        }
        let n = (i - start) as u32;
        match byte {
            UP => d.up += n,
            DOWN => d.down += n,
            STOPPED => d.stopped += n,
            _ => d.unknown += n,
        }
        if byte != UP {
            d.slices.push(Slice {
                start: start as u16,
                end: i as u16,
                state: uptime_of(byte),
            });
        }
    }
    d
}

pub fn availability(server: &Server, app: &facade_core::AppSpec, now: u64) -> Availability {
    let today = day_of(now);
    let from = today + 1 - UPTIME_DAYS;
    let stored: HashMap<u64, Vec<u8>> = server
        .store
        .uptime_days(&app.name, from)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let days: Vec<DayUptime> = (from..=today)
        .map(|day| {
            let minutes = stored.get(&day).map(Vec::as_slice);
            let len = if day == today {
                // Up to now: the current minute once it is recorded.
                let current = ((now % DAY) / 60) as usize;
                let recorded = minutes
                    .and_then(|m| m.get(current))
                    .is_some_and(|m| *m != UNKNOWN);
                current + usize::from(recorded)
            } else {
                1440
            };
            summarize(day, minutes, len)
        })
        .collect();
    let (up, down) = days.iter().fold((0, 0), |(u, d), day| {
        (u + u64::from(day.up), d + u64::from(day.down))
    });
    Availability {
        percent: uptime_percent(up, down),
        days,
        domain: app.domains.first().cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn config(resolve: &[(&str, &str)], root_ca: Option<std::path::PathBuf>) -> ChecksConfig {
        ChecksConfig {
            timeout_secs: 1,
            resolve: resolve
                .iter()
                .map(|(n, ip)| (n.to_string(), ip.parse().unwrap()))
                .collect::<BTreeMap<_, _>>(),
            root_ca,
            ..Default::default()
        }
    }

    /// Answers every connection with `status` (plain HTTP or, with `tls`,
    /// over TLS), or never answers (`None`).
    async fn server(status: Option<u16>, tls: Option<tokio_rustls::TlsAcceptor>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let tls = tls.clone();
                tokio::spawn(async move {
                    let answer = async |mut s: Box<dyn Io>| {
                        let mut buf = [0u8; 1024];
                        let _ = s.read(&mut buf).await;
                        match status {
                            Some(code) => {
                                let reply = format!(
                                    "HTTP/1.1 {code} X\r\nLocation: https://elsewhere.example/\r\nContent-Length: 0\r\n\r\n"
                                );
                                let _ = s.write_all(reply.as_bytes()).await;
                            }
                            None => tokio::time::sleep(Duration::from_secs(30)).await,
                        }
                    };
                    match tls {
                        Some(acceptor) => {
                            if let Ok(s) = acceptor.accept(sock).await {
                                answer(Box::new(s)).await
                            }
                        }
                        None => answer(Box::new(sock)).await,
                    }
                });
            }
        });
        port
    }

    trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
    impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

    /// A CA, and an acceptor with a certificate it signed for `name`.
    fn tls_for(name: &str) -> (String, tokio_rustls::TlsAcceptor) {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec![name.to_string()])
            .unwrap()
            .signed_by(&key, &issuer)
            .unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            )
            .unwrap();
        (ca.pem(), tokio_rustls::TlsAcceptor::from(Arc::new(config)))
    }

    #[tokio::test]
    async fn checks_dns_tls_and_routing() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let local = [("app.test", "127.0.0.1"), ("other.test", "127.0.0.1")];

        // Any answer counts, an error or a redirect (not followed) too.
        let port = server(Some(404), None).await;
        let checker = Checker::new(&config(&local, None)).unwrap();
        let ok = checker.check(&format!("http://app.test:{port}/")).await;
        assert!(ok.ok, "{ok:?}");
        assert_eq!(ok.status, Some(404));
        assert!(ok.ms.is_some());
        let port = server(Some(308), None).await;
        let moved = checker.check(&format!("http://app.test:{port}/")).await;
        assert_eq!((moved.ok, moved.status), (true, Some(308)));

        // Nothing listening.
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let refused = checker.check(&format!("http://app.test:{port}/")).await;
        assert_eq!(refused.error.as_deref(), Some("connection refused"));

        // Accepted, never answered.
        let port = server(None, None).await;
        let slow = checker.check(&format!("http://app.test:{port}/")).await;
        assert_eq!(slow.error.as_deref(), Some("timed out"));

        // A name that does not resolve.
        let nowhere = checker.check("http://nowhere.invalid/").await;
        assert_eq!(
            nowhere.error.as_deref(),
            Some("DNS: the name does not resolve")
        );

        // TLS: a certificate from a known authority, for the name.
        let dir = std::env::temp_dir().join(format!("facade-reach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (ca, acceptor) = tls_for("app.test");
        let ca_file = dir.join("ca.pem");
        std::fs::write(&ca_file, ca).unwrap();
        let port = server(Some(200), Some(acceptor)).await;
        let trusting = Checker::new(&config(&local, Some(ca_file))).unwrap();
        let secure = trusting.check(&format!("https://app.test:{port}/")).await;
        assert_eq!((secure.ok, secure.status), (true, Some(200)), "{secure:?}");
        // Not for this name.
        let wrong = trusting.check(&format!("https://other.test:{port}/")).await;
        assert_eq!(
            wrong.error.as_deref(),
            Some("TLS: certificate not valid for this name")
        );
        // From an authority visitors do not trust.
        let untrusted = checker.check(&format!("https://app.test:{port}/")).await;
        assert_eq!(
            untrusted.error.as_deref(),
            Some("TLS: certificate not from a trusted authority")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rounds_track_since_when_and_alert_after_a_few() {
        let reach = Reach::default();
        let ok = Outcome {
            ok: true,
            ms: Some(5),
            status: Some(200),
            error: None,
        };
        let failed = Outcome {
            ok: false,
            ms: None,
            status: None,
            error: Some("timed out".into()),
        };
        assert_eq!(reach.take("a", ok.clone(), 100), Some((Some(100), None)));
        assert_eq!(reach.take("a", ok.clone(), 160), None, "nothing to keep");
        assert_eq!(
            reach.take("a", failed.clone(), 220),
            Some((Some(160), Some(220)))
        );
        assert_eq!(reach.take("a", failed.clone(), 280), None);
        let view = reach.get("a").unwrap();
        assert_eq!(
            (view.ok, view.down_since, view.last_ok),
            (false, Some(220), Some(160))
        );
        assert_eq!(reach.domains.read()["a"].failures, 2);
        reach.take("a", failed, 340);
        assert_eq!(reach.domains.read()["a"].failures, ALERT_AFTER);
        assert_eq!(reach.take("a", ok, 400), Some((Some(400), None)));
        assert_eq!(reach.domains.read()["a"].failures, 0);
    }

    #[test]
    fn minutes_say_how_an_app_stood() {
        let up = ReachView {
            ok: true,
            ..Default::default()
        };
        let down = ReachView::default();
        let running = Some(AppState::Running);
        assert_eq!(
            minute_state(false, running, true, Some(&up), false),
            STOPPED
        );
        assert_eq!(minute_state(true, running, true, Some(&up), false), UP);
        assert_eq!(minute_state(true, running, true, Some(&down), false), DOWN);
        // Reachable, but answering for a crashed app.
        assert_eq!(
            minute_state(true, Some(AppState::Crashed), true, Some(&up), false),
            DOWN
        );
        // No domain: the container says.
        assert_eq!(minute_state(true, running, true, None, false), UP);
        assert_eq!(
            minute_state(true, Some(AppState::Failed), true, None, false),
            DOWN
        );
        assert_eq!(
            minute_state(true, running, false, None, false),
            UNKNOWN,
            "node away"
        );
        // A domain still answers while the node's agent is away.
        assert_eq!(minute_state(true, running, false, Some(&up), false), UP);
        // Running and reachable, but its health check fails: down.
        assert_eq!(minute_state(true, running, true, Some(&up), true), DOWN);
        assert_eq!(minute_state(true, running, true, None, true), DOWN);
        let check = |ok| facade_core::resources::HealthCheck {
            at_ms: 0,
            ok,
            ms: 1,
            output: String::new(),
        };
        assert!(!failing_health(&[]));
        assert!(!failing_health(&[check(false), check(true)]));
        assert!(!failing_health(&[check(true), check(false)]));
        assert!(failing_health(&[check(true), check(false), check(false)]));
        assert!(failing_health(&[check(false)]));
    }

    #[test]
    fn days_are_cut_into_runs() {
        let mut minutes = vec![UP; 1440];
        minutes[600..630].fill(DOWN);
        minutes[1000..1100].fill(STOPPED);
        minutes[..10].fill(UNKNOWN);
        let day = summarize(5, Some(&minutes), 1440);
        assert_eq!(
            (day.up, day.down, day.stopped, day.unknown),
            (1300, 30, 100, 10)
        );
        assert_eq!(
            day.slices
                .iter()
                .map(|s| (s.start, s.end, s.state))
                .collect::<Vec<_>>(),
            [
                (0, 10, Uptime::Unknown),
                (600, 630, Uptime::Down),
                (1000, 1100, Uptime::Stopped)
            ]
        );
        assert!((day.percent().unwrap() - 1300.0 * 100.0 / 1330.0).abs() < 1e-9);
        // Today, up to now.
        let today = summarize(6, Some(&minutes), 620);
        assert_eq!((today.up, today.down, today.unknown), (590, 20, 10));
        // A day never recorded.
        let none = summarize(7, None, 1440);
        assert_eq!((none.unknown, none.percent()), (1440, None));
    }
}
