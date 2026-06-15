//! Collision-free deterministic identifier allocation.
//!
//! A [`NameAllocator`] vends `_0x…` identifier names that are:
//!
//! * **unique** — never repeated, and never equal to a *reserved* name (so
//!   injected names cannot collide with user bindings, even when
//!   re-obfuscating already-`_0x…`-named code);
//! * **deterministic** — same seed ⇒ same sequence of names; and
//! * **non-sequential** — the emitted hex suffixes are a seeded scramble of the
//!   internal counter, so they do **not** leak declaration order the way a naive
//!   `_0x1, _0x2, _0x3, …` counter would.
//!
//! Non-sequentiality comes from a per-allocator affine bijection on `u64`:
//! `display = counter * mult + off (mod 2^64)`, with `mult` forced odd so the
//! map is a bijection (distinct counters ⇒ distinct displays ⇒ uniqueness is
//! preserved). Both `mult` and `off` are derived purely from the seed (via the
//! golden-ratio mixer), so they consume no RNG draws — the allocator and a
//! pass's [`crate::rng::Rng`] never interfere.

use crate::hash::golden_mix;
use std::collections::HashSet;
use std::fmt::Write;

/// Seeded, collision-free identifier-name generator. See the [module
/// docs](self) for the guarantees.
pub struct NameAllocator {
    counter: u64,
    reserved: HashSet<String>,
    name_buf: String,
    name_mult: u64,
    name_off: u64,
}

impl NameAllocator {
    /// Build an allocator from a seed. Derives the affine name-scramble
    /// (`mult`/`off`) deterministically from `seed` alone; `mult` is forced odd
    /// so `counter -> display` is a bijection mod 2^64.
    pub fn new(seed: u64) -> Self {
        // Spread the seed through the golden-ratio mixer to decorrelate the two
        // constants, then domain-separate them. `| 1` forces an odd multiplier.
        let name_mult = golden_mix(seed ^ 0xA5A5_5A5A_C3C3_3C3C) | 1;
        let name_off = golden_mix(seed).rotate_left(32) ^ 0xD1B5_4A32_D192_ED03;
        NameAllocator {
            counter: 0,
            reserved: HashSet::new(),
            name_buf: String::new(),
            name_mult,
            name_off,
        }
    }

    /// Reserve names so [`NameAllocator::fresh`] will never produce them. Use
    /// this for every identifier already present in the source.
    pub fn reserve<I: IntoIterator<Item = String>>(&mut self, names: I) {
        self.reserved.extend(names);
    }

    /// Whether `name` is currently reserved.
    pub fn is_reserved(&self, name: &str) -> bool {
        self.reserved.contains(name)
    }

    /// Yield the next deterministic, collision-free `_0x…` name. Skips any name
    /// that lands on a reserved identifier (the affine map is a bijection, so a
    /// fresh counter always yields a fresh, distinct display).
    pub fn fresh(&mut self) -> String {
        loop {
            self.counter += 1;
            let display = self
                .counter
                .wrapping_mul(self.name_mult)
                .wrapping_add(self.name_off);
            self.name_buf.clear();
            self.name_buf.push_str("_0x");
            let _ = write!(self.name_buf, "{display:x}");
            if !self.reserved.contains(self.name_buf.as_str()) {
                return self.name_buf.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_names_are_unique_and_deterministic() {
        let mut a = NameAllocator::new(42);
        let mut b = NameAllocator::new(42);
        let na: Vec<_> = (0..3).map(|_| a.fresh()).collect();
        let nb: Vec<_> = (0..3).map(|_| b.fresh()).collect();
        assert_eq!(na, nb); // same seed -> same names
        assert_eq!(na.len(), 3);
        assert_ne!(na[0], na[1]); // unique
    }

    #[test]
    fn fresh_name_skips_reserved_identifiers() {
        let first = NameAllocator::new(0).fresh();
        let mut alloc = NameAllocator::new(0);
        alloc.reserve([first.clone()]);
        let a = alloc.fresh();
        assert_ne!(a, first, "fresh must skip a reserved name");
        let b = alloc.fresh();
        assert_ne!(a, b, "successive fresh names must differ");
    }

    #[test]
    fn fresh_names_are_non_sequential() {
        let mut alloc = NameAllocator::new(123);
        let names: Vec<_> = (0..3).map(|_| alloc.fresh()).collect();
        assert_ne!(
            names,
            vec!["_0x1".to_string(), "_0x2".to_string(), "_0x3".to_string()],
            "names must not be sequential"
        );
        assert_ne!(names[0], names[1]);
        assert_ne!(names[1], names[2]);
        for n in &names {
            assert!(n.starts_with("_0x"));
        }
    }

    #[test]
    fn many_names_collision_free() {
        // The affine map is a bijection, so a large run must be all-distinct.
        let mut alloc = NameAllocator::new(0xCAFE);
        let names: HashSet<_> = (0..10_000).map(|_| alloc.fresh()).collect();
        assert_eq!(names.len(), 10_000, "every fresh name must be unique");
    }

    #[test]
    fn different_seeds_diverge() {
        let a = NameAllocator::new(1).fresh();
        let b = NameAllocator::new(2).fresh();
        assert_ne!(a, b);
    }

    #[test]
    fn is_reserved_reflects_reservations() {
        let mut alloc = NameAllocator::new(0);
        assert!(!alloc.is_reserved("x"));
        alloc.reserve(["x".to_string()]);
        assert!(alloc.is_reserved("x"));
    }
}
