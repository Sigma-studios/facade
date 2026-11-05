//! What a node runs on: its distribution, whether facade supports it, how facade
//! itself is installed there, and the answer to a host check that may not have
//! been possible on it ([`Checked`]).

use serde::{Deserialize, Serialize};

/// The distribution, from `/etc/os-release`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsId {
    /// `ID`: `ubuntu`, `nixos`, `debian`, …
    pub id: String,
    /// `ID_LIKE`, split: `["debian"]` on Ubuntu.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub like: Vec<String>,
    /// `VERSION_ID`: `26.04`, `26.05`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `VERSION_CODENAME`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codename: Option<String>,
    /// `PRETTY_NAME`: `Ubuntu 26.04 LTS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pretty: Option<String>,
}

impl OsId {
    /// `Ubuntu 26.04 LTS`, else `ubuntu 26.04`.
    pub fn name(&self) -> String {
        if let Some(p) = self.pretty.as_ref().filter(|p| !p.is_empty()) {
            return p.clone();
        }
        match &self.version {
            Some(v) => format!("{} {v}", self.id),
            None => self.id.clone(),
        }
    }

    /// Reads `/etc/os-release`'s `KEY=value` lines (values may be quoted).
    pub fn parse(text: &str) -> Self {
        let mut os = OsId::default();
        for line in text.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = unquote(value.trim());
            let set = |v: &str| (!v.is_empty()).then(|| v.to_string());
            match key.trim() {
                "ID" => os.id = value.to_ascii_lowercase(),
                "ID_LIKE" => {
                    os.like = value
                        .split_whitespace()
                        .map(str::to_ascii_lowercase)
                        .collect()
                }
                "VERSION_ID" => os.version = set(&value),
                "VERSION_CODENAME" => os.codename = set(&value),
                "PRETTY_NAME" => os.pretty = set(&value),
                _ => {}
            }
        }
        if os.id.is_empty() {
            os.id = "linux".into();
        }
        os
    }
}

fn unquote(v: &str) -> String {
    let inner = v
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(v);
    // Shell escapes, as os-release allows them.
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether facade supports the node's system.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// Ubuntu and NixOS: every host feature works.
    Supported,
    /// Anything else: apps run, and the host checks it cannot do say so.
    #[default]
    BestEffort,
}

/// How facade itself is installed on the node, which decides how it updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InstallKind {
    /// `install.sh`: facade replaces its own binary and units.
    SelfManaged,
    /// Another tool owns the binary and units (`by`: `nix`); facade never
    /// writes them, and `hint` says how to update instead.
    Managed { by: String, hint: String },
}

impl InstallKind {
    pub fn is_managed(&self) -> bool {
        matches!(self, InstallKind::Managed { .. })
    }
}

/// What an agent says about its system, in its hello.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformInfo {
    pub os: OsId,
    pub support: Support,
    pub install: InstallKind,
    /// The running kernel (`uname -r`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
}

impl PlatformInfo {
    /// `Ubuntu 26.04 LTS`, `NixOS 26.05 (Yarara)`, or `best effort: Fedora Linux 43`.
    pub fn label(&self) -> String {
        match self.support {
            Support::Supported => self.os.name(),
            Support::BestEffort => format!("best effort: {}", self.os.name()),
        }
    }
}

/// The answer to a host check: what it found, or why there is no answer.
/// "Nothing to do" is `Known` with nothing in it, never a missing value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Checked<T> {
    Known(T),
    /// This system has no way to tell (with why: `no apt on fedora`).
    Unsupported(String),
    /// It should have, and the check failed (with the error).
    Failed(String),
}

impl<T> Default for Checked<T> {
    fn default() -> Self {
        Checked::Unsupported("not reported".into())
    }
}

impl<T> Checked<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Checked::Known(t) => Some(t),
            _ => None,
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Checked<U> {
        match self {
            Checked::Known(t) => Checked::Known(f(t)),
            Checked::Unsupported(w) => Checked::Unsupported(w),
            Checked::Failed(e) => Checked::Failed(e),
        }
    }

    /// Why there is no answer: `couldn't check: …` or `not available: …`.
    pub fn why_unknown(&self) -> Option<String> {
        match self {
            Checked::Known(_) => None,
            Checked::Unsupported(w) => Some(format!("not available: {w}")),
            Checked::Failed(e) => Some(format!("couldn't check: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_os_release() {
        let ubuntu = OsId::parse(
            "PRETTY_NAME=\"Ubuntu 26.04 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"26.04\"\n\
             VERSION=\"26.04 LTS (Resolute Raccoon)\"\nVERSION_CODENAME=resolute\nID=ubuntu\n\
             ID_LIKE=debian\nUBUNTU_CODENAME=resolute\n",
        );
        assert_eq!(ubuntu.id, "ubuntu");
        assert_eq!(ubuntu.like, ["debian"]);
        assert_eq!(ubuntu.version.as_deref(), Some("26.04"));
        assert_eq!(ubuntu.codename.as_deref(), Some("resolute"));
        assert_eq!(ubuntu.name(), "Ubuntu 26.04 LTS");

        let nixos = OsId::parse(
            "ANSI_COLOR=\"1;34\"\nBUILD_ID=\"26.05.20261020.0a1b2c3\"\nID=nixos\n\
             ID_LIKE=\"\"\nNAME=NixOS\nPRETTY_NAME=\"NixOS 26.05 (Yarara)\"\n\
             VERSION_CODENAME=yarara\nVERSION_ID=\"26.05\"\n",
        );
        assert_eq!(nixos.id, "nixos");
        assert!(nixos.like.is_empty());
        assert_eq!(nixos.version.as_deref(), Some("26.05"));

        let rocky = OsId::parse("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID='9.4'\n");
        assert_eq!(rocky.like, ["rhel", "centos", "fedora"]);
        assert_eq!(rocky.version.as_deref(), Some("9.4"));
        assert_eq!(rocky.name(), "rocky 9.4");
        assert_eq!(OsId::parse("").id, "linux");
    }

    #[test]
    fn checked_round_trips() {
        let known: Checked<u32> = Checked::Known(3);
        let json = serde_json::to_string(&known).unwrap();
        assert_eq!(json, r#"{"known":3}"#);
        assert_eq!(serde_json::from_str::<Checked<u32>>(&json).unwrap(), known);
        let no: Checked<u32> = serde_json::from_str(r#"{"unsupported":"no apt"}"#).unwrap();
        assert_eq!(no.why_unknown().as_deref(), Some("not available: no apt"));
    }
}
