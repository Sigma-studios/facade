//! Failed requests and visitor sketches go to the server only when they
//! changed: a busy app's 200 error groups and two 4 KiB sketches are not worth
//! sending every 15 seconds. Each reading being cumulative (counts since the
//! entry point started, sketches merged by maxima), a report the server missed
//! only delays what the next one carries.

use std::collections::{BTreeMap, HashMap};

use facade_core::proto::EdgeReading;

/// What reports on the current connection to the server carried. A new
/// connection starts empty, so its first report says everything (an app with
/// nothing to say included): the server learns where each count stands.
#[derive(Default)]
pub struct Sent {
    apps: HashMap<String, SentApp>,
}

struct SentApp {
    since: u64,
    /// `(status, method, path)` → `(first_seen, count)`.
    errors: HashMap<(u16, String, String), (u64, u64)>,
    /// Day → encoded registers.
    visitors: HashMap<u64, String>,
}

impl Sent {
    /// What changed in `now` since the previous report, and remembers it as sent.
    pub fn changes(&mut self, now: BTreeMap<String, EdgeReading>) -> BTreeMap<String, EdgeReading> {
        self.apps.retain(|app, _| now.contains_key(app));
        let mut out = BTreeMap::new();
        for (app, reading) in now {
            let known = self.apps.get(&app).filter(|s| s.since == reading.since);
            let mut changed = EdgeReading {
                since: reading.since,
                ..Default::default()
            };
            let first = known.is_none();
            for e in &reading.errors {
                let key = (e.status, e.method.clone(), e.path.clone());
                if known.and_then(|s| s.errors.get(&key)) != Some(&(e.first_seen, e.count)) {
                    changed.errors.push(e.clone());
                }
            }
            for v in &reading.visitors {
                if known.and_then(|s| s.visitors.get(&v.day)) != Some(&v.registers) {
                    changed.visitors.push(v.clone());
                }
            }
            if first || !changed.errors.is_empty() || !changed.visitors.is_empty() {
                out.insert(app.clone(), changed);
            }
            self.apps.insert(
                app,
                SentApp {
                    since: reading.since,
                    errors: reading
                        .errors
                        .into_iter()
                        .map(|e| ((e.status, e.method, e.path), (e.first_seen, e.count)))
                        .collect(),
                    visitors: reading
                        .visitors
                        .into_iter()
                        .map(|v| (v.day, v.registers))
                        .collect(),
                },
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use facade_core::proto::{ErrorTally, VisitorSketch};

    fn tally(path: &str, count: u64) -> ErrorTally {
        ErrorTally {
            status: 404,
            method: "GET".into(),
            path: path.into(),
            count,
            first_seen: 10,
            last_seen: 20,
        }
    }

    fn reading(since: u64, errors: Vec<ErrorTally>, sketch: &str) -> EdgeReading {
        EdgeReading {
            since,
            errors,
            visitors: vec![VisitorSketch {
                day: 1,
                registers: sketch.into(),
            }],
        }
    }

    #[test]
    fn sends_what_changed() {
        let mut sent = Sent::default();
        let quiet = EdgeReading {
            since: 5,
            ..Default::default()
        };
        let first = sent.changes(BTreeMap::from([
            (
                "a".to_string(),
                reading(5, vec![tally("/x", 1), tally("/y", 1)], "A"),
            ),
            ("quiet".to_string(), quiet.clone()),
        ]));
        assert_eq!(first["a"].errors.len(), 2);
        assert_eq!(first["quiet"], quiet, "said once, even with nothing");

        let next = sent.changes(BTreeMap::from([
            (
                "a".to_string(),
                reading(5, vec![tally("/x", 1), tally("/y", 3)], "A"),
            ),
            ("quiet".to_string(), quiet.clone()),
        ]));
        assert_eq!(next.keys().collect::<Vec<_>>(), ["a"]);
        assert_eq!(next["a"].errors, [tally("/y", 3)]);
        assert!(next["a"].visitors.is_empty());

        // A new entry point: everything again.
        let restarted = sent.changes(BTreeMap::from([(
            "a".to_string(),
            reading(9, vec![tally("/x", 1)], "B"),
        )]));
        assert_eq!(restarted["a"].errors.len(), 1);
        assert_eq!(restarted["a"].visitors.len(), 1);
    }
}
