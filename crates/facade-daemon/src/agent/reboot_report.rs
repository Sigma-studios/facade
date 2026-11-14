//! After a reboot, how it went: the agent's first start on a new boot measures
//! the shutdown and the way back, and sends a [`RebootReport`] once the apps are
//! back (or after [`WAIT`]). The server puts it on the reboot's Activity entry.
//!
//! What survives a reboot to tell it:
//! * Docker keeps each container's last stop (`FinishedAt`) and its new start;
//!   its exit code is reset when it starts again, so a kill is read from
//!   dockerd's own lines in the journal ("failed to exit within 10s of signal
//!   15 - using the force"), which also say when the shutdown reached Docker.
//! * The journal's last line before the new boot is about when the machine
//!   went off.
//! * A container's health log keeps its last five checks, enough to find the
//!   first one passing after it started, as the agent looks every few seconds.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bollard::models::{ContainerInspectResponse, RestartPolicyNameEnum};
use bollard::query_parameters::{InspectContainerOptions, ListContainersOptions};
use facade_core::events::{EventDetails, EventKind, NodeEvent, RebootApp, RebootReport, Severity};
use tracing::{info, warn};

use super::Agent;

/// How long apps get to come back before the report says they did not.
const WAIT: Duration = Duration::from_secs(5 * 60);
const POLL: Duration = Duration::from_secs(2);
/// Docker lines this long before the boot are read for its shutdown.
const JOURNAL_BACK: u64 = 3600;
/// Where the agent keeps the boot it last started on.
const MARK: &str = "last-boot";

/// On the first start since a reboot, reports how it went.
pub async fn run(agent: Arc<Agent>) {
    let Some(boot) = super::events::boot_time_ms() else {
        return;
    };
    let mark = agent.runtime.dir().join(MARK);
    let before = std::fs::read_to_string(&mark)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let remember = || {
        if let Err(e) = std::fs::write(&mark, boot.to_string()) {
            warn!("saving the boot time in {}: {e}", mark.display());
        }
    };
    // First start ever, or the agent restarted on the same boot.
    let Some(before) = before else {
        return remember();
    };
    if before.abs_diff(boot) < facade_core::reboot_report::SAME_BOOT_SECS * 1000 {
        return;
    }
    let report = measure(&agent, boot).await;
    info!(summary = %report.summary(), "after the reboot");
    let troubled = report.troubled();
    agent.note(NodeEvent {
        at: boot / 1000,
        kind: EventKind::Reboot,
        severity: if troubled {
            Severity::Warn
        } else {
            Severity::Ok
        },
        problem: troubled,
        app: None,
        on_node: true,
        summary: report.summary(),
        details: EventDetails {
            reboot: Some(report),
            ..Default::default()
        },
    });
    remember();
}

async fn measure(agent: &Agent, boot: u64) -> RebootReport {
    let shutdown = previous_shutdown(boot).await;
    let mut report = RebootReport {
        shutdown_at: shutdown.started,
        docker_stopped_at: shutdown.done,
        off_at: last_line_before(boot).await,
        boot_at: boot,
        docker_up_at: docker_up(boot).await,
        clean: shutdown.started.is_some(),
        ..Default::default()
    };
    let docker = agent.runtime.docker();
    let listed = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some([("label".to_string(), vec!["facade.app".to_string()])].into()),
            ..Default::default()
        }))
        .await
        .unwrap_or_default();
    // The containers that ran when the shutdown came: stopped by it, or (the
    // machine going down under them) marked stopped by Docker as it started.
    let since = shutdown
        .started
        .map_or(boot.saturating_sub(JOURNAL_BACK * 1000), |s| {
            s.saturating_sub(60_000)
        });
    let mut names = vec![];
    for c in listed {
        let Some(id) = c.id else { continue };
        let Ok(c) = docker
            .inspect_container(&id, None::<InspectContainerOptions>)
            .await
        else {
            continue;
        };
        if !restarts_by_itself(&c) {
            continue;
        }
        let finished = finished_at(&c);
        if !finished.is_some_and(|f| f >= since) {
            continue;
        }
        let stopped_at = finished.filter(|f| *f < boot);
        let name = c
            .name
            .as_deref()
            .unwrap_or(&id)
            .trim_start_matches('/')
            .to_string();
        let config = c.config.as_ref();
        let health = config
            .and_then(|c| c.healthcheck.as_ref())
            .and_then(|h| h.test.as_ref())
            .is_some_and(|t| t.first().is_some_and(|w| w != "NONE"))
            || c.state.as_ref().is_some_and(|s| s.health.is_some());
        report.apps.push(RebootApp {
            name: name.trim_start_matches("facade-").to_string(),
            app: config
                .and_then(|c| c.labels.as_ref())
                .and_then(|l| l.get("facade.app").cloned())
                .unwrap_or_default(),
            stopped_at,
            killed: stopped_at.is_some()
                && shutdown
                    .killed
                    .iter()
                    .any(|k| id.starts_with(k) || k.starts_with(&id)),
            stop_timeout: config
                .and_then(|c| c.stop_timeout)
                .filter(|t| *t >= 0)
                .map_or(facade_core::DEFAULT_STOP_TIMEOUT, |t| t as u32),
            health,
            ..Default::default()
        });
        names.push(name);
    }
    report.apps.sort_by(|a, b| a.name.cmp(&b.name));
    names.sort();

    // Then, until each is back (or time is up), when it started and passed
    // its health check: by name, as the agent may have replaced a container.
    let deadline = Instant::now() + WAIT;
    loop {
        for (app, name) in report.apps.iter_mut().zip(&names) {
            if app.ready_at().is_some() {
                continue;
            }
            let Ok(c) = docker
                .inspect_container(name, None::<InspectContainerOptions>)
                .await
            else {
                continue;
            };
            let state = c.state.as_ref();
            if !state.and_then(|s| s.running).unwrap_or(false) {
                continue;
            }
            let Some(started) = state
                .and_then(|s| s.started_at.as_deref())
                .and_then(ms)
                .filter(|s| *s >= boot)
            else {
                continue;
            };
            app.started_at = Some(started);
            if app.health {
                app.healthy_at = state
                    .and_then(|s| s.health.as_ref())
                    .and_then(|h| h.log.as_ref())
                    .and_then(|log| {
                        log.iter()
                            .filter(|r| r.exit_code == Some(0))
                            .filter_map(|r| {
                                let start = ms(&date(r.start.as_ref()?))?;
                                let end = r.end.as_ref().and_then(|e| ms(&date(e)));
                                (start >= started).then_some(end.unwrap_or(start))
                            })
                            .min()
                    });
            }
        }
        if report.apps.iter().all(|a| a.ready_at().is_some()) || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    report.at = facade_core::now() * 1000;
    report
}

/// Containers Docker starts again by itself: apps, not one-off runs.
fn restarts_by_itself(c: &ContainerInspectResponse) -> bool {
    matches!(
        c.host_config
            .as_ref()
            .and_then(|h| h.restart_policy.as_ref())
            .and_then(|p| p.name),
        Some(RestartPolicyNameEnum::UNLESS_STOPPED | RestartPolicyNameEnum::ALWAYS)
    )
}

fn finished_at(c: &ContainerInspectResponse) -> Option<u64> {
    c.state.as_ref()?.finished_at.as_deref().and_then(ms)
}

/// Milliseconds since the epoch of a Docker time; Docker's zero time is none.
fn ms(ts: &str) -> Option<u64> {
    let nanos = super::events::docker_time(ts)?;
    (nanos > 0).then_some((nanos / 1_000_000) as u64)
}

fn date(d: &bollard::models::BollardDate) -> String {
    d.to_string()
}

/// What dockerd logged of its last shutdown before `boot`.
#[derive(Debug, Default, PartialEq)]
struct Shutdown {
    started: Option<u64>,
    done: Option<u64>,
    /// Ids of the containers it had to kill.
    killed: HashSet<String>,
}

async fn previous_shutdown(boot: u64) -> Shutdown {
    read_shutdown(&before_boot(boot, &["-u", "docker.service"]).await)
}

/// Journal entries (matching `filter`) of the hour before `boot`. journalctl
/// takes whole seconds and a shutdown may end within the second the next boot
/// starts, so it is asked a second past it, the rest cut here.
async fn before_boot(boot: u64, filter: &[&str]) -> Vec<(u64, String)> {
    let secs = boot / 1000;
    let since = format!("--since=@{}", secs.saturating_sub(JOURNAL_BACK));
    let until = format!("--until=@{}", secs + 1);
    let mut args = filter.to_vec();
    args.extend([since.as_str(), until.as_str()]);
    let mut lines = journal(&args).await;
    lines.retain(|(t, _)| *t < boot);
    lines
}

/// The last `Processing signal 'terminated'` with nothing starting Docker
/// again after it, and what followed it.
fn read_shutdown(lines: &[(u64, String)]) -> Shutdown {
    let Some(start) = lines
        .iter()
        .rposition(|(_, m)| m.contains("Processing signal 'terminated'"))
    else {
        return Shutdown::default();
    };
    let after = &lines[start + 1..];
    if after.iter().any(|(_, m)| m.contains("API listen on")) {
        return Shutdown::default();
    }
    Shutdown {
        started: Some(lines[start].0),
        done: after
            .iter()
            .find(|(_, m)| m.contains("Daemon shutdown complete"))
            .map(|(t, _)| *t),
        killed: after
            .iter()
            .filter(|(_, m)| m.contains("failed to exit within"))
            .filter_map(|(_, m)| {
                let id = m.split("container=").nth(1)?;
                Some(
                    id.split(|c: char| !c.is_ascii_hexdigit())
                        .next()?
                        .to_string(),
                )
            })
            .filter(|id| !id.is_empty())
            .collect(),
    }
}

/// The last line the system logged before `boot`.
async fn last_line_before(boot: u64) -> Option<u64> {
    // The last line of the whole seconds before it, and the lines of the
    // second it started in that came before it.
    let secs = boot / 1000;
    let before = journal(&[
        &format!("--since=@{}", secs.saturating_sub(JOURNAL_BACK)),
        &format!("--until=@{secs}"),
        "-n",
        "1",
    ])
    .await;
    let same_second = journal(&[
        &format!("--since=@{secs}"),
        &format!("--until=@{}", secs + 1),
    ])
    .await;
    before
        .into_iter()
        .chain(same_second)
        .map(|(t, _)| t)
        .filter(|t| *t < boot)
        .max()
}

/// When Docker was ready after `boot`: its "API listen on" line.
async fn docker_up(boot: u64) -> Option<u64> {
    journal(&["-u", "docker.service", &format!("--since=@{}", boot / 1000)])
        .await
        .into_iter()
        .find(|(t, m)| *t >= boot && m.contains("API listen on"))
        .map(|(t, _)| t)
}

/// Journal entries as `(milliseconds, message)`, oldest first; none when the
/// journal is not there.
async fn journal(args: &[&str]) -> Vec<(u64, String)> {
    let Some(journalctl) = crate::platform::find_tool("journalctl") else {
        return vec![];
    };
    let out = tokio::process::Command::new(journalctl)
        .args(args)
        .args(["-o", "json", "--no-pager", "-q"])
        .output()
        .await;
    let Ok(out) = out else { return vec![] };
    parse_journal(&String::from_utf8_lossy(&out.stdout))
}

fn parse_journal(text: &str) -> Vec<(u64, String)> {
    text.lines()
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            let micros: u64 = v.get("__REALTIME_TIMESTAMP")?.as_str()?.parse().ok()?;
            let message = v
                .get("MESSAGE")
                .and_then(|m| m.as_str())
                .unwrap_or_default();
            Some((micros / 1000, message.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dockerds_shutdown() {
        let journal = parse_journal(concat!(
            r#"{"__REALTIME_TIMESTAMP":"1000000","MESSAGE":"time=\"x\" level=info msg=\"API listen on /run/docker.sock\""}"#,
            "\n",
            r#"{"__REALTIME_TIMESTAMP":"39000000","MESSAGE":"time=\"x\" level=info msg=\"Processing signal 'terminated'\""}"#,
            "\n",
            r#"{"__REALTIME_TIMESTAMP":"49000000","MESSAGE":"time=\"x\" level=info msg=\"Container failed to exit within 10s of signal 15 - using the force\" container=fde4154107bd"}"#,
            "\n",
            r#"{"__REALTIME_TIMESTAMP":"49300000","MESSAGE":"time=\"x\" level=info msg=\"Daemon shutdown complete\""}"#,
            "\n",
            "not json\n",
        ));
        assert_eq!(journal.len(), 4);
        let s = read_shutdown(&journal);
        assert_eq!(s.started, Some(39_000));
        assert_eq!(s.done, Some(49_300));
        assert_eq!(s.killed, HashSet::from(["fde4154107bd".to_string()]));
    }

    #[test]
    fn a_restart_of_docker_is_not_the_shutdown() {
        let journal = vec![
            (1, "Processing signal 'terminated'".to_string()),
            (2, "Daemon shutdown complete".to_string()),
            (3, "API listen on /run/docker.sock".to_string()),
        ];
        assert_eq!(read_shutdown(&journal), Shutdown::default());
        assert_eq!(read_shutdown(&[]), Shutdown::default());
    }
}
