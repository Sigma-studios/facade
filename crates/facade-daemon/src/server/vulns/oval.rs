//! Canonical's OVAL data for one Ubuntu release
//! (`com.ubuntu.<codename>.cve.oval.xml.bz2`): every CVE affecting the
//! release, fixed or not, with the binary packages of each source package and
//! the running-kernel checks.
//!
//! It is the same data as Ubuntu's OSV records, from the same tracker, but one
//! release at a time: about 5 MB a day, where the OSV archive (every release,
//! every version ever published) is 800 MB and rewritten daily.
//!
//! Only what Canonical's files use is understood: `dpkginfo` tests (binary
//! packages, optionally below a fixed version), `uname` tests (which kernel
//! flavour runs) and the variable test comparing the running kernel's version.
//! Anything else evaluates as false, so it can only miss, never invent.

use std::collections::HashMap;
use std::io::BufRead;

use anyhow::{Context, bail};
use facade_core::vulns::{FindingStatus, Inventory, VulnSeverity};
use quick_xml::events::{BytesStart, Event};
use regex::Regex;

use super::dpkg;

/// A vulnerability matched on a node, before the server adds its history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub cve: String,
    pub package: String,
    pub installed: String,
    pub fixed: Option<String>,
    pub severity: VulnSeverity,
    pub status: FindingStatus,
    pub title: String,
    pub url: String,
}

pub struct Oval {
    /// `Ubuntu 26.04 LTS`.
    pub release: String,
    /// When Canonical generated it.
    pub generated: Option<u64>,
    defs: Vec<Def>,
    tests: HashMap<String, Test>,
}

struct Def {
    cve: String,
    severity: VulnSeverity,
    url: String,
    title: String,
    criteria: Node,
}

#[derive(Debug)]
enum Node {
    All(Vec<Node>),
    Any(Vec<Node>),
    Not(Box<Node>),
    Test(String),
    /// `extend_definition`: the release check, true for the release's own file.
    True,
}

#[derive(Debug)]
enum Test {
    /// One of the source package's binaries is installed (below `fixed`, when
    /// there is a fix).
    Dpkg {
        source: String,
        binaries: Vec<String>,
        fixed: Option<String>,
    },
    /// The running kernel is of this flavour (`linux`, `linux-aws`).
    Running {
        flavour: String,
        pattern: Regex,
    },
    /// The running kernel's version (`0:7.0.0-42`) is below this one.
    KernelBelow {
        evr: String,
    },
    Unknown,
}

// ---------------------------------------------------------------- parsing

#[derive(Default)]
struct RawTest {
    kind: String,
    comment: String,
    object: Option<String>,
    state: Option<String>,
}

#[derive(Default)]
struct RawObject {
    kind: String,
    comment: String,
    var_ref: Option<String>,
    names: Vec<String>,
}

#[derive(Default)]
struct RawState {
    operation: String,
    value: String,
}

#[derive(Default)]
struct RawDef {
    vulnerability: bool,
    cve: String,
    priority: String,
    severity: String,
    url: String,
    description: String,
    title: String,
}

/// Where the parser is.
#[derive(Default)]
enum Section {
    #[default]
    Other,
    Generator,
    Definitions,
    Tests,
    Objects,
    States,
    Variables,
}

impl Oval {
    /// Reads the (decompressed) XML.
    pub fn parse(input: impl BufRead) -> anyhow::Result<Self> {
        let mut reader = quick_xml::Reader::from_reader(input);
        reader.config_mut().trim_text(false);
        let mut buf = Vec::new();

        let mut section = Section::Other;
        let mut path: Vec<String> = Vec::new();
        let mut text = String::new();
        let mut generated = None;
        let mut release = String::new();

        let mut defs = Vec::new();
        let mut def: Option<RawDef> = None;
        // Criteria being built: a stack of (operator is AND, negate, children).
        let mut stack: Vec<(bool, bool, Vec<Node>)> = Vec::new();
        let mut criteria: Option<Node> = None;

        let mut tests: HashMap<String, RawTest> = HashMap::new();
        let mut objects: HashMap<String, RawObject> = HashMap::new();
        let mut states: HashMap<String, RawState> = HashMap::new();
        let mut variables: HashMap<String, Vec<String>> = HashMap::new();
        let mut current_id = String::new();

        loop {
            let event = reader
                .read_event_into(&mut buf)
                .with_context(|| format!("OVAL XML at byte {}", reader.buffer_position()))?;
            let empty = matches!(event, Event::Empty(_));
            match event {
                Event::Start(ref e) | Event::Empty(ref e) => {
                    let name = local(e);
                    let attr = |key: &str| attribute(e, key);
                    match (&section, name.as_str()) {
                        (_, "generator") => section = Section::Generator,
                        (_, "definitions") => section = Section::Definitions,
                        (_, "tests") => section = Section::Tests,
                        (_, "objects") => section = Section::Objects,
                        (_, "states") => section = Section::States,
                        (_, "variables") => section = Section::Variables,
                        (Section::Definitions, "definition") => {
                            def = Some(RawDef {
                                vulnerability: attr("class").as_deref() == Some("vulnerability"),
                                ..Default::default()
                            });
                            criteria = None;
                        }
                        (Section::Definitions, "cve") => {
                            if let Some(d) = def.as_mut() {
                                d.priority = attr("priority").unwrap_or_default();
                                d.url = attr("href").unwrap_or_default();
                            }
                        }
                        (Section::Definitions, "criteria") => {
                            let and =
                                attr("operator").is_none_or(|o| o.eq_ignore_ascii_case("AND"));
                            let negate = attr("negate").as_deref() == Some("true");
                            stack.push((and, negate, Vec::new()));
                            if empty {
                                close_criteria(&mut stack, &mut criteria);
                            }
                        }
                        (Section::Definitions, "criterion" | "extend_definition") => {
                            let mut node = if name == "criterion" {
                                Node::Test(attr("test_ref").unwrap_or_default())
                            } else {
                                Node::True
                            };
                            if attr("negate").as_deref() == Some("true") {
                                node = Node::Not(Box::new(node));
                            }
                            if let Some(top) = stack.last_mut() {
                                top.2.push(node);
                            }
                        }
                        (Section::Tests, n) if n.ends_with("_test") => {
                            current_id = attr("id").unwrap_or_default();
                            tests.insert(
                                current_id.clone(),
                                RawTest {
                                    kind: n.to_string(),
                                    comment: attr("comment").unwrap_or_default(),
                                    ..Default::default()
                                },
                            );
                        }
                        (Section::Tests, "object") => {
                            if let Some(t) = tests.get_mut(&current_id) {
                                t.object = attr("object_ref");
                            }
                        }
                        (Section::Tests, "state") => {
                            if let Some(t) = tests.get_mut(&current_id) {
                                t.state = attr("state_ref");
                            }
                        }
                        (Section::Objects, n) if n.ends_with("_object") => {
                            current_id = attr("id").unwrap_or_default();
                            objects.insert(
                                current_id.clone(),
                                RawObject {
                                    kind: n.to_string(),
                                    comment: attr("comment").unwrap_or_default(),
                                    ..Default::default()
                                },
                            );
                        }
                        (Section::Objects, "name") => {
                            if let Some(o) = objects.get_mut(&current_id) {
                                o.var_ref = attr("var_ref");
                            }
                        }
                        (Section::States, n) if n.ends_with("_state") => {
                            current_id = attr("id").unwrap_or_default();
                            states.insert(current_id.clone(), RawState::default());
                        }
                        (Section::States, _) => {
                            if let (Some(s), Some(op)) =
                                (states.get_mut(&current_id), attr("operation"))
                            {
                                s.operation = op;
                            }
                        }
                        (Section::Variables, "constant_variable" | "local_variable") => {
                            current_id = attr("id").unwrap_or_default();
                            variables.entry(current_id.clone()).or_default();
                        }
                        _ => {}
                    }
                    if !empty {
                        path.push(name);
                        text.clear();
                    }
                }
                Event::Text(t) => match t.unescape() {
                    Ok(t) => text.push_str(&t),
                    Err(_) => text.push_str(&String::from_utf8_lossy(&t)),
                },
                Event::CData(t) => {
                    text.push_str(&String::from_utf8_lossy(&t.into_inner()));
                }
                Event::End(_) => {
                    let name = path.pop().unwrap_or_default();
                    let value = std::mem::take(&mut text);
                    let value = value.trim();
                    match (&section, name.as_str()) {
                        (
                            _,
                            "generator" | "definitions" | "tests" | "objects" | "states"
                            | "variables",
                        ) => section = Section::Other,
                        (Section::Generator, "timestamp") => generated = parse_time(value),
                        (Section::Definitions, "definition") => {
                            if let (Some(d), Some(c)) = (def.take(), criteria.take())
                                && d.vulnerability
                                && !d.cve.is_empty()
                            {
                                defs.push(Def {
                                    severity: VulnSeverity::parse(if d.priority.is_empty() {
                                        &d.severity
                                    } else {
                                        &d.priority
                                    }),
                                    title: summary(&d.description, &d.title),
                                    cve: d.cve,
                                    url: d.url,
                                    criteria: c,
                                });
                            }
                        }
                        (Section::Definitions, "title") => {
                            if let Some(d) = def.as_mut() {
                                d.title = value.to_string();
                            }
                        }
                        (Section::Definitions, "platform") => {
                            if release.is_empty() {
                                release = value.to_string();
                            }
                        }
                        (Section::Definitions, "description") => {
                            if let Some(d) = def.as_mut() {
                                d.description = value.to_string();
                            }
                        }
                        (Section::Definitions, "severity") => {
                            if let Some(d) = def.as_mut() {
                                d.severity = value.to_string();
                            }
                        }
                        (Section::Definitions, "cve") => {
                            if let Some(d) = def.as_mut() {
                                d.cve = value.to_string();
                            }
                        }
                        (Section::Definitions, "criteria") => {
                            close_criteria(&mut stack, &mut criteria);
                        }
                        (Section::Objects, "var_ref") => {
                            if let Some(o) = objects.get_mut(&current_id) {
                                o.var_ref = Some(value.to_string());
                            }
                        }
                        (Section::Objects, "name") => {
                            if let Some(o) = objects.get_mut(&current_id)
                                && o.var_ref.is_none()
                                && !value.is_empty()
                            {
                                o.names.push(value.to_string());
                            }
                        }
                        (Section::States, n) if !n.ends_with("_state") => {
                            if let Some(s) = states.get_mut(&current_id)
                                && !value.is_empty()
                            {
                                s.value = value.to_string();
                            }
                        }
                        (Section::Variables, "value") => {
                            if let Some(v) = variables.get_mut(&current_id) {
                                v.push(value.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        if defs.is_empty() {
            bail!("no vulnerability definitions in the OVAL data");
        }

        let tests = tests
            .into_iter()
            .map(|(id, t)| {
                let test = resolve(&t, &objects, &states, &variables);
                (id, test)
            })
            .collect();
        Ok(Oval {
            release,
            generated,
            defs,
            tests,
        })
    }

    pub fn definitions(&self) -> usize {
        self.defs.len()
    }

    /// What affects a node with this inventory.
    pub fn matches(&self, inventory: &Inventory) -> Vec<Match> {
        let host = Host::new(inventory);
        let mut out = Vec::new();
        for def in &self.defs {
            let mut hits = Vec::new();
            if !self.eval(&def.criteria, &host, &mut hits) {
                continue;
            }
            out.extend(self.findings(def, &host, &hits));
        }
        out.sort_by(|a, b| (a.severity, &a.cve, &a.package).cmp(&(b.severity, &b.cve, &b.package)));
        out.dedup_by(|a, b| a.cve == b.cve && a.package == b.package);
        out
    }

    fn eval<'a>(&'a self, node: &'a Node, host: &Host, hits: &mut Vec<&'a Test>) -> bool {
        match node {
            Node::True => true,
            Node::Test(id) => {
                let Some(test) = self.tests.get(id) else {
                    return false;
                };
                let ok = host.satisfies(test);
                if ok {
                    hits.push(test);
                }
                ok
            }
            Node::Not(inner) => {
                let mut ignored = Vec::new();
                !self.eval(inner, host, &mut ignored)
            }
            Node::All(children) => {
                let mut mine = Vec::new();
                for c in children {
                    if !self.eval(c, host, &mut mine) {
                        return false;
                    }
                }
                hits.extend(mine);
                true
            }
            Node::Any(children) => {
                let mut any = false;
                for c in children {
                    let mut mine = Vec::new();
                    if self.eval(c, host, &mut mine) {
                        any = true;
                        hits.extend(mine);
                    }
                }
                any
            }
        }
    }

    fn findings(&self, def: &Def, host: &Host, hits: &[&Test]) -> Vec<Match> {
        let base = |package: &str, installed: &str, fixed: Option<String>, status| Match {
            cve: def.cve.clone(),
            package: package.to_string(),
            installed: installed.to_string(),
            fixed,
            severity: def.severity,
            status,
            title: def.title.clone(),
            url: def.url.clone(),
        };
        let mut out = Vec::new();
        let below = hits.iter().find_map(|t| match t {
            Test::KernelBelow { evr } => Some(evr.as_str()),
            _ => None,
        });
        // Flavours can share a kernel name (`linux` and `linux-riscv` both run
        // `-generic`): the one the running kernel's package comes from, else `linux`.
        let flavours: Vec<&str> = hits
            .iter()
            .filter_map(|t| match t {
                Test::Running { flavour, .. } => Some(flavour.as_str()),
                _ => None,
            })
            .collect();
        let flavour = host
            .flavour
            .as_deref()
            .filter(|f| flavours.contains(f))
            .or_else(|| flavours.iter().copied().find(|f| *f == "linux"))
            .or_else(|| flavours.first().copied());
        for test in hits {
            match test {
                Test::Dpkg {
                    source,
                    binaries,
                    fixed,
                } => {
                    let installed = binaries
                        .iter()
                        .filter_map(|b| host.packages.get(b.as_str()))
                        .find(|v| fixed.as_ref().is_none_or(|f| dpkg::less(v, f)))
                        .copied()
                        .unwrap_or_default();
                    let status = if fixed.is_some() {
                        FindingStatus::FixAvailable
                    } else {
                        FindingStatus::NoFix
                    };
                    out.push(base(source, installed, fixed.clone(), status));
                }
                Test::Running { flavour: f, .. } if Some(f.as_str()) == flavour => {
                    let running = host.kernel.unwrap_or_default();
                    let (fixed, status) = match below {
                        Some(evr) if host.installed_kernel_at_least(evr) => {
                            (Some(strip_epoch(evr)), FindingStatus::FixedAfterReboot)
                        }
                        Some(evr) => (Some(strip_epoch(evr)), FindingStatus::FixAvailable),
                        None => (None, FindingStatus::NoFix),
                    };
                    out.push(base(f, running, fixed, status));
                }
                _ => {}
            }
        }
        out
    }
}

fn close_criteria(stack: &mut Vec<(bool, bool, Vec<Node>)>, criteria: &mut Option<Node>) {
    let Some((and, negate, children)) = stack.pop() else {
        return;
    };
    let mut node = if and {
        Node::All(children)
    } else {
        Node::Any(children)
    };
    if negate {
        node = Node::Not(Box::new(node));
    }
    match stack.last_mut() {
        Some(parent) => parent.2.push(node),
        None => *criteria = Some(node),
    }
}

fn resolve(
    t: &RawTest,
    objects: &HashMap<String, RawObject>,
    states: &HashMap<String, RawState>,
    variables: &HashMap<String, Vec<String>>,
) -> Test {
    let object = t.object.as_ref().and_then(|o| objects.get(o));
    let state = t.state.as_ref().and_then(|s| states.get(s));
    match t.kind.as_str() {
        "dpkginfo_test" => {
            let Some(object) = object else {
                return Test::Unknown;
            };
            let binaries = match &object.var_ref {
                Some(v) => variables.get(v).cloned().unwrap_or_default(),
                None => object.names.clone(),
            };
            let fixed = match state {
                Some(s) if s.operation == "less than" && !s.value.is_empty() => {
                    Some(s.value.clone())
                }
                // A state this reader does not understand: never a match.
                Some(_) => return Test::Unknown,
                None => None,
            };
            Test::Dpkg {
                source: quoted(&object.comment)
                    .or_else(|| quoted(&t.comment))
                    .or_else(|| binaries.first().cloned())
                    .unwrap_or_default(),
                binaries,
                fixed,
            }
        }
        "uname_test" => match state {
            Some(s) if s.operation == "pattern match" => match Regex::new(&s.value) {
                Ok(pattern) => Test::Running {
                    flavour: quoted(&t.comment).unwrap_or_else(|| "linux".into()),
                    pattern,
                },
                Err(_) => Test::Unknown,
            },
            _ => Test::Unknown,
        },
        "variable_test" => match (object, state) {
            (Some(o), Some(s))
                if o.kind == "variable_object"
                    && s.operation == "less than"
                    && !s.value.is_empty() =>
            {
                Test::KernelBelow {
                    evr: s.value.clone(),
                }
            }
            _ => Test::Unknown,
        },
        _ => Test::Unknown,
    }
}

// ---------------------------------------------------------------- matching

struct Host<'a> {
    packages: HashMap<&'a str, &'a str>,
    kernel: Option<&'a str>,
    /// The running kernel's flavour, from its package's source (`linux`,
    /// `linux-aws`), when the inventory says.
    flavour: Option<String>,
    /// The running kernel's version as OVAL compares it: `0:7.0.0-42`.
    kernel_evr: Option<String>,
    /// Versions of the kernels installed, the same way.
    installed_kernels: Vec<String>,
}

impl<'a> Host<'a> {
    fn new(inventory: &'a Inventory) -> Self {
        let packages: HashMap<&str, &str> = inventory
            .packages
            .iter()
            .map(|p| (p.name.as_str(), p.version.as_str()))
            .collect();
        let kernel = inventory.kernel.as_deref();
        let image = Regex::new(r"^linux-image-(?:unsigned-)?(\d[\d.]*-\d+)-").expect("regex");
        let installed_kernels = packages
            .keys()
            .filter_map(|name| image.captures(name))
            .map(|c| format!("0:{}", &c[1]))
            .collect();
        // `linux-image-7.0.0-42-generic` comes from `linux-signed`: flavour `linux`.
        let flavour = kernel.and_then(|k| {
            let names = [
                format!("linux-image-{k}"),
                format!("linux-image-unsigned-{k}"),
            ];
            let p = inventory
                .packages
                .iter()
                .find(|p| names.contains(&p.name))?;
            let source = p.source.as_deref()?;
            Some(match source.strip_prefix("linux-signed") {
                Some(rest) => format!("linux{rest}"),
                None => source.to_string(),
            })
        });
        Host {
            flavour,
            kernel_evr: kernel.and_then(kernel_evr),
            packages,
            kernel,
            installed_kernels,
        }
    }

    fn satisfies(&self, test: &Test) -> bool {
        match test {
            Test::Dpkg {
                binaries, fixed, ..
            } => binaries.iter().any(|b| {
                self.packages
                    .get(b.as_str())
                    .is_some_and(|v| fixed.as_ref().is_none_or(|f| dpkg::less(v, f)))
            }),
            Test::Running { pattern, .. } => self.kernel.is_some_and(|k| pattern.is_match(k)),
            Test::KernelBelow { evr } => {
                self.kernel_evr.as_ref().is_some_and(|k| dpkg::less(k, evr))
            }
            Test::Unknown => false,
        }
    }

    fn installed_kernel_at_least(&self, evr: &str) -> bool {
        self.installed_kernels.iter().any(|k| !dpkg::less(k, evr))
    }
}

/// `7.0.0-42-generic` → `0:7.0.0-42`, as Canonical's kernel variable does
/// (`^([\d|\.]+-\d+)[-|\w]+$`).
fn kernel_evr(release: &str) -> Option<String> {
    let re = Regex::new(r"^([\d|\.]+-\d+)[-|\w]+$").expect("regex");
    re.captures(release).map(|c| format!("0:{}", &c[1]))
}

fn strip_epoch(v: &str) -> String {
    v.strip_prefix("0:").unwrap_or(v).to_string()
}

// ---------------------------------------------------------------- helpers

fn local(e: &BytesStart) -> String {
    String::from_utf8_lossy(e.local_name().as_ref()).to_string()
}

fn attribute(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (a.key.local_name().as_ref() == key.as_bytes()).then(|| {
            a.unescape_value()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).to_string())
        })
    })
}

/// The first `'quoted'` word: `The 'openssl' package binaries` → `openssl`.
fn quoted(s: &str) -> Option<String> {
    let (_, rest) = s.split_once('\'')?;
    let (word, _) = rest.split_once('\'')?;
    (!word.is_empty()).then(|| word.to_string())
}

/// One line about the CVE from its description: the kernel's boilerplate and
/// Canonical's update instructions left out.
fn summary(description: &str, title: &str) -> String {
    let mut d = description
        .split("Update Instructions:")
        .next()
        .unwrap_or("")
        .trim();
    for prefix in [
        "In the Linux kernel, the following vulnerability has been resolved:",
        "In the Linux kernel, the following vulnerability has been resolved",
    ] {
        if let Some(rest) = d.strip_prefix(prefix) {
            d = rest.trim();
            break;
        }
    }
    // The kernel's titles are the commit subject: up to the first sentence.
    let first = match d.find(". ") {
        Some(i) if i < 200 => &d[..i],
        _ => d,
    };
    // Canonical escapes some descriptions twice (`-&gt;`).
    let first = first
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&quot;", "\"")
        .replace("&amp;", "&");
    let mut line: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 120 {
        line = line
            .chars()
            .take(119)
            .collect::<String>()
            .trim_end()
            .to_string()
            + "…";
    }
    if line.is_empty() {
        title.to_string()
    } else {
        line
    }
}

/// `2026-11-04T20:12:40` (any fraction or zone ignored, UTC assumed) as seconds.
pub fn parse_time(s: &str) -> Option<u64> {
    let s = s.trim();
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (
        num(11..13).unwrap_or(0),
        num(14..16).unwrap_or(0),
        num(17..19).unwrap_or(0),
    );
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use facade_core::vulns::Package;

    /// Trimmed from Canonical's resolute file of 2026-11-04: the definitions
    /// and the tests, objects, states and variables they use, unchanged.
    const FIXTURE: &str = include_str!("testdata/resolute.oval.xml");

    fn pkg(name: &str, version: &str) -> Package {
        Package {
            name: name.into(),
            version: version.into(),
            ..Default::default()
        }
    }

    fn main_node(kernel: &str, extra: &[Package]) -> Inventory {
        let mut packages = vec![
            pkg("linux-image-7.0.0-42-generic", "7.0.0-42.42"),
            pkg("libjackson2-dataformat-smile", "2.14.1-1"),
            pkg("openssl", "3.5.3-1ubuntu1"),
        ];
        packages.extend_from_slice(extra);
        Inventory {
            hash: String::new(),
            kind: "dpkg".into(),
            packages,
            kernel: Some(kernel.into()),
        }
    }

    #[test]
    fn reads_canonicals_file() {
        let oval = Oval::parse(FIXTURE.as_bytes()).unwrap();
        assert_eq!(oval.release, "Ubuntu 26.04 LTS");
        assert!(oval.generated.is_some());
        assert!(oval.definitions() >= 3);
    }

    #[test]
    fn the_tcp_use_after_free_has_no_fix_on_resolute() {
        let oval = Oval::parse(FIXTURE.as_bytes()).unwrap();
        let found = oval.matches(&main_node("7.0.0-42-generic", &[]));
        let cve = found
            .iter()
            .find(|m| m.cve == "CVE-2026-98432")
            .expect("CVE-2026-98432 on the running kernel");
        assert_eq!(cve.package, "linux");
        assert_eq!(cve.installed, "7.0.0-42-generic");
        assert_eq!(cve.status, FindingStatus::NoFix);
        assert_eq!(cve.severity, VulnSeverity::Critical);
        assert_eq!(cve.fixed, None);
        assert!(
            cve.title.starts_with("ipv4: free inet_opt"),
            "{}",
            cve.title
        );
        assert_eq!(cve.url, "https://ubuntu.com/security/CVE-2026-98432");
        // Fixed in 7.0.0-42: the running kernel has it.
        assert!(!found.iter().any(|m| m.cve == "CVE-2026-98433"));
    }

    #[test]
    fn an_older_kernel_has_a_fix_waiting_or_after_a_reboot() {
        let oval = Oval::parse(FIXTURE.as_bytes()).unwrap();
        let older = main_node("7.0.0-38-generic", &[]);
        // 7.0.0-42 is installed: a reboot fixes it.
        let m = oval.matches(&older);
        let cve = m.iter().find(|m| m.cve == "CVE-2026-98433").unwrap();
        assert_eq!(cve.status, FindingStatus::FixedAfterReboot);
        assert_eq!(cve.fixed.as_deref(), Some("7.0.0-42"));
        let mut only_older = older.clone();
        only_older.packages[0] = pkg("linux-image-7.0.0-38-generic", "7.0.0-38.38");
        let m = oval.matches(&only_older);
        let cve = m.iter().find(|m| m.cve == "CVE-2026-98433").unwrap();
        assert_eq!(cve.status, FindingStatus::FixAvailable);
        // Another flavour's kernel matches nothing of the generic one.
        assert!(
            oval.matches(&main_node("7.0.0-1016-aws", &[]))
                .iter()
                .all(|m| m.package != "linux")
        );
    }

    #[test]
    fn packages_without_a_fix_and_below_one() {
        let oval = Oval::parse(FIXTURE.as_bytes()).unwrap();
        let m = oval.matches(&main_node("7.0.0-42-generic", &[]));
        let smile = m
            .iter()
            .find(|m| m.package == "jackson-dataformat-smile")
            .expect("unfixed package");
        assert_eq!(smile.status, FindingStatus::NoFix);
        assert_eq!(smile.installed, "2.14.1-1");
        // Not installed: nothing.
        let mut without = main_node("7.0.0-42-generic", &[]);
        without
            .packages
            .retain(|p| p.name != "libjackson2-dataformat-smile");
        assert!(
            !oval
                .matches(&without)
                .iter()
                .any(|m| m.package == "jackson-dataformat-smile")
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(
            kernel_evr("7.0.0-42-generic").as_deref(),
            Some("0:7.0.0-42")
        );
        assert_eq!(
            kernel_evr("7.0.0-1016-aws-64k").as_deref(),
            Some("0:7.0.0-1016")
        );
        assert_eq!(
            quoted("The 'openssl' package binaries").as_deref(),
            Some("openssl")
        );
        assert_eq!(parse_time("1970-01-02T00:00:01"), Some(86_401));
        assert_eq!(parse_time("2026-11-04T20:12:40"), Some(1_793_823_160));
    }

    /// `FACADE_OVAL_FILE=…/com.ubuntu.resolute.cve.oval.xml.bz2 cargo test -- --ignored`:
    /// the whole file reads, and the kernel CVE is found.
    #[test]
    #[ignore]
    fn reads_a_whole_file() {
        let path = std::env::var("FACADE_OVAL_FILE").expect("FACADE_OVAL_FILE");
        let started = std::time::Instant::now();
        let file = std::fs::File::open(path).unwrap();
        let reader = std::io::BufReader::new(bzip2::read::MultiBzDecoder::new(
            std::io::BufReader::new(file),
        ));
        let oval = Oval::parse(reader).unwrap();
        eprintln!(
            "{} definitions in {:?}",
            oval.definitions(),
            started.elapsed()
        );
        let started = std::time::Instant::now();
        let m = oval.matches(&main_node("7.0.0-42-generic", &[]));
        eprintln!("{} matches in {:?}", m.len(), started.elapsed());
        for f in m.iter().filter(|f| f.severity.at_least(VulnSeverity::High)) {
            eprintln!(
                "{} {:?} {} {} {:?} {}",
                f.cve, f.severity, f.package, f.installed, f.status, f.title
            );
        }
        assert!(
            m.iter()
                .any(|f| f.cve == "CVE-2026-98432" && f.status == FindingStatus::NoFix)
        );
    }
}
