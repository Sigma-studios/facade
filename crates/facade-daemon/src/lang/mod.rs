//! What facade knows about kinds of projects.
//!
//! Each module covers one kind and has up to two halves:
//! * `detect`, run by the server on a fresh checkout to suggest [`Candidate`]s;
//! * `dockerfile`, run by the agent to build a [`Build`] variant that has no
//!   Dockerfile of its own.
//!
//! Adding a language means one file here with a [`Detector`], registered in
//! [`detectors`]; plus, if it needs a generated Dockerfile, a `Build` variant in
//! `facade-core` and an arm in [`generated_dockerfile`].

pub mod dockerfile;
pub mod executable;
pub mod python;
pub mod rust;
pub mod static_site;

use std::path::Path;

use facade_core::Build;
use facade_core::api::Candidate;

pub trait Detector {
    /// Candidates found in the checked-out repository at `root`, best first.
    fn detect(&self, root: &Path) -> Vec<Candidate>;
}

/// Every detector, in the order their candidates are listed: a project's own
/// Dockerfile wins over anything facade would generate.
pub fn detectors() -> Vec<Box<dyn Detector>> {
    vec![
        Box::new(dockerfile::Dockerfiles),
        Box::new(rust::Cargo),
        Box::new(python::Python),
        Box::new(static_site::StaticSite),
    ]
}

pub fn detect_all(root: &Path) -> Vec<Candidate> {
    detectors().iter().flat_map(|d| d.detect(root)).collect()
}

/// The Dockerfile facade writes for builds that do not bring one.
pub fn generated_dockerfile(app: &str, build: &Build) -> Option<String> {
    match build {
        Build::Rust {
            package,
            bin,
            features,
            no_default_features,
        } => Some(rust::dockerfile(
            app,
            package.as_deref(),
            bin,
            features,
            *no_default_features,
        )),
        Build::Python { version, entry } => python::dockerfile(version.as_deref(), entry).ok(),
        Build::Dockerfile { .. } | Build::Static { .. } => None,
    }
}

// ---------------------------------------------------------------- shared hints

/// The first port bound on every interface, written as a literal address like
/// `"0.0.0.0:9090"` or Go's `":8080"`. Loopback addresses are left out: in source
/// code they are mostly something the program connects to.
pub fn port_hint(source: &str) -> Option<u16> {
    for prefix in ["0.0.0.0:", "[::]:", "\":"] {
        for (i, _) in source.match_indices(prefix) {
            let digits: String = source[i + prefix.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(port) = digits.parse::<u16>()
                && port >= 80
            {
                return Some(port);
            }
        }
    }
    None
}

/// Names passed to environment lookups like `env::var("NAME")` or
/// `process.env.NAME`, in order of appearance.
pub fn env_hints(source: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if valid && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    };
    for call in [
        "var(",
        "var_os(",
        "getenv(",
        "environ[",
        "environ.get(",
        "Getenv(",
    ] {
        for quote in ['"', '\''] {
            let pattern = format!("{call}{quote}");
            for (i, _) in source.match_indices(&pattern) {
                let rest = &source[i + pattern.len()..];
                if let Some(end) = rest.find(quote) {
                    push(&rest[..end]);
                }
            }
        }
    }
    for (i, _) in source.match_indices("process.env.") {
        let name: String = source[i + "process.env.".len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        push(&name);
    }
    names
}

/// Reads a text file, ignoring anything unreadable or larger than 1 MB.
pub fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 1_000_000 {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// `path` relative to `root`, with forward slashes.
pub fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() { ".".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_hints() {
        let src = r#"const ADDR: &str = "0.0.0.0:9090";
            let p = std::env::var("TURN_PASSWORD"); env::var_os("SIGNALLING_ADDR");
            std::env::var("lowercase_ignored"); process.env.API_URL"#;
        assert_eq!(port_hint(src), Some(9090));
        assert_eq!(
            env_hints(src),
            ["TURN_PASSWORD", "SIGNALLING_ADDR", "API_URL"]
        );
        assert_eq!(port_hint("no ports here: 12"), None);
        assert_eq!(port_hint(r#"connect("ws://localhost:9090/ws")"#), None);
        assert_eq!(
            port_hint(r#"http.ListenAndServe(":8080", nil)"#),
            Some(8080)
        );
    }
}
