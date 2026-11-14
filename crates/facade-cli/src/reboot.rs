//! `facade node reboot` and `facade node maintenance`.

use anyhow::bail;
use facade_core::api::{MaintenanceWindow, RebootPlan, RebootRequest, SetMaintenance, utc_time};
use reqwest::Method;

use crate::client::Api;
use crate::output::{ago, print_json};

pub async fn reboot(
    api: &Api,
    name: &str,
    at: Option<String>,
    cancel: bool,
    yes: bool,
    json: bool,
) -> anyhow::Result<()> {
    let path = format!("/api/nodes/{name}/reboot");
    if cancel {
        let plan: RebootPlan = api.json(api.request(Method::DELETE, &path)).await?;
        if json {
            return print_json(&plan);
        }
        eprintln!("reboot of {name} cancelled");
        return Ok(());
    }
    let at = at.map(|a| parse_time(&a, facade_core::now())).transpose()?;
    let plan: RebootPlan = api.get(&path).await?;
    // A reboot not back after 10 minutes may be asked again.
    if let Some(run) = plan
        .running
        .as_ref()
        .filter(|r| r.back.is_some() || facade_core::now().saturating_sub(r.asked) < 600)
    {
        bail!(
            "node {name} is already rebooting (asked by {} {})",
            run.by,
            ago(run.asked)
        );
    }
    if at.is_none()
        && let Some(why) = &plan.unavailable
    {
        bail!("{why}");
    }
    if !json || !yes {
        show(&plan, at);
    }
    let question = match at {
        Some(at) => format!("Schedule the reboot of {name} for {}?", utc_time(at)),
        None => format!("Reboot {name} now?"),
    };
    if !yes && !crate::confirm(&question)? {
        bail!("nothing done");
    }
    let plan: RebootPlan = api
        .json(api.request(Method::POST, &path).json(&RebootRequest { at }))
        .await?;
    if json {
        return print_json(&plan);
    }
    match at {
        Some(at) => eprintln!(
            "reboot of {name} scheduled for {}: Discord hears of it 15 minutes before, and of how it went (cancel: facade node reboot {name} --cancel)",
            utc_time(at)
        ),
        None => eprintln!(
            "{name} reboots in a few seconds; Discord hears when it is back (or if it is not within 10 minutes), and `facade activity --kind maintenance` shows it"
        ),
    }
    Ok(())
}

/// What a reboot stops and fixes.
fn show(plan: &RebootPlan, at: Option<u64>) {
    let name = &plan.node;
    if plan.reboot_required {
        if plan.reasons.is_empty() {
            println!("{name} asks for a reboot");
        } else {
            println!("{name} asks for a reboot: {}", plan.reasons.join(", "));
        }
    } else {
        println!("{name} does not ask for a reboot");
    }
    if !plan.fixes.is_empty() {
        let shown: Vec<_> = plan.fixes.iter().take(3).cloned().collect();
        let more = plan.fixes.len().saturating_sub(3);
        println!(
            "it fixes {}{}",
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        );
    }
    if plan.apps.is_empty() {
        println!("no app runs there");
    } else {
        println!(
            "stops for a minute or two, then starts again: {}",
            plan.apps.join(", ")
        );
    }
    if !plan.builds.is_empty() {
        match at {
            Some(_) => println!(
                "building now: {} (a scheduled reboot waits for builds, up to 30 minutes)",
                plan.builds.join(", ")
            ),
            None => println!("building now, and cut short: {}", plan.builds.join(", ")),
        }
    }
    if plan.local {
        println!(
            "this is the server's own node: facade (web UI, CLI, alerts) goes down with it and posts to Discord once back"
        );
    }
    if let Some(s) = &plan.scheduled {
        println!(
            "already scheduled for {} by {} (replaced)",
            utc_time(s.at),
            s.by
        );
    }
}

pub async fn maintenance(
    api: &Api,
    name: &str,
    change: WindowChange,
    json: bool,
) -> anyhow::Result<()> {
    let path = format!("/api/nodes/{name}/reboot");
    let mut plan: RebootPlan = api.get(&path).await?;
    if change.skip {
        plan = api
            .json(
                api.request(Method::DELETE, &path)
                    .query(&[("window", true)]),
            )
            .await?;
        if !json {
            eprintln!("the next maintenance of {name} is skipped");
        }
    } else if change.off {
        plan = api
            .json(
                api.request(Method::PUT, &format!("/api/nodes/{name}/maintenance"))
                    .json(&SetMaintenance { window: None }),
            )
            .await?;
        if !json {
            eprintln!("maintenance window of {name} turned off");
        }
    } else if change.any() {
        let mut w = plan.maintenance.clone().unwrap_or(MaintenanceWindow {
            days: vec![6],
            hour: 4,
            minute: 0,
            updates: plan.updates_unavailable.is_none(),
            reboot: true,
        });
        if let Some(days) = &change.days {
            w.days = parse_days(days)?;
        }
        if let Some(at) = &change.at {
            let (h, m) = parse_hour(at)?;
            (w.hour, w.minute) = (h, m);
        }
        if let Some(u) = change.updates {
            w.updates = u;
        }
        if let Some(r) = change.reboot {
            w.reboot = r;
        }
        plan = api
            .json(
                api.request(Method::PUT, &format!("/api/nodes/{name}/maintenance"))
                    .json(&SetMaintenance { window: Some(w) }),
            )
            .await?;
    }
    if json {
        return print_json(&plan);
    }
    match &plan.maintenance {
        Some(w) => {
            println!("maintenance window: {}, {}", w.when(), w.what());
            if let Some(next) = plan.next_window {
                println!("next: {}", utc_time(next));
            }
            println!(
                "Discord hears of it 15 minutes before when there is something to do, and of how it went"
            );
        }
        None => println!(
            "no maintenance window (set one: facade node maintenance {name} --days sun --at 04:00)"
        ),
    }
    if let Some(why) = &plan.updates_unavailable {
        println!("updates: {why}");
    } else if let Some(n) = plan.updates {
        println!("pending updates: {n}");
    }
    if plan.reboot_required {
        println!("a reboot is required now");
    }
    if let Some(s) = &plan.scheduled {
        println!("reboot scheduled for {} by {}", utc_time(s.at), s.by);
    }
    if let Some(last) = &plan.last {
        println!(
            "last: {} ({}) {}",
            if last.ok { "ok" } else { "problem" },
            ago(last.at),
            last.summary
        );
    }
    Ok(())
}

/// What `facade node maintenance` changes.
pub struct WindowChange {
    pub days: Option<String>,
    pub at: Option<String>,
    pub updates: Option<bool>,
    pub reboot: Option<bool>,
    pub off: bool,
    pub skip: bool,
}

impl WindowChange {
    fn any(&self) -> bool {
        self.days.is_some() || self.at.is_some() || self.updates.is_some() || self.reboot.is_some()
    }
}

/// `daily`, `sun`, `mon,thu`, `sat-sun`.
fn parse_days(s: &str) -> anyhow::Result<Vec<u8>> {
    let s = s.trim().to_ascii_lowercase();
    if matches!(s.as_str(), "daily" | "every" | "all" | "*") {
        return Ok(vec![]);
    }
    let day = |d: &str| -> anyhow::Result<u8> {
        let d = d.trim();
        MaintenanceWindow::DAY_NAMES
            .iter()
            .position(|n| d.len() >= 3 && n.starts_with(&d[..3]))
            .map(|i| i as u8)
            .ok_or_else(|| anyhow::anyhow!("`{d}`: a day is mon, tue, wed, thu, fri, sat or sun"))
    };
    let mut days = vec![];
    for part in s.split(',') {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (day(a)?, day(b)?);
                let mut d = a;
                loop {
                    days.push(d);
                    if d == b {
                        break;
                    }
                    d = (d + 1) % 7;
                }
            }
            None => days.push(day(part)?),
        }
    }
    days.sort_unstable();
    days.dedup();
    Ok(days)
}

/// `04:00`, `4`.
fn parse_hour(s: &str) -> anyhow::Result<(u8, u8)> {
    let s = s.trim().trim_end_matches("UTC").trim();
    let (h, m) = s.split_once(':').unwrap_or((s, "0"));
    match (h.parse::<u8>(), m.parse::<u8>()) {
        (Ok(h), Ok(m)) if h < 24 && m < 60 => Ok((h, m)),
        _ => bail!("`{s}`: a time is HH:MM (UTC)"),
    }
}

/// `04:00` (the next one, UTC), `2026-11-15 04:00` (UTC), `+30m`, `+2h`.
fn parse_time(s: &str, now: u64) -> anyhow::Result<u64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('+') {
        let (n, unit) = rest.split_at(
            rest.find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len()),
        );
        let n: u64 = n
            .parse()
            .map_err(|_| anyhow::anyhow!("`{s}`: say +30m or +2h"))?;
        let secs = match unit {
            "m" | "min" => n * 60,
            "h" => n * 3600,
            "d" => n * 86_400,
            _ => bail!("`{s}`: say +30m or +2h"),
        };
        return Ok(now + secs);
    }
    if let Some((date, time)) = s.split_once([' ', 'T']) {
        let parts: Vec<_> = date.split('-').collect();
        let [y, mo, d] = parts.as_slice() else {
            bail!("`{s}`: a date is YYYY-MM-DD HH:MM (UTC)");
        };
        let (Ok(y), Ok(mo), Ok(d)) = (y.parse::<i64>(), mo.parse::<u32>(), d.parse::<u32>()) else {
            bail!("`{s}`: a date is YYYY-MM-DD HH:MM (UTC)");
        };
        let (h, m) = parse_hour(time)?;
        let day = days_from_civil(y, mo, d);
        if day < 0 {
            bail!("`{s}` has passed");
        }
        return Ok(day as u64 * 86_400 + u64::from(h) * 3600 + u64::from(m) * 60);
    }
    let (h, m) = parse_hour(s)?;
    let today = now / 86_400 * 86_400 + u64::from(h) * 3600 + u64::from(m) * 60;
    Ok(if today > now { today } else { today + 86_400 })
}

/// Days since the epoch of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    // Saturday 2026-11-14 10:00 UTC.
    const NOW: u64 = 1_794_614_400 + 10 * 3600;

    #[test]
    fn times_are_utc() {
        assert_eq!(
            utc_time(parse_time("04:00", NOW).unwrap()),
            "Sun 15 Nov 04:00 UTC"
        );
        assert_eq!(
            utc_time(parse_time("23:30", NOW).unwrap()),
            "Sat 14 Nov 23:30 UTC"
        );
        assert_eq!(
            utc_time(parse_time("2026-11-20 03:15", NOW).unwrap()),
            "Fri 20 Nov 03:15 UTC"
        );
        assert_eq!(parse_time("+2h", NOW).unwrap(), NOW + 7200);
        assert!(parse_time("25:00", NOW).is_err());
    }

    #[test]
    fn days_read_as_lists_and_ranges() {
        assert_eq!(parse_days("daily").unwrap(), Vec::<u8>::new());
        assert_eq!(parse_days("sun").unwrap(), vec![6]);
        assert_eq!(parse_days("Mon,thursday").unwrap(), vec![0, 3]);
        assert_eq!(parse_days("sat-mon").unwrap(), vec![0, 5, 6]);
        assert!(parse_days("someday").is_err());
    }
}
