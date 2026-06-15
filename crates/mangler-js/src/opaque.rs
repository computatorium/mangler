//! Decoupled opaque-predicate library — runtime values that resist swc folding.
//!
//! [`opaque_u32`]`(rng, anchor, v)` returns a JS expression that evaluates to `v`
//! at runtime but cannot be constant-folded, by routing its value-determining path
//! through a call to a non-foldable **anchor** function (an impure-looking call
//! swc cannot prove pure). Used by the expression, control-flow-flatten and
//! dead-code passes for opaque constants and predicates.
//!
//! # The decoupling (design decision 4)
//!
//! Legacy opaque predicates *required* the strings decoder `core` as their anchor
//! — a hard `DecoderAnchor` edge that forced cfflatten/deadcode/expr to depend on
//! the strings pass. Here the anchor is an [`OpaqueAnchor`] the caller supplies:
//!
//! * if the strings decoder ran, pass its core name
//!   ([`OpaqueAnchor::decoder`]) — the strongest anchor (data-coupled guards read
//!   real decoded bytes);
//! * if it did NOT, the caller injects its OWN independently-seeded non-foldable
//!   opaque-seed function via [`inject_fallback_anchor`] and passes
//!   [`OpaqueAnchor::seeded`].
//!
//! Either way the anchor is `name(...)` returning a *string* swc can't model, so
//! every guard stays non-foldable. This turns the legacy hard edge into a checked
//! **optional** one: a consumer reads `DecoderAnchorArtifact` if present, else
//! falls back. See [`anchor_from_bus_or_inject`] for the one-call helper.
//!
//! # Determinism
//!
//! All randomness comes from the per-pass [`Rng`] the caller threads in (never
//! `thread_rng`), so a given `(eff_seed, pass_id)` always emits the same shapes.
//! Built on [`mangler_jsast::build`] node helpers — no hand-spelled node structs.

use crate::artifacts::DecoderAnchorArtifact;
use mangler_jsast::build as b;
use mangler_jsast::codegen;
use mangler_core::Rng;
use mangler_passgraph::ArtifactBus;
use swc_core::ecma::ast::{BinaryOp, Expr, Lit, ModuleItem, Program, Stmt, VarDeclKind};

/// The non-foldable anchor an opaque expression couples its value to.
///
/// Holds the name of a JS function that, called as `name(0)`, returns a string
/// swc cannot model — so any comparison against its result is non-foldable while
/// being provably constant at runtime. Either the strings decoder core, or a
/// fallback function injected by [`inject_fallback_anchor`].
#[derive(Debug, Clone)]
pub struct OpaqueAnchor {
    /// Name of the anchor function. `name(0)` yields a (string) value the opaque
    /// guards read.
    name: String,
}

impl OpaqueAnchor {
    /// Anchor on the strings decoder `core` (the strongest anchor — its guards can
    /// read real decoded bytes via `charCodeAt`).
    pub fn decoder(core_name: impl Into<String>) -> Self {
        OpaqueAnchor {
            name: core_name.into(),
        }
    }

    /// Anchor on an independently-injected fallback opaque-seed function (see
    /// [`inject_fallback_anchor`]). Behaviorally identical envelope to the decoder
    /// anchor; used when no strings decoder is present.
    pub fn seeded(name: impl Into<String>) -> Self {
        OpaqueAnchor { name: name.into() }
    }

    /// The anchor function's name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Build the fallback anchor *function declaration statement* to splice at the top
/// of the program body, returning `(stmt, anchor)`.
///
/// This is the "absent decoder" branch of design decision 4: the injected function
/// `function <name>(i){ var _i = i; return "<seed_word>"; }` returns a fixed string
/// swc cannot model through a call, so `name(0)` is a non-foldable anchor — exactly
/// the property the decoder `core` provides, but independent of the strings pass.
/// The minifier cannot fold the function away because every opaque expression
/// references it.
///
/// `name` should be a collision-free identifier (e.g. from
/// `FileConfig::fresh_name`); `seed_word` is a per-file string baked into the
/// returned constant (use a seed-derived word so the anchor varies per build).
/// Callers prepend `stmt` to the module/script body once, then thread `anchor` to
/// every [`opaque_u32`] call. See [`anchor_from_bus_or_inject`] for the one-call
/// helper that prefers the decoder and only injects this when it is absent.
pub fn fallback_anchor_decl(name: &str, seed_word: &str) -> (Stmt, OpaqueAnchor) {
    // function <name>(i){ var _ = i; return "<seed_word>"; }
    // The `var _ = i` use of the param keeps swc from trivially proving the
    // function pure-and-constant at the call site after inlining-resistance.
    let body = vec![
        b::var_decl(VarDeclKind::Var, "_i", b::ident_expr("i")),
        b::return_stmt(b::str_lit(seed_word)),
    ];
    let decl = codegen::fn_decl(name, &["i"], body);
    (decl, OpaqueAnchor::seeded(name))
}

/// Obtain an [`OpaqueAnchor`] for a pass: prefer the strings decoder if it ran,
/// otherwise inject an independent fallback anchor at the top of `program`.
///
/// This is the one-call entry point for the expr / cf-flatten / dead-code passes
/// (design decision 4). The pass must declare `reads() = [Resource::decoder_anchor()]`
/// so the bus permits the `get`. Behavior:
///
/// * decoder present → [`OpaqueAnchor::decoder`] over its `core_name`. No
///   injection (the pass does not own/skip the decoder declarator).
/// * decoder absent → splice [`fallback_anchor_decl`] at the front of the program
///   body once and return its [`OpaqueAnchor::seeded`]. `fresh` vends the
///   collision-free anchor name; `seed_word` derives the per-file anchor constant.
///
/// `injected` is set `true` iff a fallback was spliced — useful if the caller
/// wants to record the anchor name as a "skip this declarator" guard (mirroring
/// the legacy decoder-`core` guard).
pub fn anchor_from_bus_or_inject(
    program: &mut Program,
    bus: &ArtifactBus,
    fresh: impl FnOnce() -> String,
    seed_word: &str,
) -> Result<(OpaqueAnchor, bool), mangler_passgraph::BusError> {
    if let Some(d) = bus.get::<DecoderAnchorArtifact>()? {
        return Ok((OpaqueAnchor::decoder(d.core_name.clone()), false));
    }
    let name = fresh();
    let (decl, anchor) = fallback_anchor_decl(&name, seed_word);
    prepend_stmt(program, decl);
    Ok((anchor, true))
}

/// Prepend one statement to the program body (module or script).
fn prepend_stmt(program: &mut Program, stmt: Stmt) {
    match program {
        Program::Module(m) => m.body.insert(0, ModuleItem::Stmt(stmt)),
        Program::Script(s) => s.body.insert(0, stmt),
    }
}

/// Number of value-determining templates the selector chooses among. Each shares a
/// `(inner) >>> 0` (or `| 0`) envelope and routes through an anchor-call guard, but
/// varies `inner` so there is no single signature to strip.
const TEMPLATE_COUNT: usize = 10;

/// Number of distinct opaque-predicate guard shapes. Each supplies a matched
/// (always-true, always-false) pair anchored on `s = name(0)`.
const GUARD_COUNT: usize = 7;

/// Number of semantically-equivalent boolean wrapper forms.
const BOOL_TEMPLATE_COUNT: usize = 5;

/// An expression that evaluates to the unsigned 32-bit `value` at runtime but
/// resists swc constant-folding, drawn from one of [`TEMPLATE_COUNT`] seeded,
/// equivalent templates. `value >>> 0 == value` for all u32, so the `>>> 0`
/// envelope is exact.
pub fn opaque_u32(rng: &mut Rng, anchor: &OpaqueAnchor, value: u32) -> Expr {
    let template = rng.pick(TEMPLATE_COUNT);
    let guard = rng.pick(GUARD_COUNT);
    let a = match rng.random_u32() {
        0 => 1,
        n => n,
    };
    let bconst = rng.random_u32();
    zero_fill(build_template(template, guard, anchor.name(), value, a, bconst))
}

/// An expression that evaluates to the SIGNED 32-bit `value` (including
/// negatives), coerced with `| 0` (JS `ToInt32`) so a high-bit pattern round-trips
/// to its negative. Shares [`build_template`] with [`opaque_u32`].
pub fn opaque_i32(rng: &mut Rng, anchor: &OpaqueAnchor, value: i32) -> Expr {
    let template = rng.pick(TEMPLATE_COUNT);
    let guard = rng.pick(GUARD_COUNT);
    let a = match rng.random_u32() {
        0 => 1,
        n => n,
    };
    let bconst = rng.random_u32();
    int32_coerce(build_template(template, guard, anchor.name(), value as u32, a, bconst))
}

/// An expression that evaluates to the boolean `value` but resists folding, drawn
/// from one of [`BOOL_TEMPLATE_COUNT`] seeded forms (so no single `(opaque) > 0`
/// signature exists). Each derives the boolean from a non-foldable [`opaque_u32`].
pub fn opaque_bool(rng: &mut Rng, anchor: &OpaqueAnchor, value: bool) -> Expr {
    let form = rng.pick(BOOL_TEMPLATE_COUNT);
    match form {
        // (opaque(b?1:0)) > 0
        0 => b::bin(
            BinaryOp::Gt,
            b::paren(opaque_u32(rng, anchor, value as u32)),
            b::num_u32(0),
        ),
        // (opaque(b?1:0)) === 1
        1 => b::bin(
            BinaryOp::EqEqEq,
            b::paren(opaque_u32(rng, anchor, value as u32)),
            b::num_u32(1),
        ),
        // (opaque(b?0:1)) < 1
        2 => b::bin(
            BinaryOp::Lt,
            b::paren(opaque_u32(rng, anchor, (!value) as u32)),
            b::num_u32(1),
        ),
        // !(opaque(b?0:1))
        3 => b::not(b::paren(opaque_u32(rng, anchor, (!value) as u32))),
        // (opaque(b?2:3)) % 2 === 0
        _ => b::bin(
            BinaryOp::EqEqEq,
            b::paren(b::bin(
                BinaryOp::Mod,
                b::paren(opaque_u32(rng, anchor, if value { 2 } else { 3 })),
                b::num_u32(2),
            )),
            b::num_u32(0),
        ),
    }
}

/// Dispatch on a literal, producing a non-foldable opaque equivalent when one
/// applies (booleans; integers in `[2, u32::MAX]`). Returns `None` for literals
/// this library does not obfuscate (strings, floats, out-of-range, the trivially
/// inferable `0`/`1`). The single "obfuscate whatever literal this is" entry point.
pub fn opaque_lit(rng: &mut Rng, anchor: &OpaqueAnchor, lit: &Lit) -> Option<Expr> {
    match lit {
        Lit::Bool(bl) => Some(opaque_bool(rng, anchor, bl.value)),
        Lit::Num(n) => {
            let v = n.value;
            if v.fract() == 0.0 && v >= 2.0 && v <= u32::MAX as f64 {
                Some(opaque_u32(rng, anchor, v as u32))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Build the inner (pre-coercion) expression for template `t`. `a` is the
/// obscuring constant (forced non-zero by callers); `b` is a never-taken
/// dead-branch constant. `guard` selects which always-true ([`true_guard`]) /
/// always-false ([`false_guard`]) anchored predicate gates the value path. Each
/// form is `≡ value (mod 2^32)` once coerced.
fn build_template(t: usize, guard: usize, anchor: &str, value: u32, a: u32, b: u32) -> Expr {
    let tg = |c: &str| true_guard(guard, c);
    let fg = |c: &str| false_guard(guard, c);
    match t {
        // c ^ (g ? a : b)    [c = value ^ a]
        0 => {
            let c = value ^ a;
            b::bin(
                BinaryOp::BitXor,
                b::num_u32(c),
                b::paren(b::ternary(tg(anchor), b::num_u32(a), b::num_u32(b))),
            )
        }
        // c ^ (!g ? b : a)   [c = value ^ a]
        1 => {
            let c = value ^ a;
            b::bin(
                BinaryOp::BitXor,
                b::num_u32(c),
                b::paren(b::ternary(fg(anchor), b::num_u32(b), b::num_u32(a))),
            )
        }
        // k - (g ? a : b)    [k = value + a]
        2 => {
            let k = value.wrapping_add(a);
            b::bin(
                BinaryOp::Sub,
                b::num_u32(k),
                b::paren(b::ternary(tg(anchor), b::num_u32(a), b::num_u32(b))),
            )
        }
        // k + (g ? a : b)    [k = value - a]
        3 => {
            let k = value.wrapping_sub(a);
            b::bin(
                BinaryOp::Add,
                b::num_u32(k),
                b::paren(b::ternary(tg(anchor), b::num_u32(a), b::num_u32(b))),
            )
        }
        // (g ? h : b) ^ a    [h = value ^ a]  (ternary on the left)
        4 => {
            let h = value ^ a;
            b::bin(
                BinaryOp::BitXor,
                b::paren(b::ternary(tg(anchor), b::num_u32(h), b::num_u32(b))),
                b::num_u32(a),
            )
        }
        // (g ? q : b) + a    [q = value - a]  (ternary on the left)
        5 => {
            let q = value.wrapping_sub(a);
            b::bin(
                BinaryOp::Add,
                b::paren(b::ternary(tg(anchor), b::num_u32(q), b::num_u32(b))),
                b::num_u32(a),
            )
        }
        // k - (!g ? b : a)   [k = value + a]
        6 => {
            let k = value.wrapping_add(a);
            b::bin(
                BinaryOp::Sub,
                b::num_u32(k),
                b::paren(b::ternary(fg(anchor), b::num_u32(b), b::num_u32(a))),
            )
        }
        // k + (!g ? b : a)   [k = value - a]
        7 => {
            let k = value.wrapping_sub(a);
            b::bin(
                BinaryOp::Add,
                b::num_u32(k),
                b::paren(b::ternary(fg(anchor), b::num_u32(b), b::num_u32(a))),
            )
        }
        // MBA XOR: (c | t) - (c & t)   [c = value ^ a, t = g?a:b → a]
        8 => {
            let c = value ^ a;
            let t = || b::paren(b::ternary(tg(anchor), b::num_u32(a), b::num_u32(b)));
            b::bin(
                BinaryOp::Sub,
                b::paren(b::bin(BinaryOp::BitOr, b::num_u32(c), t())),
                b::paren(b::bin(BinaryOp::BitAnd, b::num_u32(c), t())),
            )
        }
        // MBA ADD: (k ^ t) + 2 * (k & t)   [k = value - a, t = g?a:b → a]
        _ => {
            let k = value.wrapping_sub(a);
            let t = || b::paren(b::ternary(tg(anchor), b::num_u32(a), b::num_u32(b)));
            b::bin(
                BinaryOp::Add,
                b::paren(b::bin(BinaryOp::BitXor, b::num_u32(k), t())),
                b::paren(b::bin(
                    BinaryOp::Mul,
                    b::num_u32(2),
                    b::paren(b::bin(BinaryOp::BitAnd, b::num_u32(k), t())),
                )),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Coercion envelopes
// ---------------------------------------------------------------------------

/// `(inner) >>> 0` — the unsigned u32 coercion envelope.
fn zero_fill(inner: Expr) -> Expr {
    b::bin(BinaryOp::ZeroFillRShift, b::paren(inner), b::num_u32(0))
}

/// `(inner) | 0` — the signed `ToInt32` envelope.
fn int32_coerce(inner: Expr) -> Expr {
    b::bin(BinaryOp::BitOr, b::paren(inner), b::num_u32(0))
}

// ---------------------------------------------------------------------------
// Anchored guard library
// ---------------------------------------------------------------------------
//
// Every guard is anchored on `s = anchor(0)`, which always returns a JS string
// (the decoder, or the fallback). swc cannot model the call's string return, so it
// cannot fold any of these — yet each is provably constant at runtime. g4/g5 read
// an actual byte (`charCodeAt(0)`), so truth depends on the returned DATA, staying
// total via the `length === 0 ||` short-circuit that avoids the empty-string `NaN`.

/// `anchor(0)`.
fn anchor_call(anchor: &str) -> Expr {
    b::call(b::ident_expr(anchor), vec![b::num_u32(0)])
}

/// `anchor(0).length`.
fn anchor_len(anchor: &str) -> Expr {
    b::member_ident(anchor_call(anchor), "length")
}

/// `anchor(0).charCodeAt(0)`.
fn anchor_char0(anchor: &str) -> Expr {
    b::call(
        b::member_ident(anchor_call(anchor), "charCodeAt"),
        vec![b::num_u32(0)],
    )
}

/// `anchor(0).indexOf(anchor(0))` — always 0.
fn anchor_index_of_self(anchor: &str) -> Expr {
    b::call(
        b::member_ident(anchor_call(anchor), "indexOf"),
        vec![anchor_call(anchor)],
    )
}

/// `(anchor(0) + "").length`.
fn anchor_concat_empty_len(anchor: &str) -> Expr {
    let concat = b::bin(BinaryOp::Add, anchor_call(anchor), b::str_lit(""));
    b::member_ident(b::paren(concat), "length")
}

/// `anchor(0).length === 0` — the empty-string short-circuit guard.
fn anchor_len_is_zero(anchor: &str) -> Expr {
    b::bin(BinaryOp::EqEqEq, anchor_len(anchor), b::num_u32(0))
}

/// An always-TRUE opaque predicate, shape selected by `guard % GUARD_COUNT`. Each
/// is constant-true at runtime but non-foldable by swc.
fn true_guard(guard: usize, anchor: &str) -> Expr {
    match guard % GUARD_COUNT {
        // s.length >= 0
        0 => b::bin(BinaryOp::GtEq, anchor_len(anchor), b::num_u32(0)),
        // s.length % 1 === 0
        1 => b::bin(
            BinaryOp::EqEqEq,
            b::paren(b::bin(BinaryOp::Mod, anchor_len(anchor), b::num_u32(1))),
            b::num_u32(0),
        ),
        // s.indexOf(s) === 0
        2 => b::bin(BinaryOp::EqEqEq, anchor_index_of_self(anchor), b::num_u32(0)),
        // s.length === (s + "").length
        3 => b::bin(
            BinaryOp::EqEqEq,
            anchor_len(anchor),
            anchor_concat_empty_len(anchor),
        ),
        // s.length === 0 || (s.charCodeAt(0) & 65535) === s.charCodeAt(0)
        4 => b::bin(
            BinaryOp::LogicalOr,
            anchor_len_is_zero(anchor),
            b::paren(b::bin(
                BinaryOp::EqEqEq,
                b::paren(b::bin(BinaryOp::BitAnd, anchor_char0(anchor), b::num_u32(0xFFFF))),
                anchor_char0(anchor),
            )),
        ),
        // s.length === 0 || s.charCodeAt(0) >= 0
        5 => b::bin(
            BinaryOp::LogicalOr,
            anchor_len_is_zero(anchor),
            b::paren(b::bin(BinaryOp::GtEq, anchor_char0(anchor), b::num_u32(0))),
        ),
        // (s.length & 1) === (s.length % 2)
        _ => b::bin(
            BinaryOp::EqEqEq,
            b::paren(b::bin(BinaryOp::BitAnd, anchor_len(anchor), b::num_u32(1))),
            b::paren(b::bin(BinaryOp::Mod, anchor_len(anchor), b::num_u32(2))),
        ),
    }
}

/// An always-FALSE opaque predicate — the exact logical negation of [`true_guard`]
/// for the same index. Constant-false at runtime, non-foldable by swc.
fn false_guard(guard: usize, anchor: &str) -> Expr {
    match guard % GUARD_COUNT {
        // s.length < 0
        0 => b::bin(BinaryOp::Lt, anchor_len(anchor), b::num_u32(0)),
        // s.length % 1 !== 0
        1 => b::bin(
            BinaryOp::NotEqEq,
            b::paren(b::bin(BinaryOp::Mod, anchor_len(anchor), b::num_u32(1))),
            b::num_u32(0),
        ),
        // s.indexOf(s) !== 0
        2 => b::bin(BinaryOp::NotEqEq, anchor_index_of_self(anchor), b::num_u32(0)),
        // s.length !== (s + "").length
        3 => b::bin(
            BinaryOp::NotEqEq,
            anchor_len(anchor),
            anchor_concat_empty_len(anchor),
        ),
        // s.length !== 0 && (s.charCodeAt(0) & 65535) !== s.charCodeAt(0)
        4 => b::bin(
            BinaryOp::LogicalAnd,
            b::paren(b::bin(BinaryOp::NotEqEq, anchor_len(anchor), b::num_u32(0))),
            b::paren(b::bin(
                BinaryOp::NotEqEq,
                b::paren(b::bin(BinaryOp::BitAnd, anchor_char0(anchor), b::num_u32(0xFFFF))),
                anchor_char0(anchor),
            )),
        ),
        // s.length !== 0 && s.charCodeAt(0) < 0
        5 => b::bin(
            BinaryOp::LogicalAnd,
            b::paren(b::bin(BinaryOp::NotEqEq, anchor_len(anchor), b::num_u32(0))),
            b::paren(b::bin(BinaryOp::Lt, anchor_char0(anchor), b::num_u32(0))),
        ),
        // (s.length & 1) !== (s.length % 2)
        _ => b::bin(
            BinaryOp::NotEqEq,
            b::paren(b::bin(BinaryOp::BitAnd, anchor_len(anchor), b::num_u32(1))),
            b::paren(b::bin(BinaryOp::Mod, anchor_len(anchor), b::num_u32(2))),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_testkit::eval::assert_behaviorally_equal;
    use swc_core::ecma::ast::Script;
    use swc_core::ecma::codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};
    use swc_core::common::sync::Lrc;
    use swc_core::common::SourceMap;

    /// Render a single expression to a bare `(<expr>)` source fragment.
    fn render(e: Expr) -> String {
        let cm: Lrc<SourceMap> = Default::default();
        let mut buf = Vec::new();
        {
            let wr = JsWriter::new(cm.clone(), "", &mut buf, None);
            let mut emitter = Emitter {
                cfg: CodegenConfig::default().with_minify(true),
                cm,
                comments: None,
                wr,
            };
            let program = Program::Script(Script {
                span: mangler_jsast::span::injected_span(),
                body: vec![b::expr_stmt(e)],
                shebang: None,
            });
            emitter.emit_program(&program).unwrap();
        }
        let s = String::from_utf8(buf).unwrap();
        s.trim().strip_suffix(';').unwrap_or(s.trim()).trim().to_string()
    }

    /// A program that sinks `inner` after defining a decoder stub returning "abc".
    fn decoder_program(inner: &str) -> String {
        format!("var _core=function(i){{return \"abc\";}};globalThis.__out=String(({inner}));")
    }

    /// A program that sinks the same value computed plainly (the reference).
    fn reference_program(expr: &str) -> String {
        format!("globalThis.__out=String(({expr}));")
    }

    #[test]
    fn opaque_u32_calls_anchor_and_uses_zero_fill() {
        let mut rng = Rng::for_pass(1, "opaque");
        let out = render(opaque_u32(&mut rng, &OpaqueAnchor::decoder("_core"), 42));
        assert!(out.contains("_core("), "must call anchor: {out}");
        assert!(out.contains(">>>"), "must coerce u32: {out}");
    }

    #[test]
    fn opaque_u32_is_deterministic_for_same_seed() {
        let mut a = Rng::for_pass(7, "opaque");
        let mut bb = Rng::for_pass(7, "opaque");
        let anchor = OpaqueAnchor::decoder("_c");
        assert_eq!(
            render(opaque_u32(&mut a, &anchor, 5)),
            render(opaque_u32(&mut bb, &anchor, 5)),
        );
    }

    /// WITH a decoder anchor present: every template/seed evaluates to the value.
    /// Compared behaviorally against the plain literal via the testkit sink harness
    /// (rquickjs, never bare node).
    #[test]
    fn evaluates_to_value_with_decoder_anchor() {
        let values = [0u32, 1, 2, 42, 255, 65535, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff];
        let anchor = OpaqueAnchor::decoder("_core");
        for seed in 0..24u64 {
            let mut rng = Rng::for_pass(seed, "opaque");
            for &v in &values {
                let inner = render(opaque_u32(&mut rng, &anchor, v));
                assert_behaviorally_equal(
                    &reference_program(&v.to_string()),
                    &decoder_program(&inner),
                );
            }
        }
    }

    /// WITHOUT a decoder anchor: the same library works against an injected
    /// fallback anchor — proving the decoupling (design decision 4).
    #[test]
    fn evaluates_to_value_with_fallback_anchor() {
        let (_decl, anchor) = fallback_anchor_decl("_0xanchor", "seedword");
        let values = [2u32, 42, 65535, 0xffff_ffff];
        for seed in 0..16u64 {
            let mut rng = Rng::for_pass(seed, "opaque");
            for &v in &values {
                let inner = render(opaque_u32(&mut rng, &anchor, v));
                // Stub the fallback anchor exactly as `fallback_anchor_decl` shapes it.
                let prog = format!(
                    "function _0xanchor(i){{var _i=i;return \"seedword\";}}\
                     globalThis.__out=String(({inner}));"
                );
                assert_behaviorally_equal(&reference_program(&v.to_string()), &prog);
            }
        }
    }

    #[test]
    fn opaque_i32_round_trips_signed() {
        let anchor = OpaqueAnchor::decoder("_core");
        let values: [i32; 6] = [-1, -2, -65536, i32::MIN, 42, i32::MAX];
        for seed in 0..16u64 {
            let mut rng = Rng::for_pass(seed, "opaque");
            for &v in &values {
                let inner = render(opaque_i32(&mut rng, &anchor, v));
                assert_behaviorally_equal(
                    &reference_program(&v.to_string()),
                    &decoder_program(&inner),
                );
            }
        }
    }

    #[test]
    fn opaque_bool_round_trips() {
        let anchor = OpaqueAnchor::decoder("_core");
        for seed in 0..16u64 {
            let mut rng = Rng::for_pass(seed, "opaque");
            for v in [true, false] {
                let inner = render(opaque_bool(&mut rng, &anchor, v));
                let prog = format!(
                    "var _core=function(i){{return \"abc\";}};globalThis.__out=String(!!({inner}));"
                );
                assert_behaviorally_equal(&reference_program(&v.to_string()), &prog);
            }
        }
    }

    #[test]
    fn templates_diversify_across_seeds() {
        let mut saw_xor = false;
        let mut saw_additive = false;
        let anchor = OpaqueAnchor::decoder("_core");
        for seed in 0..64u64 {
            let mut rng = Rng::for_pass(seed, "opaque");
            let s = render(opaque_u32(&mut rng, &anchor, 42));
            if s.contains('^') {
                saw_xor = true;
            }
            if s.contains('+') || s.contains('-') {
                saw_additive = true;
            }
        }
        assert!(saw_xor && saw_additive, "templates must diversify");
    }
}
