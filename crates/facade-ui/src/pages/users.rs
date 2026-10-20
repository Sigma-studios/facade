//! Accounts: admins manage everything, members only their apps, within shared caps.

use std::time::Duration;

use facade_core::api::{CreateUser, NodeView, OneTimePassword, UpdateUser, UserView};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;

use crate::api;
use crate::components::{Amount, AmountSlider, CopyField, ErrorNote, Loading, Modal};
use crate::util::{bytes, cpus, me, polled, run, toasts};

/// Form values for a role and caps, shared by "Add user" and "Edit".
#[derive(Clone, Copy)]
struct CapsForm {
    role: RwSignal<String>,
    memory: RwSignal<String>,
    cpus: RwSignal<String>,
}

impl CapsForm {
    fn new() -> Self {
        Self {
            role: RwSignal::new("member".into()),
            memory: RwSignal::new(String::new()),
            cpus: RwSignal::new(String::new()),
        }
    }

    fn fill(&self, user: &UserView) {
        self.role.set(user.role.clone());
        self.memory.set(
            user.memory_cap_mb
                .map(|m| m.to_string())
                .unwrap_or_default(),
        );
        self.cpus
            .set(user.cpu_cap.map(|c| c.to_string()).unwrap_or_default());
    }

    /// `(role, memory cap, CPU cap)`; empty caps mean no limit.
    fn values(&self) -> Result<(String, Option<u64>, Option<f64>), String> {
        let memory = Some(self.memory.get_untracked().trim().to_string())
            .filter(|m| !m.is_empty())
            .map(|m| {
                m.parse::<u64>()
                    .map_err(|_| "the memory cap must be a number of MB".to_string())
            })
            .transpose()?;
        let cpus = Some(self.cpus.get_untracked().trim().to_string())
            .filter(|c| !c.is_empty())
            .map(|c| {
                c.parse::<f64>()
                    .map_err(|_| "the CPU cap must be a number".to_string())
            })
            .transpose()?;
        Ok((self.role.get_untracked(), memory, cpus))
    }
}

/// Memory (MB) and cores of every node that reports them, which bound the caps.
#[derive(Clone, Copy, Default, PartialEq)]
struct Totals {
    memory_mb: Option<f64>,
    cpus: Option<f64>,
}

impl Totals {
    fn of(nodes: &[NodeView]) -> Self {
        let stats = nodes.iter().filter_map(|n| n.stats.as_ref());
        let memory: u64 = stats.clone().map(|s| s.mem_total).sum();
        let cpus: usize = stats.map(|s| s.cpus).sum();
        Self {
            memory_mb: Some((memory / (1024 * 1024)) as f64).filter(|m| *m > 0.0),
            cpus: Some(cpus as f64).filter(|c| *c > 0.0),
        }
    }
}

#[component]
fn CapsFields(form: CapsForm, #[prop(into)] totals: Signal<Totals>) -> impl IntoView {
    let member = move || form.role.with(|r| r == "member");
    view! {
        <label class="field">
            <span>"Role"</span>
            <select on:change=move |ev| form.role.set(event_target_value(&ev))
                prop:value=move || form.role.get()>
                <option value="member" selected=member>"Member"</option>
                <option value="admin" selected=move || !member()>"Admin"</option>
            </select>
            <small>{move || if member() {
                "Manages only the apps they create or are given, within the caps below."
            } else {
                "Manages everything: nodes, domains, settings, users and every app."
            }}</small>
        </label>
        <Show when=member>
            <AmountSlider label="Memory cap" amount=Amount::Memory value=form.memory
                total=Signal::derive(move || totals.get().memory_mb)
                optional=true none_label="No cap" available=Signal::derive(|| None)/>
            <AmountSlider label="CPU cap" amount=Amount::Cpus value=form.cpus
                total=Signal::derive(move || totals.get().cpus)
                optional=true none_label="No cap" available=Signal::derive(|| None)/>
            <p class="muted small">
                "Shared by all their apps. With a cap, each of their apps needs its own limit."
            </p>
        </Show>
    }
}

#[component]
pub fn Users() -> impl IntoView {
    let me = me();
    let users = polled(Some(Duration::from_secs(15)), || async {
        api::get::<Vec<UserView>>("/api/users").await
    });
    let nodes = polled(None, || async {
        api::get::<Vec<NodeView>>("/api/nodes").await
    });
    let totals = Memo::new(move |_| {
        nodes
            .data
            .with(|n| n.as_deref().map(Totals::of).unwrap_or_default())
    });
    // The one-time password of a new or reset account, shown once.
    let issued = RwSignal::new(None::<OneTimePassword>);
    let issued_open = RwSignal::new(false);
    let show_password = move |p: OneTimePassword| {
        issued.set(Some(p));
        issued_open.set(true);
    };

    let add_open = RwSignal::new(false);
    let new_name = RwSignal::new(String::new());
    let add_form = CapsForm::new();
    let add_error = RwSignal::new(None::<String>);
    let open_add = move |_| {
        new_name.set(String::new());
        add_form.role.set("member".into());
        add_form.memory.set(String::new());
        add_form.cpus.set(String::new());
        add_error.set(None);
        add_open.set(true);
    };
    let add = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let (role, memory_cap_mb, cpu_cap) = match add_form.values() {
            Ok(v) => v,
            Err(e) => return add_error.set(Some(e)),
        };
        add_error.set(None);
        let body = CreateUser {
            username: new_name.get_untracked().trim().to_string(),
            memory_cap_mb: memory_cap_mb.filter(|_| role == "member"),
            cpu_cap: cpu_cap.filter(|_| role == "member"),
            role,
        };
        spawn_local(async move {
            match api::post::<_, OneTimePassword>("/api/users", &body).await {
                Ok(p) => {
                    add_open.set(false);
                    show_password(p);
                    users.reload();
                }
                Err(e) => {
                    add_error.set(Some(e));
                }
            }
        });
    };

    let edit_open = RwSignal::new(false);
    let edit_target = RwSignal::new(None::<String>);
    let edit_form = CapsForm::new();
    let edit_error = RwSignal::new(None::<String>);
    let open_edit = move |user: UserView| {
        edit_form.fill(&user);
        edit_target.set(Some(user.username));
        edit_error.set(None);
        edit_open.set(true);
    };
    let save_edit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let Some(name) = edit_target.get_untracked() else {
            return;
        };
        let (role, memory_cap_mb, cpu_cap) = match edit_form.values() {
            Ok(v) => v,
            Err(e) => return edit_error.set(Some(e)),
        };
        edit_error.set(None);
        let body = UpdateUser {
            memory_cap_mb: memory_cap_mb.filter(|_| role == "member"),
            cpu_cap: cpu_cap.filter(|_| role == "member"),
            role,
        };
        spawn_local(async move {
            match api::put::<_, UserView>(&format!("/api/users/{name}"), &body).await {
                Ok(_) => {
                    toasts().ok(format!("{name} saved"));
                    edit_open.set(false);
                    users.reload();
                    me.0.reload();
                }
                Err(e) => {
                    edit_error.set(Some(e));
                }
            }
        });
    };

    let reset_open = RwSignal::new(false);
    let reset_target = RwSignal::new(None::<String>);
    let reset = move |_| {
        let Some(name) = reset_target.get_untracked() else {
            return;
        };
        spawn_local(async move {
            match api::act_json::<OneTimePassword>(&format!("/api/users/{name}/password")).await {
                Ok(p) => {
                    reset_open.set(false);
                    show_password(p);
                    users.reload();
                }
                Err(e) => toasts().err(e),
            }
        });
    };

    let delete_open = RwSignal::new(false);
    let delete_target = RwSignal::new(None::<String>);
    let delete = move |_| {
        let Some(name) = delete_target.get_untracked() else {
            return;
        };
        run(
            format!("{name} deleted"),
            async move { api::delete(&format!("/api/users/{name}")).await },
            move || {
                delete_open.set(false);
                users.reload();
            },
        );
    };

    let password_value =
        Signal::derive(move || issued.get().map(|p| p.password).unwrap_or_default());

    view! {
        <div class="page">
            <header class="page-head">
                <h1>"Users"</h1>
                <button class="button button-primary" on:click=open_add>"Add user"</button>
            </header>
            <p class="muted lead">
                "Admins manage everything. Members manage only the apps they create or are given access to, "
                "build with their own registry and git logins, and share one memory and CPU cap."
            </p>
            <ErrorNote error=users.error/>
            <Show when=move || users.loaded() fallback=Loading>
                <div class="table-wrap">
                    <table class="table">
                        <thead>
                            <tr>
                                <th>"User"</th>
                                <th>"Role"</th>
                                <th>"Memory"</th>
                                <th>"CPUs"</th>
                                <th>"Owns"</th>
                                <th>"Shared with them"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            <For each=move || users.data.get().unwrap_or_default()
                                key=|u| (
                                    u.username.clone(), u.role.clone(), u.memory_cap_mb, u.cpu_cap.map(f64::to_bits),
                                    u.memory_allocated_mb, u.cpus_allocated.to_bits(), u.apps.clone(),
                                    u.shared_apps.clone(), u.must_change_password,
                                )
                                let:u>
                                {
                                    let edit_user = u.clone();
                                    let name_reset = u.username.clone();
                                    let name_delete = u.username.clone();
                                    let member = u.role != "admin";
                                    view! {
                                        <tr>
                                            <td>
                                                <strong>{u.username.clone()}</strong>
                                                {u.must_change_password.then(|| view! {
                                                    <div class="muted small">"new password pending"</div>
                                                })}
                                            </td>
                                            <td>{u.role.clone()}</td>
                                            <td>{move || if member {
                                                usage(
                                                    format!("{} MB", u.memory_allocated_mb),
                                                    u.memory_cap_mb.map(|c| bytes(c * 1024 * 1024)),
                                                    share(u.memory_cap_mb.map(|c| c as f64), totals.get().memory_mb),
                                                )
                                            } else { "—".into() }}</td>
                                            <td>{move || if member {
                                                usage(cpus(u.cpus_allocated), u.cpu_cap.map(cpus), share(u.cpu_cap, totals.get().cpus))
                                            } else { "—".into() }}</td>
                                            <td><AppLinks apps=u.apps.clone()/></td>
                                            <td><AppLinks apps=u.shared_apps.clone()/></td>
                                            <td class="row-actions">
                                                <button class="link-button" on:click=move |_| open_edit(edit_user.clone())>"Edit"</button>
                                                <button class="link-button" on:click=move |_| {
                                                    reset_target.set(Some(name_reset.clone()));
                                                    reset_open.set(true);
                                                }>"Reset password"</button>
                                                <button class="link-button danger" on:click=move |_| {
                                                    delete_target.set(Some(name_delete.clone()));
                                                    delete_open.set(true);
                                                }>"Delete"</button>
                                            </td>
                                        </tr>
                                    }
                                }
                            </For>
                        </tbody>
                    </table>
                </div>
            </Show>
        </div>

        <Modal open=add_open title="Add user">
            <form class="form" on:submit=add>
                <label class="field">
                    <span>"Name"</span>
                    <input type="text" bind:value=new_name required placeholder="alice"
                        pattern="[a-z0-9]([a-z0-9\\-]*[a-z0-9])?" autocomplete="off"/>
                    <small>"Lowercase letters, digits and dashes. Used to sign in."</small>
                </label>
                <CapsFields form=add_form totals=totals/>
                <ErrorNote error=add_error/>
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| add_open.set(false)>"Cancel"</button>
                    <button type="submit" class="button button-primary">"Create user"</button>
                </div>
            </form>
        </Modal>

        <Modal open=edit_open title="Edit user">
            <form class="form" on:submit=save_edit>
                <p class="muted small">{move || edit_target.get().unwrap_or_default()}</p>
                <CapsFields form=edit_form totals=totals/>
                <ErrorNote error=edit_error/>
                <div class="form-actions">
                    <button type="button" class="button" on:click=move |_| edit_open.set(false)>"Cancel"</button>
                    <button type="submit" class="button button-primary">"Save"</button>
                </div>
            </form>
        </Modal>

        <Modal open=issued_open title="One-time password">
            <p>
                "Hand this password to " <strong>{move || issued.get().map(|p| p.username).unwrap_or_default()}</strong>
                ". They sign in with it once and then choose their own. It is not shown again."
            </p>
            <CopyField value=password_value/>
            <div class="form-actions">
                <button class="button button-primary" on:click=move |_| {
                    issued_open.set(false);
                    issued.set(None);
                }>"Done"</button>
            </div>
        </Modal>

        <Modal open=reset_open title="Reset password">
            <p>
                "Give " <strong>{move || reset_target.get().unwrap_or_default()}</strong>
                " a new one-time password? Their current password stops working and they are signed out everywhere."
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| reset_open.set(false)>"Cancel"</button>
                <button class="button button-danger" on:click=reset>"Reset password"</button>
            </div>
        </Modal>

        <Modal open=delete_open title="Delete user">
            <p>
                "Delete " <strong>{move || delete_target.get().unwrap_or_default()}</strong>
                "? Their apps keep running and go back to the admins; their registry and git logins are deleted."
            </p>
            <div class="form-actions">
                <button class="button" on:click=move |_| delete_open.set(false)>"Cancel"</button>
                <button class="button button-danger" on:click=delete>"Delete"</button>
            </div>
        </Modal>
    }
}

/// `used of cap (share)`, or just `used` without a cap.
fn usage(used: String, cap: Option<String>, share: Option<String>) -> String {
    match (cap, share) {
        (Some(cap), Some(share)) => format!("{used} of {cap} ({share})"),
        (Some(cap), None) => format!("{used} of {cap}"),
        (None, _) => format!("{used}, no cap"),
    }
}

/// A cap as a percentage of what all the nodes have.
fn share(cap: Option<f64>, total: Option<f64>) -> Option<String> {
    Some(format!("{:.0} %", cap? * 100.0 / total?))
}

#[component]
fn AppLinks(apps: Vec<String>) -> impl IntoView {
    if apps.is_empty() {
        return view! { <span class="muted">"—"</span> }.into_any();
    }
    view! {
        <div class="domains">
            {apps.into_iter().map(|a| view! {
                <A href=format!("/apps/{a}")>{a.clone()}</A>
            }).collect_view()}
        </div>
    }
    .into_any()
}
