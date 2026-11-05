//! Debian version ordering, as `dpkg --compare-versions` does it:
//! `[epoch:]upstream[-revision]`, where `~` sorts before anything (even the
//! end), letters before other characters, and digit runs compare as numbers.

use std::cmp::Ordering;

pub fn compare(a: &str, b: &str) -> Ordering {
    let (ea, ua, ra) = split(a);
    let (eb, ub, rb) = split(b);
    ea.cmp(&eb)
        .then_with(|| verrevcmp(ua, ub))
        .then_with(|| verrevcmp(ra, rb))
}

/// `a < b`.
pub fn less(a: &str, b: &str) -> bool {
    compare(a, b) == Ordering::Less
}

fn split(v: &str) -> (u64, &str, &str) {
    let v = v.trim();
    let (epoch, rest) = match v.split_once(':') {
        Some((e, rest)) if !e.is_empty() && e.bytes().all(|c| c.is_ascii_digit()) => {
            (e.parse().unwrap_or(0), rest)
        }
        _ => (0, v),
    };
    match rest.rsplit_once('-') {
        Some((upstream, revision)) => (epoch, upstream, revision),
        None => (epoch, rest, ""),
    }
}

/// The weight of a non-digit character.
fn order(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(b'~') => -1,
        Some(c) if c.is_ascii_digit() => 0,
        Some(c) if c.is_ascii_alphabetic() => i32::from(c),
        Some(c) => i32::from(c) + 256,
    }
}

fn verrevcmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        // The non-digit parts, character by character.
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let ca = a.get(i).copied().filter(|c| !c.is_ascii_digit());
            let cb = b.get(j).copied().filter(|c| !c.is_ascii_digit());
            let (oa, ob) = (order(ca), order(cb));
            if oa != ob {
                return oa.cmp(&ob);
            }
            if ca.is_some() {
                i += 1;
            }
            if cb.is_some() {
                j += 1;
            }
        }
        // Then the digit runs, as numbers (leading zeros ignored).
        while i < a.len() && a[i] == b'0' {
            i += 1;
        }
        while j < b.len() && b[j] == b'0' {
            j += 1;
        }
        let si = i;
        while i < a.len() && a[i].is_ascii_digit() {
            i += 1;
        }
        let sj = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        let (da, db) = (&a[si..i], &b[sj..j]);
        let by_len = da.len().cmp(&db.len());
        if by_len != Ordering::Equal {
            return by_len;
        }
        let by_digits = da.cmp(db);
        if by_digits != Ordering::Equal {
            return by_digits;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::{Equal, Greater, Less};

    /// Vectors from dpkg's own tests (lib/dpkg/t/t-version.c) and Debian Policy.
    #[test]
    fn orders_like_dpkg() {
        let cases = [
            ("0", "0", Equal),
            ("0", "00", Equal),
            ("1.0", "1.0-0", Equal),
            ("1:0", "0:0", Greater),
            ("1:0", "0:999", Greater),
            ("0:1.0", "1.0", Equal),
            ("1.0", "1.1", Less),
            ("1.2", "1.10", Less),
            ("1.0", "1.0a", Less),
            ("1.0a", "1.0+", Less),
            ("1.0~rc1", "1.0", Less),
            ("1.0~rc1", "1.0~rc2", Less),
            ("1.0~~", "1.0~~a", Less),
            ("1.0~~a", "1.0~", Less),
            ("1.0~", "1.0", Less),
            ("1.0", "1.0a", Less),
            ("1.0-1", "1.0-2", Less),
            ("1.0-1ubuntu1", "1.0-1ubuntu1.1", Less),
            ("1.0-1ubuntu0.2", "1.0-1ubuntu1", Less),
            ("1.0-1build1", "1.0-1ubuntu1", Less),
            ("2.12.0-1ubuntu5.3", "2.12.0-1ubuntu5.4", Less),
            ("7.0.0-42.42", "7.0.0-42.42.1", Less),
            ("0:7.0.0-42", "0:7.0.0-43", Less),
            ("0:7.0.0-42", "0:7.0.0-42", Equal),
            ("3.0.13-0ubuntu3.4", "3.0.13-0ubuntu3.10", Less),
            ("1:2.0", "2.0", Greater),
            ("a", "b", Less),
            ("1.0+dfsg-1", "1.0-1", Greater),
            ("1.0.0+git20260101-1", "1.0.0-1", Greater),
            ("7.7.0+7.0.0-42.42", "7.7.0+7.0.0-43.43", Less),
        ];
        for (a, b, want) in cases {
            assert_eq!(compare(a, b), want, "{a} vs {b}");
            assert_eq!(compare(b, a), want.reverse(), "{b} vs {a}");
        }
        assert!(less("1.0", "1.1"));
    }
}
