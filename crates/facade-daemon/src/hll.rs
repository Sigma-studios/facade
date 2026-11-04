//! HyperLogLog: how many distinct visitors, without keeping who they were.
//!
//! Each visitor's hash sets one of 4096 registers to the larger of what it
//! holds and the position of the hash's first one bit. Sketches merge by
//! taking the larger of each register, so the entry point, the server and any
//! number of readings in between agree, and resending one is harmless. The
//! estimate (Ertl's improved estimator, no bias tables) is within about 1.6 %
//! (one standard error), and exact enough for a handful of visitors.

use base64::Engine;

/// Bits of the hash choosing the register.
pub const PRECISION: u32 = 12;
pub const REGISTERS: usize = 1 << PRECISION;
/// The largest value a register can take: past the index bits, a hash has
/// `64 - PRECISION` bits left.
const MAX_RANK: u8 = (64 - PRECISION + 1) as u8;

/// The register a hash goes to, and the value it offers it.
pub fn place(hash: u64) -> (usize, u8) {
    let index = (hash >> (64 - PRECISION)) as usize;
    let rest = hash << PRECISION;
    let rank = (rest.leading_zeros() + 1).min(u32::from(MAX_RANK)) as u8;
    (index, rank)
}

/// Takes the larger of each register.
pub fn merge(into: &mut [u8], other: &[u8]) {
    for (a, b) in into.iter_mut().zip(other) {
        *a = (*a).max(*b);
    }
}

pub fn encode(registers: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(registers)
}

/// The registers of an encoded sketch, if it is one.
pub fn decode(encoded: &str) -> Option<Vec<u8>> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    (raw.len() == REGISTERS && raw.iter().all(|r| *r <= MAX_RANK)).then_some(raw)
}

/// How many distinct hashes went into the registers.
pub fn estimate(registers: &[u8]) -> f64 {
    let m = registers.len() as f64;
    let q = (64 - PRECISION) as usize;
    let mut counts = vec![0u32; q + 2];
    for &r in registers {
        counts[usize::from(r).min(q + 1)] += 1;
    }
    let mut z = m * tau(1.0 - f64::from(counts[q + 1]) / m);
    for k in (1..=q).rev() {
        z = 0.5 * (z + f64::from(counts[k]));
    }
    z += m * sigma(f64::from(counts[0]) / m);
    if z.is_infinite() {
        return 0.0;
    }
    m * m / (2.0 * std::f64::consts::LN_2) / z
}

fn sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let mut y = 1.0;
    let mut z = x;
    loop {
        x *= x;
        let before = z;
        z += x * y;
        y += y;
        if z == before {
            return z;
        }
    }
}

fn tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let mut y = 1.0;
    let mut z = 1.0 - x;
    loop {
        x = x.sqrt();
        let before = z;
        y *= 0.5;
        z -= (1.0 - x).powi(2) * y;
        if z == before {
            return z / 3.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-mixed 64-bit hash of a counter (splitmix64).
    fn mix(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }

    fn sketch(from: u64, n: u64) -> Vec<u8> {
        let mut registers = vec![0u8; REGISTERS];
        for i in from..from + n {
            let (index, rank) = place(mix(i));
            registers[index] = registers[index].max(rank);
        }
        registers
    }

    #[test]
    fn estimates_within_the_expected_error() {
        assert_eq!(estimate(&vec![0; REGISTERS]), 0.0);
        for n in [1u64, 7, 60, 500, 4_000, 12_000, 50_000, 400_000] {
            let estimate = estimate(&sketch(0, n));
            let error = (estimate - n as f64).abs() / n as f64;
            // Small counts are near exact; large ones within 3 standard errors.
            let allowed = if n <= 60 { 0.02 } else { 0.05 };
            assert!(error <= allowed, "{n}: {estimate} ({:.1} %)", error * 100.0);
        }
    }

    #[test]
    fn merging_counts_each_once() {
        let mut a = sketch(0, 3_000);
        let b = sketch(2_000, 3_000);
        // The same visitors again change nothing.
        let mut again = a.clone();
        merge(&mut again, &sketch(0, 3_000));
        assert_eq!(again, a);
        merge(&mut a, &b);
        let n = estimate(&a);
        assert!((n - 5_000.0).abs() < 5_000.0 * 0.05, "{n}");
    }

    #[test]
    fn round_trips() {
        let registers = sketch(0, 100);
        assert_eq!(decode(&encode(&registers)), Some(registers));
        assert_eq!(decode("bm9wZQ=="), None, "not a sketch");
        assert_eq!(place(0), (0, MAX_RANK));
        assert_eq!(place(u64::MAX), (REGISTERS - 1, 1));
    }
}
