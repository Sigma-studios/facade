//! Projects that bring their own Dockerfile: at the root, or deeper in a monorepo
//! (`apps/api/Dockerfile`, `services/web/Dockerfile.prod`).

use std::path::{Path, PathBuf};

use facade_core::Build;
use facade_core::api::Candidate;

use super::{Detector, read_small, relative};

pub struct Dockerfiles;

/// How deep below the root Dockerfiles are looked for.
const MAX_DEPTH: usize = 3;
const MAX_FOUND: usize = 30;
/// Directories that hold dependencies or build output, not services.
const SKIP: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "out",
    "venv",
    "__pycache__",
];

fn is_dockerfile(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "dockerfile"
        || (lower.starts_with("dockerfile.") && !lower.ends_with(".dockerignore"))
        || lower.ends_with(".dockerfile")
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    let mut subdirs = Vec::new();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if path.is_dir() {
            if !name.starts_with('.') && !SKIP.contains(&name.as_str()) {
                subdirs.push(path);
            }
        } else if is_dockerfile(&name) && found.len() < MAX_FOUND {
            found.push(path);
        }
    }
    if depth < MAX_DEPTH {
        for sub in subdirs {
            walk(&sub, depth + 1, found);
        }
    }
}

/// Source paths of the `COPY` and `ADD` instructions that take files from the
/// build context.
fn copied_paths(dockerfile: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in dockerfile.lines() {
        let mut words = line.split_whitespace();
        let Some(instruction) = words.next() else {
            continue;
        };
        if !instruction.eq_ignore_ascii_case("copy") && !instruction.eq_ignore_ascii_case("add") {
            continue;
        }
        let args: Vec<&str> = words.collect();
        if args.iter().any(|a| a.starts_with("--from")) || args.first() == Some(&"[") {
            continue;
        }
        let args: Vec<&str> = args.into_iter().filter(|a| !a.starts_with("--")).collect();
        if args.len() < 2 {
            continue;
        }
        for source in &args[..args.len() - 1] {
            if source.contains("://") || source.starts_with('$') {
                continue;
            }
            // `src/*.rs` → `src`.
            let fixed: String = source.chars().take_while(|c| !"*?[".contains(*c)).collect();
            let fixed = fixed.trim_end_matches('/').trim_start_matches("./");
            if !fixed.is_empty() && fixed != "." {
                paths.push(fixed.to_string());
            }
        }
    }
    paths
}

/// The build context: the Dockerfile's directory, unless what it copies is only
/// found at the repository root (a monorepo Dockerfile built from the root).
fn context_for(root: &Path, dir: &Path, contents: &str) -> PathBuf {
    if dir == root {
        return dir.to_path_buf();
    }
    let paths = copied_paths(contents);
    let in_dir = paths.iter().filter(|p| dir.join(p).exists()).count();
    let in_root = paths.iter().filter(|p| root.join(p).exists()).count();
    if in_root > in_dir {
        root.to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

impl Detector for Dockerfiles {
    fn detect(&self, root: &Path) -> Vec<Candidate> {
        let mut found = Vec::new();
        walk(root, 0, &mut found);
        // The root's first, then shallower before deeper, plain `Dockerfile` first.
        found.sort_by_key(|p| {
            let depth = p
                .strip_prefix(root)
                .map(|r| r.components().count())
                .unwrap_or(0);
            let plain = p.file_name().is_some_and(|n| n == "Dockerfile");
            (depth, !plain, p.clone())
        });
        found
            .into_iter()
            .filter_map(|path| {
                let contents = read_small(&path)?;
                let dir = path.parent()?.to_path_buf();
                let http_port = contents.lines().find_map(|line| {
                    let rest = line.trim().strip_prefix("EXPOSE ")?;
                    rest.split_whitespace()
                        .next()?
                        .split('/')
                        .next()?
                        .parse()
                        .ok()
                });
                let context = relative(root, &context_for(root, &dir, &contents));
                let dockerfile = relative(root, &path);
                let where_ = relative(root, &dir);
                let mut title = format!(
                    "Dockerfile in {}",
                    if where_ == "." { "the root" } else { &where_ }
                );
                if path.file_name().is_some_and(|n| n != "Dockerfile") {
                    title = dockerfile.clone();
                }
                let mut detail = dockerfile.clone();
                if context != where_ {
                    detail += &format!(
                        " · built from {}",
                        if context == "." { "the root" } else { &context }
                    );
                }
                if let Some(p) = http_port {
                    detail += &format!(" · exposes {p}");
                }
                Some(Candidate {
                    detector: "dockerfile".into(),
                    title,
                    detail,
                    build: Build::Dockerfile {
                        dockerfile,
                        context,
                    },
                    http_port,
                    env: vec![],
                    command: vec![],
                    notes: vec![],
                })
            })
            .collect()
    }
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
    fn finds_monorepo_dockerfiles() {
        let dir = std::env::temp_dir().join(format!("facade-docker-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "package.json", "{}");
        write(&dir, "pnpm-lock.yaml", "");
        // Built from the root: copies the workspace files.
        write(
            &dir,
            "apps/web/Dockerfile",
            "FROM node:22\nCOPY package.json pnpm-lock.yaml ./\nCOPY apps/web apps/web\nEXPOSE 3000\n",
        );
        // Self-contained.
        write(&dir, "services/api/src/main.rs", "");
        write(
            &dir,
            "services/api/Dockerfile",
            "FROM rust\nCOPY --from=x /a /b\nCOPY src ./src\n",
        );
        write(&dir, "services/api/Dockerfile.dev", "FROM rust\n");
        write(&dir, "node_modules/x/Dockerfile", "FROM x\n");
        let found = Dockerfiles.detect(&dir);
        let builds: Vec<_> = found
            .iter()
            .map(|c| match &c.build {
                Build::Dockerfile {
                    dockerfile,
                    context,
                } => (dockerfile.as_str(), context.as_str()),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            builds,
            [
                ("apps/web/Dockerfile", "."),
                ("services/api/Dockerfile", "services/api"),
                ("services/api/Dockerfile.dev", "services/api"),
            ]
        );
        assert_eq!(found[0].http_port, Some(3000));
        assert_eq!(
            found[0].detail,
            "apps/web/Dockerfile · built from the root · exposes 3000"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
