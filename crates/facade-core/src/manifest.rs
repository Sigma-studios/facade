//! The exportable, re-appliable description of a whole facade installation.
//!
//! ```toml
//! [[node]]
//! name = "main"
//!
//! [[app]]
//! name = "signalling"
//! node = "main"
//! http_port = 9090
//! domains = ["signal.example.com"]
//! source = { type = "git", repo = "https://github.com/me/repo", build = { kind = "rust", bin = "server" } }
//!
//! [app.env]
//! SIGNALLING_ADDR = "0.0.0.0:9090"     # plain string: a public variable
//! TURN_PASSWORD = { sensitive = true } # value withheld; set it with `facade env set`
//! GAME_KEY = { sensitive = true, build = true } # also handed to builds as a secret
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::spec::AppSpec;
use crate::stack::StackSpec;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default, rename = "node", skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeDecl>,
    #[serde(default, rename = "app", skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<AppManifest>,
    #[serde(default, rename = "stack", skip_serializing_if = "Vec::is_empty")]
    pub stacks: Vec<StackManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackManifest {
    #[serde(flatten)]
    pub spec: StackSpec,
    /// The stack's variables, used by its compose file (`${DB_PASSWORD}`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, EnvEntry>,
    /// Variables set in facade for each service, by service.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, BTreeMap<String, EnvEntry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeDecl {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppManifest {
    #[serde(flatten)]
    pub spec: AppSpec,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, EnvEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EnvEntry {
    /// Written out in the file, so public.
    Plain(String),
    Detailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        #[serde(default = "yes")]
        sensitive: bool,
        /// Handed to builds as a secret too.
        #[serde(default, skip_serializing_if = "is_false")]
        build: bool,
    },
}

fn is_false(b: &bool) -> bool {
    !b
}

fn yes() -> bool {
    true
}

impl EnvEntry {
    /// `(value, sensitive)`; a `None` value means "keep whatever is set".
    pub fn resolve(&self) -> (Option<&str>, bool) {
        match self {
            EnvEntry::Plain(v) => (Some(v), false),
            EnvEntry::Detailed {
                value, sensitive, ..
            } => (value.as_deref(), *sensitive),
        }
    }

    /// Whether builds get it.
    pub fn build(&self) -> bool {
        matches!(self, EnvEntry::Detailed { build: true, .. })
    }
}

impl Manifest {
    pub fn parse(toml_src: &str) -> Result<Self, String> {
        let manifest: Manifest = toml::from_str(toml_src).map_err(|e| e.to_string())?;
        for app in &manifest.apps {
            app.spec
                .validate()
                .map_err(|e| format!("app `{}`: {e}", app.spec.name))?;
            if app.spec.stack.is_some() {
                return Err(format!(
                    "app `{}`: services of a stack are declared in its [[stack]]",
                    app.spec.name
                ));
            }
        }
        for stack in &manifest.stacks {
            stack
                .spec
                .validate()
                .map_err(|e| format!("stack `{}`: {e}", stack.spec.name))?;
        }
        Ok(manifest)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("manifest serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Build, Source};

    #[test]
    fn parses_and_round_trips() {
        let src = r#"
[[node]]
name = "main"

[[app]]
name = "signalling"
node = "main"
http_port = 9090
domains = ["signal.example.com"]
ports = [{ host = 3478, container = 3478, protocol = "udp" }]
source = { type = "git", repo = "https://github.com/me/repo", build = { kind = "rust", bin = "server", no_default_features = true, features = ["server"] } }

[app.env]
SIGNALLING_ADDR = "0.0.0.0:9090"
TURN_PASSWORD = { sensitive = true }
GAME_KEY = { sensitive = true, build = true }

[[app]]
name = "landing"
node = "main"
domains = ["example.com"]
source = { type = "git", repo = "https://github.com/me/site", build = { kind = "static", dir = "public" } }

[[app]]
name = "api"
node = "main"
http_port = 8080
command = ["./api", "--workers", "4"]
release_command = "./api migrate"
volumes = ["uploads:/srv/uploads"]
links = ["db", "cache:redis"]
source = { type = "git", repo = "git@github.com:me/shop.git", build = { kind = "dockerfile", dockerfile = "api/Dockerfile", context = "." } }

[app.env]
DATABASE_URL = "${{db.DATABASE_URL}}"

[[app]]
name = "paint"
node = "main"
source = { type = "git", repo = "https://github.com/me/paint", build = { kind = "trunk", dir = "apps/web" } }

[[app]]
name = "editor"
node = "main"
domains = ["editor.example.com"]
source = { type = "git", repo = "https://github.com/me/editor", build = { kind = "static", dockerfile = "web/Dockerfile" } }
"#;
        let manifest = Manifest::parse(src).unwrap();
        assert_eq!(manifest.apps.len(), 5);
        assert!(manifest.apps[3].spec.is_static());
        let editor = &manifest.apps[4].spec;
        assert!(editor.is_static());
        assert_eq!(
            editor.source,
            Source::Git {
                repo: "https://github.com/me/editor".into(),
                branch: None,
                build: Build::Static {
                    dir: ".".into(),
                    dockerfile: Some("web/Dockerfile".into()),
                    context: None,
                },
            }
        );
        let mut no_dockerfile = editor.clone();
        no_dockerfile.source = Source::Git {
            repo: "https://github.com/me/editor".into(),
            branch: None,
            build: Build::Static {
                dir: ".".into(),
                dockerfile: None,
                context: Some("web".into()),
            },
        };
        assert!(no_dockerfile.validate().is_err());
        let api = &manifest.apps[2].spec;
        assert_eq!(api.links[1].host(), "redis");
        assert_eq!(api.volumes[0].path, "/srv/uploads");
        assert!(manifest.apps[1].spec.is_static());
        assert_eq!(
            manifest.apps[0].env["TURN_PASSWORD"].resolve(),
            (None, true)
        );
        assert!(!manifest.apps[0].env["TURN_PASSWORD"].build());
        assert!(manifest.apps[0].env["GAME_KEY"].build());
        let again = Manifest::parse(&manifest.to_toml()).unwrap();
        for (a, b) in again.apps.iter().zip(&manifest.apps) {
            assert_eq!(a.spec, b.spec);
            assert_eq!(a.env, b.env);
        }
    }
}
