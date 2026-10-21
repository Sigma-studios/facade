//! Python programs: a script with a `__main__` block, a package with `__main__.py`,
//! or a command declared in `pyproject.toml`; dependencies from `requirements.txt`
//! and/or `pyproject.toml`.

use std::path::{Path, PathBuf};

use facade_core::api::Candidate;
use facade_core::{Build, DEFAULT_PYTHON, PythonEntry, SpecError};

use super::{Detector, env_hints, port_hint, read_small};

pub struct Python;

/// Directories never scanned: environments, caches and other languages' trees.
const SKIP_DIRS: &[&str] = &[
    "venv",
    ".venv",
    "env",
    "node_modules",
    "__pycache__",
    "site-packages",
    "target",
    "dist",
    "build",
];

impl Detector for Python {
    fn detect(&self, root: &Path) -> Vec<Candidate> {
        let requirements = root.join("requirements.txt").is_file();
        let pyproject =
            read_small(&root.join("pyproject.toml")).and_then(|s| s.parse::<toml::Table>().ok());
        let mut entries: Vec<(PythonEntry, Option<PathBuf>)> = Vec::new();
        if let Some(project) = &pyproject {
            for scripts in [
                project.get("project").and_then(|p| p.get("scripts")),
                project
                    .get("tool")
                    .and_then(|t| t.get("poetry"))
                    .and_then(|p| p.get("scripts")),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(table) = scripts.as_table() {
                    for name in table.keys() {
                        if let Ok(entry) = PythonEntry::parse(name) {
                            entries.push((entry, None));
                        }
                    }
                }
            }
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        files.sort();
        for path in &files {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if path.is_file() && name.ends_with(".py") && name != "setup.py" {
                if read_small(path).is_some_and(|s| has_main_block(&s)) {
                    entries.push((PythonEntry::Script(name.to_string()), Some(path.clone())));
                }
            } else if path.is_dir()
                && path.join("__init__.py").is_file()
                && path.join("__main__.py").is_file()
            {
                entries.push((
                    PythonEntry::Module(name.to_string()),
                    Some(path.join("__main__.py")),
                ));
            }
        }
        if entries.is_empty() {
            return vec![];
        }

        let version = python_version(root, pyproject.as_ref());
        let sources = python_sources(root);
        let env: Vec<String> = {
            let all: String = sources.iter().filter_map(|p| read_small(p)).collect();
            env_hints(&all)
        };
        let deps = match (requirements, pyproject.is_some()) {
            (true, true) => "requirements.txt + pyproject.toml",
            (true, false) => "requirements.txt",
            (false, true) => "pyproject.toml",
            (false, false) => "standard library only",
        };

        let mut candidates: Vec<(i32, Candidate)> = entries
            .into_iter()
            .map(|(entry, file)| {
                let source = file.as_deref().and_then(read_small).unwrap_or_default();
                let cli = cli_hints(&source);
                let http_port = cli.port.or_else(|| port_hint(&source));
                let mut command = Vec::new();
                let mut notes = Vec::new();
                match cli.host.as_deref() {
                    Some("127.0.0.1" | "localhost" | "::1") => {
                        command = entry.command();
                        command.extend(["--host".to_string(), "0.0.0.0".to_string()]);
                        notes.push(format!(
                            "listens on 127.0.0.1 by default, which is unreachable from outside \
                             its container: suggested command {}",
                            facade_core::join_command(&command)
                        ));
                    }
                    None if binds_loopback(&source) => notes.push(
                        "binds 127.0.0.1 in its code: it must listen on 0.0.0.0 to be reachable \
                         from outside its container"
                            .into(),
                    ),
                    _ => {}
                }
                let shown = match &entry {
                    PythonEntry::Script(s) => s.clone(),
                    PythonEntry::Module(m) => format!("-m {m}"),
                    PythonEntry::Command(c) => format!("{c} (from pyproject.toml)"),
                };
                let mut detail = format!(
                    "{shown} · Python {} · {deps}",
                    version.as_deref().unwrap_or(DEFAULT_PYTHON)
                );
                if let Some(port) = http_port {
                    detail += &format!(" · listens on {port}");
                }
                let stem = shown.to_lowercase();
                let server_like = ["server", "app", "main", "bot", "run", "serve", "api", "web"]
                    .iter()
                    .any(|w| stem.contains(w));
                let score = http_port.is_some() as i32 * 2
                    + server_like as i32
                    + matches!(entry, PythonEntry::Command(_)) as i32;
                let entry = match entry {
                    PythonEntry::Script(s) => s,
                    PythonEntry::Module(m) => format!("-m {m}"),
                    PythonEntry::Command(c) => c,
                };
                (
                    score,
                    Candidate {
                        detector: "python".into(),
                        title: format!("Python {shown}"),
                        detail,
                        build: Build::Python {
                            version: version.clone(),
                            entry,
                        },
                        http_port,
                        env: env.clone(),
                        command,
                        notes,
                    },
                )
            })
            .collect();
        candidates.sort_by_key(|(score, _)| -score);
        candidates.into_iter().map(|(_, c)| c).collect()
    }
}

fn has_main_block(source: &str) -> bool {
    source.lines().any(|l| {
        let l = l.trim().replace('\'', "\"").replace(' ', "");
        l.starts_with("if__name__==\"__main__\"") || l.starts_with("if\"__main__\"==__name__")
    })
}

#[derive(Default, Debug, PartialEq)]
struct CliHints {
    host: Option<String>,
    port: Option<u16>,
}

/// Defaults of `--host` / `--port` options declared with argparse or click, and
/// `port=` / `host=` keywords of the usual `run(...)` calls.
fn cli_hints(source: &str) -> CliHints {
    let mut hints = CliHints::default();
    for (i, _) in source
        .match_indices("add_argument(")
        .chain(source.match_indices("option("))
    {
        let call = call_text(&source[i..]);
        let default = keyword(call, "default");
        if call.contains("\"--port\"") || call.contains("'--port'") {
            hints.port = hints.port.or(default.and_then(|d| d.parse().ok()));
        } else if call.contains("\"--host\"") || call.contains("'--host'") {
            hints.host = hints.host.or(default.map(str::to_string));
        }
    }
    for (i, _) in source.match_indices(".run(") {
        let call = call_text(&source[i..]);
        hints.port = hints
            .port
            .or(keyword(call, "port").and_then(|p| p.parse().ok()));
        hints.host = hints
            .host
            .clone()
            .or(keyword(call, "host").map(str::to_string));
    }
    if hints.port.is_none() {
        // os.environ.get("PORT", "8000") / os.getenv("PORT", 8000)
        for (i, _) in source.match_indices("PORT") {
            let rest = &source[i + 4..];
            let rest = rest.trim_start_matches(['"', '\'']).trim_start();
            if let Some(rest) = rest.strip_prefix(',') {
                let value: String = rest
                    .trim_start()
                    .trim_start_matches(['"', '\''])
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Ok(port) = value.parse() {
                    hints.port = Some(port);
                    break;
                }
            }
        }
    }
    hints
}

/// The text of a call up to its closing parenthesis (or a reasonable length).
fn call_text(from: &str) -> &str {
    let mut depth = 0;
    for (i, c) in from.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &from[..i];
                }
            }
            _ => {}
        }
        if i > 600 {
            return &from[..i];
        }
    }
    from
}

/// The literal value of `name=` in a call: a number or a quoted string.
fn keyword<'a>(call: &'a str, name: &str) -> Option<&'a str> {
    let pattern = format!("{name}=");
    let start = call.match_indices(&pattern).find_map(|(i, _)| {
        let before = call[..i].chars().last();
        (before.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')).then_some(i)
    })? + pattern.len();
    let rest = call[start..].trim_start();
    if let Some(q) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let inner = &rest[1..];
        return inner.find(q).map(|end| &inner[..end]);
    }
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '_')
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

fn binds_loopback(source: &str) -> bool {
    [
        "(\"127.0.0.1\",",
        "('127.0.0.1',",
        "host=\"127.0.0.1\"",
        "host='127.0.0.1'",
    ]
    .iter()
    .any(|p| source.contains(p))
}

/// The Python a project asks for: `.python-version`, `runtime.txt`, or an exact
/// or upper bound in `requires-python`. `None` means the default.
fn python_version(root: &Path, pyproject: Option<&toml::Table>) -> Option<String> {
    let minor = |s: &str| -> Option<u32> {
        let rest = s.trim().strip_prefix("3.")?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    if let Some(v) =
        read_small(&root.join(".python-version")).and_then(|s| s.lines().next().and_then(minor))
    {
        return Some(format!("3.{v}"));
    }
    if let Some(v) = read_small(&root.join("runtime.txt"))
        .and_then(|s| minor(s.trim().trim_start_matches("python-")))
    {
        return Some(format!("3.{v}"));
    }
    let requires = pyproject?
        .get("project")?
        .get("requires-python")?
        .as_str()?
        .to_string();
    let default = minor(DEFAULT_PYTHON)?;
    let mut ceiling: Option<u32> = None;
    for spec in requires.split(',') {
        let spec = spec.trim();
        if let Some(v) = spec.strip_prefix("==").or_else(|| spec.strip_prefix("~=")) {
            return minor(v).map(|m| format!("3.{m}"));
        }
        if let Some(v) = spec.strip_prefix("<=") {
            ceiling = minor(v);
        } else if let Some(v) = spec.strip_prefix('<') {
            ceiling = minor(v).map(|m| m.saturating_sub(1));
        }
    }
    ceiling.filter(|c| *c < default).map(|c| format!("3.{c}"))
}

/// `.py` files of the project, for environment variable hints (at most 300).
fn python_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
            } else if name.ends_with(".py") && out.len() < 300 {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Installs what the project declares, then runs its entry. `pip install .` only
/// for real packages (with a build system); an application `pyproject.toml` with
/// just `[project] dependencies` gets those installed instead, since pip cannot
/// build a project that does not say how.
pub fn dockerfile(version: Option<&str>, entry: &str) -> Result<String, SpecError> {
    let version = version.unwrap_or(DEFAULT_PYTHON);
    // A CMD, which an app's `command` replaces (with arguments added, say).
    let cmd =
        serde_json::to_string(&PythonEntry::parse(entry)?.command()).expect("strings serialize");
    Ok(format!(
        r#"FROM docker.io/library/python:{version}-slim
ENV PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1 PIP_DISABLE_PIP_VERSION_CHECK=1 PIP_ROOT_USER_ACTION=ignore
RUN apt-get update && apt-get install -y --no-install-recommends tzdata && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY . .
RUN --mount=type=cache,target=/root/.cache/pip,id=facade-pip python - <<'EOF'
import os, subprocess, sys

def pip(*args):
    print("pip install", *args, flush=True)
    subprocess.check_call([sys.executable, "-m", "pip", "install", *args])

if os.path.exists("requirements.txt"):
    pip("-r", "requirements.txt")
if os.path.exists("pyproject.toml"):
    try:
        import tomllib
        with open("pyproject.toml", "rb") as f:
            data = tomllib.load(f)
    except ImportError:
        text = open("pyproject.toml").read()
        data = {{"build-system": {{}}}} if "[build-system]" in text or "[project]" in text else {{}}
    if "build-system" in data or "poetry" in data.get("tool", {{}}):
        pip(".")
    elif data.get("project", {{}}).get("dependencies"):
        pip(*data["project"]["dependencies"])
EOF
CMD {cmd}
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, contents: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("facade-py-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_a_stdlib_server_that_listens_on_loopback() {
        // Shaped like scrum-online: argparse defaults to 127.0.0.1.
        let dir = scratch("stdlib");
        write(
            &dir,
            "server.py",
            r#"import argparse
def build_parser():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1", help="bind address")
    ap.add_argument("--port", type=int, default=8000, help="port (default: 8000)")
    return ap
if __name__ == "__main__":
    main()
"#,
        );
        write(
            &dir,
            "poker/handler.py",
            "import os\nX = os.environ.get('POKER_SECRET')\n",
        );
        write(&dir, "static/index.html", "<html></html>");
        let found = Python.detect(&dir);
        assert_eq!(found.len(), 1);
        let c = &found[0];
        assert_eq!(c.http_port, Some(8000));
        assert_eq!(c.command, ["python", "server.py", "--host", "0.0.0.0"]);
        assert_eq!(c.env, ["POKER_SECRET"]);
        assert_eq!(
            c.build,
            Build::Python {
                version: None,
                entry: "server.py".into()
            }
        );
        assert!(c.detail.contains("standard library only"), "{}", c.detail);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finds_a_bot_with_requirements_and_a_pinned_python() {
        let dir = scratch("bot");
        write(&dir, "requirements.txt", "discord.py==2.4.0\n");
        write(&dir, ".python-version", "3.12.4\n");
        write(&dir, "helpers.py", "def f(): pass\n");
        write(
            &dir,
            "bot.py",
            "import os\nTOKEN = os.getenv('DISCORD_TOKEN')\nif __name__ == '__main__':\n    run()\n",
        );
        write(&dir, "venv/lib/x.py", "os.getenv('IGNORED')\n");
        let found = Python.detect(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].env, ["DISCORD_TOKEN"]);
        assert_eq!(
            found[0].build,
            Build::Python {
                version: Some("3.12".into()),
                entry: "bot.py".into()
            }
        );
        assert!(found[0].command.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_pyproject() {
        let dir = scratch("pyproject");
        write(
            &dir,
            "pyproject.toml",
            "[project]\nname = \"api\"\nrequires-python = \">=3.10,<3.12\"\n\
             [project.scripts]\napi-server = \"api.main:run\"\n",
        );
        write(&dir, "api/__init__.py", "");
        write(
            &dir,
            "api/__main__.py",
            "import uvicorn\nuvicorn.run(app, host=\"0.0.0.0\", port=8080)\n",
        );
        let found = Python.detect(&dir);
        let entries: Vec<_> = found
            .iter()
            .map(|c| match &c.build {
                Build::Python { entry, version } => (entry.as_str(), version.as_deref()),
                _ => unreachable!(),
            })
            .collect();
        // The module listens on a port, so it ranks first.
        assert_eq!(
            entries,
            [("-m api", Some("3.11")), ("api-server", Some("3.11"))]
        );
        assert_eq!(found[0].http_port, Some(8080));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ignores_projects_without_python_entry_points() {
        let dir = scratch("none");
        write(&dir, "Cargo.toml", "[package]\nname = \"x\"\n");
        write(&dir, "scripts/gen.py", "print('hi')\n");
        assert!(Python.detect(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_entrypoints() {
        let df = dockerfile(None, "server.py").unwrap();
        assert!(df.starts_with(&format!(
            "FROM docker.io/library/python:{DEFAULT_PYTHON}-slim"
        )));
        assert!(df.contains(r#"CMD ["python","server.py"]"#));
        assert!(
            dockerfile(Some("3.12"), "-m poker")
                .unwrap()
                .contains(r#"CMD ["python","-m","poker"]"#)
        );
        assert!(
            dockerfile(None, "mybot")
                .unwrap()
                .contains(r#"CMD ["mybot"]"#)
        );
        assert!(dockerfile(None, "../evil.py").is_err());
    }
}
