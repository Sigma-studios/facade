//! What visitors see of an app: availability, whether its domains answer,
//! visitors, bandwidth, and its failed requests.

use clap::{Args, ValueEnum};
use facade_core::insights::{
    AppInsights, Bandwidth, ErrorPeriod, ErrorRow, ErrorsView, NodeInsights, civil, day_of,
};

use crate::client::Api;
use crate::output::{self, bytes, print_json, table};

/// The insights of an app, or `None` from a server without them.
pub async fn app_insights(api: &Api, app: &str) -> Option<AppInsights> {
    api.get(&format!("/api/apps/{app}/insights")).await.ok()
}

pub async fn node_insights(api: &Api, node: &str) -> Option<NodeInsights> {
    api.get(&format!("/api/nodes/{node}/insights")).await.ok()
}

/// A few lines summing up an app's insights, for `app show` and `metrics`.
pub fn summary(i: &AppInsights) -> Vec<(&'static str, String)> {
    let mut lines = Vec::new();
    let a = &i.availability;
    if let Some(percent) = a.percent {
        let how = match &a.domain {
            Some(d) => format!("seen from outside at {d}"),
            None => "by its container's state".into(),
        };
        lines.push((
            "available",
            format!(
                "up {} over {} days ({how})",
                percent_text(percent),
                a.days.len()
            ),
        ));
    }
    if !i.reach.is_empty() {
        let parts: Vec<String> = i
            .reach
            .iter()
            .map(|r| format!("{} {}", r.domain, reach_text(&r.reach)))
            .collect();
        lines.push(("reachable", parts.join(" · ")));
    }
    if i.routed {
        let today = day_of(facade_core::now());
        let (t, y) = (
            i.visitors_on(today).unwrap_or(0),
            i.visitors_on(today.saturating_sub(1)).unwrap_or(0),
        );
        lines.push(("visitors", format!("{t} today, {y} yesterday")));
        lines.push(("bandwidth", bandwidth_text(&i.bandwidth)));
    }
    lines
}

/// The summary, lined up with `app show`'s other lines.
pub fn print_summary(i: &AppInsights) {
    for (label, text) in summary(i) {
        let label = format!("{label}:");
        println!("{label:<10} {text}");
    }
}

/// `99.95 %`, with the digits that tell apart what matters near 100 %.
pub fn percent_text(p: f64) -> String {
    if p >= 100.0 {
        "100 %".into()
    } else if p >= 99.995 {
        "99.99 %".into()
    } else if p >= 99.0 {
        format!("{p:.2} %")
    } else {
        format!("{p:.1} %")
    }
}

pub fn reach_text(r: &facade_core::insights::Reach) -> String {
    if r.ok {
        match r.ms {
            Some(ms) => format!("reachable ({ms} ms)"),
            None => "reachable".into(),
        }
    } else {
        let since = r
            .down_since
            .map(|t| format!(" since {}", output::ago(t)))
            .unwrap_or_default();
        let why = r
            .error
            .as_deref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default();
        format!("UNREACHABLE{since}{why}")
    }
}

pub fn bandwidth_text(b: &Bandwidth) -> String {
    format!(
        "{} served in {}, {} received ({} served today)",
        bytes(b.month_out),
        month_name(b.month_start),
        bytes(b.month_in),
        bytes(b.today_out)
    )
}

fn month_name(day: u64) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let (_, m, _) = civil(day);
    MONTHS[(m - 1) as usize].to_string()
}

fn day_text(day: u64) -> String {
    let (y, m, d) = civil(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `facade insights <app>`: everything, day by day.
pub async fn show(api: &Api, app: &str, json: bool) -> anyhow::Result<()> {
    let i: AppInsights = api.get(&format!("/api/apps/{app}/insights")).await?;
    if json {
        return print_json(&i);
    }
    print_summary(&i);
    println!();
    let visitors: std::collections::HashMap<u64, u64> =
        i.visitors.iter().map(|v| (v.day, v.visitors)).collect();
    let rows = i.availability.days.iter().rev().map(|d| {
        let down = d
            .slices
            .iter()
            .filter(|s| s.state == facade_core::insights::Uptime::Down)
            .map(|s| {
                format!(
                    "{:02}:{:02}–{:02}:{:02}",
                    s.start / 60,
                    s.start % 60,
                    s.end / 60,
                    s.end % 60
                )
            })
            .collect::<Vec<_>>();
        vec![
            day_text(d.day),
            d.percent().map(percent_text).unwrap_or_else(|| "-".into()),
            visitors.get(&d.day).copied().unwrap_or(0).to_string(),
            if down.is_empty() {
                String::new()
            } else {
                format!("down {} (UTC)", down.join(", "))
            },
        ]
    });
    table(&["DAY", "UP", "VISITORS", "OUTAGES"], rows);
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Period {
    #[value(name = "1h")]
    Hour,
    #[value(name = "24h")]
    Day,
    #[value(name = "7d")]
    Week,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Class {
    #[value(name = "4xx")]
    Client,
    #[value(name = "5xx")]
    Server,
    All,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SortBy {
    Count,
    Last,
    First,
    Status,
    Method,
    Path,
}

/// `facade errors`' options.
#[derive(Args)]
pub struct ErrorsArgs {
    pub app: String,
    /// The period counted.
    #[arg(long, value_enum, default_value = "24h")]
    pub period: Period,
    /// Only client (4xx) or server (5xx) errors.
    #[arg(long, value_enum, default_value = "all")]
    pub class: Class,
    /// Only paths containing this text.
    #[arg(long)]
    pub path: Option<String>,
    /// The column to sort by (counts and times: largest first).
    #[arg(long, value_enum, default_value = "count")]
    pub sort: SortBy,
    /// Reverse the order.
    #[arg(long)]
    pub reverse: bool,
    /// Lines shown at most (0: all).
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

/// The rows to show, filtered and sorted as asked.
pub fn select(view: &ErrorsView, args: &ErrorsArgs) -> Vec<ErrorRow> {
    let needle = args.path.as_deref().map(str::to_lowercase);
    let mut rows: Vec<ErrorRow> = view
        .rows
        .iter()
        .filter(|r| match args.class {
            Class::Client => (400..500).contains(&r.status),
            Class::Server => r.status >= 500,
            Class::All => true,
        })
        .filter(|r| {
            needle
                .as_ref()
                .is_none_or(|n| r.path.to_lowercase().contains(n))
        })
        .cloned()
        .collect();
    rows.sort_by(|a, b| match args.sort {
        SortBy::Count => b.count.cmp(&a.count),
        SortBy::Last => b.last_seen.cmp(&a.last_seen),
        SortBy::First => b.first_seen.cmp(&a.first_seen),
        SortBy::Status => a.status.cmp(&b.status),
        SortBy::Method => a.method.cmp(&b.method),
        SortBy::Path => a.path.cmp(&b.path),
    });
    if args.reverse {
        rows.reverse();
    }
    if args.limit > 0 {
        rows.truncate(args.limit);
    }
    rows
}

pub async fn errors(api: &Api, args: &ErrorsArgs, json: bool) -> anyhow::Result<()> {
    let period = match args.period {
        Period::Hour => ErrorPeriod::Hour,
        Period::Day => ErrorPeriod::Day,
        Period::Week => ErrorPeriod::Week,
    };
    let view: ErrorsView = api
        .get(&format!(
            "/api/apps/{}/errors?period={}",
            args.app,
            period.key()
        ))
        .await?;
    let rows = select(&view, args);
    if json {
        return print_json(&ErrorsView {
            period: view.period,
            more: view.more,
            rows,
        });
    }
    if rows.is_empty() {
        println!("no failed requests over the last {}", period_text(period));
        return Ok(());
    }
    let total: u64 = rows.iter().map(|r| r.count).sum();
    table(
        &[
            "STATUS",
            "METHOD",
            "PATH",
            "COUNT",
            "LAST SEEN",
            "FIRST SEEN",
        ],
        rows.iter().map(|r| {
            vec![
                r.status.to_string(),
                r.method.clone(),
                r.path.clone(),
                r.count.to_string(),
                output::ago(r.last_seen),
                output::ago(r.first_seen),
            ]
        }),
    );
    println!(
        "\n{total} failed requests in {} group{}, over the last {}{}",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        period_text(period),
        if view.more > 0 {
            format!(" ({} rarer groups not listed)", view.more)
        } else {
            String::new()
        }
    );
    Ok(())
}

fn period_text(p: ErrorPeriod) -> &'static str {
    match p {
        ErrorPeriod::Hour => "hour",
        ErrorPeriod::Day => "24 hours",
        ErrorPeriod::Week => "7 days",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: u16, path: &str, count: u64, last_seen: u64) -> ErrorRow {
        ErrorRow {
            status,
            method: "GET".into(),
            path: path.into(),
            count,
            first_seen: 1,
            last_seen,
        }
    }

    #[test]
    fn errors_filter_and_sort() {
        let view = ErrorsView {
            period: ErrorPeriod::Day,
            rows: vec![
                row(404, "/a", 9, 10),
                row(500, "/api/b", 3, 30),
                row(502, "/api/c", 5, 20),
            ],
            more: 0,
        };
        let args = |class, path: Option<&str>, sort, reverse| ErrorsArgs {
            app: "x".into(),
            period: Period::Day,
            class,
            path: path.map(str::to_string),
            sort,
            reverse,
            limit: 0,
        };
        let paths = |rows: Vec<ErrorRow>| rows.into_iter().map(|r| r.path).collect::<Vec<_>>();
        assert_eq!(
            paths(select(&view, &args(Class::All, None, SortBy::Count, false))),
            ["/a", "/api/c", "/api/b"]
        );
        assert_eq!(
            paths(select(
                &view,
                &args(Class::Server, None, SortBy::Last, false)
            )),
            ["/api/b", "/api/c"]
        );
        assert_eq!(
            paths(select(
                &view,
                &args(Class::All, Some("API"), SortBy::Path, true)
            )),
            ["/api/c", "/api/b"]
        );
        assert_eq!(
            paths(select(
                &view,
                &args(Class::Client, None, SortBy::Status, false)
            )),
            ["/a"]
        );
        assert_eq!(percent_text(99.951), "99.95 %");
        assert_eq!(percent_text(99.999), "99.99 %");
        assert_eq!(percent_text(100.0), "100 %");
        assert_eq!(percent_text(87.24), "87.2 %");
    }
}
