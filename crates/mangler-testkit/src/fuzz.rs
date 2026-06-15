//! Proptest + deterministic VM/expression fuzzing.
//!
//! Ported and generalized from `tests/vm_fuzz.rs`. Provides:
//! * A self-contained deterministic JS *generator* ([`Gen`]) over the
//!   virtualizer-eligible construct set (arithmetic, calls, branches, bounded loops,
//!   switch, try/catch/finally, throw, for-of, destructuring, spread) — fully
//!   reproducible from a `u64` seed.
//! * Program builders ([`build_program`], [`build_sink_program`]) that wrap a
//!   generated function body into a complete program whose result lands in
//!   `globalThis.__out` (so [`crate::eval`]'s sink path compares it).
//! * proptest strategies ([`arith_expr`], [`bitwise_expr`]) for randomly-shaped
//!   scope-free expressions, with shrinking.
//! * Round-trip harnesses ([`check_program`], [`fuzz_transform`],
//!   [`proptest_transform`]) that push a generated/strategy program through a
//!   caller-provided `Fn(&str) -> String` transform and assert behavioral
//!   equivalence — the reusable safety net any pass crate plugs its transform into.
//!
//! The generator's soundness obligations (so behavioral-equivalence is the only
//! thing under test): no nested functions/`with`/direct-`eval`/tagged-templates in
//! the generic template; all loops terminate (`for(var i=0;i<K;i++)` with constant
//! `K`, frozen counter); only deterministic ops (no `Date`/`Math.random`); every
//! variable reference is to an in-scope, source-order-preceding name.

use crate::eval::{eval_same_value_with, CaptureMode, DiffResult};

// ---------------------------------------------------------------------------
// Deterministic PRNG (splitmix64). Self-contained, byte-reproducible from a u64.
// ---------------------------------------------------------------------------
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % n as u64) as u32
    }
    fn chance(&mut self, num: u32, den: u32) -> bool {
        self.below(den) < num
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u32) as usize]
    }
}

/// Arithmetic ops used by the generator and the proptest strategies. Public so a
/// caller can build its own strategies over the same op set.
pub const ARITH: &[&str] = &["+", "-", "*", "%", "&", "|", "^"];
const RELOPS: &[&str] = &["<", "<=", ">", ">=", "===", "!==", "==", "!="];

/// Deterministic JS function-body generator. Build one with [`Gen::new`] from a
/// `u64` seed, then call [`Gen::function`] to emit a `function`-eligible body.
pub struct Gen {
    rng: Rng,
    locals: Vec<String>,
    scoped: Vec<String>,
    frozen: Vec<String>,
    next_id: u32,
    out: String,
    indent: u32,
}

impl Gen {
    /// A fresh generator seeded by `seed`. Params `a`, `b`, `c` are pre-declared as
    /// readable/assignable locals (the body becomes `function f(a,b,c){ ... }`).
    pub fn new(seed: u64) -> Self {
        Gen {
            rng: Rng::new(seed),
            locals: vec!["a".into(), "b".into(), "c".into()],
            scoped: Vec::new(),
            frozen: Vec::new(),
            next_id: 0,
            out: String::new(),
            indent: 0,
        }
    }

    fn fresh(&mut self) -> String {
        let n = format!("v{}", self.next_id);
        self.next_id += 1;
        n
    }

    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push(' ');
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn is_frozen(&self, name: &str) -> bool {
        self.frozen.iter().any(|n| n == name)
    }

    fn ref_var(&mut self) -> String {
        let n = self.locals.len() + self.scoped.len();
        let i = self.rng.below(n as u32) as usize;
        if i < self.locals.len() {
            self.locals[i].clone()
        } else {
            self.scoped[i - self.locals.len()].clone()
        }
    }

    fn assignable(&mut self) -> String {
        let candidates: Vec<String> = self
            .locals
            .iter()
            .chain(self.scoped.iter())
            .filter(|n| !self.is_frozen(n))
            .cloned()
            .collect();
        self.rng.pick(&candidates).clone()
    }

    fn expr(&mut self, depth: u32) -> String {
        if depth == 0 || self.rng.chance(2, 5) {
            if self.rng.chance(1, 2) {
                let v = self.rng.below(40) as i64 - 12;
                format!("{v}")
            } else {
                self.ref_var()
            }
        } else {
            let op = *self.rng.pick(ARITH);
            let l = self.expr(depth - 1);
            let r = self.expr(depth - 1);
            format!("({l} {op} {r})")
        }
    }

    fn cond(&mut self, depth: u32) -> String {
        let op = *self.rng.pick(RELOPS);
        let l = self.expr(depth);
        let r = self.expr(depth);
        format!("{l} {op} {r}")
    }

    fn int_array(&mut self) -> String {
        let n = 1 + self.rng.below(4);
        let mut parts = Vec::new();
        for _ in 0..n {
            parts.push(format!("{}", self.rng.below(20) as i64 - 6));
        }
        format!("[{}]", parts.join(", "))
    }

    fn block(&mut self, stmts: u32, depth: u32) {
        self.line("{");
        self.indent += 2;
        let mut emitted_terminal = false;
        for _ in 0..stmts {
            if emitted_terminal {
                break;
            }
            emitted_terminal = self.stmt(depth);
        }
        self.indent -= 2;
        self.line("}");
    }

    /// Emit one statement. Returns true if it is a terminal `return`.
    fn stmt(&mut self, depth: u32) -> bool {
        let kind = if depth == 0 {
            self.rng.below(3)
        } else {
            self.rng.below(12)
        };
        match kind {
            0 => {
                let name = self.fresh();
                let e = self.expr(depth.min(2));
                self.line(&format!("var {name} = {e};"));
                self.locals.push(name);
                false
            }
            1 => {
                let t = self.assignable();
                let e = self.expr(depth.min(2));
                let compound = *self.rng.pick(&["=", "+=", "-=", "*="]);
                self.line(&format!("{t} {compound} {e};"));
                false
            }
            2 => {
                let e = self.expr(depth.min(2));
                self.line(&format!("return {e};"));
                true
            }
            3 => {
                let c = self.cond(depth.min(2));
                self.line(&format!("if ({c})"));
                let n = 1 + self.rng.below(3);
                self.block(n, depth - 1);
                if self.rng.chance(1, 2) {
                    self.line("else");
                    let n = 1 + self.rng.below(3);
                    self.block(n, depth - 1);
                }
                false
            }
            4 => {
                let i = self.fresh();
                let k = self.rng.below(5);
                self.line(&format!("for (var {i} = 0; {i} < {k}; {i}++)"));
                self.locals.push(i.clone());
                self.frozen.push(i);
                let n = 1 + self.rng.below(3);
                self.block(n, depth - 1);
                false
            }
            5 => {
                let d = self.expr(depth.min(2));
                self.line(&format!("switch ((({d}) % 3 + 3) % 3) {{"));
                self.indent += 2;
                for case in 0..3 {
                    self.line(&format!("case {case}:"));
                    self.indent += 2;
                    let _ = self.stmt(0);
                    self.line("break;");
                    self.indent -= 2;
                }
                self.line("default:");
                self.indent += 2;
                let _ = self.stmt(0);
                self.indent -= 2;
                self.indent -= 2;
                self.line("}");
                false
            }
            6 => {
                self.line("try");
                self.line("{");
                self.indent += 2;
                let _ = self.stmt(0);
                if self.rng.chance(1, 2) {
                    let c = self.cond(depth.min(2));
                    let thrown = self.rng.below(99);
                    self.line(&format!("if ({c}) {{ throw {thrown}; }}"));
                }
                let _ = self.stmt(0);
                self.indent -= 2;
                self.line("}");
                let e = self.fresh();
                self.line(&format!("catch ({e})"));
                self.scoped.push(e.clone());
                self.frozen.push(e.clone());
                let scoped_mark = self.scoped.len() - 1;
                let n = 1 + self.rng.below(2);
                self.block(n, 0);
                self.scoped.truncate(scoped_mark);
                self.frozen.retain(|n| n != &e);
                if self.rng.chance(1, 2) {
                    self.line("finally");
                    let n = 1 + self.rng.below(2);
                    self.block(n, 0);
                }
                false
            }
            7 => {
                let x = self.fresh();
                let arr = self.int_array();
                self.line(&format!("for (var {x} of {arr})"));
                self.locals.push(x.clone());
                self.frozen.push(x);
                let n = 1 + self.rng.below(2);
                self.block(n, depth - 1);
                false
            }
            8 => {
                let p = self.fresh();
                let q = self.fresh();
                let arr = self.int_array();
                self.line(&format!("var [{p}, {q}] = {arr};"));
                self.locals.push(p);
                self.locals.push(q);
                false
            }
            9 => {
                let m = self.fresh();
                let nn = self.fresh();
                let em = self.expr(depth.min(2));
                let en = self.expr(depth.min(2));
                self.line(&format!("var {{ m: {m}, n: {nn} }} = {{ m: {em}, n: {en} }};"));
                self.locals.push(m);
                self.locals.push(nn);
                false
            }
            10 => {
                let name = self.fresh();
                let outer = self.expr(depth.min(2));
                let inner = self.expr(depth.min(2));
                let sink_in = self.fresh();
                let sink_out = self.fresh();
                self.line(&format!("var {sink_in} = 0, {sink_out} = 0;"));
                self.line(&format!("let {name} = {outer};"));
                self.line("{");
                self.indent += 2;
                self.line(&format!("let {name} = {inner};"));
                self.line(&format!("{sink_in} = {name} + 1;"));
                self.indent -= 2;
                self.line("}");
                self.line(&format!("{sink_out} = {name} - 1;"));
                self.locals.push(sink_in);
                self.locals.push(sink_out);
                false
            }
            _ => {
                let s = self.fresh();
                let a1 = self.int_array();
                let extra = self.expr(depth.min(2));
                self.line(&format!("var {s} = [...{a1}, {extra}].length;"));
                self.locals.push(s);
                false
            }
        }
    }

    /// Emit a full `function f(a,b,c)` body. The body always ends with a numeric
    /// `return`, so the function has a well-defined value. Consumes the generator's
    /// buffer (call once).
    pub fn function(&mut self) -> String {
        let stmts = 3 + self.rng.below(5);
        for _ in 0..stmts {
            if self.stmt(3) {
                return std::mem::take(&mut self.out);
            }
        }
        let e = self.expr(2);
        self.line(&format!("return {e};"));
        std::mem::take(&mut self.out)
    }
}

/// Wrap a generated function `body` into a complete program. The program defines
/// `function f(a,b,c){<body>}`, folds several fixed argument tuples into one string,
/// and assigns it to `globalThis.__out` so [`crate::eval`]'s sink path compares it.
pub fn build_program(body: &str) -> String {
    build_sink_program(body)
}

/// As [`build_program`]: defines `f`, drives it over fixed tuples, and stores the
/// concatenated result string in `globalThis.__out`.
pub fn build_sink_program(body: &str) -> String {
    format!(
        "(function(){{\n\
         function f(a, b, c) {{\n{body}}}\n\
         var __o = '';\n\
         __o += String(f(1, 2, 3));\n\
         __o += '|' + String(f(0, -1, 7));\n\
         __o += '|' + String(f(5, 5, 5));\n\
         __o += '|' + String(f(-3, 4, -2));\n\
         __o += '|' + String(f(11, 0, -8));\n\
         globalThis.__out = String(__o);\n\
         }})();\n"
    )
}

/// Differentially check one complete program: run it (original) and `transform(it)`
/// (transformed), comparing under the sink capture mode. Returns the [`DiffResult`].
pub fn check_program<F>(program: &str, transform: &mut F) -> DiffResult
where
    F: FnMut(&str) -> String,
{
    let transformed = transform(program);
    eval_same_value_with(program, &transformed, &CaptureMode::sink())
}

/// Generate `n` programs deterministically from `base_seed`, push each through
/// `transform`, and return any that diverged (empty = all equivalent). The reusable
/// VM/expression fuzz net: `fuzz_transform(600, BASE, |src| my_pass(src))`.
pub fn fuzz_transform<F>(n: u64, base_seed: u64, mut transform: F) -> Vec<(u64, String, DiffResult)>
where
    F: FnMut(&str) -> String,
{
    let mut failures = Vec::new();
    for i in 0..n {
        let gen_seed = base_seed.wrapping_add(i.wrapping_mul(0x0100_0001));
        let body = Gen::new(gen_seed).function();
        let program = build_program(&body);
        let diff = check_program(&program, &mut transform);
        if diff.is_divergent() {
            failures.push((gen_seed, program, diff));
        }
    }
    failures
}

/// Panicking wrapper over [`fuzz_transform`]: asserts every generated program
/// round-trips equivalently through `transform`, printing the offending seed +
/// source + reason on the first divergence.
pub fn assert_fuzz_transform<F>(n: u64, base_seed: u64, transform: F)
where
    F: FnMut(&str) -> String,
{
    let failures = fuzz_transform(n, base_seed, transform);
    if let Some((seed, program, diff)) = failures.first() {
        panic!(
            "VM fuzz divergence (seed {seed}): {}\n  original:    {}\n  transformed: {}\n--- program ---\n{program}",
            diff.reason, diff.original, diff.transformed
        );
    }
}

#[cfg(feature = "proptest")]
mod strategies {
    use proptest::prelude::*;

    /// proptest strategy: an arithmetic expression over params `a,b,c` and small
    /// integer literals, combined with `+ - * % & | ^`. Scope-free, so it is a clean
    /// recursive strategy with shrinking.
    pub fn arith_expr() -> impl Strategy<Value = String> {
        let leaf = prop_oneof![
            (-12i64..28).prop_map(|n| n.to_string()),
            prop::sample::select(vec!["a", "b", "c"]).prop_map(|s| s.to_string()),
        ];
        leaf.prop_recursive(5, 48, 2, |inner| {
            (
                inner.clone(),
                prop::sample::select(super::ARITH.to_vec()),
                inner,
            )
                .prop_map(|(l, op, r)| format!("({l} {op} {r})"))
        })
    }

    /// proptest strategy: a BITWISE-ONLY expression over `a,b,c` and small int
    /// literals, combined only with `& | ^` — drives integer-domain handlers.
    pub fn bitwise_expr() -> impl Strategy<Value = String> {
        let leaf = prop_oneof![
            (-12i64..28).prop_map(|n| n.to_string()),
            prop::sample::select(vec!["a", "b", "c"]).prop_map(|s| s.to_string()),
        ];
        let ops = vec!["&", "|", "^"];
        leaf.prop_recursive(5, 48, 2, move |inner| {
            (inner.clone(), prop::sample::select(ops.clone()), inner)
                .prop_map(|(l, op, r)| format!("({l} {op} {r})"))
        })
    }
}

#[cfg(feature = "proptest")]
pub use strategies::{arith_expr, bitwise_expr};

/// Run a proptest `expr`-producing strategy through `transform`, building each
/// expression into a `return <expr>;` sink program and asserting behavioral
/// equivalence. `cases` controls the proptest sample count; the run is deterministic
/// (fixed ChaCha seed) so a green run stays green and a red one is reproducible.
#[cfg(feature = "proptest")]
pub fn proptest_transform<S, F>(strategy: S, cases: u32, transform: F)
where
    S: proptest::strategy::Strategy<Value = String>,
    F: FnMut(&str) -> String,
{
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    use std::cell::RefCell;
    // proptest's test closure is `Fn`, so wrap the `FnMut` transform in a RefCell to
    // get interior mutability (the runner calls it sequentially, never re-entrantly).
    let transform = RefCell::new(transform);
    let cfg = Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    };
    let mut runner =
        TestRunner::new_with_rng(cfg, TestRng::from_seed(RngAlgorithm::ChaCha, &[0x5A; 32]));
    runner
        .run(&strategy, |expr| {
            let body = format!("  return {expr};\n");
            let program = build_program(&body);
            let diff = check_program(&program, &mut *transform.borrow_mut());
            proptest::prop_assert!(
                diff.equal,
                "divergence on `{}`: {}\n{}",
                expr,
                diff.reason,
                program
            );
            Ok(())
        })
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_is_deterministic() {
        let a = Gen::new(12345).function();
        let b = Gen::new(12345).function();
        assert_eq!(a, b, "same seed must yield identical body");
        let c = Gen::new(99999).function();
        assert_ne!(a, c, "different seeds should (almost surely) differ");
    }

    #[test]
    fn generated_programs_eval_in_quickjs() {
        // Every generated program must be sound: identity transform => equal.
        let failures = fuzz_transform(150, 0xF0F0_1234_5678_9ABC, |s| s.to_string());
        assert!(
            failures.is_empty(),
            "{} generated programs failed identity round-trip; first: {:?}",
            failures.len(),
            failures.first().map(|(seed, _, d)| (seed, &d.reason))
        );
    }

    #[test]
    fn fuzz_detects_a_corrupting_transform() {
        // A transform that overwrites the sink must diverge on (essentially) all.
        let failures = fuzz_transform(20, 0xABCD, |s| {
            format!("{s}\nglobalThis.__out = 'WRONG';")
        });
        assert!(!failures.is_empty(), "corrupting transform must be caught");
    }

    #[test]
    fn build_program_uses_sink() {
        let p = build_program("  return a + b + c;\n");
        assert!(p.contains("globalThis.__out"));
    }

    #[cfg(feature = "proptest")]
    #[test]
    fn proptest_arith_identity_passes() {
        proptest_transform(arith_expr(), 64, |s| s.to_string());
    }

    #[cfg(feature = "proptest")]
    #[test]
    fn proptest_bitwise_identity_passes() {
        proptest_transform(bitwise_expr(), 48, |s| s.to_string());
    }
}
