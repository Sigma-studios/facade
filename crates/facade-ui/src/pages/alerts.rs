use std::time::Duration;

use facade_core::api::{AlertView, Mention, NotifySettings, NotifyUpdate};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::{ErrorNote, Loading};
use crate::util::{ago, me, polled, run, toasts};

#[component]
pub fn Alerts() -> impl IntoView {
    let me = me();
    let alerts = polled(Some(Duration::from_secs(10)), || async {
        api::get::<Vec<AlertView>>("/api/alerts").await
    });
    view! {
        <div class="page">
            <header class="page-head"><h1>"Alerts"</h1></header>
            <p class="muted lead">
                "Raised when a problem lasts a minute: a node offline, an app crashed or failing to deploy, "
                "disk ≥ 90 %, memory ≥ 95 %, an app at its memory or CPU limit, or an app writing over 100 MB inside its container (instead of a volume)."
            </p>
            <ErrorNote error=alerts.error/>
            <Show when=move || alerts.loaded() fallback=Loading>
                <Show when=move || alerts.data.with(|a| a.as_ref().is_some_and(|a| !a.is_empty()))
                    fallback=|| view! { <div class="empty"><p>"✓ Nothing to report."</p></div> }>
                    <ul class="alert-list">
                        <For each=move || alerts.data.get().unwrap_or_default()
                            key=|a| (a.key.clone(), a.since, a.resolved_at)
                            let:alert>
                            <li class=if alert.resolved_at.is_some() { "alert-item resolved" } else { "alert-item firing" }>
                                {if alert.resolved_at.is_some() {
                                    view! { <span class="badge badge-good"><span class="badge-icon" aria-hidden="true">"✓"</span>"resolved"</span> }
                                } else {
                                    view! { <span class="badge badge-critical"><span class="badge-icon" aria-hidden="true">"⚠"</span>"firing"</span> }
                                }}
                                <span class="alert-message">{alert.message.clone()}</span>
                                <span class="muted small">
                                    {match alert.resolved_at {
                                        Some(at) => format!("resolved {}", ago(at)),
                                        None => format!("since {}", ago(alert.since)),
                                    }}
                                </span>
                            </li>
                        </For>
                    </ul>
                </Show>
            </Show>
            <Show when=move || me.is_admin()>
                <DiscordSettings/>
            </Show>
        </div>
    }
}

#[component]
fn DiscordSettings() -> impl IntoView {
    let settings = polled(None, || async {
        api::get::<NotifySettings>("/api/alerts/settings").await
    });
    let webhook = RwSignal::new(String::new());
    let mentions = RwSignal::new(String::new());
    // Fill the mentions field once the settings arrive.
    Effect::new(move |_| {
        if let Some(s) = settings.data.get() {
            mentions.set(s.mentions.join(" "));
        }
    });
    let error = RwSignal::new(None::<String>);

    let save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let list: Vec<String> = mentions
            .get_untracked()
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect();
        if let Some(bad) = list.iter().find_map(|m| m.parse::<Mention>().err()) {
            return error.set(Some(bad));
        }
        error.set(None);
        let w = webhook.get_untracked().trim().to_string();
        let update = NotifyUpdate {
            discord_webhook: (!w.is_empty()).then_some(w),
            mentions: Some(list),
        };
        run(
            "Alert settings saved",
            async move { api::put_empty("/api/alerts/settings", &update).await },
            move || {
                webhook.set(String::new());
                settings.reload();
            },
        );
    };
    let remove_webhook = move |_| {
        let update = NotifyUpdate {
            discord_webhook: Some(String::new()),
            mentions: None,
        };
        run(
            "Webhook removed",
            async move { api::put_empty("/api/alerts/settings", &update).await },
            move || settings.reload(),
        );
    };
    let test = move |_| {
        spawn_local(async move {
            match api::act("/api/alerts/test").await {
                Ok(()) => toasts().ok("Test message sent. Check your Discord channel."),
                Err(e) => toasts().err(e),
            }
        });
    };
    let configured = move || {
        settings
            .data
            .with(|s| s.as_ref().is_some_and(|s| s.discord_webhook.is_some()))
    };

    view! {
        <section class="card form">
            <h2 class="section-title">"Discord notifications"</h2>
            <p class="muted small">
                "Problems and their resolutions are posted to a channel. Create a webhook in the channel's "
                "settings: Integrations → Webhooks → New Webhook → Copy URL."
            </p>
            <form class="form" on:submit=save>
                <label class="field">
                    <span>"Webhook URL"</span>
                    <input type="password" bind:value=webhook autocomplete="off"
                        placeholder=move || settings.data.get()
                            .and_then(|s| s.discord_webhook)
                            .unwrap_or_else(|| "https://discord.com/api/webhooks/…".into())/>
                    <small>{move || if configured() {
                        "Set. Leave empty to keep the current one."
                    } else {
                        "Not set: alerts are only written to the server log."
                    }}</small>
                </label>
                <label class="field">
                    <span>"Tag on problems"</span>
                    <input type="text" bind:value=mentions placeholder="@here user:123456789 role:987654321"/>
                    <small>
                        "@here, @everyone, user:<id> or role:<id>, separated by spaces. Resolutions never tag. "
                        "Copy ids with Discord's developer mode (right-click → Copy ID)."
                    </small>
                </label>
                <ErrorNote error=error/>
                <div class="form-actions">
                    <Show when=configured>
                        <button type="button" class="button button-danger-quiet" on:click=remove_webhook>
                            "Remove webhook"
                        </button>
                        <button type="button" class="button" on:click=test>"Send test"</button>
                    </Show>
                    <button type="submit" class="button button-primary">"Save"</button>
                </div>
            </form>
        </section>
    }
}
