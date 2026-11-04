//! What takes the space in an image: its largest layers, from Docker's image
//! history, each with the Dockerfile step that made it.

use bollard::Docker;
use facade_core::proto::ImageLayer;

/// Layers listed by name; the others are counted together.
const SHOWN: usize = 5;
/// Smaller layers are not worth a line of their own.
const MIN_SHOWN: u64 = 2 * 1000 * 1000;
/// Longest step text kept.
const MAX_COMMAND: usize = 120;
/// How the classic builder and BuildKit record a `RUN`.
const SHELLS: [&str; 3] = ["/bin/sh -c ", "/bin/bash -c ", "sh -c "];

/// The image's largest layers, largest first, and how many other layers hold
/// files, with what they take together.
pub async fn biggest(docker: &Docker, image: &str) -> (Vec<ImageLayer>, u32, u64) {
    let Ok(history) = docker.image_history(image).await else {
        return (vec![], 0, 0);
    };
    split(
        history
            .into_iter()
            .map(|h| (h.size.max(0) as u64, h.created_by))
            .collect(),
    )
}

fn split(layers: Vec<(u64, String)>) -> (Vec<ImageLayer>, u32, u64) {
    let mut layers: Vec<(u64, String)> = layers.into_iter().filter(|(s, _)| *s > 0).collect();
    layers.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
    let shown = layers
        .iter()
        .take(SHOWN)
        .take_while(|(size, _)| *size >= MIN_SHOWN)
        .count();
    let rest = &layers[shown..];
    (
        layers[..shown]
            .iter()
            .map(|(size, created_by)| ImageLayer {
                size: *size,
                command: command(created_by),
            })
            .collect(),
        rest.len() as u32,
        rest.iter().map(|(size, _)| size).sum(),
    )
}

/// A history entry's `created_by`, as the Dockerfile step it was:
/// `/bin/sh -c #(nop) ADD file:5d67…e4 in /` → `ADD file:5d673d25da3a… in /`,
/// `RUN |2 A=1 B=2 /bin/sh -c cargo build # buildkit` → `RUN cargo build`.
fn command(created_by: &str) -> String {
    let mut text = created_by.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(rest) = text.strip_suffix("# buildkit") {
        text = rest.trim_end().to_string();
    }
    let mut run = false;
    if let Some(rest) = text.strip_prefix("RUN ") {
        text = rest.to_string();
        run = true;
    }
    // Build arguments the step saw: `|2 A=1 B=2 `. Values may hold spaces, so
    // the shell is looked for rather than the arguments counted.
    if let Some(rest) = text.strip_prefix('|')
        && let Some((count, args)) = rest.split_once(' ')
        && let Ok(count) = count.parse::<usize>()
    {
        let shell = SHELLS
            .iter()
            .filter_map(|s| args.find(&format!(" {s}")).map(|i| i + 1))
            .min();
        text = match shell {
            Some(at) => args[at..].to_string(),
            None => args.splitn(count + 1, ' ').last().unwrap_or("").to_string(),
        };
        run = true;
    }
    for shell in SHELLS {
        if let Some(rest) = text.strip_prefix(shell) {
            text = rest.to_string();
            run = true;
            break;
        }
    }
    if let Some(rest) = text.strip_prefix("#(nop)") {
        text = rest.trim_start().to_string();
    } else if run {
        text = format!("RUN {text}");
    }
    if text.is_empty() {
        return "(no step recorded)".into();
    }
    // `file:` and `dir:` digests of ADD and COPY say nothing at that length.
    let text = text
        .split(' ')
        .map(|word| match word.split_once(':') {
            Some((kind @ ("file" | "dir"), digest))
                if digest.len() > 12 && digest.chars().all(|c| c.is_ascii_hexdigit()) =>
            {
                format!("{kind}:{}…", &digest[..12])
            }
            _ => word.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() > MAX_COMMAND {
        let cut: String = text.chars().take(MAX_COMMAND - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_read_like_the_dockerfile() {
        assert_eq!(
            command(
                "/bin/sh -c #(nop) ADD file:5d673d25da3a14ce1f6cf66e4c7fd4f4b85a3759a9d93efb3fd9ff852b5b56e4 in / "
            ),
            "ADD file:5d673d25da3a… in /"
        );
        assert_eq!(
            command("/bin/sh -c apt-get update &&     apt-get install -y \tcurl"),
            "RUN apt-get update && apt-get install -y curl"
        );
        assert_eq!(
            command("RUN /bin/sh -c pip install -r requirements.txt # buildkit"),
            "RUN pip install -r requirements.txt"
        );
        assert_eq!(
            command(
                "RUN |2 TARGETARCH=amd64 FEATURES=a b /bin/sh -c cargo build --release # buildkit"
            ),
            "RUN cargo build --release"
        );
        assert_eq!(command("|1 VERSION=3 /bin/sh -c make"), "RUN make");
        assert_eq!(
            command("COPY /src/target/release/app /usr/local/bin/app # buildkit"),
            "COPY /src/target/release/app /usr/local/bin/app"
        );
        assert_eq!(command(""), "(no step recorded)");
        let long = command(&format!("RUN /bin/sh -c {}", "x".repeat(300)));
        assert_eq!(long.chars().count(), MAX_COMMAND);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn keeps_the_largest_layers() {
        let mb = 1000 * 1000;
        let layers = vec![
            (0, "CMD".to_string()),
            (80 * mb, "a".into()),
            (3 * mb, "b".into()),
            (500 * mb, "c".into()),
            (mb, "d".into()),
            (20 * mb, "e".into()),
            (4 * mb, "f".into()),
            (5 * mb, "g".into()),
            (1000, "h".into()),
        ];
        let (shown, others, others_size) = split(layers);
        let names: Vec<&str> = shown.iter().map(|l| l.command.as_str()).collect();
        assert_eq!(names, ["c", "a", "e", "g", "f"]);
        assert_eq!(others, 3);
        assert_eq!(others_size, 3 * mb + mb + 1000);
        // Below the threshold, nothing gets a line of its own.
        let (shown, others, _) = split(vec![(mb, "a".into()), (mb, "b".into())]);
        assert!(shown.is_empty());
        assert_eq!(others, 2);
    }
}
