//! `facade vulns`: vulnerabilities in what the nodes run.

use clap::{Args, Subcommand};
use facade_core::api::NodeView;
use facade_core::vulns::{AckRequest, Finding, FindingStatus, NodeVulns, VulnSeverity};
use reqwest::Method;

use crate::client::Api;
use crate::output::{ago, print_json, table};

#[derive(Args)]
pub struct VulnsArgs {
    #[command(subcommand)]
    action: Option<VulnsCommand>,
    /// Only this node.
    #[arg(long)]
    node: Option<String>,
    /// Medium and low ones too (high and critical only by default).
    #[arg(long)]
    all: bool,
}

#[derive(Subcommand)]
enum VulnsCommand {
    /// Silence a finding until its status changes (a fix comes out, a reboot
    /// applies it), e.g. after mitigating it.
    Ack {
        /// `CVE-2026-98432`.
        cve: String,
        /// Only on this node (every node it affects otherwise).
        #[arg(long)]
        node: Option<String>,
        /// Why: what mitigates it.
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Alert on it again.
    Unack {
        cve: String,
        #[arg(long)]
        node: Option<String>,
    },
}

pub async fn run(api: &Api, args: VulnsArgs, json: bool) -> anyhow::Result<()> {
    match args.action {
        Some(VulnsCommand::Ack { cve, node, note }) => {
            let path = format!("/api/vulns/{}/ack", cve.trim());
            let covered: usize = api
                .json(
                    api.request(Method::POST, &path)
                        .json(&AckRequest { node, note }),
                )
                .await?;
            eprintln!(
                "acknowledged {cve} ({covered} finding{}): silent until its status changes",
                if covered == 1 { "" } else { "s" }
            );
            Ok(())
        }
        Some(VulnsCommand::Unack { cve, node }) => {
            let mut path = format!("/api/vulns/{}/ack", cve.trim());
            if let Some(n) = node {
                path.push_str(&format!("?node={n}"));
            }
            api.send(api.request(Method::DELETE, &path)).await?;
            eprintln!("{cve} alerts again");
            Ok(())
        }
        None => list(api, args.node, args.all, json).await,
    }
}

async fn list(api: &Api, node: Option<String>, all: bool, json: bool) -> anyhow::Result<()> {
    let names: Vec<String> = match node {
        Some(n) => vec![n],
        None => api
            .get::<Vec<NodeView>>("/api/nodes")
            .await?
            .into_iter()
            .map(|n| n.name)
            .collect(),
    };
    let mut nodes = vec![];
    for name in names {
        let mut v: NodeVulns = api.get(&format!("/api/nodes/{name}/vulns")).await?;
        if !all {
            v.findings
                .retain(|f| f.severity.at_least(VulnSeverity::High));
        }
        nodes.push(v);
    }
    if json {
        return print_json(&nodes);
    }
    for (i, v) in nodes.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_node(v, all);
    }
    Ok(())
}

fn print_node(v: &NodeVulns, all: bool) {
    let s = &v.summary;
    let source = v.source.as_deref().unwrap_or("no data");
    let as_of = s
        .data_as_of
        .map(|t| format!(", data from {}", ago(t)))
        .unwrap_or_default();
    println!(
        "{}: {} critical, {} high, {} other ({source}{as_of})",
        v.node, s.critical, s.high, s.other
    );
    if let Some(why) = &s.unknown {
        println!("  not checked: {why}");
    }
    if v.findings.is_empty() {
        if s.unknown.is_none() {
            println!(
                "  nothing {}",
                if all { "found" } else { "high or critical" }
            );
        }
        return;
    }
    table(
        &[
            "CVE",
            "SEVERITY",
            "PACKAGE",
            "INSTALLED",
            "STATUS",
            "SINCE",
            "WHAT",
        ],
        v.findings.iter().map(|f| {
            vec![
                f.cve.clone(),
                f.severity.as_str().to_string(),
                f.package.clone(),
                f.installed.clone(),
                status(f),
                ago(f.first_seen),
                short(&f.title, 60),
            ]
        }),
    );
    let acked: Vec<&Finding> = v.findings.iter().filter(|f| f.ack.is_some()).collect();
    for f in acked {
        let a = f.ack.as_ref().expect("filtered");
        println!(
            "  {} acknowledged by {}{}",
            f.cve,
            a.by,
            if a.note.is_empty() {
                String::new()
            } else {
                format!(": {}", a.note)
            }
        );
    }
}

fn status(f: &Finding) -> String {
    let mut s = match (f.status, &f.fixed) {
        (FindingStatus::FixAvailable, Some(v)) => format!("fix available ({v})"),
        (FindingStatus::FixedAfterReboot, _) => "fixed after reboot".into(),
        (s, _) => s.label().to_string(),
    };
    if f.ack.is_some() {
        s.push_str(" (acked)");
    }
    s
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max - 1).collect::<String>() + "…"
    }
}
