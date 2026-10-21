//! Uploaded Linux executables.

/// Architecture of an ELF executable, from its header: `x86_64`, `aarch64`, or an
/// error saying what the file is instead.
pub fn elf_arch(header: &[u8]) -> Result<&'static str, String> {
    if header.len() < 20 || &header[..4] != b"\x7fELF" {
        return Err("not a Linux executable (no ELF header). Build for Linux, \
                    e.g. --target x86_64-unknown-linux-musl"
            .into());
    }
    if header[4] != 2 {
        return Err("32-bit executables are not supported".into());
    }
    // e_machine, little-endian on both supported architectures.
    match u16::from_le_bytes([header[18], header[19]]) {
        62 => Ok("x86_64"),
        183 => Ok("aarch64"),
        other => Err(format!(
            "unsupported CPU architecture (ELF machine {other})"
        )),
    }
}

/// Runs the executable in a small, recent Debian (glibc 2.41), so binaries linked
/// against glibc work as well as static ones.
pub fn dockerfile(filename: &str, args: &[String]) -> String {
    let cmd = serde_json::to_string(args).expect("strings serialize");
    format!(
        r#"FROM docker.io/library/debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata && rm -rf /var/lib/apt/lists/*
COPY --chmod=755 app /usr/local/bin/{filename}
ENTRYPOINT ["/usr/local/bin/{filename}"]
CMD {cmd}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_elf_architecture() {
        let mut header = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0];
        header.resize(20, 0);
        header[18] = 62;
        assert_eq!(elf_arch(&header), Ok("x86_64"));
        header[18] = 183;
        assert_eq!(elf_arch(&header), Ok("aarch64"));
        assert!(elf_arch(b"#!/bin/sh\necho hi\n").is_err());
    }
}
