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
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::spec::AppSpec;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default, rename = "node", skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeDecl>,
    #[serde(default, rename = "app", skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<AppManifest>,
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
    },
}

fn yes() -> bool {
    true
}

impl EnvEntry {
    /// `(value, sensitive)`; a `None` value means "keep whatever is set".
    pub fn resolve(&self) -> (Option<&str>, bool) {
        match self {
            EnvEntry::Plain(v) => (Some(v), false),
            EnvEntry::Detailed { value, sensitive } => (value.as_deref(), *sensitive),
        }
    }
}

impl Manifest {
    pub fn parse(toml_src: &str) -> Result<Self, String> {
        let manifest: Manifest = toml::from_str(toml_src).map_err(|e| e.to_string())?;
        for app in &manifest.apps {
            app.spec
                .validate()
                .map_err(|e| format!("app `{}`: {e}", app.spec.name))?;
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

[[app]]
name = "landing"
node = "main"
domains = ["example.com"]
source = { type = "git", repo = "https://github.com/me/site", build = { kind = "static", dir = "public" } }
"#;
        let manifest = Manifest::parse(src).unwrap();
        assert_eq!(manifest.apps.len(), 2);
        assert!(manifest.apps[1].spec.is_static());
        assert_eq!(
            manifest.apps[0].env["TURN_PASSWORD"].resolve(),
            (None, true)
        );
        let again = Manifest::parse(&manifest.to_toml()).unwrap();
        assert_eq!(again.apps[0].spec, manifest.apps[0].spec);
        assert_eq!(again.apps[0].env, manifest.apps[0].env);
    }
}
