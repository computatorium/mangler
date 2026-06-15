//! Per-pass derived randomness — the heart of the determinism contract.
//!
//! # The problem this fixes
//!
//! The original pipeline shared a single seeded RNG across every pass. Each
//! pass drew from it in turn, so a pass's randomness depended on *how much*
//! every earlier pass happened to draw. Adding, removing, or reordering passes
//! — or even changing an early pass's draw count by one — silently shifted the
//! random stream of every later pass. Output was deterministic for a fixed
//! pipeline, but fragile: the structural coupling made the obfuscator's
//! randomness an emergent property of pass order rather than a stable function
//! of the seed.
//!
//! # The contract
//!
//! [`Rng::for_pass`] derives an **independent** ChaCha8 stream from
//! `(seed, pass_id)` alone. A pass's randomness is therefore a pure function of
//! the global `seed` and that pass's own stable `pass_id` string — it does
//! **not** depend on draw order relative to any other pass. Concretely:
//!
//! * Same `(seed, pass_id)` → byte-identical stream, always, everywhere.
//! * Different `pass_id` (same `seed`) → independent streams.
//! * Consuming from one pass's stream never perturbs another's.
//!
//! All randomness is seeded (ChaCha8); nothing here ever consults the OS or
//! `thread_rng`. `Date.now()` / `Math.random()`-style entropy is forbidden on
//! every path.
//!
//! The draw API ([`Rng::pick`], [`Rng::random_perm`], …) is ported verbatim in
//! behaviour from the original shared context, so existing passes produce
//! identical sequences once they switch to a per-pass stream.

use crate::hash::{Fnv64, golden_mix};
use rand::{RngExt, SeedableRng};
use rand::Rng as _;
use rand_chacha::ChaCha8Rng;

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Derive the 32-byte ChaCha8 seed for `(seed, pass_id)`.
///
/// We fold the global seed and the pass id through FNV-1a, then expand to the
/// full 256-bit ChaCha key by running a small counter-keyed golden-ratio mixer.
/// Both inputs are length-tagged so no two `(seed, pass_id)` pairs alias. This
/// keeps each pass's key well-separated from every other pass's, even when the
/// `pass_id` strings share long common prefixes.
fn derive_seed(seed: u64, pass_id: &str) -> [u8; 32] {
    let mut h = Fnv64::new();
    h.write_u64(seed);
    h.write_u64(pass_id.len() as u64);
    h.write(pass_id.as_bytes());
    let base = h.finish();

    let mut out = [0u8; 32];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        // Mix the digest with a domain-separating counter so the four 64-bit
        // lanes of the key are independent of one another.
        let lane = golden_mix(base ^ golden_mix(i as u64).wrapping_add(seed));
        chunk.copy_from_slice(&lane.to_le_bytes());
    }
    out
}

/// An independent, seeded random stream owned by a single pass.
///
/// Construct one with [`Rng::for_pass`]. The draw methods mirror the original
/// `PassContext` API exactly (same algorithms, same constants) so a pass's
/// output is unchanged by the move to per-pass streams.
pub struct Rng {
    inner: ChaCha8Rng,
}

impl Rng {
    /// Derive an independent ChaCha8 stream for `pass_id` under the global
    /// `seed`. The returned stream depends *only* on `(seed, pass_id)` — never
    /// on what any other pass has drawn. See the [module docs](self) for the
    /// full determinism contract.
    pub fn for_pass(seed: u64, pass_id: &str) -> Self {
        Rng {
            inner: ChaCha8Rng::from_seed(derive_seed(seed, pass_id)),
        }
    }

    /// Construct directly from a 32-byte ChaCha8 key. Escape hatch for callers
    /// that already hold a derived key; most code should use [`Rng::for_pass`].
    pub fn from_seed_bytes(seed: [u8; 32]) -> Self {
        Rng {
            inner: ChaCha8Rng::from_seed(seed),
        }
    }

    /// Seeded uniform integer in `[0, n)`. Returns `0` when `n == 0`.
    pub fn pick(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            self.inner.random_range(0..n)
        }
    }

    /// `n` fresh seeded bytes.
    pub fn random_bytes(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        self.inner.fill_bytes(&mut buf);
        buf
    }

    /// One `u32` from the seeded stream.
    pub fn random_u32(&mut self) -> u32 {
        self.inner.random::<u32>()
    }

    /// One `u64` from the seeded stream.
    pub fn random_u64(&mut self) -> u64 {
        self.inner.random::<u64>()
    }

    /// An odd integer in `[3, modulus)` that is coprime with `modulus`. For
    /// `modulus <= 2` returns `1`. Used to pick step constants for
    /// cross-referential derivations.
    ///
    /// Rejection-samples (coprimes are dense, so this terminates quickly); on
    /// the pathological exhaustion path it falls back to the documented
    /// degenerate `1`, which is coprime with everything.
    pub fn random_odd_coprime_with(&mut self, modulus: u64) -> u64 {
        const MAX_ATTEMPTS: usize = 4096;
        if modulus <= 2 {
            return 1;
        }
        for _ in 0..MAX_ATTEMPTS {
            let mut r = (self.random_u32() as u64) % modulus;
            if r < 3 {
                continue;
            }
            if r.is_multiple_of(2) {
                r |= 1;
            }
            if r >= modulus {
                continue;
            }
            if gcd(r, modulus) == 1 {
                return r;
            }
        }
        1
    }

    /// Pick one element uniformly from a non-empty slice. Panics on an empty
    /// slice.
    pub fn pick_one<T: Copy>(&mut self, xs: &[T]) -> T {
        let i = self.pick(xs.len());
        xs[i]
    }

    /// A seeded Fisher–Yates permutation of `0..n`. Empty for `n == 0`; always
    /// a valid permutation (each of `0..n` appears exactly once).
    pub fn random_perm(&mut self, n: usize) -> Vec<usize> {
        let mut v: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            let j = self.pick(i + 1);
            v.swap(i, j);
        }
        v
    }

    /// Seeded float in `[0, 1)` with 24-bit mantissa precision. Useful for rate
    /// math (e.g. probabilistic dead-state counts).
    pub fn random_f32_unit(&mut self) -> f32 {
        let bits = self.random_u32() >> 8; // top 24 bits
        (bits as f32) / ((1u32 << 24) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE critical property: a pass's stream depends only on `(seed, pass_id)`,
    /// not on draw-order relative to other passes. Build `Rng::for_pass(s, "a")`
    /// twice — once fresh, once after heavily consuming several *other* pass
    /// streams — and assert the "a" stream is byte-identical both times.
    #[test]
    fn per_pass_independence_under_reordering() {
        let seed = 0xDEAD_BEEF;

        // Run 1: draw "a" first, with no other interference.
        let a1: Vec<u32> = {
            let mut a = Rng::for_pass(seed, "a");
            (0..16).map(|_| a.random_u32()).collect()
        };

        // Run 2: consume wildly different amounts from other pass streams first,
        // in a different order, THEN build "a".
        let a2: Vec<u32> = {
            let mut z = Rng::for_pass(seed, "zzz");
            for _ in 0..123 {
                z.random_u32();
            }
            let mut b = Rng::for_pass(seed, "b");
            for _ in 0..7 {
                b.random_bytes(99);
            }
            let mut c = Rng::for_pass(seed, "cff-flatten");
            let _ = c.random_perm(40);
            let mut a = Rng::for_pass(seed, "a");
            (0..16).map(|_| a.random_u32()).collect()
        };

        assert_eq!(a1, a2, "pass \"a\" stream must not depend on other passes");
    }

    #[test]
    fn same_seed_same_pass_is_byte_identical() {
        let x: Vec<u8> = Rng::for_pass(7, "strings").random_bytes(64);
        let y: Vec<u8> = Rng::for_pass(7, "strings").random_bytes(64);
        assert_eq!(x, y);
    }

    #[test]
    fn different_pass_ids_give_independent_streams() {
        let a: Vec<u8> = Rng::for_pass(7, "strings").random_bytes(64);
        let b: Vec<u8> = Rng::for_pass(7, "opaque").random_bytes(64);
        assert_ne!(a, b, "distinct pass_ids must yield distinct streams");
    }

    #[test]
    fn different_seeds_give_independent_streams() {
        let a: Vec<u8> = Rng::for_pass(1, "strings").random_bytes(64);
        let b: Vec<u8> = Rng::for_pass(2, "strings").random_bytes(64);
        assert_ne!(a, b, "distinct seeds must yield distinct streams");
    }

    #[test]
    fn similar_pass_ids_do_not_alias() {
        // Length-tagging must keep prefix-sharing ids well separated.
        let a: Vec<u8> = Rng::for_pass(5, "pass").random_bytes(32);
        let b: Vec<u8> = Rng::for_pass(5, "passs").random_bytes(32);
        let c: Vec<u8> = Rng::for_pass(5, "").random_bytes(32);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn pick_is_bounded() {
        let mut r = Rng::for_pass(1, "p");
        for _ in 0..50 {
            assert!(r.pick(10) < 10);
        }
        assert_eq!(r.pick(0), 0);
    }

    #[test]
    fn random_bytes_deterministic_and_correct_length() {
        let xa = Rng::for_pass(7, "p").random_bytes(16);
        let xb = Rng::for_pass(7, "p").random_bytes(16);
        assert_eq!(xa.len(), 16);
        assert_eq!(xa, xb);
    }

    #[test]
    fn random_odd_coprime_with_returns_valid_value() {
        let mut r = Rng::for_pass(1, "p");
        for _ in 0..50 {
            let v = r.random_odd_coprime_with(7);
            assert!(v >= 1);
        }
        let mut r2 = Rng::for_pass(1, "p");
        assert_eq!(r2.random_odd_coprime_with(0), 1);
        assert_eq!(r2.random_odd_coprime_with(2), 1);
    }

    #[test]
    fn random_odd_coprime_actually_coprime() {
        let mut r = Rng::for_pass(2, "p");
        for _ in 0..50 {
            let v = r.random_odd_coprime_with(30);
            if v != 1 {
                assert_eq!(v % 2, 1, "must be odd: {v}");
                assert_eq!(gcd(v, 30), 1, "gcd(v,30) must be 1: {v}");
            }
        }
    }

    #[test]
    fn random_perm_is_deterministic_and_valid() {
        let pa = Rng::for_pass(123, "p").random_perm(64);
        let pb = Rng::for_pass(123, "p").random_perm(64);
        assert_eq!(pa, pb);
        assert_eq!(pa.len(), 64);
        let mut sorted = pa.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..64).collect::<Vec<_>>());

        let mut r = Rng::for_pass(5, "p");
        assert_eq!(r.random_perm(0), Vec::<usize>::new());
        assert_eq!(r.random_perm(1), vec![0]);
    }

    #[test]
    fn random_f32_unit_in_range_and_deterministic() {
        let mut a = Rng::for_pass(77, "p");
        for _ in 0..100 {
            let x = a.random_f32_unit();
            assert!((0.0..1.0).contains(&x), "out of [0,1): {x}");
        }
        let xa = Rng::for_pass(77, "p").random_f32_unit();
        let xb = Rng::for_pass(77, "p").random_f32_unit();
        assert_eq!(xa, xb);
    }

    #[test]
    fn pick_one_deterministic() {
        let xs = [10u8, 20, 30, 40];
        let va: Vec<_> = {
            let mut a = Rng::for_pass(99, "p");
            (0..10).map(|_| a.pick_one(&xs)).collect()
        };
        let vb: Vec<_> = {
            let mut b = Rng::for_pass(99, "p");
            (0..10).map(|_| b.pick_one(&xs)).collect()
        };
        assert_eq!(va, vb);
        for v in &va {
            assert!(xs.contains(v));
        }
    }
}
