//! Docker Compose files as facade apps: each service becomes an app on one node,
//! linked to the others under its service name, so the hosts written in the file
//! (`postgres://…@db:5432`) keep working.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use facade_core::manifest::EnvEntry;
use facade_core::stack::ServiceSpec;
use facade_core::{Build, HealthCheck, PortMap, Protocol, Volume};
use serde_yaml_ng::Value;

/// File names Compose itself looks for, plus the usual variants (`compose.prod.yml`).
pub fn is_compose_file(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let Some(stem) = name
        .strip_suffix(".yml")
        .or_else(|| name.strip_suffix(".yaml"))
    else {
        return false;
    };
    let stem = stem.strip_prefix("docker-").unwrap_or(stem);
    stem == "compose" || stem.strip_prefix("compose.").is_some_and(|v| !v.is_empty())
}

/// Compose files at the root of the repository and one directory down.
pub fn find(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut subdirs: Vec<PathBuf> = entries
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
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .is_some_and(|n| is_compose_file(&n.to_string_lossy()))
            })
            .collect();
        // `compose.yaml` before `compose.prod.yaml`.
        files.sort_by_key(|p| (p.to_string_lossy().matches('.').count(), p.clone()));
        found.extend(files.iter().map(|p| crate::lang::relative(root, p)));
    }
    found
}

/// One service of a compose file, as a stack service.
pub struct FileService {
    /// The service's name in facade (the compose name, lowercase, `_` → `-`).
    pub service: String,
    /// Its name in the file, which the other services use as its host name.
    pub host: String,
    pub spec: ServiceSpec,
    pub env: BTreeMap<String, EnvEntry>,
}

pub struct Plan {
    pub services: Vec<FileService>,
    pub notes: Vec<String>,
    /// Values generated for secrets the file expects from the environment, to keep
    /// as stack variables so the next deploys use the same.
    pub generated: Vec<(String, String)>,
}

/// Images whose ports are only for other services: they stay unpublished.
const INTERNAL_IMAGES: &[&str] = &[
    "postgres",
    "postgis",
    "timescale",
    "mysql",
    "mariadb",
    "redis",
    "valkey",
    "keydb",
    "dragonfly",
    "mongo",
    "memcached",
    "rabbitmq",
    "nats",
    "clickhouse",
    "elasticsearch",
    "opensearch",
    "minio",
    "meilisearch",
    "typesense",
    "qdrant",
];

fn is_internal_image(image: &str) -> bool {
    let name = image.rsplit('/').next().unwrap_or(image);
    let name = name.split([':', '@']).next().unwrap_or(name);
    INTERNAL_IMAGES.iter().any(|i| name.starts_with(i))
}

/// `my_service.v2` → `my-service-v2`, as a service or volume name.
fn service_name(name: &str) -> String {
    let name: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let mut name: String = name
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    name.truncate(40);
    name.trim_end_matches('-').to_string()
}

fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    [
        "PASSWORD",
        "PASSWD",
        "SECRET",
        "TOKEN",
        "PRIVATE",
        "CREDENTIAL",
    ]
    .iter()
    .any(|w| k.contains(w))
        || k.ends_with("_KEY")
        || k == "KEY"
}

/// Compose's `${VAR}`, `${VAR:-default}`, `${VAR-default}`, `${VAR:?err}`, `$VAR` and
/// `$$`, from the `.env` file next to the compose file. `Err` names a variable
/// with no value.
fn interpolate(s: &str, vars: &HashMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(after) = after.strip_prefix('$') {
            out.push('$');
            rest = after;
        } else if let Some(body) = after.strip_prefix('{') {
            let Some(end) = body.find('}') else {
                out.push_str(&rest[i..]);
                return Ok(out);
            };
            let expr = &body[..end];
            let (name, op, default) = match expr.find([':', '-', '?']) {
                Some(j) => {
                    let op_len = if expr[j..].starts_with(":-") || expr[j..].starts_with(":?") {
                        2
                    } else {
                        1
                    };
                    (&expr[..j], &expr[j..j + op_len], Some(&expr[j + op_len..]))
                }
                None => (expr, "", None),
            };
            let value = vars
                .get(name)
                .filter(|v| !(op.starts_with(':') && v.is_empty()));
            match (value, op, default) {
                (Some(v), _, _) => out.push_str(v),
                (None, ":-" | "-", Some(d)) => out.push_str(d),
                _ => return Err(name.to_string()),
            }
            rest = &body[end + 1..];
        } else {
            let name: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                out.push('$');
                rest = after;
                continue;
            }
            match vars.get(&name) {
                Some(v) => out.push_str(v),
                None => return Err(name),
            }
            rest = &after[name.len()..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Names of the variables a text interpolates (`${NAME…}` and `$NAME`).
fn variables_in(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find('$') {
        let after = &rest[i + 1..];
        if let Some(after) = after.strip_prefix('$') {
            rest = after;
            continue;
        }
        let body = after.strip_prefix('{').unwrap_or(after);
        let name: String = body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && !names.contains(&name)
        {
            names.push(name);
        }
        rest = body;
    }
    names
}

/// `KEY=value` lines of a `.env` file.
fn dotenv(path: &Path) -> HashMap<String, String> {
    let Some(text) = crate::lang::read_small(path) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line);
            if line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

fn shell_words(line: &str) -> Vec<String> {
    facade_core::split_command(line)
        .unwrap_or_else(|_| line.split_whitespace().map(str::to_string).collect())
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::Sequence(seq) => seq.iter().filter_map(scalar).collect(),
        other => scalar(other).map(|s| shell_words(&s)).unwrap_or_default(),
    }
}

/// `512m`, `1g`, `1.5G`, a byte count → MB.
fn memory_mb(v: &Value) -> Option<u64> {
    let s = scalar(v)?.trim().to_ascii_lowercase();
    let s = s.strip_suffix('b').unwrap_or(&s);
    let (num, unit) = match s.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let n: f64 = num.trim().parse().ok()?;
    let bytes = match unit {
        "" => n,
        "k" => n * 1024.0,
        "m" => n * 1024.0 * 1024.0,
        "g" => n * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(((bytes / (1024.0 * 1024.0)).round() as u64).max(1))
}

/// A path written in the compose file, relative to the repository root. Fails if
/// it leaves the repository.
fn repo_path(compose_dir: &Path, path: &str) -> Option<String> {
    let mut out = PathBuf::new();
    for c in compose_dir.join(path).components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            Component::Normal(p) => out.push(p),
            _ => return None,
        }
    }
    let s = out.to_string_lossy().replace('\\', "/");
    Some(if s.is_empty() { ".".into() } else { s })
}

/// A port a service listens on (where Compose would publish it does not matter:
/// facade publishes nothing unless `x-facade` says so).
struct Port {
    container: u16,
    count: u16,
    protocol: Protocol,
}

fn parse_port(v: &Value) -> Option<Port> {
    if let Value::Mapping(m) = v {
        let get = |k: &str| m.get(Value::String(k.into())).and_then(scalar);
        let container: u16 = get("target")?.parse().ok()?;
        let protocol = match get("protocol").as_deref() {
            Some("udp") => Protocol::Udp,
            _ => Protocol::Tcp,
        };
        return Some(Port {
            container,
            count: 1,
            protocol,
        });
    }
    let s = scalar(v)?;
    let (s, protocol) = match s.rsplit_once('/') {
        Some((p, "udp")) => (p.to_string(), Protocol::Udp),
        Some((p, _)) => (p.to_string(), Protocol::Tcp),
        None => (s, Protocol::Tcp),
    };
    // `[[ip:]host:]container`: the container side is last.
    let container = s.rsplit(':').next()?;
    let range = |r: &str| -> Option<(u16, u16)> {
        match r.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u16, u16) = (a.parse().ok()?, b.parse().ok()?);
                (b >= a).then_some((a, b - a + 1))
            }
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (container, count) = range(container)?;
    Some(Port {
        container,
        count,
        protocol,
    })
}

/// Reads `file` (relative to `root`) and turns each service into a stack service.
/// `variables` are the stack's, used like Compose's environment (over the `.env`
/// next to the file); `stack` names the stack in messages.
pub fn plan(
    root: &Path,
    file: &str,
    stack: &str,
    variables: &HashMap<String, String>,
    secret: &dyn Fn() -> String,
) -> anyhow::Result<Plan> {
    let path = root.join(file);
    let text = crate::lang::read_small(&path).with_context(|| format!("cannot read {file}"))?;
    let mut doc: Value =
        serde_yaml_ng::from_str(&text).with_context(|| format!("{file} is not valid YAML"))?;
    doc.apply_merge().ok();
    let compose_dir = Path::new(file)
        .parent()
        .unwrap_or(Path::new(""))
        .to_path_buf();
    let mut vars = dotenv(&path.with_file_name(".env"));
    vars.extend(variables.iter().map(|(k, v)| (k.clone(), v.clone())));
    // Passwords the file expects from the environment (`${DB_PASSWORD}`) get one
    // random value, shared by every service that uses them, as Compose would
    // share the variable.
    let mut generated: Vec<(String, String)> = Vec::new();
    for var in variables_in(&text) {
        if !vars.contains_key(&var) && is_sensitive_key(&var) {
            let value = secret();
            vars.insert(var.clone(), value.clone());
            generated.push((var, value));
        }
    }
    let Some(Value::Mapping(services)) = doc.get("services") else {
        bail!("{file} has no services");
    };

    let mut notes: Vec<String> = generated
        .iter()
        .map(|(v, _)| {
            format!("${{{v}}} had no value: facade generated one, kept in the stack's variables")
        })
        .collect();
    let mut out: Vec<FileService> = Vec::new();
    for (host, def) in services {
        let Some(host) = scalar(host) else {
            continue;
        };
        let service = service_name(&host);
        let app = format!("{stack}.{service}");
        let mut note = |n: String| notes.push(format!("{host}: {n}"));
        if facade_core::validate_name("service name", &service).is_err() {
            note("skipped: its name cannot be used".into());
            continue;
        }
        let field = |k: &str| def.get(k);
        if field("profiles").is_some_and(|p| !strings(p).is_empty()) {
            note("skipped (only started with a profile)".into());
            continue;
        }
        let text_field = |k: &str, note: &mut dyn FnMut(String)| -> Option<String> {
            let raw = scalar(field(k)?)?;
            match interpolate(&raw, &vars) {
                Ok(v) => Some(v),
                Err(var) => {
                    note(format!(
                        "{k} uses ${{{var}}}, which has no value; left empty"
                    ));
                    interpolate_lossy(&raw, &vars)
                }
            }
        };

        let image = text_field("image", &mut note);
        let mut spec = ServiceSpec::default();
        match field("build") {
            Some(build) => {
                let (context, dockerfile) = match build {
                    Value::Mapping(_) => (
                        build.get("context").and_then(scalar).unwrap_or(".".into()),
                        build
                            .get("dockerfile")
                            .and_then(scalar)
                            .unwrap_or("Dockerfile".into()),
                    ),
                    other => (scalar(other).unwrap_or(".".into()), "Dockerfile".into()),
                };
                if build.get("args").is_some() || build.get("target").is_some() {
                    note("build args and target are not supported; the Dockerfile's defaults are used".into());
                }
                if context.contains("://") || context.starts_with("git@") {
                    note(format!("skipped: remote build context {context}"));
                    continue;
                }
                let Some(context) = repo_path(&compose_dir, &context) else {
                    note("skipped: its build context is outside the repository".into());
                    continue;
                };
                let Some(dockerfile) = repo_path(Path::new(&context), &dockerfile) else {
                    note("skipped: its Dockerfile is outside the repository".into());
                    continue;
                };
                spec.build = Some(Build::Dockerfile {
                    dockerfile,
                    context,
                });
            }
            None => match &image {
                Some(image) => spec.image = Some(image.clone()),
                None => {
                    note("skipped: no image and no build".into());
                    continue;
                }
            },
        }
        let internal = spec.image.as_deref().is_some_and(is_internal_image);

        let command = field("command").map(strings).unwrap_or_default();
        if !command.is_empty() {
            spec.command = Some(command);
        }
        for unsupported in [
            "entrypoint",
            "user",
            "working_dir",
            "privileged",
            "cap_add",
            "devices",
            "extends",
        ] {
            if field(unsupported).is_some() {
                note(format!("`{unsupported}` is not supported and was left out"));
            }
        }
        if field("network_mode").is_some() {
            note("network_mode is ignored: stack services share the stack's network".into());
        }
        if let Some(files) = field("env_file") {
            let files = match files {
                Value::Sequence(s) => s
                    .iter()
                    .filter_map(|f| scalar(f).or_else(|| f.get("path").and_then(scalar)))
                    .collect(),
                other => scalar(other).into_iter().collect::<Vec<_>>(),
            };
            note(format!(
                "env_file {} not read: set those variables with `facade env set {app}`",
                files.join(", ")
            ));
        }

        // Environment.
        let generated_names: Vec<&String> = generated.iter().map(|(k, _)| k).collect();
        let mut env = BTreeMap::new();
        let mut put = |key: String, raw: Option<String>, note: &mut dyn FnMut(String)| {
            let sensitive = is_sensitive_key(&key)
                || raw
                    .as_deref()
                    .is_some_and(|r| variables_in(r).iter().any(|v| generated_names.contains(&v)));
            let value = match raw.map(|r| interpolate(&r, &vars)) {
                Some(Ok(v)) => Some(v),
                Some(Err(var)) => {
                    note(format!(
                        "{key} needs ${{{var}}}: set it with `facade env set {stack} {var}`"
                    ));
                    None
                }
                None => {
                    note(format!(
                        "{key} has no value: set it with `facade env set {app} {key}`"
                    ));
                    None
                }
            };
            let entry = match value {
                Some(v) if !sensitive => EnvEntry::Plain(v),
                value => EnvEntry::Detailed { value, sensitive },
            };
            env.insert(key, entry);
        };
        match field("environment") {
            Some(Value::Mapping(m)) => {
                for (k, v) in m {
                    if let Some(k) = scalar(k) {
                        put(k, scalar(v), &mut note);
                    }
                }
            }
            Some(Value::Sequence(list)) => {
                for item in list.iter().filter_map(scalar) {
                    match item.split_once('=') {
                        Some((k, v)) => put(k.to_string(), Some(v.to_string()), &mut note),
                        None => put(item, None, &mut note),
                    }
                }
            }
            _ => {}
        }

        // Ports: other services reach every port anyway. The first TCP one is where
        // HTTP for the service's domains goes; nothing is published on the node's
        // public address unless `x-facade` says so.
        let declared: Vec<Port> = field("ports")
            .and_then(Value::as_sequence)
            .map(|s| s.iter().filter_map(parse_port).collect())
            .unwrap_or_default();
        let mut unpublished = Vec::new();
        for port in declared {
            if !internal
                && spec.http_port.is_none()
                && port.protocol == Protocol::Tcp
                && port.count == 1
            {
                spec.http_port = Some(port.container);
            } else {
                unpublished.push(port.container.to_string());
            }
        }
        if !unpublished.is_empty() {
            note(format!(
                "port {} not published (the other services reach it at {host}:{}); publish it with x-facade",
                unpublished.join(", "),
                unpublished[0]
            ));
        }

        // Volumes: named ones are the stack's, shared by the services naming them;
        // anonymous ones get a name.
        let mut volumes: Vec<Volume> = Vec::new();
        for v in field("volumes")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
        {
            let (kind, source, target) = match v {
                Value::Mapping(_) => (
                    v.get("type").and_then(scalar).unwrap_or("volume".into()),
                    v.get("source").and_then(scalar),
                    v.get("target").and_then(scalar),
                ),
                other => {
                    let Some(s) = scalar(other) else { continue };
                    let parts: Vec<&str> = s.split(':').collect();
                    match parts.as_slice() {
                        [target] => ("volume".into(), None, Some(target.to_string())),
                        [source, target, ..] => {
                            let bind = source.starts_with(['.', '/', '~', '$']);
                            (
                                if bind { "bind" } else { "volume" }.into(),
                                Some(source.to_string()),
                                Some(target.to_string()),
                            )
                        }
                        _ => continue,
                    }
                }
            };
            let Some(target) = target else { continue };
            if kind != "volume" {
                note(format!(
                    "{kind} mount {} → {target} skipped: build the files into the image, or use a volume",
                    source.unwrap_or_default()
                ));
                continue;
            }
            // A named volume's size, from its `x-facade:` in the top-level `volumes:`.
            let size_mb = match source.as_deref().map(|s| {
                doc.get("volumes")
                    .and_then(|v| v.get(s))
                    .and_then(|v| v.get("x-facade"))
                    .and_then(|x| x.get("size"))
                    .and_then(scalar)
            }) {
                Some(Some(size)) => match facade_core::parse_size_mb(&size) {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        note(format!("volume size ignored: {e}"));
                        None
                    }
                },
                _ => None,
            };
            let base = source.unwrap_or_else(|| {
                let last = Path::new(&target)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or("data".into());
                format!("{service}-{last}")
            });
            let vol_name = service_name(&base);
            let volume = Volume {
                name: if vol_name.is_empty() {
                    "data".into()
                } else {
                    vol_name
                },
                path: target.trim_end_matches('/').to_string(),
                size_mb,
            };
            if volume.validate().is_err()
                || volumes
                    .iter()
                    .any(|v| v.name == volume.name || v.path == volume.path)
            {
                note(format!("volume {volume} skipped (invalid or repeated)"));
                continue;
            }
            volumes.push(volume);
        }
        if !volumes.is_empty() {
            spec.volumes = Some(volumes);
        }

        let resource_limits = def
            .get("deploy")
            .and_then(|d| d.get("resources"))
            .and_then(|r| r.get("limits"));
        if let Some(m) = resource_limits
            .and_then(|l| l.get("memory"))
            .or(field("mem_limit"))
        {
            spec.limits.memory_mb = memory_mb(m);
        }
        if let Some(c) = resource_limits
            .and_then(|l| l.get("cpus"))
            .or(field("cpus"))
        {
            spec.limits.cpus = scalar(c).and_then(|c| c.parse().ok()).filter(|c| *c > 0.0);
        }
        if let Some(p) = resource_limits
            .and_then(|l| l.get("pids"))
            .or(field("pids_limit"))
            .and_then(scalar)
        {
            // -1 and 0 mean no limit to Compose; facade keeps its default then.
            spec.limits.pids = p.parse().ok().filter(|p| *p > 0);
            if let Some(n) = spec.limits.pids.filter(|n| *n < 16) {
                note(format!("pids limit {n} raised to 16"));
                spec.limits.pids = Some(16);
            }
        }
        if let Some(t) = field("stop_grace_period") {
            match duration_secs(t) {
                Some(secs) if secs <= facade_core::MAX_STOP_TIMEOUT => {
                    spec.stop_timeout_secs = Some(secs)
                }
                Some(_) => {
                    note(format!(
                        "stop_grace_period lowered to {} seconds",
                        facade_core::MAX_STOP_TIMEOUT
                    ));
                    spec.stop_timeout_secs = Some(facade_core::MAX_STOP_TIMEOUT);
                }
                None => note("stop_grace_period is not a duration and was left out".into()),
            }
        }
        if let Some(signal) = field("stop_signal").and_then(scalar) {
            match facade_core::validate_signal(&signal) {
                Ok(()) => spec.stop_signal = Some(signal),
                Err(e) => note(format!("stop_signal left out: {e}")),
            }
        }

        match field("healthcheck") {
            Some(h) => spec.healthcheck = healthcheck(h, &mut note),
            None => {
                spec.healthcheck = spec
                    .image
                    .as_deref()
                    .and_then(facade_core::templates::default_healthcheck)
            }
        }

        let mut depends_on: Vec<String> = Vec::new();
        for key in ["depends_on", "links"] {
            match field(key) {
                Some(Value::Mapping(m)) => depends_on.extend(m.keys().filter_map(scalar)),
                Some(Value::Sequence(s)) => depends_on.extend(
                    s.iter()
                        .filter_map(scalar)
                        .map(|l| l.split(':').next().unwrap_or(&l).to_string()),
                ),
                _ => {}
            }
        }
        let depends_on: Vec<String> = depends_on.iter().map(|d| service_name(d)).collect();
        if !depends_on.is_empty() {
            spec.depends_on = Some(depends_on.clone());
        }

        // `x-facade:` holds what Compose has no words for, over the rest.
        let spec = match field("x-facade") {
            Some(x) => facade_settings(x, &mut note).merged_over(&spec),
            None => spec,
        };
        let host = if facade_core::validate_host(&host).is_ok() {
            host.clone()
        } else {
            service.clone()
        };
        out.push(FileService {
            service,
            host,
            spec,
            env,
        });
    }
    if out.is_empty() {
        bail!("{file} has no service facade can run");
    }
    Ok(Plan {
        services: out,
        notes,
        generated,
    })
}

/// `30s`, `1m30s`, `500ms`, `2h` → seconds (rounded up).
fn duration_secs(v: &Value) -> Option<u32> {
    let s = scalar(v)?;
    let mut total_ms: u64 = 0;
    let mut number = String::new();
    let mut chars = s.trim().chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            continue;
        }
        let mut unit = c.to_string();
        if c == 'm' && chars.peek() == Some(&'s') {
            unit.push(chars.next()?);
        }
        let n: f64 = number.parse().ok()?;
        number.clear();
        total_ms += (n * match unit.as_str() {
            "ms" => 1.0,
            "s" => 1000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            _ => return None,
        }) as u64;
    }
    if !number.is_empty() {
        total_ms += (number.parse::<f64>().ok()? * 1000.0) as u64;
    }
    Some(total_ms.div_ceil(1000) as u32)
}

/// A service's `healthcheck:`.
fn healthcheck(h: &Value, note: &mut dyn FnMut(String)) -> Option<HealthCheck> {
    if h.get("disable").and_then(scalar).as_deref() == Some("true") {
        return Some(HealthCheck {
            test: vec!["NONE".into()],
            ..HealthCheck::shell("")
        });
    }
    let test = match h.get("test")? {
        Value::Sequence(_) => strings(h.get("test")?),
        other => vec!["CMD-SHELL".to_string(), scalar(other)?],
    };
    let mut check = HealthCheck {
        test,
        ..HealthCheck::shell("")
    };
    if let Some(n) = h.get("interval").and_then(duration_secs) {
        check.interval_secs = n.max(1);
    }
    if let Some(n) = h.get("timeout").and_then(duration_secs) {
        check.timeout_secs = n.max(1);
    }
    if let Some(n) = h
        .get("retries")
        .and_then(scalar)
        .and_then(|r| r.parse().ok())
    {
        check.retries = n;
    }
    if let Some(n) = h.get("start_period").and_then(duration_secs) {
        check.start_period_secs = n;
    }
    match check.validate() {
        Ok(()) => Some(check),
        Err(e) => {
            note(format!("healthcheck ignored: {e}"));
            None
        }
    }
}

/// A service's `x-facade:` section: domains, http_port, release, memory (MB),
/// cpus, publish (`3478:3478/udp`, …), delegate_cgroups, backup (`before`, `after`).
fn facade_settings(x: &Value, note: &mut dyn FnMut(String)) -> ServiceSpec {
    let mut spec = ServiceSpec::default();
    let get = |k: &str| x.get(k);
    if let Some(d) = get("domains") {
        spec.domains = Some(strings(d).into_iter().map(|d| d.to_lowercase()).collect());
    }
    if let Some(p) = get("http_port").and_then(scalar) {
        match p.parse() {
            Ok(p) => spec.http_port = Some(p),
            Err(_) => note(format!("x-facade http_port `{p}` is not a port")),
        }
    }
    if let Some(r) = get("release").or(get("release_command")).and_then(scalar) {
        spec.release_command = Some(r);
    }
    if let Some(m) = get("memory") {
        spec.limits.memory_mb = match m {
            Value::Number(n) => n.as_u64(),
            other => memory_mb(other),
        };
    }
    if let Some(c) = get("cpus").and_then(scalar) {
        spec.limits.cpus = c.parse().ok().filter(|c: &f64| *c > 0.0);
    }
    if let Some(p) = get("publish") {
        let mut ports = Vec::new();
        for item in strings(p) {
            match item.parse::<PortMap>() {
                Ok(map) => ports.push(map),
                Err(e) => note(format!("x-facade publish `{item}`: {e}")),
            }
        }
        spec.ports = Some(ports);
    }
    if let Some(b) = get("backup") {
        match serde_yaml_ng::from_value::<facade_core::BackupSpec>(b.clone()) {
            Ok(backup) => spec.backup = Some(backup),
            Err(e) => note(format!("x-facade backup ignored: {e}")),
        }
    }
    if let Some(d) = get("delegate_cgroups").and_then(scalar) {
        match d.as_str() {
            "true" => spec.delegate_cgroups = Some(true),
            "false" => spec.delegate_cgroups = Some(false),
            other => note(format!(
                "x-facade delegate_cgroups `{other}` is not true or false"
            )),
        }
    }
    spec
}

/// Interpolation with unknown variables left empty.
fn interpolate_lossy(s: &str, vars: &HashMap<String, String>) -> Option<String> {
    let mut vars = vars.clone();
    for _ in 0..16 {
        match interpolate(s, &vars) {
            Ok(v) => return Some(v),
            Err(missing) => {
                vars.insert(missing, String::new());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_compose_files() {
        for name in ["compose.yaml", "docker-compose.yml", "compose.prod.yml"] {
            assert!(is_compose_file(name), "{name}");
        }
        for name in ["composer.json", "compose.json", "docker-compose", "x.yml"] {
            assert!(!is_compose_file(name), "{name}");
        }
    }

    #[test]
    fn reads_durations() {
        let v = |s: &str| Value::String(s.into());
        assert_eq!(duration_secs(&v("1m30s")), Some(90));
        assert_eq!(duration_secs(&v("500ms")), Some(1));
        assert_eq!(duration_secs(&v("2h")), Some(7200));
        assert_eq!(duration_secs(&v("10")), Some(10));
        assert_eq!(duration_secs(&v("soon")), None);
    }

    #[test]
    fn interpolates() {
        let vars = HashMap::from([("TAG".to_string(), "16".to_string())]);
        assert_eq!(
            interpolate("postgres:${TAG}", &vars).unwrap(),
            "postgres:16"
        );
        assert_eq!(interpolate("${MISSING:-x}-$$", &vars).unwrap(), "x-$");
        assert_eq!(interpolate("$TAG/a", &vars).unwrap(), "16/a");
        assert_eq!(interpolate("${NOPE}", &vars), Err("NOPE".into()));
    }

    #[test]
    fn plans_a_fullstack_monorepo() {
        let dir = std::env::temp_dir().join(format!("facade-compose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("deploy")).unwrap();
        std::fs::write(
            dir.join("deploy/docker-compose.yml"),
            r#"
x-env: &env
  RUST_LOG: info
services:
  db:
    image: postgres:${PG_VERSION:-16}
    environment:
      POSTGRES_PASSWORD: ${DB_PASSWORD}
      POSTGRES_DB: shop
    ports: ["5432:5432"]
    volumes: ["pgdata:/var/lib/postgresql/data"]
  api_server:
    build:
      context: ..
      dockerfile: apps/api/Dockerfile
    command: ./api --migrate
    environment:
      <<: *env
      DATABASE_URL: postgres://postgres:${DB_PASSWORD:-secret}@db:5432/shop
      API_TOKEN: abc
    ports: ["8080:8080", "9000-9001:9000-9001/udp"]
    depends_on: [db]
    volumes: ["./config:/config", "/tmp/cache", "pgdata:/backup"]
    deploy: { resources: { limits: { memory: 512M, cpus: "0.5", pids: 8192 } } }
    stop_grace_period: 1m30s
    stop_signal: SIGINT
    x-facade:
      domains: [API.example.com]
      release: ./api migrate
      publish: ["9000-9001:9000-9001/udp"]
      backup: { before: ./api freeze, after: ./api thaw }
  web:
    build: ../apps/web
    ports: ["3000"]
    depends_on:
      api_server: { condition: service_started }
  tools:
    image: busybox
    profiles: [debug]
volumes:
  pgdata:
    x-facade: { size: 20G }
"#,
        )
        .unwrap();
        let vars = HashMap::from([("PG_VERSION".to_string(), "17".to_string())]);
        let plan = plan(&dir, "deploy/docker-compose.yml", "shop", &vars, &|| {
            "generated".to_string()
        })
        .unwrap();
        let names: Vec<_> = plan.services.iter().map(|s| s.service.as_str()).collect();
        assert_eq!(names, ["db", "api-server", "web"]);
        assert_eq!(
            plan.generated,
            [("DB_PASSWORD".to_string(), "generated".to_string())]
        );

        let db = &plan.services[0];
        assert_eq!(db.spec.image.as_deref(), Some("postgres:17"));
        // Known database images get a readiness check.
        assert!(
            db.spec
                .healthcheck
                .as_ref()
                .unwrap()
                .command()
                .starts_with("pg_isready")
        );
        assert_eq!(db.spec.http_port, None);
        assert_eq!(
            db.spec.volumes.as_ref().unwrap()[0].to_string(),
            "pgdata:/var/lib/postgresql/data:20G"
        );
        assert_eq!(
            db.env["POSTGRES_PASSWORD"],
            EnvEntry::Detailed {
                value: Some("generated".into()),
                sensitive: true
            }
        );

        let api = &plan.services[1];
        assert_eq!(api.host, "api_server");
        assert_eq!(
            api.spec.build,
            Some(Build::Dockerfile {
                dockerfile: "apps/api/Dockerfile".into(),
                context: ".".into()
            })
        );
        assert_eq!(api.spec.command.as_ref().unwrap(), &["./api", "--migrate"]);
        assert_eq!(api.spec.http_port, Some(8080));
        assert_eq!(api.spec.release_command.as_deref(), Some("./api migrate"));
        assert_eq!(api.spec.domains.as_ref().unwrap(), &["api.example.com"]);
        assert_eq!(
            api.spec.ports.as_ref().unwrap()[0].to_string(),
            "9000-9001:9000-9001/udp"
        );
        let volumes: Vec<String> = api
            .spec
            .volumes
            .iter()
            .flatten()
            .map(|v| v.to_string())
            .collect();
        // The named volume is the same one the database uses.
        assert_eq!(
            volumes,
            ["api-server-cache:/tmp/cache", "pgdata:/backup:20G"]
        );
        assert_eq!(api.spec.limits.memory_mb, Some(512));
        assert_eq!(api.spec.limits.cpus, Some(0.5));
        assert_eq!(api.spec.limits.pids, Some(8192));
        assert_eq!(api.spec.stop_timeout_secs, Some(90));
        assert_eq!(api.spec.stop_signal.as_deref(), Some("SIGINT"));
        let backup = api.spec.backup.as_ref().unwrap();
        assert_eq!(backup.before.as_deref(), Some("./api freeze"));
        assert_eq!(backup.after.as_deref(), Some("./api thaw"));
        assert_eq!(api.spec.depends_on.as_ref().unwrap(), &["db"]);
        assert_eq!(api.env["RUST_LOG"], EnvEntry::Plain("info".into()));
        // The generated password is shared, so the API reaches the database.
        assert_eq!(
            api.env["DATABASE_URL"],
            EnvEntry::Detailed {
                value: Some("postgres://postgres:generated@db:5432/shop".into()),
                sensitive: true
            }
        );
        assert!(matches!(
            api.env["API_TOKEN"],
            EnvEntry::Detailed {
                sensitive: true,
                ..
            }
        ));

        let web = &plan.services[2];
        assert_eq!(
            web.spec.build,
            Some(Build::Dockerfile {
                dockerfile: "apps/web/Dockerfile".into(),
                context: "apps/web".into()
            })
        );
        assert_eq!(web.spec.http_port, Some(3000));
        assert_eq!(web.spec.depends_on.as_ref().unwrap(), &["api-server"]);
        assert!(plan.notes.iter().any(|n| n.starts_with("tools: skipped")));
        assert!(plan.notes.iter().any(|n| n.contains("./config")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
