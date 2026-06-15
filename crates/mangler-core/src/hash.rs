//! The single, canonical home for the small deterministic hash primitives the
//! pipeline relies on. Historically each of these was copy-pasted into several
//! passes; consolidating them here guarantees every site agrees byte-for-byte
//! and documents the *exact* integer semantics — which matters because some of
//! these hashes are re-emitted as JavaScript and must match the host runtime.
//!
//! All three are hand-rolled (no `std::hash::Hasher`, no process-randomized
//! `RandomState`/`DefaultHasher`): the same input always yields the same digest
//! across invocations, processes, and platforms. This is part of the crate-wide
//! determinism contract.
//!
//! # Primitives
//!
//! * [`Fnv64`] / [`fnv1a64`] — FNV-1a 64-bit. Used for content fingerprints.
//! * [`golden_mix`] — the 64-bit golden-ratio multiplicative mixer. Used to
//!   spread a value's bits before it is combined into a seed.
//! * [`Djb2`] / [`djb2`] / [`djb2_utf16`] — DJB2 32-bit. This is THE canonical
//!   definition; codegen crates emit the equivalent loop into JavaScript, so its
//!   integer semantics are load-bearing (see [`Djb2`]).

/// FNV-1a 64-bit offset basis (the standard constant).
pub const FNV_OFFSET_BASIS_64: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime (the standard constant).
pub const FNV_PRIME_64: u64 = 0x100_0000_01b3;

/// The 64-bit fractional part of the golden ratio (`2^64 / phi`), forced odd.
/// Used as the multiplier in [`golden_mix`] and as a generic bit-spreading
/// constant throughout the pipeline. This is the canonical value the scattered
/// `0x9E3779B97F4A7C15` copies were standardised on.
pub const GOLDEN_RATIO_64: u64 = 0x9E37_79B9_7F4A_7C15;

// ---------------------------------------------------------------------------
// FNV-1a 64-bit
// ---------------------------------------------------------------------------

/// Minimal deterministic FNV-1a 64-bit accumulator.
///
/// FNV-1a processes each byte as `hash = (hash XOR byte) * prime` (the XOR
/// happens *before* the multiply — that is what distinguishes FNV-1a from the
/// original FNV-1). Usable directly as a fold accumulator without the
/// `std::hash::Hasher` trait.
///
/// ```
/// use mangler_core::hash::Fnv64;
/// let mut h = Fnv64::new();
/// h.write(b"abc");
/// assert_eq!(h.finish(), mangler_core::hash::fnv1a64(b"abc"));
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Fnv64(u64);

impl Fnv64 {
    /// A fresh accumulator seeded with the FNV-1a offset basis.
    #[inline]
    pub fn new() -> Self {
        Fnv64(FNV_OFFSET_BASIS_64)
    }

    /// Mix a single byte: `h = (h ^ b) * prime`.
    #[inline]
    pub fn write_byte(&mut self, b: u8) {
        self.0 ^= b as u64;
        self.0 = self.0.wrapping_mul(FNV_PRIME_64);
    }

    /// Mix a byte slice, byte by byte.
    #[inline]
    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_byte(b);
        }
    }

    /// Mix a `u64` in little-endian byte order (platform-independent).
    #[inline]
    pub fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }

    /// The current 64-bit digest.
    #[inline]
    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Fnv64 {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot FNV-1a 64-bit hash of a byte slice.
#[inline]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = Fnv64::new();
    h.write(bytes);
    h.finish()
}

// ---------------------------------------------------------------------------
// Golden-ratio mixer
// ---------------------------------------------------------------------------

/// The 64-bit golden-ratio multiplicative mixer.
///
/// Multiplies by [`GOLDEN_RATIO_64`] and XOR-folds the high bits down, giving a
/// cheap avalanche so that small differences in the input (e.g. an incrementing
/// counter or a short id hash) produce well-spread outputs. This is *not* a
/// cryptographic hash; it is the bit-spreader used when combining values into a
/// seed.
///
/// ```
/// use mangler_core::hash::golden_mix;
/// assert_ne!(golden_mix(0), golden_mix(1));
/// // Stable across calls (deterministic).
/// assert_eq!(golden_mix(42), golden_mix(42));
/// ```
#[inline]
pub fn golden_mix(x: u64) -> u64 {
    let mut z = x.wrapping_mul(GOLDEN_RATIO_64);
    z ^= z >> 32;
    z = z.wrapping_mul(GOLDEN_RATIO_64);
    z ^= z >> 32;
    z
}

// ---------------------------------------------------------------------------
// DJB2 (the canonical definition)
// ---------------------------------------------------------------------------

/// DJB2 32-bit seed (the classic `5381`).
pub const DJB2_SEED: u32 = 5381;

/// Canonical DJB2 32-bit string hash accumulator.
///
/// **Integer semantics (load-bearing).** This crate's DJB2 uses the
/// *additive* recurrence
///
/// ```text
/// h = h * 33 + c   (mod 2^32)
/// ```
///
/// seeded at [`DJB2_SEED`] (`5381`), with all arithmetic closed over `u32`
/// (wrapping). This is the `h*33 + c` form — **not** the XOR variant
/// `h*33 ^ c`. It matches the existing in-JS loop
/// `h = (h*33 + s.charCodeAt(k)) >>> 0` byte-for-byte: `>>> 0` is JS's
/// ToUint32, the exact analogue of `u32` wrapping. Because codegen crates emit
/// that JS loop and compare against a constant computed here, changing this
/// recurrence would silently break every self-integrity guard. Mix code units
/// with [`Djb2::write_u16`] / the [`djb2_utf16`] helper to mirror the JS
/// `charCodeAt` (UTF-16) iteration.
///
/// ```
/// use mangler_core::hash::Djb2;
/// let mut h = Djb2::new();
/// for c in "hi".encode_utf16() { h.write_u16(c); }
/// assert_eq!(h.finish(), mangler_core::hash::djb2_utf16("hi"));
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Djb2(u32);

impl Djb2 {
    /// A fresh DJB2 accumulator seeded with [`DJB2_SEED`].
    #[inline]
    pub fn new() -> Self {
        Djb2(DJB2_SEED)
    }

    /// Mix one value via `h = h*33 + v` (mod 2^32).
    #[inline]
    pub fn write_u32(&mut self, v: u32) {
        self.0 = self.0.wrapping_mul(33).wrapping_add(v);
    }

    /// Mix one UTF-16 code unit (zero-extended), mirroring JS `charCodeAt`.
    #[inline]
    pub fn write_u16(&mut self, v: u16) {
        self.write_u32(v as u32);
    }

    /// Mix one byte (zero-extended).
    #[inline]
    pub fn write_byte(&mut self, b: u8) {
        self.write_u32(b as u32);
    }

    /// The current 32-bit digest.
    #[inline]
    pub fn finish(&self) -> u32 {
        self.0
    }
}

impl Default for Djb2 {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot DJB2 over raw bytes (each byte zero-extended): `h = h*33 + b`.
#[inline]
pub fn djb2(bytes: &[u8]) -> u32 {
    let mut h = Djb2::new();
    for &b in bytes {
        h.write_byte(b);
    }
    h.finish()
}

/// One-shot DJB2 over the UTF-16 code units of a string — the byte-exact mirror
/// of the in-JS `for(k) h=(h*33+s.charCodeAt(k))>>>0` loop. Use this (not
/// [`djb2`]) whenever the digest must agree with JavaScript, since JS strings
/// iterate UTF-16 code units, not UTF-8 bytes.
#[inline]
pub fn djb2_utf16(s: &str) -> u32 {
    let mut h = Djb2::new();
    for c in s.encode_utf16() {
        h.write_u16(c);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_test_vectors() {
        // Standard FNV-1a 64-bit test vectors.
        assert_eq!(fnv1a64(b""), FNV_OFFSET_BASIS_64);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn fnv1a64_incremental_matches_oneshot() {
        let mut h = Fnv64::new();
        h.write(b"foo");
        h.write(b"bar");
        assert_eq!(h.finish(), fnv1a64(b"foobar"));
    }

    #[test]
    fn fnv1a64_xor_before_multiply() {
        // FNV-1a (not FNV-1): one byte b is (offset ^ b) * prime.
        let mut h = Fnv64::new();
        h.write_byte(0x61); // 'a'
        let expected = (FNV_OFFSET_BASIS_64 ^ 0x61).wrapping_mul(FNV_PRIME_64);
        assert_eq!(h.finish(), expected);
        assert_eq!(h.finish(), fnv1a64(b"a"));
    }

    #[test]
    fn write_u64_is_little_endian() {
        let mut h = Fnv64::new();
        h.write_u64(0x0102_0304_0506_0708);
        let mut h2 = Fnv64::new();
        h2.write(&[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(h.finish(), h2.finish());
    }

    #[test]
    fn golden_mix_is_deterministic_and_spreads() {
        assert_eq!(golden_mix(42), golden_mix(42));
        // Distinct inputs map to distinct outputs for a swathe of values, and a
        // single-bit input change avalanches many output bits.
        let mut seen = std::collections::HashSet::new();
        for i in 0..1000u64 {
            assert!(seen.insert(golden_mix(i)), "collision at {i}");
        }
        let diff = (golden_mix(0) ^ golden_mix(1)).count_ones();
        assert!(diff > 8, "poor avalanche: {diff} bits changed");
    }

    #[test]
    fn djb2_canonical_is_additive_not_xor() {
        // Two bytes [a, b]: ((5381*33 + a)*33 + b). Prove it's +, not ^.
        let a = 0x12u32;
        let b = 0x34u32;
        let additive = DJB2_SEED
            .wrapping_mul(33)
            .wrapping_add(a)
            .wrapping_mul(33)
            .wrapping_add(b);
        assert_eq!(djb2(&[0x12, 0x34]), additive);

        let xor_variant = (DJB2_SEED.wrapping_mul(33) ^ a).wrapping_mul(33) ^ b;
        assert_ne!(
            djb2(&[0x12, 0x34]),
            xor_variant,
            "DJB2 must be the additive (h*33+c) form"
        );
    }

    #[test]
    fn djb2_empty_is_seed() {
        assert_eq!(djb2(b""), DJB2_SEED);
        assert_eq!(djb2_utf16(""), DJB2_SEED);
    }

    #[test]
    fn djb2_known_vector() {
        // Mirror of the in-JS loop for an ASCII string (UTF-16 == byte values).
        // Hand-computed reference for "hi": h0=5381
        //   h1 = 5381*33 + 104 = 177677
        //   h2 = 177677*33 + 105 = 5863446
        assert_eq!(djb2_utf16("hi"), 5_863_446);
        // ASCII: utf16 and byte forms agree.
        assert_eq!(djb2(b"hi"), djb2_utf16("hi"));
    }

    #[test]
    fn djb2_utf16_differs_from_bytes_for_non_ascii() {
        // 'é' is one UTF-16 code unit (0x00E9) but two UTF-8 bytes.
        assert_ne!(djb2(("é").as_bytes()), djb2_utf16("é"));
        assert_eq!(djb2_utf16("é"), DJB2_SEED.wrapping_mul(33).wrapping_add(0xE9));
    }

    #[test]
    fn djb2_incremental_matches_oneshot() {
        let mut h = Djb2::new();
        h.write_byte(b'a');
        h.write_byte(b'b');
        assert_eq!(h.finish(), djb2(b"ab"));
    }
}
