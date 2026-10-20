//! The app settings form, shared by "New app" and an app's Settings tab.

use facade_core::api::{ArtifactView, Candidate, InspectReport, InspectRequest, NodeView};
use facade_core::{AppSpec, Build, DEFAULT_PYTHON, Limits, PortMap, Protocol, Source, Volume};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::ErrorNote;
use crate::util::{bytes, cpus, me};

#[derive(Clone, Copy)]
pub struct SpecForm {
    pub name: RwSignal<String>,
    pub node: RwSignal<String>,
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
    python_version: RwSignal<String>,
    python_entry: RwSignal<String>,
    static_dir: RwSignal<String>,
    http_port: RwSignal<String>,
    domains: RwSignal<String>,
    redirects: RwSignal<String>,
    ports: RwSignal<Vec<PortRow>>,
    host_network: RwSignal<bool>,
    volumes: RwSignal<String>,
    memory: RwSignal<String>,
    cpus: RwSignal<String>,
    enabled: RwSignal<bool>,
    artifact: RwSignal<Option<ArtifactView>>,
    args: RwSignal<String>,
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
            python_version: s(""),
            python_entry: s(""),
            static_dir: s("."),
            http_port: s(""),
            domains: s(""),
            redirects: s(""),
            ports: RwSignal::new(vec![]),
            host_network: RwSignal::new(false),
            volumes: s(""),
            memory: s(""),
            cpus: s(""),
            enabled: RwSignal::new(true),
            artifact: RwSignal::new(None),
            args: s(""),
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
            } => {
                form.source.set("executable");
                form.artifact.set(Some(ArtifactView {
                    sha256: sha256.clone(),
                    filename: filename.clone(),
                    arch: arch.clone(),
                    size: 0,
                }));
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
                    Build::Python { version, entry } => {
                        form.build.set("python");
                        form.python_version.set(version.clone().unwrap_or_default());
                        form.python_entry.set(entry.clone());
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
        form.redirects.set(spec.redirects.join("\n"));
        form.ports
            .set(spec.ports.iter().map(PortRow::from_map).collect());
        form.host_network.set(spec.host_network);
        form.args.set(join_args(&spec.args));
        form.volumes.set(
            spec.volumes
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        form.memory.set(opt_text(spec.limits.memory_mb));
        form.cpus.set(opt_text(spec.limits.cpus));
        form.enabled.set(spec.enabled);
        form
    }

    /// Picks `node` unless one is already chosen.
    pub fn default_node(&self, node: String) {
        if self.node.with_untracked(String::is_empty) {
            self.node.set(node);
        }
    }

    /// Fills the build settings (and the port and arguments, if empty) from a scan
    /// candidate.
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
            Build::Python { version, entry } => {
                self.build.set("python");
                self.python_version.set(version.unwrap_or_default());
                self.python_entry.set(entry);
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
        if !candidate.args.is_empty() && self.args.with_untracked(|a| a.trim().is_empty()) {
            self.args.set(join_args(&candidate.args));
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
            "python" => format!(
                "Python {} · {}",
                Some(self.python_version.get())
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_PYTHON.into()),
                self.python_entry.get()
            ),
            "static" => format!("static files from {}", self.static_dir.get()),
            _ => format!("{} in {}", self.dockerfile.get(), self.context.get()),
        }
    }

    /// The program comes from an image, so arguments replace its `CMD`.
    fn uses_cmd(&self) -> bool {
        match self.source.get() {
            "image" => true,
            "git" => self.build.get() == "dockerfile",
            _ => false,
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
            }
        } else {
            let build = match self.build.get_untracked() {
                "rust" => Build::Rust {
                    package: optional(self.rust_package),
                    bin: text(self.rust_bin),
                    features: list(self.features),
                    no_default_features: self.no_default_features.get_untracked(),
                },
                "python" => Build::Python {
                    version: optional(self.python_version),
                    entry: text(self.python_entry),
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
        // On the node's network they are not remapped but still declared, so
        // ports held by something else are caught before the app starts.
        let ports = if is_static {
            vec![]
        } else {
            let host_network = self.host_network.get_untracked();
            self.ports
                .get_untracked()
                .iter()
                .filter(|row| !row.is_blank())
                .map(|row| row.port_map(host_network))
                .collect::<Result<_, _>>()?
        };
        let args = if is_static {
            vec![]
        } else {
            split_args(&self.args.get_untracked())?
        };
        let volumes = if is_static {
            vec![]
        } else {
            self.volumes
                .get_untracked()
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| l.parse::<Volume>().map_err(|e| e.to_string()))
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
        let spec = AppSpec {
            name: text(self.name),
            node: text(self.node),
            source,
            http_port,
            domains: list(self.domains)
                .into_iter()
                .map(|d| d.to_lowercase())
                .collect(),
            redirects: list(self.redirects)
                .into_iter()
                .map(|d| d.to_lowercase())
                .collect(),
            ports,
            host_network: !is_static && self.host_network.get_untracked(),
            args,
            volumes,
            limits: Limits { memory_mb, cpus },
            enabled: self.enabled.get_untracked(),
        };
        spec.validate().map_err(|e| e.to_string())?;
        Ok(spec)
    }
}

/// Splits arguments on spaces, keeping "quoted parts" together.
fn split_args(line: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        return Err("unclosed quote in the arguments".into());
    }
    if started || !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains(char::is_whitespace) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One line of the ports editor: a port or a range, and where it goes inside the
/// app when that differs.
#[derive(Clone, Copy)]
pub struct PortRow {
    id: usize,
    protocol: RwSignal<&'static str>,
    first: RwSignal<String>,
    /// Last port of a range; empty for a single port.
    last: RwSignal<String>,
    /// Port inside the app; empty when the same.
    inner: RwSignal<String>,
}

impl PortRow {
    fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            protocol: RwSignal::new("tcp"),
            first: RwSignal::new(String::new()),
            last: RwSignal::new(String::new()),
            inner: RwSignal::new(String::new()),
        }
    }

    fn from_map(map: &PortMap) -> Self {
        let row = Self::new();
        row.protocol.set(match map.protocol {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        });
        row.first.set(map.host.to_string());
        if map.count > 1 {
            row.last
                .set((u32::from(map.host) + u32::from(map.count) - 1).to_string());
        }
        if map.container != map.host {
            row.inner.set(map.container.to_string());
        }
        row
    }

    fn is_blank(&self) -> bool {
        self.first.get_untracked().trim().is_empty() && self.last.get_untracked().trim().is_empty()
    }

    /// Ports in the row, while it is being typed.
    fn count(&self) -> Option<u32> {
        let first: u32 = self.first.get().trim().parse().ok()?;
        match self.last.get().trim() {
            "" => Some(1),
            last => last
                .parse::<u32>()
                .ok()
                .filter(|l| *l >= first)
                .map(|l| l - first + 1),
        }
    }

    fn port_map(self, host_network: bool) -> Result<PortMap, String> {
        let port = |what: &str, text: String| {
            text.trim()
                .parse::<u16>()
                .map_err(|_| format!("{what} `{}` is not a port number", text.trim()))
        };
        let host = port("port", self.first.get_untracked())?;
        let count = match self.last.get_untracked().trim() {
            "" => 1,
            _ => {
                let last = port("port", self.last.get_untracked())?;
                if last < host {
                    return Err(format!("the range {host}–{last} ends before it starts"));
                }
                last - host + 1
            }
        };
        let container = match self.inner.get_untracked().trim() {
            "" => host,
            _ if host_network => host,
            _ => port("port in the app", self.inner.get_untracked())?,
        };
        let protocol = match self.protocol.get_untracked() {
            "udp" => Protocol::Udp,
            _ => Protocol::Tcp,
        };
        let map = PortMap {
            host,
            container,
            count,
            protocol,
        };
        map.validate().map_err(|e| e.to_string())?;
        Ok(map)
    }
}

/// Ports as rows: protocol, port or range, and (unless on the node's network)
/// the port inside the app.
#[component]
fn PortsEditor(form: SpecForm) -> impl IntoView {
    let rows = form.ports;
    let host_network = form.host_network;
    view! {
        <div class="ports-editor">
            <For each=move || rows.get() key=|row| row.id let:row>
                <div class="port-row">
                    <Segmented value=row.protocol options=&[("tcp", "TCP"), ("udp", "UDP")] label="Protocol"/>
                    <input type="number" min="1" max="65535" placeholder="port"
                        aria-label="Port, or first port of a range" bind:value=row.first/>
                    <span class="muted">"to"</span>
                    <input type="number" min="1" max="65535" placeholder="—"
                        aria-label="Last port of a range (empty for one port)" bind:value=row.last/>
                    <Show when=move || !host_network.get()>
                        <span class="muted">"→ app"</span>
                        <input type="number" min="1" max="65535" placeholder="same"
                            aria-label="Port inside the app, when it differs" bind:value=row.inner/>
                    </Show>
                    <span class="muted small port-count">
                        {move || match row.count() {
                            Some(1) | None => String::new(),
                            Some(n) => format!("{n} ports"),
                        }}
                    </span>
                    <button type="button" class="icon-button" aria-label="Remove this port"
                        on:click=move |_| rows.update(|r| r.retain(|x| x.id != row.id))>"×"</button>
                </div>
            </For>
            <button type="button" class="button" on:click=move |_| rows.update(|r| r.push(PortRow::new()))>
                "Add a port or range"
            </button>
        </div>
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
    /// The name is fixed once an app exists.
    editing: bool,
) -> impl IntoView {
    let f = form;
    let me = me();
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
            </Show>
            <Show when=move || f.source.get() == "git">
                <div class="form-row repo-row">
                    <label class="field grow">
                        <span>"Repository URL"</span>
                        <input type="url" bind:value=f.repo placeholder="https://github.com/me/repo"
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
                <ScanResults form=f/>
                <details class="build-details"
                    prop:open=move || f.scan.with(|r| r.as_ref().is_some_and(|r| r.candidates.is_empty()))>
                    <summary>
                        <span>"Build settings"</span>
                        <span class="muted small">{move || f.build_summary()}</span>
                    </summary>
                    <div class="build-details-body">
                        <Segmented value=f.build label="Build"
                            options=&[("dockerfile", "Dockerfile"), ("rust", "Rust binary"), ("python", "Python"), ("static", "Static site")]/>
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
                        <Show when=move || f.build.get() == "python">
                            <div class="form-row">
                                <label class="field grow">
                                    <span>"Run"</span>
                                    <input type="text" bind:value=f.python_entry placeholder="server.py"/>
                                    <small>"A script (server.py), a module (-m mypkg) or a command the project installs."</small>
                                </label>
                                <label class="field">
                                    <span>"Python version"</span>
                                    <input type="text" bind:value=f.python_version placeholder=DEFAULT_PYTHON/>
                                </label>
                            </div>
                            <p class="muted small">"Dependencies come from requirements.txt and/or pyproject.toml."</p>
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
            <Show when=move || !f.is_static()>
                <label class="field">
                    <span>"Arguments"</span>
                    <input type="text" bind:value=f.args autocomplete="off"
                        placeholder=move || if f.uses_cmd() { "(the image's CMD)" } else { "--port 8080" }/>
                    <small>{move || if f.uses_cmd() {
                        "Replace the image's CMD. Separate with spaces; put an argument containing spaces in quotes."
                    } else {
                        "Passed to the program. Separate with spaces; put an argument containing spaces in quotes."
                    }}</small>
                </label>
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
                    <small>"One per line; the first is the main domain. Each must point at the app's node; the Domains page shows how, and HTTPS follows on its own."</small>
                </label>
            </div>
            <label class="field">
                <span>"Redirects"</span>
                <textarea rows="2" bind:value=f.redirects placeholder="www.example.com"></textarea>
                <small>"One per line. Visitors are sent to the main domain, keeping the path."</small>
            </label>
            <Show when=move || !f.is_static()>
                <div class="form-row">
                    <div class="field grow">
                        <span>{move || if f.host_network.get() { "Ports it listens on" } else { "Published ports" }}</span>
                        <PortsEditor form=f/>
                        <small>{move || if f.host_network.get() {
                            "Nothing is remapped on the node's network: listing them lets façade wait for, \
                             or refuse, a program already holding one."
                        } else {
                            "Opened on the node's public interface, outside the web entry point (e.g. a game server or TURN relay)."
                        }}</small>
                    </div>
                    <div class="check-field">
                        <label class="check">
                            <input type="checkbox" bind:checked=f.host_network disabled=move || me.is_member()/>
                            "Use the node's network"
                        </label>
                        <Show when=move || me.is_member()>
                            <small class="muted small">"Only an admin can do this; publish ports instead."</small>
                        </Show>
                    </div>
                </div>
            </Show>
        </fieldset>

        <Show when=move || !f.is_static()>
            <fieldset class="form-section">
                <legend>"Storage"</legend>
                <label class="field">
                    <span>"Volumes"</span>
                    <textarea rows="2" bind:value=f.volumes placeholder="data:/data"></textarea>
                    <small>"name:/path, one per line. These directories keep their files across deploys and restarts; they live on the app's node."</small>
                </label>
            </fieldset>
            <fieldset class="form-section">
                <legend>"Limits"</legend>
                <div class="form-row">
                    <label class="field">
                        <span>"Memory (MB)"</span>
                        <input type="number" min="16" bind:value=f.memory
                            required=move || me.memory_cap().is_some()
                            placeholder=move || if me.memory_cap().is_some() { "required" } else { "no limit" }/>
                        {move || me.memory_cap().map(|(used, cap)| view! {
                            <small>{format!("{used} of {cap} MB used by your apps.")}</small>
                        })}
                    </label>
                    <label class="field">
                        <span>"CPUs"</span>
                        <input type="number" min="0.1" step="0.1" bind:value=f.cpus
                            required=move || me.cpu_cap().is_some()
                            placeholder=move || if me.cpu_cap().is_some() { "required" } else { "no limit" }/>
                        {move || me.cpu_cap().map(|(used, cap)| view! {
                            <small>{format!("{} of {} CPUs used by your apps.", cpus(used), cpus(cap))}</small>
                        })}
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
                    <div class="muted small">"Scanned " <code>{commit}</code></div>
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
                                {(!c.args.is_empty()).then(|| view! {
                                    <span class="candidate-env">
                                        "Runs with " {join_args(&c.args)}
                                    </span>
                                })}
                                {c.notes.iter().map(|n| view! {
                                    <span class="candidate-note">
                                        <span aria-hidden="true">"⚠ "</span>{n.clone()}
                                    </span>
                                }).collect_view()}
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
