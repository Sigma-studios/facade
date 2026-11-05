//! dpkg: what is installed, with the source package each binary comes from
//! (Ubuntu's vulnerability data is about source packages).

use std::path::PathBuf;

use facade_core::platform::Checked;
use facade_core::vulns::{Inventory, Package};
use tokio::process::Command;

/// Changes whenever packages are installed, upgraded or removed.
pub fn marker() -> PathBuf {
    PathBuf::from("/var/lib/dpkg/status")
}

pub async fn inventory() -> Checked<Inventory> {
    let out = Command::new("dpkg-query")
        .args([
            "-W",
            "-f",
            "${db:Status-Abbrev}\\t${Package}\\t${Version}\\t${source:Package}\\t${source:Version}\\n",
        ])
        .env("LANG", "C")
        .kill_on_drop(true)
        .output()
        .await;
    let out = match out {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            return Checked::Failed(format!(
                "dpkg-query: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Err(e) => return Checked::Failed(format!("running dpkg-query: {e}")),
    };
    let packages = parse(&String::from_utf8_lossy(&out.stdout));
    let kernel = super::super::running_kernel();
    Checked::Known(Inventory {
        hash: Inventory::hash_of("dpkg", &packages, kernel.as_deref()),
        kind: "dpkg".into(),
        packages,
        kernel,
    })
}

/// Installed packages only (`ii`, or held `hi`), once each.
fn parse(text: &str) -> Vec<Package> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split('\t');
        let (Some(status), Some(name), Some(version)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if status.as_bytes().get(1) != Some(&b'i') || name.is_empty() || version.is_empty() {
            continue;
        }
        if !seen.insert(name.to_string()) {
            continue;
        }
        let source = f.next().map(str::trim).filter(|s| !s.is_empty());
        let source_version = f.next().map(str::trim).filter(|s| !s.is_empty());
        out.push(Package {
            name: name.to_string(),
            version: version.to_string(),
            source: source.filter(|s| *s != name).map(str::to_string),
            source_version: source_version.filter(|v| *v != version).map(str::to_string),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dpkg_query() {
        // Ubuntu 26.04, trimmed.
        let text = "ii \tlinux-image-7.0.0-42-generic\t7.0.0-42.42\tlinux-signed\t7.0.0-42.42\n\
                    ii \topenssl\t3.5.3-1ubuntu1\topenssl\t3.5.3-1ubuntu1\n\
                    rc \told-thing\t1.0-1\told-thing\t1.0-1\n\
                    ii \tlibc6\t2.42-0ubuntu3\tglibc\t2.42-0ubuntu3\n\
                    ii \tlibc6\t2.42-0ubuntu3\tglibc\t2.42-0ubuntu3\n\
                    hi \tdocker-ce\t5:28.5.1-1~ubuntu.26.04~resolute\tdocker-ce\t5:28.5.1-1~ubuntu.26.04~resolute\n";
        let p = parse(text);
        let names: Vec<&str> = p.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "linux-image-7.0.0-42-generic",
                "openssl",
                "libc6",
                "docker-ce"
            ]
        );
        assert_eq!(p[0].source.as_deref(), Some("linux-signed"));
        assert_eq!(p[0].source_version, None);
        assert_eq!(p[1].source, None);
        assert_eq!(p[2].source.as_deref(), Some("glibc"));
    }
}
