//! [`VmDiversity`] — the ONE value bundling every VM diversification seed, plus the
//! deterministic seed-derivation functions.
//!
//! The legacy emitter threaded `handler_seed`, `dispatch_seed`, `mba_seed`,
//! `skeleton_variant`, and the `perm`/`bin_perm`/`un_perm`/`code_key` through 13
//! positional parameters across `interpreter_src` / `embed_core` / the
//! `DeferredStringsVm` hand-off. This module collapses ALL of them into one value
//! drawn once ([`VmDiversity::draw`]) and routed everywhere — so the strings client
//! and the virtualize client provably ride the SAME diversification.
//!
//! Every derive_* function is a pure function of its seed (plus the opcode/sub-op
//! index). The `*_BASELINE` seed forces each knob to its historical variant-0 form,
//! which is the byte-identity baseline the legacy gates pinned.

use mangler_core::Rng;

use crate::isa::{N_BIN_OPS, N_OPCODES, N_UN_OPS};

/// Number of seed-chosen interpreter SKELETON variants (FU3): a coordinated
/// (dispatch-loop frame, helper form/order, thunk call form) triple. Variant 0 is
/// the historical fixed shape.
pub const SKELETON_VARIANTS: usize = 3;

/// Number of seed-selected, semantically-identical handler-body variants per opcode
/// (Stage-1b). Variant 0 is the historical body.
pub const HANDLER_VARIANTS: usize = 3;

/// Number of distinct decoy (junk-opcode) body shapes.
pub const DECOY_FORMS: usize = 8;

/// Number of seed-selected VM dispatch shapes (Stage-1a): 0 = `switch`, 1 =
/// array-of-closures (lean-only).
pub const DISPATCH_SHAPES: usize = 2;

/// Number of proven-exact MBA rewrite forms per eligible bitwise op (Stage-3b).
pub const MBA_FORMS: usize = 2;

/// The handler-seed forcing every opcode/decoy to its historical variant-0 form.
pub const HANDLER_SEED_BASELINE: u32 = 0;
/// The dispatch-seed forcing the historical `switch` shape.
pub const DISPATCH_SEED_BASELINE: u32 = 0;
/// The mba-seed disabling ALL handler-body MBA.
pub const MBA_SEED_BASELINE: u32 = 0;

/// Per-op probability (out of 256) of MBA-tangling an ELIGIBLE bitwise handler.
const MBA_RATE_NUM: u32 = 160;

/// Canonical bin sub-codes that are PROVABLY int32-exact, so the carry-sensitive
/// MBA identities apply soundly: `14 a&b`, `15 a|b`, `16 a^b`. Every other op is
/// excluded (the integer-domain bail).
const MBA_ELIGIBLE_BIN: [usize; 3] = [14, 15, 16];

/// The one value bundling every per-file VM diversification choice. Drawn once and
/// shared by both VM clients so the assembled table + interpreter(s) ride one
/// coherent diversification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmDiversity {
    /// Opcode-selector permutation, a bijection on `0..N_OPCODES + junk`. The first
    /// `N_OPCODES` entries are the real opcodes; the rest are decoy slots.
    pub perm: Vec<usize>,
    /// Bin sub-code permutation (bijection on `0..N_BIN_OPS`).
    pub bin_perm: Vec<usize>,
    /// Un sub-code permutation (bijection on `0..N_UN_OPS`).
    pub un_perm: Vec<usize>,
    /// Seed material for the packed code-group mask and UTF-16 constant mask.
    /// The high bit stays set to preserve the diversification draw contract.
    pub code_key: u32,
    /// FU3 skeleton variant (`0..SKELETON_VARIANTS`).
    pub skeleton_variant: usize,
    /// Stage-1b handler-body / decoy-form seed.
    pub handler_seed: u32,
    /// Stage-1a dispatch-shape seed.
    pub dispatch_seed: u32,
    /// Stage-3b integer-domain-MBA seed.
    pub mba_seed: u32,
}

impl VmDiversity {
    /// Draw a fresh diversification from a seeded RNG, in the canonical order the
    /// legacy `embed_core` used so a given seed reproduces the historical knobs:
    /// `junk` (2..=5 extra opcode slots) → `perm` → `bin_perm` → `un_perm` →
    /// `code_key` → `skeleton_variant` → `handler_seed` → `dispatch_seed` →
    /// `mba_seed`.
    pub fn draw(rng: &mut Rng) -> Self {
        let junk = 2 + rng.pick(4);
        let perm = rng.random_perm(N_OPCODES + junk);
        let bin_perm = rng.random_perm(N_BIN_OPS);
        let un_perm = rng.random_perm(N_UN_OPS);
        let code_key = rng.random_u32() | 0x8000_0000;
        let skeleton_variant = rng.pick(SKELETON_VARIANTS);
        let handler_seed = rng.random_u32();
        let dispatch_seed = rng.random_u32();
        let mba_seed = rng.random_u32();
        VmDiversity {
            perm,
            bin_perm,
            un_perm,
            code_key,
            skeleton_variant,
            handler_seed,
            dispatch_seed,
            mba_seed,
        }
    }

    /// A baseline diversification with identity permutations and every knob forced
    /// to its historical variant-0 form. Useful for tests and for any caller that
    /// wants the un-diversified canonical interpreter.
    pub fn baseline(junk: usize) -> Self {
        VmDiversity {
            perm: (0..N_OPCODES + junk).collect(),
            bin_perm: (0..N_BIN_OPS).collect(),
            un_perm: (0..N_UN_OPS).collect(),
            code_key: 0x8000_0001,
            skeleton_variant: 0,
            handler_seed: HANDLER_SEED_BASELINE,
            dispatch_seed: DISPATCH_SEED_BASELINE,
            mba_seed: MBA_SEED_BASELINE,
        }
    }

    /// The FU3 skeleton variant, reduced defensively.
    pub fn skeleton(&self) -> usize {
        self.skeleton_variant % SKELETON_VARIANTS
    }

    /// The dispatch shape for this file (`0` switch, `1` closures), forced to `0`
    /// when `needs_eh` (closures are lean-only).
    pub fn dispatch_shape(&self, needs_eh: bool) -> usize {
        derive_dispatch_shape(self.dispatch_seed, needs_eh)
    }

    /// The semantically-identical handler-body variant for canonical opcode `k`.
    pub fn handler_variant(&self, k: usize) -> usize {
        derive_handler_variant(self.handler_seed, k)
    }

    /// The decoy-body form for a junk slot at permuted `label`.
    pub fn decoy_form(&self, label: usize) -> usize {
        derive_decoy_form(self.handler_seed, label)
    }

    /// The MBA-tangle decision for eligible bitwise sub-op `k` (`Some(form)` to
    /// rewrite, `None` for the plain native op).
    pub fn bin_mba(&self, k: usize) -> Option<usize> {
        derive_bin_mba(self.mba_seed, k)
    }
}

/// Stage-1a: derive the dispatch shape (`0..DISPATCH_SHAPES`). Returns `0`
/// (`switch`) when `needs_eh` (the EH machinery is never composed with closures —
/// a deliberate, proven-sound bail) or when the seed is the baseline.
fn derive_dispatch_shape(seed: u32, needs_eh: bool) -> usize {
    if needs_eh || seed == DISPATCH_SEED_BASELINE {
        return 0;
    }
    let mut z = seed.wrapping_mul(0x9E37_79B9).wrapping_add(0x6D2B_79F5);
    z ^= z >> 15;
    z = z.wrapping_mul(0x85EB_CA77);
    z ^= z >> 13;
    (z as usize) % DISPATCH_SHAPES
}

/// Stage-1b: derive the body-variant index (`0..HANDLER_VARIANTS`) for canonical
/// opcode `k`. Seed `HANDLER_SEED_BASELINE` maps every opcode to variant 0.
fn derive_handler_variant(seed: u32, k: usize) -> usize {
    if seed == HANDLER_SEED_BASELINE {
        return 0;
    }
    let mut z = seed
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add((k as u32).wrapping_mul(0x85EB_CA77).wrapping_add(0x1));
    z ^= z >> 15;
    z = z.wrapping_mul(0x2C1B_3C6D);
    z ^= z >> 13;
    (z as usize) % HANDLER_VARIANTS
}

/// Stage-1b: derive the decoy-body form for a junk slot. Independent of the
/// real-handler variant stream (different mixing constants). Seed
/// `HANDLER_SEED_BASELINE` reduces to the fixed `label % DECOY_FORMS` numbering.
fn derive_decoy_form(seed: u32, label: usize) -> usize {
    if seed == HANDLER_SEED_BASELINE {
        return label % DECOY_FORMS;
    }
    let mut z = seed
        .wrapping_mul(0xA136_AAAD)
        .wrapping_add((label as u32).wrapping_mul(0xC2B2_AE35).wrapping_add(0x9));
    z ^= z >> 16;
    z = z.wrapping_mul(0x7FEB_352D);
    z ^= z >> 15;
    (z as usize) % DECOY_FORMS
}

/// Stage-3b: decide whether (and how) to MBA-tangle eligible bitwise sub-op `k`.
/// `Some(form)` when rewritten, `None` to emit the plain native op. Returns `None`
/// for the baseline seed and for every non-integer-domain op.
fn derive_bin_mba(mba_seed: u32, k: usize) -> Option<usize> {
    if mba_seed == MBA_SEED_BASELINE || !MBA_ELIGIBLE_BIN.contains(&k) {
        return None;
    }
    let mut z = mba_seed
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add((k as u32).wrapping_mul(0x9E37_79B1).wrapping_add(0x7));
    z ^= z >> 16;
    z = z.wrapping_mul(0x7FEB_352D);
    z ^= z >> 15;
    if (z & 0xFF) >= MBA_RATE_NUM {
        return None;
    }
    Some(((z >> 8) as usize) % MBA_FORMS)
}

/// Stage-3b: the MBA-tangled REPLACEMENT expression for eligible bitwise sub-op `k`
/// under `form`. EXACTLY equivalent to the plain op for every int32 `a`,`b`.
pub fn bin_mba_expr(k: usize, form: usize) -> &'static str {
    match (k, form % MBA_FORMS) {
        (14, 0) => "(a|b)-(a^b)",
        (14, _) => "~(~a|~b)",
        (15, 0) => "(a^b)+(a&b)",
        (15, _) => "~(~a&~b)",
        (16, 0) => "(a|b)-(a&b)",
        (16, _) => "(a|b)&~(a&b)",
        _ => unreachable!("bin_mba_expr called for non-integer-domain sub-op {k}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draw_is_deterministic() {
        let a = VmDiversity::draw(&mut Rng::for_pass(42, "vm"));
        let b = VmDiversity::draw(&mut Rng::for_pass(42, "vm"));
        assert_eq!(a, b);
        // High bit always set on the code key.
        assert_ne!(a.code_key & 0x8000_0000, 0);
        // perm is a valid bijection on its length.
        let mut sorted = a.perm.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..a.perm.len()).collect::<Vec<_>>());
    }

    #[test]
    fn baseline_forces_variant_zero() {
        let d = VmDiversity::baseline(2);
        assert_eq!(d.skeleton(), 0);
        assert_eq!(d.dispatch_shape(false), 0);
        assert_eq!(d.dispatch_shape(true), 0);
        for k in 0..N_OPCODES {
            assert_eq!(d.handler_variant(k), 0);
            assert_eq!(d.bin_mba(k), None);
        }
    }

    #[test]
    fn dispatch_shape_forced_switch_under_eh() {
        // Even with a non-baseline seed, EH always selects the switch shape.
        for seed in [1u32, 7, 12345, 0xDEAD_BEEF] {
            assert_eq!(derive_dispatch_shape(seed, true), 0);
        }
    }

    #[test]
    fn mba_only_eligible_bitwise() {
        // For a heavily-tangling seed, only 14/15/16 can ever be Some.
        for k in 0..N_BIN_OPS {
            let r = derive_bin_mba(0xFFFF_FFFF, k);
            if !(14..=16).contains(&k) {
                assert_eq!(r, None, "non-integer op {k} must never tangle");
            }
        }
    }

    /// The MBA identities are exact over all int32 (representative sweep).
    #[test]
    fn mba_forms_are_int32_exact() {
        let samples: [i32; 9] = [
            0,
            1,
            -1,
            2,
            -2,
            i32::MAX,
            i32::MIN,
            0x5555_5555u32 as i32,
            0x0F0F_0F0Fu32 as i32,
        ];
        for &a in &samples {
            for &b in &samples {
                // a & b
                assert_eq!((a | b).wrapping_sub(a ^ b), a & b);
                assert_eq!(!(!a | !b), a & b);
                // a | b
                assert_eq!((a ^ b).wrapping_add(a & b), a | b);
                assert_eq!(!(!a & !b), a | b);
                // a ^ b
                assert_eq!((a | b).wrapping_sub(a & b), a ^ b);
                assert_eq!((a | b) & !(a & b), a ^ b);
            }
        }
    }
}
