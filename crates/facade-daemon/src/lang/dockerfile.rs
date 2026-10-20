//! Projects that bring their own Dockerfile, at the root or one level down.

use std::path::Path;

use facade_core::Build;
use facade_core::api::Candidate;

use super::{Detector, read_small, relative};

pub struct Dockerfiles;

impl Detector for Dockerfiles {
    fn detect(&self, root: &Path) -> Vec<Candidate> {
        let mut dirs = vec![root.to_path_buf()];
        if let Ok(entries) = std::fs::read_dir(root) {
            let mut subdirs: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_dir()
                        && !p
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                })
                .collect();
            subdirs.sort();
            dirs.extend(subdirs);
        }
        dirs.into_iter()
            .filter_map(|dir| {
                let path = dir.join("Dockerfile");
                let contents = read_small(&path)?;
                let http_port = contents.lines().find_map(|line| {
                    let rest = line.trim().strip_prefix("EXPOSE ")?;
                    rest.split_whitespace()
                        .next()?
                        .split('/')
                        .next()?
                        .parse()
                        .ok()
                });
                let context = relative(root, &dir);
                Some(Candidate {
                    detector: "dockerfile".into(),
                    title: format!(
                        "Dockerfile in {}",
                        if context == "." { "the root" } else { &context }
                    ),
                    detail: match http_port {
                        Some(p) => format!("{} · exposes {p}", relative(root, &path)),
                        None => relative(root, &path),
                    },
                    build: Build::Dockerfile {
                        dockerfile: relative(root, &path),
                        context,
                    },
                    http_port,
                    env: vec![],
                    args: vec![],
                    notes: vec![],
                })
            })
            .collect()
    }
}
