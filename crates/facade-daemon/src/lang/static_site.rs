//! Plain websites: a directory with an `index.html`, served as-is.

use std::path::Path;

use facade_core::Build;
use facade_core::api::Candidate;

use super::{Detector, relative};

pub struct StaticSite;

/// Where built or hand-written sites usually live.
const DIRS: &[&str] = &[".", "public", "dist", "build", "site", "docs", "www", "out"];

impl Detector for StaticSite {
    fn detect(&self, root: &Path) -> Vec<Candidate> {
        DIRS.iter()
            .map(|d| root.join(d))
            .filter(|dir| dir.join("index.html").is_file())
            .map(|dir| {
                let rel = relative(root, &dir);
                Candidate {
                    detector: "static".into(),
                    title: format!(
                        "Static site in {}",
                        if rel == "." { "the root" } else { &rel }
                    ),
                    detail: format!("{rel}/index.html, served as-is"),
                    build: Build::Static {
                        dir: rel,
                        dockerfile: None,
                        context: None,
                    },
                    http_port: None,
                    env: vec![],
                    command: vec![],
                    notes: vec![],
                }
            })
            .collect()
    }
}
