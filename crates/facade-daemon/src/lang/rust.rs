//! Cargo projects: every binary target of every package in the repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use facade_core::Build;
use facade_core::api::Candidate;
use toml::Value;

use super::{Detector, env_hints, port_hint, read_small, relative};

pub struct Cargo;

struct BinTarget {
    name: String,
    path: PathBuf,
    required_features: Vec<String>,
}

struct Package {
    name: String,
    dir: PathBuf,
    default_features: Vec<String>,
    bins: Vec<BinTarget>,
}

impl Detector for Cargo {
    fn detect(&self, root: &Path) -> Vec<Candidate> {
        let Some(manifest) = load(&root.join("Cargo.toml")) else {
            return vec![];
        };
        let packages: Vec<Package> = package_dirs(root, &manifest)
            .into_iter()
            .filter_map(|dir| read_package(&dir))
            .collect();
        let mut candidates: Vec<(i32, Candidate)> = Vec::new();
        for package in &packages {
            for bin in &package.bins {
                let source = read_small(&bin.path).unwrap_or_default();
                let http_port = port_hint(&source);
                let env = env_hints(&source);
                // A binary that needs features usually wants only those: defaults
                // tend to pull in the client side (as with bevy_ensemble's server).
                let no_default_features = !bin.required_features.is_empty()
                    && package
                        .default_features
                        .iter()
                        .any(|f| !bin.required_features.contains(f));
                let mut detail =
                    format!("package {} · {}", package.name, relative(root, &bin.path));
                if !bin.required_features.is_empty() {
                    detail += &format!(" · features {}", bin.required_features.join(", "));
                }
                if no_default_features {
                    detail += " · no default features";
                }
                if let Some(port) = http_port {
                    detail += &format!(" · listens on {port}");
                }
                let lower = bin.name.to_lowercase();
                let server_like = ["server", "serve", "api", "web", "http", "daemon"]
                    .iter()
                    .any(|w| lower.contains(w));
                let score = http_port.is_some() as i32 * 2 + server_like as i32;
                candidates.push((
                    score,
                    Candidate {
                        detector: "rust".into(),
                        title: format!("Rust binary {}", bin.name),
                        detail,
                        build: Build::Rust {
                            package: Some(package.name.clone()),
                            bin: bin.name.clone(),
                            features: bin.required_features.clone(),
                            no_default_features,
                        },
                        http_port,
                        env,
                        command: vec![],
                        notes: vec![],
                    },
                ));
            }
        }
        // Best first, keeping the manifest order among equals.
        candidates.sort_by_key(|(score, _)| -score);
        candidates.into_iter().map(|(_, c)| c).collect()
    }
}

fn load(path: &Path) -> Option<Value> {
    read_small(path)?
        .parse::<toml::Table>()
        .ok()
        .map(Value::Table)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The root package (if any) and workspace members, with `dir/*` globs expanded.
fn package_dirs(root: &Path, manifest: &Value) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if manifest.get("package").is_some() {
        dirs.push(root.to_path_buf());
    }
    let workspace = manifest.get("workspace");
    let excluded: Vec<PathBuf> = strings(workspace.and_then(|w| w.get("exclude")))
        .iter()
        .map(|e| root.join(e))
        .collect();
    for member in strings(workspace.and_then(|w| w.get("members"))) {
        if let Some(prefix) = member.strip_suffix("/*") {
            let Ok(entries) = std::fs::read_dir(root.join(prefix)) else {
                continue;
            };
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.join("Cargo.toml").is_file())
                .collect();
            found.sort();
            dirs.extend(found);
        } else if !member.contains('*') {
            dirs.push(root.join(member));
        }
    }
    dirs.retain(|d| !excluded.contains(d));
    dirs.dedup();
    dirs
}

fn read_package(dir: &Path) -> Option<Package> {
    let manifest = load(&dir.join("Cargo.toml"))?;
    let package = manifest.get("package")?;
    let name = package.get("name")?.as_str()?.to_string();
    let default_features = strings(manifest.get("features").and_then(|f| f.get("default")));

    // Explicit [[bin]] targets first; then Cargo's automatic ones, unless a target
    // already claims that file.
    let mut bins: BTreeMap<PathBuf, BinTarget> = BTreeMap::new();
    let mut order: Vec<PathBuf> = Vec::new();
    for bin in manifest
        .get("bin")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(bin_name) = bin.get("name").and_then(Value::as_str) else {
            continue;
        };
        let path = match bin.get("path").and_then(Value::as_str) {
            Some(p) => dir.join(p),
            None if bin_name == name => dir.join("src/main.rs"),
            None => dir.join(format!("src/bin/{bin_name}.rs")),
        };
        order.push(path.clone());
        bins.insert(
            path.clone(),
            BinTarget {
                name: bin_name.to_string(),
                path,
                required_features: strings(bin.get("required-features")),
            },
        );
    }
    let autobins = package
        .get("autobins")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if autobins {
        let mut auto: Vec<(String, PathBuf)> = Vec::new();
        let main = dir.join("src/main.rs");
        if main.is_file() {
            auto.push((name.clone(), main));
        }
        if let Ok(entries) = std::fs::read_dir(dir.join("src/bin")) {
            let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            entries.sort();
            for path in entries {
                if path.extension().is_some_and(|e| e == "rs") {
                    let stem = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    auto.push((stem, path));
                } else if path.join("main.rs").is_file() {
                    let stem = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    auto.push((stem, path.join("main.rs")));
                }
            }
        }
        for (bin_name, path) in auto {
            if !bins.contains_key(&path) {
                order.push(path.clone());
                bins.insert(
                    path.clone(),
                    BinTarget {
                        name: bin_name,
                        path,
                        required_features: vec![],
                    },
                );
            }
        }
    }
    let bins = order
        .into_iter()
        .filter_map(|p| bins.remove(&p))
        .filter(|b| b.path.is_file())
        .collect();
    Some(Package {
        name,
        dir: dir.to_path_buf(),
        default_features,
        bins,
    })
    .filter(|p: &Package| !p.bins.is_empty() && p.dir.exists())
}

/// Multi-stage build of one cargo binary, with the registry and target directory in
/// BuildKit cache mounts so redeploys only rebuild what changed. Git dependencies
/// are fetched with the app's git credential (handed over as build secrets), so
/// private ones work when the credential covers them.
pub fn dockerfile(
    app: &str,
    package: Option<&str>,
    bin: &str,
    features: &[String],
    no_default_features: bool,
) -> String {
    let mut args = String::new();
    if let Some(package) = package {
        args += &format!(" -p {package}");
    }
    args += &format!(" --bin {bin}");
    if no_default_features {
        args += " --no-default-features";
    }
    if !features.is_empty() {
        args += &format!(" --features {}", features.join(","));
    }
    format!(
        r#"FROM docker.io/library/rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev git openssh-client && rm -rf /var/lib/apt/lists/*
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=facade-cargo-registry \
    --mount=type=cache,target=/usr/local/cargo/git,id=facade-cargo-git \
    --mount=type=cache,target=/src/target,id=facade-{app}-target \
    --mount=type=secret,id=gitconfig,target=/root/.gitconfig \
    --mount=type=secret,id=known_hosts,target=/root/.ssh/known_hosts \
    --mount=type=ssh \
    cargo build --release{args} && cp target/release/{bin} /facade-app

FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 tzdata && rm -rf /var/lib/apt/lists/*
COPY --from=build /facade-app /usr/local/bin/{bin}
CMD ["/usr/local/bin/{bin}"]
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, contents: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    #[test]
    fn finds_workspace_binaries() {
        let dir = std::env::temp_dir().join(format!("facade-rust-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(
            &dir,
            "Cargo.toml",
            "[package]\nname = \"lib-root\"\n[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write(&dir, "src/lib.rs", "");
        write(
            &dir,
            "crates/net/Cargo.toml",
            "[package]\nname = \"net\"\n[features]\ndefault = [\"client\"]\nclient = []\nserver = []\n\
             [[bin]]\nname = \"net_server\"\npath = \"src/bin/server.rs\"\nrequired-features = [\"server\"]\n",
        );
        write(
            &dir,
            "crates/net/src/bin/server.rs",
            "const A: &str = \"0.0.0.0:9090\"; fn main() { std::env::var(\"TURN_PASSWORD\"); }",
        );
        write(&dir, "crates/net/src/bin/tool.rs", "fn main() {}");
        write(&dir, "crates/web/Cargo.toml", "[package]\nname = \"web\"\n");
        write(&dir, "crates/web/src/main.rs", "fn main() {}");

        let found = Cargo.detect(&dir);
        let names: Vec<_> = found.iter().map(|c| c.title.as_str()).collect();
        // Listening first, then server-like names, then the rest.
        assert_eq!(
            names,
            [
                "Rust binary net_server",
                "Rust binary web",
                "Rust binary tool"
            ]
        );
        let best = &found[0];
        assert_eq!(best.http_port, Some(9090));
        assert_eq!(best.env, ["TURN_PASSWORD"]);
        assert_eq!(
            best.build,
            Build::Rust {
                package: Some("net".into()),
                bin: "net_server".into(),
                features: vec!["server".into()],
                no_default_features: true,
            }
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
