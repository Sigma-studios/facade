//! A node's Reboot dialog: what a reboot stops and fixes, rebooting now or at
//! a time, and its maintenance window (updates, then a reboot when required).

use facade_core::api::{MaintenanceWindow, RebootPlan, RebootRequest, SetMaintenance, utc_time};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::{Ago, ErrorNote, Loading, Modal};
use crate::util::toasts;

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// The browser's offset from UTC, in minutes (positive east of Greenwich).
fn local_offset_minutes() -> i64 {
    -(js_sys::Date::new_0().get_timezone_offset() as i64)
}

/// `05:00 your time` for a UTC hour and minute.
fn local_hint(hour: u8, minute: u8) -> String {
    let offset = local_offset_minutes();
    if offset == 0 {
        return "UTC is your time zone too.".into();
    }
    let t = (i64::from(hour) * 60 + i64::from(minute) + offset).rem_euclid(24 * 60);
    format!("{:02}:{:02} your time.", t / 60, t % 60)
}

/// `YYYY-MM-DDTHH:MM` from a `datetime-local` input (the browser's time) to
/// seconds since the epoch.
fn parse_local(value: &str) -> Option<u64> {
    let ms = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(value)).get_time();
    (ms.is_finite() && ms > 0.0).then(|| (ms / 1000.0) as u64)
}

/// `HH:MM` to hour and minute.
fn parse_hhmm(value: &str) -> Option<(u8, u8)> {
    let (h, m) = value.split_once(':')?;
    let (h, m) = (h.parse::<u8>().ok()?, m.get(..2)?.parse::<u8>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

#[component]
pub fn RebootDialog(#[prop(into)] node: Signal<String>, open: RwSignal<bool>) -> impl IntoView {
    let plan = RwSignal::new(None::<RebootPlan>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let confirm_now = RwSignal::new(false);
    let at = RwSignal::new(String::new());
    // The window being edited.
    let days = RwSignal::new(vec![6u8]);
    let time = RwSignal::new("04:00".to_string());
    let updates = RwSignal::new(true);
    let reboot = RwSignal::new(true);

    let fill = move |p: RebootPlan| {
        match &p.maintenance {
            Some(w) => {
                days.set(w.days.clone());
                time.set(format!("{:02}:{:02}", w.hour, w.minute));
                updates.set(w.updates);
                reboot.set(w.reboot);
            }
            None => {
                days.set(vec![6]);
                time.set("04:00".into());
                updates.set(p.updates_unavailable.is_none());
                reboot.set(true);
            }
        }
        plan.set(Some(p));
    };
    let load = move || {
        let name = node.get_untracked();
        spawn_local(async move {
            match api::get::<RebootPlan>(&format!("/api/nodes/{name}/reboot")).await {
                Ok(p) => {
                    error.try_set(None);
                    fill(p);
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
        });
    };
    Effect::new(move |_| {
        if open.get() {
            plan.set(None);
            confirm_now.set(false);
            at.set(String::new());
            load();
        }
    });

    // Every action answers with the new plan.
    let act =
        move |done: String,
              call: std::pin::Pin<Box<dyn Future<Output = Result<RebootPlan, String>>>>| {
            busy.set(true);
            spawn_local(async move {
                match call.await {
                    Ok(p) => {
                        toasts().ok(done);
                        fill(p);
                        confirm_now.try_set(false);
                    }
                    Err(e) => toasts().err(e),
                }
                busy.try_set(false);
            });
        };
    let reboot_now = move |_| {
        let name = node.get_untracked();
        act(
            format!("{name} reboots in a few seconds"),
            Box::pin(async move {
                api::post(
                    &format!("/api/nodes/{name}/reboot"),
                    &RebootRequest { at: None },
                )
                .await
            }),
        );
    };
    let schedule = move |_| {
        let name = node.get_untracked();
        let Some(when) = parse_local(&at.get_untracked()) else {
            toasts().err("Pick a date and time");
            return;
        };
        act(
            format!("Reboot of {name} scheduled for {}", utc_time(when)),
            Box::pin(async move {
                api::post(
                    &format!("/api/nodes/{name}/reboot"),
                    &RebootRequest { at: Some(when) },
                )
                .await
            }),
        );
    };
    let cancel = move |window: bool| {
        let name = node.get_untracked();
        let done = if window {
            "Next maintenance skipped"
        } else {
            "Scheduled reboot cancelled"
        };
        busy.set(true);
        spawn_local(async move {
            let path = format!("/api/nodes/{name}/reboot?window={window}");
            match api::delete(&path).await {
                Ok(()) => {
                    toasts().ok(done);
                    load();
                }
                Err(e) => toasts().err(e),
            }
            busy.try_set(false);
        });
    };
    let save_window = move |on: bool| {
        let name = node.get_untracked();
        let window = if on {
            let Some((hour, minute)) = parse_hhmm(&time.get_untracked()) else {
                toasts().err("The time is HH:MM");
                return;
            };
            let mut d = days.get_untracked();
            d.sort_unstable();
            if d.len() == 7 {
                d.clear();
            }
            Some(MaintenanceWindow {
                days: d,
                hour,
                minute,
                updates: updates.get_untracked(),
                reboot: reboot.get_untracked(),
            })
        } else {
            None
        };
        let done = if on {
            "Maintenance window saved"
        } else {
            "Maintenance window turned off"
        };
        act(
            done.into(),
            Box::pin(async move {
                api::put(
                    &format!("/api/nodes/{name}/maintenance"),
                    &SetMaintenance { window },
                )
                .await
            }),
        );
    };

    let p = move || plan.get();
    let unavailable = move || p().and_then(|p| p.unavailable);
    let window_hint = move || {
        parse_hhmm(&time.get())
            .map(|(h, m)| format!("UTC: {}", local_hint(h, m)))
            .unwrap_or_default()
    };

    view! {
        <Modal open title="Reboot and maintenance">
            <ErrorNote error/>
            <Show when=move || plan.with(Option::is_some) fallback=Loading>
                {move || p().map(|p| view! { <PlanFacts plan=p/> })}

                <Show when=move || p().is_some_and(|p| p.running.is_some())>
                    <div class="note note-info">
                        {move || p().and_then(|p| p.running).map(|r| match r.back {
                            None => view! {
                                {format!("Rebooting: asked by {} ", r.by)} <Ago at=r.asked/> ". Waiting for it to come back."
                            }.into_any(),
                            Some(_) => view! {
                                {format!("Back from its reboot (asked by {} ", r.by)} <Ago at=r.asked/> "): checking that its apps run again."
                            }.into_any(),
                        })}
                    </div>
                </Show>

                <h3 class="section-title">"Reboot"</h3>
                <Show when=move || p().is_some_and(|p| p.scheduled.is_some())>
                    <div class="form-row">
                        <p class="grow">
                            {move || p().and_then(|p| p.scheduled).map(|s| format!(
                                "Scheduled for {} by {}. Discord hears of it 15 minutes before.", utc_time(s.at), s.by
                            ))}
                        </p>
                        <button class="button" disabled=busy on:click=move |_| cancel(false)>"Cancel it"</button>
                    </div>
                </Show>
                <Show when=move || unavailable().is_some()>
                    <p class="muted small">{move || unavailable().map(|u| format!("Rebooting now is not possible: {u}."))}</p>
                </Show>
                <div class="form-row">
                    <Show
                        when=move || confirm_now.get()
                        fallback=move || view! {
                            <button class="button button-danger" disabled=move || busy.get() || unavailable().is_some()
                                    || p().is_some_and(|p| p.running.is_some_and(|r| {
                                        r.back.is_some() || crate::util::now().saturating_sub(r.asked) < 600
                                    }))
                                on:click=move |_| confirm_now.set(true)>"Reboot now…"</button>
                        }
                    >
                        <button class="button button-danger" disabled=busy on:click=reboot_now>"Yes, reboot now"</button>
                        <button class="button" on:click=move |_| confirm_now.set(false)>"Cancel"</button>
                    </Show>
                    <input type="datetime-local" bind:value=at title="In your time"/>
                    <button class="button" disabled=move || busy.get() || at.with(String::is_empty)
                        on:click=schedule>"Schedule"</button>
                </div>
                <p class="muted small">
                    "Discord hears of every reboot as it starts and of how it went: back, apps running, reboot applied. A node not back within 10 minutes is a problem. A scheduled reboot waits for running builds (up to 30 minutes)."
                </p>

                <h3 class="section-title">"Maintenance window"</h3>
                <p class="muted small">
                    "On these days, at this time, façade installs the pending updates and then reboots the node if a reboot is required. Discord hears of it 15 minutes before when there is something to do, and of how it went."
                </p>
                <div class="form-row">
                    {DAYS.iter().enumerate().map(|(i, label)| {
                        let i = i as u8;
                        view! {
                            <label class="check">
                                <input type="checkbox"
                                    prop:checked=move || days.with(|d| d.is_empty() || d.contains(&i))
                                    on:change=move |ev| {
                                        let on = event_target_checked(&ev);
                                        days.update(|d| {
                                            if d.is_empty() {
                                                *d = (0..7).collect();
                                            }
                                            d.retain(|x| *x != i);
                                            if on {
                                                d.push(i);
                                            }
                                        });
                                    }/>
                                {*label}
                            </label>
                        }
                    }).collect_view()}
                </div>
                <div class="form-row">
                    <label class="field">
                        <span>"Time (UTC)"</span>
                        <input type="time" bind:value=time required/>
                        <small>{window_hint}</small>
                    </label>
                </div>
                <label class="check">
                    <input type="checkbox" bind:checked=updates
                        disabled=move || p().is_some_and(|p| p.updates_unavailable.is_some())/>
                    "Install pending updates first"
                    {move || p().and_then(|p| p.updates_unavailable).map(|why| view! {
                        <span class="muted small">{format!(" ({why})")}</span>
                    })}
                </label>
                <label class="check">
                    <input type="checkbox" bind:checked=reboot/>
                    "Reboot when a reboot is required"
                </label>
                <Show when=move || p().is_some_and(|p| p.maintenance.is_some())>
                    <p class="small">
                        {move || p().and_then(|p| Some(format!(
                            "Now: {}, {}. Next: {}.",
                            p.maintenance.as_ref()?.when(),
                            p.maintenance.as_ref()?.what(),
                            p.next_window.map(utc_time).unwrap_or_default()
                        )))}
                    </p>
                </Show>
                <div class="form-actions">
                    <Show when=move || p().is_some_and(|p| p.maintenance.is_some())>
                        <button class="button" disabled=busy on:click=move |_| cancel(true)>"Skip the next one"</button>
                        <button class="button" disabled=busy on:click=move |_| save_window(false)>"Turn off"</button>
                    </Show>
                    <button class="button button-primary" disabled=busy on:click=move |_| save_window(true)>
                        {move || if p().is_some_and(|p| p.maintenance.is_some()) { "Save" } else { "Turn on" }}
                    </button>
                </div>
                {move || p().and_then(|p| p.last).map(|last| view! {
                    <p class=if last.ok { "muted small" } else { "note note-warning small" }>
                        "Last (" <Ago at=last.at/> "): " {last.summary}
                    </p>
                })}
                <super::reboot_timeline::PastReboots node/>
            </Show>
        </Modal>
    }
}

/// Why it asks for a reboot, what that fixes, what stops.
#[component]
fn PlanFacts(plan: RebootPlan) -> impl IntoView {
    let required = if plan.reboot_required {
        if plan.reasons.is_empty() {
            "A reboot is required.".to_string()
        } else {
            format!("A reboot is required by {}.", plan.reasons.join(", "))
        }
    } else {
        "No reboot is required.".to_string()
    };
    let fixes = (!plan.fixes.is_empty()).then(|| {
        let shown: Vec<_> = plan.fixes.iter().take(3).cloned().collect();
        let more = plan.fixes.len().saturating_sub(3);
        format!(
            "It fixes {}{} (high and critical).",
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        )
    });
    let apps = if plan.apps.is_empty() {
        "No app runs there.".to_string()
    } else {
        format!(
            "Stops for a minute or two, then starts again by itself: {}.",
            plan.apps.join(", ")
        )
    };
    let updates = plan
        .updates
        .filter(|n| *n > 0)
        .map(|n| format!("{n} update{} pending.", if n == 1 { "" } else { "s" }));
    let builds = (!plan.builds.is_empty()).then(|| {
        format!(
            "Building now (a reboot cuts it short): {}.",
            plan.builds.join(", ")
        )
    });
    view! {
        <p>{required} " " {fixes} " " {updates}</p>
        <p class="muted small">{apps}</p>
        {builds.map(|b| view! { <div class="note note-warning">{b}</div> })}
        {plan.local.then(|| view! {
            <div class="note note-info">
                "This is façade's own server: the web UI goes down with it and comes back by itself. Discord hears when it is back; no message within 15 minutes means it did not come back (check it from your provider's console)."
            </div>
        })}
    }
}
