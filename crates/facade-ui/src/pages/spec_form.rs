//! The app settings form, shared by "New app" and an app's Settings tab.

use facade_core::api::{
    AppView, ArtifactView, Candidate, CreateGitCredential, GitCredentialView, InspectReport,
    InspectRequest, NewGitSecret, NodeView,
};
use facade_core::{AppSpec, Build, Limits, Link, PortMap, Source, Volume};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;

use crate::api;
use crate::components::{CopyField, ErrorNote};
use crate::util::bytes;

#[derive(Clone, Copy)]
pub struct SpecForm {
    pub name: RwSignal<String>,
    node: RwSignal<String>,
    source: RwSignal<&'static str>,
    image: RwSignal<String>,
    repo: RwSignal<String>,
    branch: RwSignal<String>,
    build: RwSignal<&'static str>,
    dockerfile: RwSignal<String>,
    context: RwSignal<String>,
    rust_package: RwSignal<String>,
    rust_bin: RwSignal<String>,
    features: RwSignal<String>,
    no_default_features: RwSignal<bool>,
    static_dir: RwSignal<String>,
    http_port: RwSignal<String>,
    domains: RwSignal<String>,
    ports: RwSignal<String>,
    host_network: RwSignal<bool>,
    memory: RwSignal<String>,
    cpus: RwSignal<String>,
    enabled: RwSignal<bool>,
    command: RwSignal<String>,
    release: RwSignal<String>,
    health: RwSignal<String>,
    /// `name:/path`, one per line.
    volumes: RwSignal<String>,
    links: RwSignal<Vec<Link>>,
    artifact: RwSignal<Option<ArtifactView>>,
    exec_args: RwSignal<String>,
    scan: RwSignal<Option<InspectReport>>,
    scan_error: RwSignal<Option<String>>,
    scanning: RwSignal<bool>,
    /// Index of the scan candidate the build settings came from.
    chosen: RwSignal<Option<usize>>,
    /// What was last scanned, to avoid scanning the same thing twice.
    scanned: RwSignal<(String, String)>,
}

fn s(v: impl Into<String>) -> RwSignal<String> {
    RwSignal::new(v.into())
}

fn opt_text<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

impl SpecForm {
    pub fn new(node: String) -> Self {
        Self {
            name: s(""),
            node: s(node),
            source: RwSignal::new("git"),
            image: s(""),
            repo: s(""),
            branch: s(""),
            build: RwSignal::new("dockerfile"),
            dockerfile: s("Dockerfile"),
            context: s("."),
            rust_package: s(""),
            rust_bin: s(""),
            features: s(""),
            no_default_features: RwSignal::new(false),
            static_dir: s("."),
            http_port: s(""),
            domains: s(""),
            ports: s(""),
            host_network: RwSignal::new(false),
            memory: s(""),
            cpus: s(""),
            enabled: RwSignal::new(true),
            command: s(""),
            release: s(""),
            health: s(""),
            volumes: s(""),
            links: RwSignal::new(vec![]),
            artifact: RwSignal::new(None),
            exec_args: s(""),
            scan: RwSignal::new(None),
            scan_error: RwSignal::new(None),
            scanning: RwSignal::new(false),
            chosen: RwSignal::new(None),
            scanned: RwSignal::new(Default::default()),
        }
    }

    pub fn from_spec(spec: &AppSpec) -> Self {
        let form = Self::new(spec.node.clone());
        form.name.set(spec.name.clone());
        match &spec.source {
            Source::Image { image } => {
                form.source.set("image");
                form.image.set(image.clone());
            }
            Source::Executable {
                sha256,
                filename,
                arch,
                args,
            } => {
                form.source.set("executable");
                form.artifact.set(Some(ArtifactView {
                    sha256: sha256.clone(),
                    filename: filename.clone(),
                    arch: arch.clone(),
                    size: 0,
                }));
                form.exec_args.set(facade_core::join_command(args));
            }
            Source::Git {
                repo,
                branch,
                build,
            } => {
                form.source.set("git");
                form.repo.set(repo.clone());
                form.branch.set(branch.clone().unwrap_or_default());
                match build {
                    Build::Dockerfile {
                        dockerfile,
                        context,
                    } => {
                        form.build.set("dockerfile");
                        form.dockerfile.set(dockerfile.clone());
                        form.context.set(context.clone());
                    }
                    Build::Rust {
                        package,
                        bin,
                        features,
                        no_default_features,
                    } => {
                        form.build.set("rust");
                        form.rust_package.set(package.clone().unwrap_or_default());
                        form.rust_bin.set(bin.clone());
                        form.features.set(features.join(", "));
                        form.no_default_features.set(*no_default_features);
                    }
                    Build::Static { dir } => {
                        form.build.set("static");
                        form.static_dir.set(dir.clone());
                    }
                }
            }
        }
        form.http_port.set(opt_text(spec.http_port));
        form.domains.set(spec.domains.join("\n"));
        form.ports.set(
            spec.ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        form.host_network.set(spec.host_network);
        form.memory.set(opt_text(spec.limits.memory_mb));
        form.cpus.set(opt_text(spec.limits.cpus));
        form.enabled.set(spec.enabled);
        form.command.set(facade_core::join_command(&spec.command));
        form.release
            .set(spec.release_command.clone().unwrap_or_default());
        form.health.set(
            spec.healthcheck
                .as_ref()
                .map(facade_core::HealthCheck::command)
                .unwrap_or_default(),
        );
        form.volumes.set(
            spec.volumes
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        form.links.set(spec.links.clone());
        form
    }

    /// Picks `node` unless one is already chosen.
    pub fn default_node(&self, node: String) {
        if self.node.with_untracked(String::is_empty) {
            self.node.set(node);
        }
    }

    /// Fills the build settings (and the port, if empty) from a scan candidate.
    fn choose(&self, index: usize) {
        let Some(candidate) = self
            .scan
            .with_untracked(|r| r.as_ref().and_then(|r| r.candidates.get(index).cloned()))
        else {
            return;
        };
        self.chosen.set(Some(index));
        match candidate.build {
            Build::Dockerfile {
                dockerfile,
                context,
            } => {
                self.build.set("dockerfile");
                self.dockerfile.set(dockerfile);
                self.context.set(context);
            }
            Build::Rust {
                package,
                bin,
                features,
                no_default_features,
            } => {
                self.build.set("rust");
                self.rust_package.set(package.unwrap_or_default());
                self.rust_bin.set(bin);
                self.features.set(features.join(", "));
                self.no_default_features.set(no_default_features);
            }
            Build::Static { dir } => {
                self.build.set("static");
                self.static_dir.set(dir);
            }
        }
        if let Some(port) = candidate.http_port
            && self.http_port.with_untracked(String::is_empty)
        {
            self.http_port.set(port.to_string());
        }
    }

    /// Clones and inspects the repository on the server. With `apply`, the best
    /// candidate fills in the build settings.
    fn scan(&self, apply: bool) {
        let repo = self.repo.get_untracked().trim().to_string();
        let branch = self.branch.get_untracked().trim().to_string();
        if repo.is_empty() || self.scanning.get_untracked() {
            return;
        }
        if apply && self.scanned.get_untracked() == (repo.clone(), branch.clone()) {
            return;
        }
        let form = *self;
        form.scanning.set(true);
        form.scan_error.set(None);
        form.scanned.set((repo.clone(), branch.clone()));
        spawn_local(async move {
            let request = InspectRequest {
                repo,
                branch: Some(branch).filter(|b| !b.is_empty()),
            };
            match api::post::<_, InspectReport>("/api/inspect", &request).await {
                Ok(report) => {
                    let found = !report.candidates.is_empty();
                    form.scan.try_set(Some(report));
                    form.chosen.try_set(None);
                    if apply && found {
                        form.choose(0);
                    }
                }
                Err(e) => {
                    form.scan.try_set(None);
                    form.scan_error.try_set(Some(e));
                }
            }
            form.scanning.try_set(false);
        });
    }

    fn build_summary(&self) -> String {
        match self.build.get() {
            "rust" => {
                let mut s = format!("Rust binary {}", self.rust_bin.get());
                let features = self.features.get();
                if !features.trim().is_empty() {
                    s += &format!(" · features {features}");
                }
                if self.no_default_features.get() {
                    s += " · no default features";
                }
                s
            }
            "static" => format!("static files from {}", self.static_dir.get()),
            _ => format!("{} in {}", self.dockerfile.get(), self.context.get()),
        }
    }

    fn is_static(&self) -> bool {
        self.source.get() == "git" && self.build.get() == "static"
    }

    pub fn to_spec(self) -> Result<AppSpec, String> {
        let text = |v: RwSignal<String>| v.get_untracked().trim().to_string();
        let optional = |v: RwSignal<String>| Some(text(v)).filter(|v| !v.is_empty());
        let list = |v: RwSignal<String>| {
            v.get_untracked()
                .split(|c: char| c == ',' || c.is_whitespace())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let source = if self.source.get_untracked() == "image" {
            Source::Image {
                image: text(self.image),
            }
        } else if self.source.get_untracked() == "executable" {
            let artifact = self
                .artifact
                .get_untracked()
                .ok_or("Upload an executable first")?;
            Source::Executable {
                sha256: artifact.sha256,
                filename: artifact.filename,
                arch: artifact.arch,
                args: facade_core::split_command(&self.exec_args.get_untracked())
                    .map_err(|e| e.to_string())?,
            }
        } else {
            let build = match self.build.get_untracked() {
                "rust" => Build::Rust {
                    package: optional(self.rust_package),
                    bin: text(self.rust_bin),
                    features: list(self.features),
                    no_default_features: self.no_default_features.get_untracked(),
                },
                "static" => Build::Static {
                    dir: optional(self.static_dir).unwrap_or_else(|| ".".into()),
                },
                _ => Build::Dockerfile {
                    dockerfile: optional(self.dockerfile).unwrap_or_else(|| "Dockerfile".into()),
                    context: optional(self.context).unwrap_or_else(|| ".".into()),
                },
            };
            Source::Git {
                repo: text(self.repo),
                branch: optional(self.branch),
                build,
            }
        };
        let is_static = matches!(
            &source,
            Source::Git {
                build: Build::Static { .. },
                ..
            }
        );
        let http_port = match optional(self.http_port) {
            Some(p) if !is_static => Some(
                p.parse::<u16>()
                    .map_err(|_| format!("HTTP port `{p}` is not a port number"))?,
            ),
            _ => None,
        };
        let ports = if is_static || self.host_network.get_untracked() {
            vec![]
        } else {
            self.ports
                .get_untracked()
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| l.parse::<PortMap>().map_err(|e| e.to_string()))
                .collect::<Result<_, _>>()?
        };
        let memory_mb = optional(self.memory)
            .map(|m| {
                m.parse::<u64>()
                    .map_err(|_| "memory must be a number of MB".to_string())
            })
            .transpose()?;
        let cpus = optional(self.cpus)
            .map(|c| {
                c.parse::<f64>()
                    .map_err(|_| "CPUs must be a number".to_string())
            })
            .transpose()?;
        let host_network = !is_static && self.host_network.get_untracked();
        let (command, release_command, volumes, links) = if is_static {
            (vec![], None, vec![], vec![])
        } else {
            let command = if self.source.get_untracked() == "executable" {
                vec![]
            } else {
                facade_core::split_command(&self.command.get_untracked())
                    .map_err(|e| e.to_string())?
            };
            let volumes = self
                .volumes
                .get_untracked()
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| l.parse::<Volume>().map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            let links = if host_network {
                vec![]
            } else {
                self.links.get_untracked()
            };
            (command, optional(self.release), volumes, links)
        };
        let spec = AppSpec {
            name: text(self.name),
            node: text(self.node),
            source,
            http_port,
            domains: list(self.domains)
                .into_iter()
                .map(|d| d.to_lowercase())
                .collect(),
            ports,
            host_network,
            limits: Limits { memory_mb, cpus },
            enabled: self.enabled.get_untracked(),
            command,
            release_command,
            volumes,
            links,
            stack: None,
            healthcheck: (!is_static)
                .then(|| optional(self.health))
                .flatten()
                .map(|h| facade_core::HealthCheck::shell(&h)),
        };
        spec.validate().map_err(|e| e.to_string())?;
        Ok(spec)
    }
}

/// A segmented control over a small set of string options.
#[component]
fn Segmented(
    value: RwSignal<&'static str>,
    options: &'static [(&'static str, &'static str)],
    label: &'static str,
) -> impl IntoView {
    view! {
        <div class="segmented" role="radiogroup" aria-label=label>
            {options.iter().map(|(key, text)| {
                let key = *key;
                view! {
                    <button type="button" role="radio"
                        class=move || if value.get() == key { "segment active" } else { "segment" }
                        aria-checked=move || (value.get() == key).to_string()
                        on:click=move |_| value.set(key)>
                        {*text}
                    </button>
                }
            }).collect_view()}
        </div>
    }
}

#[component]
pub fn SpecFields(
    form: SpecForm,
    #[prop(into)] nodes: Signal<Vec<NodeView>>,
    /// Every app, to offer links to those on the same node.
    #[prop(into)]
    apps: Signal<Vec<AppView>>,
    /// The name is fixed once an app exists.
    editing: bool,
) -> impl IntoView {
    let f = form;
    // Apps of the chosen node with a private network: what this one can link to.
    let linkable = move || {
        let node = f.node.get();
        let me = f.name.get();
        apps.get()
            .into_iter()
            .filter(|a| {
                a.spec.node == node
                    && a.spec.name != me
                    && !a.spec.host_network
                    && !a.spec.is_static()
            })
            .map(|a| a.spec.name)
            .collect::<Vec<_>>()
    };
    view! {
        <fieldset class="form-section">
            <legend>"App"</legend>
            <div class="form-row">
                <label class="field">
                    <span>"Name"</span>
                    <input type="text" bind:value=f.name disabled=editing required
                        placeholder="my-app" pattern="[a-z0-9]([a-z0-9\\-]*[a-z0-9])?"
                        autocomplete="off"/>
                    <small>"Lowercase letters, digits and dashes."</small>
                </label>
                <label class="field">
                    <span>"Node"</span>
                    <select on:change=move |ev| f.node.set(event_target_value(&ev))
                        prop:value=move || f.node.get()>
                        <For each=move || nodes.get() key=|n| n.name.clone() let:node>
                            <option value=node.name.clone()
                                selected={
                                    let name = node.name.clone();
                                    move || f.node.with(|n| *n == name)
                                }>
                                {format!("{}{}", node.name, if node.local { " (server)" } else { "" })}
                            </option>
                        </For>
                    </select>
                    <Show when=move || editing>
                        <small>"Changing it moves the app (it is rebuilt on the new node)."</small>
                    </Show>
                </label>
            </div>
        </fieldset>

        <fieldset class="form-section">
            <legend>"Source"</legend>
            <Segmented value=f.source label="Source"
                options=&[("git", "Git repository"), ("executable", "Executable"), ("image", "Docker image")]/>
            <Show when=move || f.source.get() == "image">
                <label class="field">
                    <span>"Image"</span>
                    <input type="text" bind:value=f.image placeholder="traefik/whoami:latest"/>
                    <small>"Pulled again on every deploy, so moving tags like latest update."</small>
                </label>
            </Show>
            <Show when=move || f.source.get() == "executable">
                <DropZone form=f nodes=nodes/>
                <label class="field">
                    <span>"Arguments"</span>
                    <input type="text" bind:value=f.exec_args placeholder="--port 8080" autocomplete="off"/>
                    <small>"Passed to the program. Separate with spaces; put an argument containing spaces in quotes."</small>
                </label>
            </Show>
            <Show when=move || f.source.get() == "git">
                <div class="form-row repo-row">
                    <label class="field grow">
                        <span>"Repository URL"</span>
                        <input type="text" inputmode="url" autocomplete="off" spellcheck="false" bind:value=f.repo placeholder="https://github.com/me/repo"
                            on:change=move |_| if !editing { f.scan(true) }/>
                    </label>
                    <label class="field">
                        <span>"Branch"</span>
                        <input type="text" bind:value=f.branch placeholder="default branch"
                            on:change=move |_| if !editing { f.scan(true) }/>
                    </label>
                    <button type="button" class="button scan-button"
                        disabled=move || f.scanning.get() || f.repo.with(|r| r.trim().is_empty())
                        on:click=move |_| { f.scanned.set(Default::default()); f.scan(!editing) }>
                        {move || if f.scanning.get() { "Scanning…" } else { "Scan" }}
                    </button>
                </div>
                <ErrorNote error=f.scan_error/>
                <Show when=move || f.scan_error.with(|e| e.as_deref().is_some_and(|e| e.contains(facade_core::api::NO_ACCESS)))>
                    <PrivateRepo form=f editing=editing/>
                </Show>
                <ScanResults form=f/>
                <details class="build-details"
                    prop:open=move || f.scan.with(|r| r.as_ref().is_some_and(|r| r.candidates.is_empty()))>
                    <summary>
                        <span>"Build settings"</span>
                        <span class="muted small">{move || f.build_summary()}</span>
                    </summary>
                    <div class="build-details-body">
                        <Segmented value=f.build label="Build"
                            options=&[("dockerfile", "Dockerfile"), ("rust", "Rust binary"), ("static", "Static site")]/>
                        <Show when=move || f.build.get() == "dockerfile">
                            <div class="form-row">
                                <label class="field">
                                    <span>"Dockerfile"</span>
                                    <input type="text" bind:value=f.dockerfile/>
                                </label>
                                <label class="field">
                                    <span>"Build context"</span>
                                    <input type="text" bind:value=f.context/>
                                </label>
                            </div>
                        </Show>
                        <Show when=move || f.build.get() == "rust">
                            <div class="form-row">
                                <label class="field">
                                    <span>"Binary"</span>
                                    <input type="text" bind:value=f.rust_bin placeholder="server"/>
                                </label>
                                <label class="field">
                                    <span>"Package"</span>
                                    <input type="text" bind:value=f.rust_package placeholder="(workspace root)"/>
                                </label>
                            </div>
                            <div class="form-row">
                                <label class="field grow">
                                    <span>"Features"</span>
                                    <input type="text" bind:value=f.features placeholder="server, metrics"/>
                                </label>
                                <label class="check">
                                    <input type="checkbox" bind:checked=f.no_default_features/>
                                    "No default features"
                                </label>
                            </div>
                        </Show>
                        <Show when=move || f.build.get() == "static">
                            <label class="field">
                                <span>"Directory to serve"</span>
                                <input type="text" bind:value=f.static_dir placeholder="public"/>
                                <small>"Served as-is; index.html and 404.html are used when present."</small>
                            </label>
                        </Show>
                    </div>
                </details>
            </Show>
        </fieldset>

        <fieldset class="form-section">
            <legend>"Networking"</legend>
            <div class="form-row">
                <Show when=move || !f.is_static()>
                    <label class="field">
                        <span>"HTTP port"</span>
                        <input type="number" min="1" max="65535" bind:value=f.http_port placeholder="8080"/>
                        <small>"The port your app listens on. Needed for domains."</small>
                    </label>
                </Show>
                <label class="field grow">
                    <span>"Domains"</span>
                    <textarea rows="2" bind:value=f.domains placeholder="app.example.com"></textarea>
                    <small>"One per line. Each must point at the app's node; the Domains page shows how, and HTTPS follows on its own."</small>
                </label>
            </div>
            <Show when=move || !f.is_static()>
                <div class="form-row">
                    <label class="field grow">
                        <span>"Published ports"</span>
                        <textarea rows="2" bind:value=f.ports disabled=move || f.host_network.get()
                            placeholder="3478:3478/udp\n49160-49260:49160-49260/udp"></textarea>
                        <small>"host:container/protocol, one per line. Opened on the node's public interface."</small>
                    </label>
                    <label class="check">
                        <input type="checkbox" bind:checked=f.host_network/>
                        "Use the node's network"
                    </label>
                </div>
            </Show>
        </fieldset>

        <Show when=move || !f.is_static()>
            <fieldset class="form-section">
                <legend>"Run"</legend>
                <Show when=move || f.source.get() != "executable">
                    <label class="field">
                        <span>"Command"</span>
                        <input type="text" bind:value=f.command autocomplete="off"
                            placeholder="(the image's own command)"/>
                        <small>"Replaces the image's command, e.g. to run a worker from the same build."</small>
                    </label>
                </Show>
                <label class="field">
                    <span>"Release command"</span>
                    <input type="text" bind:value=f.release autocomplete="off" placeholder="./migrate up"/>
                    <small>
                        "Runs with " <code>"sh -c"</code> " in a container of the new build before it replaces the running one, "
                        "with the same variables, volumes and links. Use it for database migrations: if it fails, the deploy stops and the running version stays."
                    </small>
                </label>
                <label class="field">
                    <span>"Health check"</span>
                    <input type="text" bind:value=f.health autocomplete="off" placeholder="curl -fs localhost:8080/health"/>
                    <small>"Run in the container; exit 0 means ready. A deploy waits for it before counting as done."</small>
                </label>
            </fieldset>

            <fieldset class="form-section">
                <legend>"Data and connections"</legend>
                <label class="field">
                    <span>"Volumes"</span>
                    <textarea rows="2" class="mono" bind:value=f.volumes placeholder="data:/var/lib/data"></textarea>
                    <small>
                        "name:/path, one per line. What the app writes there survives restarts and deploys. "
                        "Volumes stay on their node: an app with volumes cannot move to another node."
                    </small>
                </label>
                <div class="field">
                    <span>"Links"</span>
                    <Show when=move || !f.host_network.get()
                        fallback=|| view! { <small>"Apps on the node's network reach others through their published ports."</small> }>
                        <Show when=move || !linkable().is_empty()
                            fallback=|| view! { <small>"No other app on this node to link to yet."</small> }>
                            <div class="checks">
                                {move || linkable().into_iter().map(|app| {
                                    let checked_app = app.clone();
                                    let toggled_app = app.clone();
                                    let alias = {
                                        let app = app.clone();
                                        move || f.links.with(|l| l.iter().find(|l| l.app == app).and_then(|l| l.alias.clone()))
                                    };
                                    view! {
                                        <label class="check">
                                            <input type="checkbox"
                                                prop:checked=move || f.links.with(|l| l.iter().any(|l| l.app == checked_app))
                                                on:change=move |ev| {
                                                    let on = event_target_checked(&ev);
                                                    let app = toggled_app.clone();
                                                    f.links.update(|l| {
                                                        l.retain(|x| x.app != app);
                                                        if on {
                                                            l.push(Link { app, alias: None });
                                                        }
                                                    });
                                                }/>
                                            {app.clone()}
                                            {move || alias().map(|a| view! { <span class="muted small">{format!(" as {a}")}</span> })}
                                        </label>
                                    }
                                }).collect_view()}
                            </div>
                        </Show>
                        <small>"A linked app is reachable from this one by its name (e.g. " <code>"postgres://…@db:5432"</code> "), over a network private to them."</small>
                    </Show>
                </div>
            </fieldset>

            <fieldset class="form-section">
                <legend>"Limits"</legend>
                <div class="form-row">
                    <label class="field">
                        <span>"Memory (MB)"</span>
                        <input type="number" min="16" bind:value=f.memory placeholder="no limit"/>
                    </label>
                    <label class="field">
                        <span>"CPUs"</span>
                        <input type="number" min="0.1" step="0.1" bind:value=f.cpus placeholder="no limit"/>
                    </label>
                </div>
            </fieldset>
        </Show>
    }
}

/// What the repository scan found, as choices that fill in the build settings.
#[component]
fn ScanResults(form: SpecForm) -> impl IntoView {
    let f = form;
    let report = move || f.scan.get();
    view! {
        {move || report().map(|r| {
            let commit = r.commit.clone();
            let candidates: Vec<(usize, Candidate)> = r.candidates.into_iter().enumerate().collect();
            let empty = candidates.is_empty();
            view! {
                <div class="scan">
                    <div class="muted small">
                        "Scanned " <code>{commit}</code>
                        {r.credential.clone().map(|c| view! { " · fetched with " <strong>{c}</strong> })}
                    </div>
                    {r.compose_files.first().cloned().map(|file| {
                        let href = format!(
                            "/stacks/new?repo={}&branch={}&file={}",
                            encode(&f.repo.get_untracked()),
                            encode(&f.branch.get_untracked()),
                            encode(&file)
                        );
                        view! {
                            <div class="note">
                                "This repository has " <code>{file.clone()}</code> ". "
                                <A href=href>"Deploy it as a stack"</A>
                                " (its services together, at one commit, on a network of their own), or pick one thing to deploy below."
                            </div>
                        }
                    })}
                    {empty.then(|| view! {
                        <div class="note">"Nothing recognised in this repository. Set the build up below."</div>
                    })}
                    <div class="candidates" role="radiogroup" aria-label="What to deploy">
                        {candidates.into_iter().map(|(i, c)| view! {
                            <button type="button" role="radio"
                                class=move || if f.chosen.get() == Some(i) { "candidate active" } else { "candidate" }
                                aria-checked=move || (f.chosen.get() == Some(i)).to_string()
                                on:click=move |_| f.choose(i)>
                                <span class="candidate-kind">{c.detector.clone()}</span>
                                <span class="candidate-title">{c.title.clone()}</span>
                                <span class="candidate-detail">{c.detail.clone()}</span>
                                {(!c.env.is_empty()).then(|| view! {
                                    <span class="candidate-env">
                                        "Reads " {c.env.join(", ")}
                                    </span>
                                })}
                            </button>
                        }).collect_view()}
                    </div>
                </div>
            }
        })}
    }
}

/// Drop or pick a Linux executable; it is uploaded right away.
#[component]
fn DropZone(form: SpecForm, #[prop(into)] nodes: Signal<Vec<NodeView>>) -> impl IntoView {
    let f = form;
    let dragging = RwSignal::new(false);
    let uploading = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let input = NodeRef::<leptos::html::Input>::new();

    let send = move |file: web_sys::File| {
        let name = file.name();
        uploading.set(Some(name));
        error.set(None);
        spawn_local(async move {
            match api::upload(&file).await {
                Ok(artifact) => {
                    f.artifact.try_set(Some(artifact));
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            uploading.try_set(None);
        });
    };
    let node_arch = move || {
        let node = f.node.get();
        nodes.with(|ns| {
            ns.iter()
                .find(|n| n.name == node)
                .and_then(|n| n.stats.as_ref().map(|s| s.arch.clone()))
                .filter(|a| !a.is_empty())
        })
    };
    let mismatch = move || {
        let artifact = f.artifact.get()?;
        let arch = node_arch()?;
        (artifact.arch != arch).then(|| {
            format!(
                "{} is built for {}, but node {} is {arch}. Build it for {arch} or pick another node.",
                artifact.filename,
                artifact.arch,
                f.node.get()
            )
        })
    };

    view! {
        <div
            class=move || if dragging.get() { "dropzone dragging" } else { "dropzone" }
            role="button" tabindex="0"
            on:click=move |_| { if let Some(el) = input.get() { el.click() } }
            on:keydown=move |ev| {
                if ev.key() == "Enter" || ev.key() == " " {
                    ev.prevent_default();
                    if let Some(el) = input.get() { el.click() }
                }
            }
            on:dragover=move |ev: leptos::ev::DragEvent| { ev.prevent_default(); dragging.set(true) }
            on:dragleave=move |_| dragging.set(false)
            on:drop=move |ev: leptos::ev::DragEvent| {
                ev.prevent_default();
                dragging.set(false);
                if let Some(file) = ev.data_transfer().and_then(|d| d.files()).and_then(|f| f.get(0)) {
                    send(file);
                }
            }
        >
            <input type="file" node_ref=input class="visually-hidden" tabindex="-1"
                on:change=move |_| {
                    if let Some(file) = input.get().and_then(|el| el.files()).and_then(|f| f.get(0)) {
                        send(file);
                    }
                    if let Some(el) = input.get() { el.set_value("") }
                }/>
            {move || match (uploading.get(), f.artifact.get()) {
                (Some(name), _) => view! {
                    <span class="dropzone-main">{format!("Uploading {name}…")}</span>
                }.into_any(),
                (None, Some(a)) => view! {
                    <span class="dropzone-main">
                        <span aria-hidden="true">"✓ "</span>{a.filename.clone()}
                    </span>
                    <span class="dropzone-sub">
                        {format!("{} executable", a.arch)}
                        {(a.size > 0).then(|| format!(" · {}", bytes(a.size)))}
                        " · drop another file to replace it"
                    </span>
                }.into_any(),
                (None, None) => view! {
                    <span class="dropzone-main">"Drop a Linux executable here, or click to choose one"</span>
                    <span class="dropzone-sub">
                        {move || format!(
                            "Built for {} (the selected node). A static build (musl) runs anywhere.",
                            node_arch().unwrap_or_else(|| "the node's CPU".into())
                        )}
                    </span>
                }.into_any(),
            }}
        </div>
        <ErrorNote error=error/>
        <ErrorNote error=Signal::derive(mismatch)/>
    }
}

fn encode(s: &str) -> String {
    String::from(js_sys::encode_uri_component(s))
}

/// When a scan was refused: create a deploy key for the repository, show where to
/// add it, and scan again.
#[component]
fn PrivateRepo(form: SpecForm, editing: bool) -> impl IntoView {
    let f = form;
    let key = RwSignal::new(None::<GitCredentialView>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let create = move |_| {
        let repo = f.repo.get_untracked().trim().to_string();
        let base: String = facade_core::git::parse(&repo)
            .and_then(|l| l.path.rsplit('/').next().map(str::to_lowercase))
            .unwrap_or_else(|| "repo".into())
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(30)
            .collect();
        let name = format!("{}-deploy-key", base.trim_matches('-'));
        let body = CreateGitCredential {
            name,
            scope: repo,
            secret: NewGitSecret::SshKey { private_key: None },
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::post::<_, GitCredentialView>("/api/git/credentials", &body).await {
                Ok(created) => {
                    key.try_set(Some(created));
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            busy.try_set(false);
        });
    };
    let rescan = move |_| {
        f.scanned.set(Default::default());
        f.scan(!editing);
    };
    view! {
        <div class="note private-repo">
            <Show when=move || key.with(Option::is_none) fallback=move || view! {
                <p><strong>"Add this deploy key to the repository"</strong> " (read-only access is enough):"</p>
                <CopyField value=Signal::derive(move || key.get().and_then(|k| k.public_key).unwrap_or_default())/>
                <p class="small">
                    {move || match key.get().and_then(|k| k.add_key_url) {
                        Some(url) => view! {
                            <a href=url target="_blank" rel="noopener">"Open the repository's deploy key page ↗"</a>
                            ", paste the key, save, then scan again."
                        }.into_any(),
                        None => view! { "Add it in the repository's settings, then scan again." }.into_any(),
                    }}
                </p>
                <button type="button" class="button button-primary" on:click=rescan
                    disabled=move || f.scanning.get()>
                    {move || if f.scanning.get() { "Scanning…" } else { "Scan again" }}
                </button>
            }>
                <p>
                    <strong>"facade cannot read this repository."</strong>
                    " If it is private, give facade read access with a deploy key: facade makes one, you add it to the repository."
                </p>
                <button type="button" class="button" on:click=create disabled=busy>
                    {move || if busy.get() { "Creating…" } else { "Create a deploy key" }}
                </button>
                <span class="muted small">
                    " Several private repositories? Add one access token for all of them in "
                    <A href="/settings">"Settings → Git access"</A> "."
                </span>
            </Show>
            <ErrorNote error=error/>
        </div>
    }
}
