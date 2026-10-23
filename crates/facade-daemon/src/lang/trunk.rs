//! Rust web apps built with trunk: an `index.html` linking the crate with
//! `<link data-trunk rel="rust">`. trunk compiles it to WebAssembly and writes the
//! site (HTML, JS glue, `.wasm`, assets), which is served as a static site.

use std::path::{Path, PathBuf};

use facade_core::Build;
use facade_core::api::Candidate;

use super::{Detector, read_small, relative};

pub struct Trunk;

/// The trunk release facade builds with.
pub const TRUNK_VERSION: &str = "0.21.14";

const MAX_DEPTH: usize = 3;
/// Directories that hold dependencies or build output, not apps.
const SKIP: &[&str] = &["node_modules", "target", "vendor", "dist", "build", "out"];

/// Whether an `index.html` is a trunk entry point for a Rust crate.
fn links_rust(html: &str) -> bool {
    html.split("<link").skip(1).any(|tag| {
        let tag = tag.split('>').next().unwrap_or_default();
        tag.contains("data-trunk")
            && ["rel=\"rust\"", "rel='rust'", "rel=rust"]
                .iter()
                .any(|r| tag.contains(r))
    })
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    let index = dir.join("index.html");
    if read_small(&index).is_some_and(|html| links_rust(&html)) {
        found.push(dir.to_path_buf());
    }
    if depth >= MAX_DEPTH {
        return;
    }
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if path.is_dir() && !name.starts_with('.') && !SKIP.contains(&name.as_str()) {
            walk(&path, depth + 1, found);
        }
    }
}

impl Detector for Trunk {
    fn detect(&self, root: &Path) -> Vec<Candidate> {
        let mut found = Vec::new();
        walk(root, 0, &mut found);
        found
            .into_iter()
            .map(|dir| {
                let rel = relative(root, &dir);
                let where_ = if rel == "." { "the root" } else { &rel };
                Candidate {
                    detector: "trunk".into(),
                    title: format!("Rust web app (trunk) in {where_}"),
                    detail: format!(
                        "{rel}/index.html · built to WebAssembly with trunk, served as static files"
                    ),
                    build: Build::Trunk { dir: rel },
                    http_port: None,
                    env: vec![],
                    command: vec![],
                    notes: vec![],
                }
            })
            .collect()
    }
}

/// The Dockerfile building the app in `dir`: its last stage holds only the site,
/// for `docker build --output`. The repository is the build context, so path
/// dependencies and workspaces resolve; a `rust-toolchain.toml` is honoured.
pub fn dockerfile(app: &str, dir: &str, build_env: &[String]) -> String {
    let super::BuildVarLines {
        arg,
        secrets,
        exports,
    } = super::build_var_lines(build_env);
    format!(
        r#"FROM docker.io/library/rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl pkg-config libssl-dev git openssh-client && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL "https://github.com/trunk-rs/trunk/releases/download/v{TRUNK_VERSION}/trunk-$(uname -m)-unknown-linux-gnu.tar.gz" | tar -xz -C /usr/local/bin trunk
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true CARGO_TARGET_DIR=/facade-target
WORKDIR /src
COPY . .
WORKDIR /src/{dir}
{arg}RUN --mount=type=cache,target=/usr/local/cargo/registry,id=facade-cargo-registry \
    --mount=type=cache,target=/usr/local/cargo/git,id=facade-cargo-git \
    --mount=type=cache,target=/facade-target,id=facade-{app}-target \
    --mount=type=cache,target=/root/.cache/trunk,id=facade-trunk \
    --mount=type=secret,id=gitconfig,target=/root/.gitconfig \
    --mount=type=secret,id=known_hosts,target=/root/.ssh/known_hosts \
    --mount=type=ssh \
{secrets}    {exports}(rustup toolchain install 2>/dev/null || true) \
    && rustup target add wasm32-unknown-unknown \
    && trunk build --release --dist /facade-site

FROM scratch
COPY --from=build /facade-site/ /
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
    fn hands_build_variables_to_trunk() {
        let plain = dockerfile("web", "app", &[]);
        assert!(!plain.contains("/run/secrets/"));
        assert!(!crate::agent::build::declares_build_vars_arg(&plain));

        let file = dockerfile("web", "app", &["TURN_PASSWORD".into()]);
        assert!(crate::agent::build::declares_build_vars_arg(&file));
        assert!(file.contains("    --mount=type=secret,id=TURN_PASSWORD \\\n"));
        assert!(file.contains(
            "export TURN_PASSWORD=\"$(cat /run/secrets/TURN_PASSWORD)\" && (rustup toolchain install"
        ));
    }

    #[test]
    fn finds_trunk_apps() {
        let dir = std::env::temp_dir().join(format!("facade-trunk-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "Cargo.toml", "[workspace]\nmembers = [\"apps/*\"]\n");
        write(
            &dir,
            "apps/web/index.html",
            "<html><head>\n  <link data-trunk rel=\"rust\" data-wasm-opt=\"z\" />\n</head></html>",
        );
        // A trunk page without a Rust crate, and an ordinary page.
        write(
            &dir,
            "apps/styles/index.html",
            "<link data-trunk rel=\"css\" href=\"x.css\">",
        );
        write(
            &dir,
            "public/index.html",
            "<link rel=\"stylesheet\" href=\"rust.css\">",
        );
        // Build output is not an app.
        write(
            &dir,
            "apps/web/dist/index.html",
            "<link data-trunk rel=\"rust\">",
        );
        let found = Trunk.detect(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Rust web app (trunk) in apps/web");
        assert_eq!(
            found[0].build,
            Build::Trunk {
                dir: "apps/web".into()
            }
        );
        assert!(links_rust("<link rel='rust' data-trunk/>"));
        // Not trunk's: the attribute is what marks its links.
        assert!(!links_rust(
            "<link rel=\"rust\" href=\"x\"><p data-trunk></p>"
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
