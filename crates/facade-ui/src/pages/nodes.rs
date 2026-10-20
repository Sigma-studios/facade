use std::time::Duration;

use facade_core::api::{CreateNode, CreatedNode, NodeView, SetNodeIp};
use facade_core::proto::Listener;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;

use super::overview::NodeCard;
use crate::api;
use crate::components::{CopyField, ErrorNote, Loading, Modal};
use crate::util::{polled, run, toasts};

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
    // Whose listening sockets the Ports section shows; the first node until one is picked.
    let ports_node = RwSignal::new(None::<String>);
    let ports_of = Memo::new(move |_| {
        ports_node.get().or_else(|| {
            nodes
                .data
                .with(|n| n.as_ref().and_then(|n| n.first().map(|n| n.name.clone())))
        })
    });
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
                        key=|n| (n.name.clone(), n.connected, n.last_seen, n.public_ip.clone(), n.stats.as_ref().map(|s| s.mem_used), n.ingress_errors.clone())
                        let:node>
                        {
                            let name = node.name.clone();
                            let name2 = node.name.clone();
                            let name3 = node.name.clone();
                            let name4 = node.name.clone();
                            let current_ip = node.public_ip.clone().unwrap_or_default();
                            let local = node.local;
                            view! {
                                <div class="node-wrap">
                                    <NodeCard node=node/>
                                    <div class="card-actions">
                                        <button class="link-button" on:click=move |_| {
                                            ip_target.set(Some(name3.clone()));
                                            ip_value.set(current_ip.clone());
                                            ip_open.set(true);
                                        }>"Public IP"</button>
                                        <button class="link-button" on:click=move |_| ports_node.set(Some(name4.clone()))>"Ports"</button>
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
                <Show when=move || ports_of.with(Option::is_some)>
                    <Ports
                        node=Signal::derive(move || ports_of.get().unwrap_or_default())
                        nodes=Signal::derive(move || nodes.data.get().unwrap_or_default().into_iter().map(|n| n.name).collect::<Vec<_>>())
                        on_pick=move |n| ports_node.set(Some(n))/>
                </Show>
            </Show>
        </div>

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
                <p>"On the new server (with the façade release folder), install the agent as a service:"</p>
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

/// What listens on a node, and which app (or facade itself) holds each port.
#[component]
fn Ports(
    #[prop(into)] node: Signal<String>,
    #[prop(into)] nodes: Signal<Vec<String>>,
    on_pick: impl Fn(String) + Send + Sync + 'static,
) -> impl IntoView {
    let listeners = polled(Some(Duration::from_secs(10)), move || {
        let node = node.get();
        async move { api::get::<Vec<Listener>>(&format!("/api/nodes/{node}/ports")).await }
    });
    view! {
        <section class="node-ports">
            <div class="toolbar">
                <h2 class="section-title">"Ports"</h2>
                <label class="inline-field">
                    "on"
                    <select on:change=move |ev| on_pick(event_target_value(&ev))
                        prop:value=move || node.get()>
                        <For each=move || nodes.get() key=|n| n.clone() let:n>
                            <option value=n.clone()
                                selected={
                                    let name = n.clone();
                                    move || node.with(|x| *x == name)
                                }>{n.clone()}</option>
                        </For>
                    </select>
                </label>
            </div>
            <p class="muted small">"Every TCP and UDP port open on the node, façade's or not. A port held by another program cannot be published by an app."</p>
            <ErrorNote error=listeners.error/>
            <Show when=move || listeners.loaded() fallback=Loading>
                <Show when=move || listeners.data.with(|l| l.as_ref().is_some_and(|l| !l.is_empty()))
                    fallback=|| view! { <div class="empty"><p>"Nothing is listening."</p></div> }>
                    <div class="table-wrap">
                        <table class="table">
                            <thead>
                                <tr>
                                    <th class="num">"Port"</th>
                                    <th>"Protocol"</th>
                                    <th>"Address"</th>
                                    <th>"Process"</th>
                                    <th>"Used by"</th>
                                </tr>
                            </thead>
                            <tbody>
                                <For each=move || listeners.data.get().unwrap_or_default()
                                    key=|l| (l.protocol, l.address.clone(), l.port, l.pid, l.app.clone())
                                    let:l>
                                    <tr>
                                        <td class="num">{l.port}</td>
                                        <td>{l.protocol.to_string()}</td>
                                        <td><code>{l.address.clone()}</code></td>
                                        <td class="muted">
                                            {match (l.process.clone(), l.pid) {
                                                (Some(p), Some(pid)) => format!("{p} ({pid})"),
                                                (Some(p), None) => p,
                                                (None, Some(pid)) => format!("pid {pid}"),
                                                (None, None) => "—".into(),
                                            }}
                                        </td>
                                        <td>
                                            {match l.app.clone() {
                                                Some(app) if app == "facade" => view! { <span>"façade"</span> }.into_any(),
                                                Some(app) => view! {
                                                    <A href=format!("/apps/{app}") attr:class="strong-link">{app.clone()}</A>
                                                }.into_any(),
                                                None => view! { <span class="muted">"another program"</span> }.into_any(),
                                            }}
                                        </td>
                                    </tr>
                                </For>
                            </tbody>
                        </table>
                    </div>
                </Show>
            </Show>
        </section>
    }
}
