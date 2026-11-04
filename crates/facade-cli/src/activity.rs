//! `facade activity`: what happened to apps and nodes, newest first.

use clap::Args;
use facade_core::events::{Category, EventView, EventsPage, Scope, Severity};

use crate::client::Api;
use crate::output::{ago, date, print_json, table};

#[derive(Args)]
pub struct ActivityArgs {
    /// Only this app's entries.
    #[arg(long)]
    app: Option<String>,
    /// Only this node's entries (admins).
    #[arg(long)]
    node: Option<String>,
    /// deploys, restarts, health, changes, alerts, logins, certificates,
    /// system, or one kind (oom_kill, reboot, ssh_login…).
    #[arg(long)]
    kind: Option<String>,
    /// Only bad or unusual entries: crashes, memory kills, failing health
    /// checks, failed deploys and renewals, alerts, disk errors, reboots.
    #[arg(long)]
    problems: bool,
    /// How many entries.
    #[arg(short = 'n', long, default_value_t = 30)]
    limit: u32,
    /// Entries older than this one (its id), for the next page.
    #[arg(long)]
    before: Option<i64>,
    /// Show one entry in full, with its log lines.
    #[arg(long, conflicts_with_all = ["app", "node", "kind", "problems", "before"])]
    id: Option<i64>,
}

pub async fn run(api: &Api, args: ActivityArgs, json: bool) -> anyhow::Result<()> {
    if let Some(id) = args.id {
        let e: EventView = api.get(&format!("/api/events/{id}")).await?;
        if json {
            return print_json(&e);
        }
        show(&e);
        return Ok(());
    }
    if let Some(kind) = &args.kind
        && Category::parse(kind).is_none()
        && facade_core::events::EventKind::parse(kind) == facade_core::events::EventKind::Other
    {
        anyhow::bail!(
            "unknown kind {kind}: one of {}, or a kind of entry (oom_kill, reboot…)",
            Category::ALL.map(|c| c.as_str()).join(", ")
        );
    }
    let mut q = vec![format!("limit={}", args.limit)];
    for (key, value) in [
        ("app", &args.app),
        ("node", &args.node),
        ("kind", &args.kind),
    ] {
        if let Some(v) = value {
            q.push(format!("{key}={v}"));
        }
    }
    if args.problems {
        q.push("problems=true".into());
    }
    if let Some(before) = args.before {
        q.push(format!("before={before}"));
    }
    let page: EventsPage = api.get(&format!("/api/events?{}", q.join("&"))).await?;
    if json {
        return print_json(&page);
    }
    if page.events.is_empty() {
        println!("nothing yet");
        return Ok(());
    }
    let one_place = args.app.is_some() || args.node.is_some();
    let mut headers = vec!["ID", "WHEN"];
    if !one_place {
        headers.push("WHERE");
    }
    headers.push("WHAT");
    table(
        &headers,
        page.events.iter().map(|e| {
            let mut row = vec![e.id.to_string(), when(e.at)];
            if !one_place {
                row.push(place(e));
            }
            let count = if e.count > 1 {
                format!(" (×{})", e.count)
            } else {
                String::new()
            };
            row.push(format!("{} {}{count}", mark(e.severity), e.summary));
            row
        }),
    );
    if page.more
        && let Some(last) = page.events.last()
    {
        eprintln!("\nolder: --before {}", last.id);
    }
    Ok(())
}

fn mark(s: Severity) -> &'static str {
    match s {
        Severity::Info => "·",
        Severity::Ok => "✓",
        Severity::Warn => "!",
        Severity::Error => "✕",
    }
}

fn place(e: &EventView) -> String {
    match (&e.app, &e.node, e.scope) {
        (Some(app), _, _) => app.clone(),
        (None, Some(node), Scope::Node) => format!("node {node}"),
        _ => "-".into(),
    }
}

/// `3m ago` today, the date and UTC time before.
fn when(at: u64) -> String {
    if facade_core::now().saturating_sub(at) < 86400 {
        ago(at)
    } else {
        format!(
            "{} {:02}:{:02}",
            date(at),
            at % 86400 / 3600,
            at % 3600 / 60
        )
    }
}

fn show(e: &EventView) {
    println!("{} {}", mark(e.severity), e.summary);
    println!(
        "  when:    {} {:02}:{:02} UTC ({})",
        date(e.at),
        e.at % 86400 / 3600,
        e.at % 3600 / 60,
        ago(e.at)
    );
    if e.count > 1 {
        println!("  times:   {} since {}", e.count, when(e.first_at));
    }
    if let Some(app) = &e.app {
        println!("  app:     {app}");
    }
    if let Some(node) = &e.node {
        println!("  node:    {node}");
    }
    if let Some(actor) = &e.actor {
        println!("  by:      {actor}");
    }
    println!(
        "  kind:    {} ({}){}",
        e.kind.as_str(),
        e.kind.category().as_str(),
        if e.problem { ", a problem" } else { "" }
    );
    let d = &e.details;
    if !d.items.is_empty() {
        println!("  items:   {}", d.items.join(", "));
    }
    if let Some(deployment) = d.deployment {
        println!("  deploy:  {deployment}");
    }
    if !d.logs.is_empty() {
        println!();
        for line in &d.logs {
            println!("  {line}");
        }
    }
}
