use std::time::Duration;

use facade_core::api::{CreateNode, CreatedNode, NodeView, SetNodeIp, VolumeView};
use facade_core::proto::StorageReport;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::overview::NodeCard;
use crate::api;
use crate::components::{CopyField, ErrorNote, Loading, Modal};
use crate::util::{ago, bytes, polled, run, toasts};

#[component]
pub fn Nodes() -> impl IntoView {
    let nodes = polled(Some(Duration::from_secs(5)), || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });

    let add_open = RwSignal::new(false);
    let new_name = RwSignal::new(String::new());
    let add_error = RwSignal::new(None::<String>);
    let joined = RwSignal::new(None::<CreatedNode>);
    let add = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        add_error.set(None);
        let body = CreateNode {
            name: new_name.get_untracked().trim().to_string(),
        };
        spawn_local(async move {
            match api::post::<_, CreatedNode>("/api/nodes", &body).await {
                Ok(created) => {
                    joined.set(Some(created));
                    nodes.reload();
                }
                Err(e) => add_error.set(Some(e)),
            }
        });
    };
    let close_add = move || {
        add_open.set(false);
        joined.set(None);
        new_name.set(String::new());
        add_error.set(None);
    };

    let token_for = move |name: String| {
        spawn_local(async move {
            match api::act_json::<CreatedNode>(&format!("/api/nodes/{name}/token")).await {
                Ok(created) => {
                    joined.set(Some(created));
                    add_open.set(true);
                }
                Err(e) => toasts().err(e),
            }
        });
    };
    let remove_target = RwSignal::new(None::<String>);
    let remove_open = RwSignal::new(false);
    let remove = move |_| {
        let Some(name) = remove_target.get_untracked() else {
            return;
        };
        run(
            format!("Node {name} removed"),
            async move { api::delete(&format!("/api/nodes/{name}")).await },
            move || {
                remove_open.set(false);
                nodes.reload();
            },
        );
    };
    let ip_open = RwSignal::new(false);
    let ip_target = RwSignal::new(None::<String>);
    let ip_value = RwSignal::new(String::new());
    let save_ip = move |auto: bool| {
        let Some(name) = ip_target.get_untracked() else {
            return;
        };
        let body = SetNodeIp {
            public_ip: (!auto).then(|| ip_value.get_untracked().trim().to_string()),
        };
        run(
            "Public IP saved; DNS records follow",
            async move { api::put_empty(&format!("/api/nodes/{name}/ip"), &body).await },
            move || {
                ip_open.set(false);
                nodes.reload();
            },
        );
    };
    let prune = move |name: String, aggressive: bool| {
        let path = format!("/api/nodes/{name}/prune?aggressive={aggressive}");
        run(
            format!("Cleaning up {name}; the figures update in a minute"),
            async move { api::act(&path).await },
            move || {},
        );
    };
    let volumes = polled(Some(Duration::from_secs(30)), || async {
        api::get::<Vec<VolumeView>>("/api/volumes").await
    });
    let volume_target = RwSignal::new(None::<VolumeView>);
    let volume_open = RwSignal::new(false);
    let delete_volume = move |_| {
        let Some(v) = volume_target.get_untracked() else {
            return;
        };
        let path = format!("/api/nodes/{}/volumes/{}", v.node, v.name);
        run(
            format!("{} deleted", v.name),
            async move { api::delete(&path).await },
            move || {
                volume_open.set(false);
                volumes.reload();
            },
        );
    };
    let join_command = Signal::derive(move || {
        joined
            .get()
            .map(|j| format!("facaded agent --join {}", j.join_token))
            .unwrap_or_default()
    });
    let install_command = Signal::derive(move || {
        joined
            .get()
            .map(|j| format!("./install.sh agent {}", j.join_token))
            .unwrap_or_default()
    });

    view! {
        <div class="page">
            <header class="page-head">
                <h1>"Nodes"</h1>
                <button class="button button-primary" on:click=move |_| add_open.set(true)>"Add node"</button>
            </header>
            <p class="muted lead">
                "Servers that run apps. Each one runs an agent that connects to this server and serves its own apps' domains: it needs Docker, and ports 80 and 443 open."
            </p>
            <ErrorNote error=nodes.error/>
            <Show when=move || nodes.loaded() fallback=Loading>
                <div class="grid-cards">
                    <For each=move || nodes.data.get().unwrap_or_default()
                        key=|n| (n.name.clone(), n.connected, n.last_seen, n.public_ip.clone(), n.stats.as_ref().map(|s| s.mem_used), n.storage.as_ref().map(|s| s.at))
                        let:node>
                        {
                            let name = node.name.clone();
                            let name2 = node.name.clone();
                            let name3 = node.name.clone();
                            let name4 = node.name.clone();
                            let name5 = node.name.clone();
                            let current_ip = node.public_ip.clone().unwrap_or_default();
                            let local = node.local;
                            let connected = node.connected;
                            let storage = node.storage.clone();
                            view! {
                                <div class="node-wrap">
                                    <NodeCard node=node/>
                                    {storage.map(|s| view! { <StorageLine storage=s/> })}
                                    <div class="card-actions">
                                        <Show when=move || connected>
                                            <button class="link-button" title="Remove dangling images and build cache unused for a week"
                                                on:click={
                                                    let name = name4.clone();
                                                    move |_| prune(name.clone(), false)
                                                }>"Clean up"</button>
                                            <button class="link-button" title="Also drop the whole build cache (next builds start from scratch) and the images kept for rollbacks"
                                                on:click={
                                                    let name = name5.clone();
                                                    move |_| prune(name.clone(), true)
                                                }>"Deep clean"</button>
                                        </Show>
                                        <button class="link-button" on:click=move |_| {
                                            ip_target.set(Some(name3.clone()));
                                            ip_value.set(current_ip.clone());
                                            ip_open.set(true);
                                        }>"Public IP"</button>
                                        <Show when=move || !local>
                                            <button class="link-button" on:click={
                                                let name = name.clone();
                                                move |_| token_for(name.clone())
                                            }>"New join token"</button>
                                            <button class="link-button danger" on:click={
                                                let name = name2.clone();
                                                move |_| {
                                                    remove_target.set(Some(name.clone()));
                                                    remove_open.set(true);
                                                }
                                            }>"Remove"</button>
                                        </Show>
                                    </div>
                                </div>
                            }
                        }
                    </For>
                </div>
                <p class="muted small after-cards">
                    "Nodes clean up after builds on their own: every 6 hours, and harder when the disk is 85 % full."
                </p>
            </Show>
            <section>
                <h2 class="section-title">"Volumes"</h2>
                <Show when=move || volumes.data.with(|v| v.as_ref().is_some_and(|v| !v.is_empty()))
                    fallback=|| view! { <p class="muted small">"No volumes yet. Apps get one when their settings list it (databases have one)."</p> }>
                    <div class="table-wrap">
                        <table class="table">
                            <thead><tr><th>"Volume"</th><th>"Node"</th><th>"App"</th><th class="num">"Size"</th><th>"State"</th><th></th></tr></thead>
                            <tbody>
                                <For each=move || volumes.data.get().unwrap_or_default()
                                    key=|v| (v.node.clone(), v.name.clone(), v.size, v.in_use, v.declared)
                                    let:v>
                                    {
                                        let target = v.clone();
                                        view! {
                                            <tr>
                                                <td><code>{v.name.clone()}</code></td>
                                                <td class="muted">{v.node.clone()}</td>
                                                <td>{v.app.clone()}</td>
                                                <td class="num">{v.size.map(bytes).unwrap_or_else(|| "—".into())}</td>
                                                <td>{if !v.declared {
                                                    view! { <span class="badge badge-neutral">"orphaned"</span> }.into_any()
                                                } else if v.in_use {
                                                    view! { <span class="badge badge-good">"in use"</span> }.into_any()
                                                } else {
                                                    view! { <span class="muted small">"not mounted"</span> }.into_any()
                                                }}</td>
                                                <td class="row-actions">
                                                    <Show when=move || !v.declared>
                                                        <button class="link-button danger" on:click={
                                                            let target = target.clone();
                                                            move |_| {
                                                                volume_target.set(Some(target.clone()));
                                                                volume_open.set(true);
                                                            }
                                                        }>"Delete"</button>
                                                    </Show>
                                                </td>
                                            </tr>
                                        }
                                    }
                                </For>
                            </tbody>
                        </table>
                    </div>
                    <p class="muted small">"Orphaned volumes belong to deleted apps, or were removed from an app's settings. Their data stays until you delete them."</p>
                </Show>
            </section>
        </div>

        <Modal open=volume_open title="Delete volume">
            <p>
                "Delete " <code>{move || volume_target.get().map(|v| v.name).unwrap_or_default()}</code>
                " and all the data in it? This cannot be undone."
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| volume_open.set(false)>"Cancel"</button>
                <button class="button button-danger" on:click=delete_volume>"Delete"</button>
            </div>
        </Modal>

        <Modal open=add_open title="Add a node">
            <Show
                when=move || joined.with(Option::is_some)
                fallback=move || view! {
                    <form class="form" on:submit=add>
                        <label class="field">
                            <span>"Name"</span>
                            <input type="text" bind:value=new_name required placeholder="eu-2"
                                pattern="[a-z0-9]([a-z0-9\\-]*[a-z0-9])?" autocomplete="off"/>
                            <small>"How apps refer to this server."</small>
                        </label>
                        <ErrorNote error=add_error/>
                        <div class="form-actions">
                            <button type="button" class="button" on:click=move |_| close_add()>"Cancel"</button>
                            <button type="submit" class="button button-primary">"Create join token"</button>
                        </div>
                    </form>
                }
            >
                <p>"On the new server (with the facade release folder), install the agent as a service:"</p>
                <CopyField value=install_command/>
                <p class="muted small">"Or run it directly:"</p>
                <CopyField value=join_command/>
                <p class="muted small">
                    "The token is shown once and lets that machine join as this node. Issue a new one if it leaks."
                </p>
                <div class="form-actions">
                    <button class="button button-primary" on:click=move |_| close_add()>"Done"</button>
                </div>
            </Show>
        </Modal>

        <Modal open=ip_open title="Public IP">
            <form class="form" on:submit=move |ev: leptos::ev::SubmitEvent| { ev.prevent_default(); save_ip(false) }>
                <p class="muted small">
                    "Domains of apps on " <strong>{move || ip_target.get().unwrap_or_default()}</strong>
                    " point here. It is detected from where the agent connects (or the server's public_host); "
                    "set it when that is not the address the internet reaches the node at."
                </p>
                <label class="field">
                    <span>"IP address"</span>
                    <input type="text" bind:value=ip_value placeholder="203.0.113.10" autocomplete="off" required/>
                </label>
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| save_ip(true)>"Detect automatically"</button>
                    <button type="submit" class="button button-primary">"Save"</button>
                </div>
            </form>
        </Modal>

        <Modal open=remove_open title="Remove node">
            <p>
                "Remove " <strong>{move || remove_target.get().unwrap_or_default()}</strong>
                "? Its apps must be moved or deleted first. Stop its agent afterwards ("
                <code>"systemctl disable --now facaded-agent"</code>")."
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| remove_open.set(false)>"Cancel"</button>
                <button class="button button-danger" on:click=remove>"Remove"</button>
            </div>
        </Modal>
    }
}

/// Docker's share of the node's disk, and the last cleanup.
#[component]
fn StorageLine(storage: StorageReport) -> impl IntoView {
    let cleanup = storage.last_cleanup.as_ref().map(|c| {
        format!(
            "{}cleaned {}, freed {}",
            if c.aggressive { "deep-" } else { "" },
            ago(c.at),
            bytes(c.freed)
        )
    });
    view! {
        <div class="storage-line small muted">
            {format!("Images {} · build cache {}", bytes(storage.images), bytes(storage.build_cache))}
            {(storage.build_cache_reclaimable > 0).then(|| format!(" ({} unused)", bytes(storage.build_cache_reclaimable)))}
            {(!storage.volumes.is_empty()).then(|| format!(" · {} volume{}", storage.volumes.len(), if storage.volumes.len() == 1 { "" } else { "s" }))}
            {cleanup.map(|c| view! { <br/>{c} })}
        </div>
    }
}
