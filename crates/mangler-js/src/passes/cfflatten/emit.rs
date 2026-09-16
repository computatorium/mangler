//! Renders a basic-block CFG into a `while (1) switch (key) { ... }` state
//! machine [`BlockStmt`].
//!
//! State IDs (block indices) are decoupled from the emitted state *values* via a
//! seeded label permutation (`labels`): block `id` is emitted as `case
//! labels[id]:`, the machine is initialized to `labels[entry]`, and every
//! transition assigns `labels[target]`. The dispatcher switches on the state
//! value. Each block becomes one `case` that runs its straight-line statements
//! then performs its edge:
//!   * `Sequential(n)`        → `<set state labels[n]> break;`
//!   * `Branch{cond,t,e}`     → `if (cond) { <set labels[t]> } else { <set labels[e]> } break;`
//!   * `Return(Some(e))`      → `return e;`
//!   * `Return(None)`         → `return;`
//!   * `Throw(e)`             → `throw e;`
//!
//! Two-variable dispatch (`Dispatch::Two`): the state value `V` is split across
//! two variables `s1 = V / K`, `s2 = V % K` (`K = ceil(sqrt(total))`), and the
//! dispatcher switches on a seed-chosen recombination that evaluates to exactly
//! `s1 * K + s2 == V`.
//!
//! Opaque (non-foldable) state values come from the decoupled
//! [`crate::opaque::opaque_u32`] library: an `Option<&OpaqueAnchor>`. When `Some`,
//! every emitted state value (in inits and assignments, NOT in `case` labels,
//! which must stay constant) is routed through an anchor-coupled non-foldable
//! expression; when `None` (e.g. inside the decoder's own initializer) it falls
//! back to a bare numeric literal.

use super::cfg::{BasicBlock, Edge};
use crate::opaque::{OpaqueAnchor, opaque_u32};
use mangler_core::Rng;
use swc_core::common::{DUMMY_SP, SyntaxContext};
use swc_core::ecma::ast::*;

/// How the dispatcher key is represented in emitted code.
pub enum Dispatch {
    /// Single state variable `name`.
    Single { name: String },
    /// Two state variables. State value `V` is stored as `name1 = V / k`,
    /// `name2 = V % k`. Discriminant is a seed-chosen recombination evaluating to
    /// `name1 * k + name2 == V`.
    Two {
        name1: String,
        name2: String,
        k: usize,
    },
}

/// Options controlling how the state machine is rendered.
pub struct RenderOpts<'a> {
    /// `labels[id]` is the emitted state value for block `id`. Must be a
    /// permutation of `0..labels.len()` with `labels.len() >= blocks.len()`.
    pub labels: &'a [usize],
    /// Dispatcher representation (single or two state variables).
    pub dispatch: Dispatch,
    /// Opaque anchor, when present. Enables non-foldable opaque transitions: each
    /// emitted state value `V` (in inits and assignments, but NOT in `case`
    /// labels) is rendered as an anchor-coupled expression that evaluates to `V`
    /// but cannot be constant-folded. `None` falls back to bare literals.
    pub anchor: Option<&'a OpaqueAnchor>,
    /// Names of runtime values guaranteed in scope on EVERY path of the emitted
    /// machine (the "offset trick"). Read through a `typeof`-guarded coercion (see
    /// [`u32_of`]) safe for any runtime type. For a seeded subset of emitted
    /// successor values the renderer rewrites the bare state constant `TARGET` as
    /// `TARGET <op> (<self-cancelling zero in v>)` for one of these `v`.
    ///
    /// These are the ORIGINAL binding `Ident`s (carrying their resolver
    /// `SyntaxContext`), NOT freshly minted names: the post-flatten renamer keys on
    /// `(sym, ctxt)`, so a reference cloned from the real binding is renamed in
    /// lockstep with it.
    pub inscope_vars: &'a [Ident],
    /// Probability in `[0, 1]` that an eligible emitted successor value receives
    /// the data-dependent offset trick. `0.0` disables it.
    pub data_dep_rate: f32,
}

impl<'a> RenderOpts<'a> {
    /// Convenience constructor for the single-state-var form with bare literals.
    #[cfg(test)]
    pub fn single(labels: &'a [usize], state_name: String) -> Self {
        RenderOpts {
            labels,
            dispatch: Dispatch::Single { name: state_name },
            anchor: None,
            inscope_vars: &[],
            data_dep_rate: 0.0,
        }
    }
}

/// Produces the expression for an emitted state `value`: opaque when an anchor is
/// available, otherwise a bare numeric literal. State values are always `< K*K`,
/// so the `u32` cast is lossless.
fn value_expr(rng: &mut Rng, anchor: Option<&OpaqueAnchor>, value: usize) -> Expr {
    match anchor {
        Some(a) => opaque_u32(rng, a, value as u32),
        None => num_lit(value),
    }
}

/// Configuration for the data-dependent ("offset trick") transition rewrite.
struct DataDep<'a> {
    vars: &'a [Ident],
    rate: f32,
}

impl<'a> DataDep<'a> {
    fn disabled() -> Self {
        DataDep {
            vars: &[],
            rate: 0.0,
        }
    }

    fn enabled(&self) -> bool {
        !self.vars.is_empty() && self.rate > 0.0
    }
}

/// Number of distinct provably-zero, data-dependent offset shapes.
const ZERO_OFFSET_FORMS: usize = 8;

/// `(typeof v === 'number' ? v >>> 0 : 0)` — a TYPE-SAFE u32 read of an in-scope
/// live value. Clones the original binding ident so its resolver `SyntaxContext`
/// is preserved and the post-flatten renamer rewrites it in lockstep.
///
/// The `typeof` guard is load-bearing for CORRECTNESS, not obfuscation: the
/// operands are arbitrary function-hoisted `var` bindings (may hold ANY runtime
/// value). `typeof` never throws and never coerces, so the guard is always safe.
fn u32_of(var: &Ident) -> Expr {
    let is_number = Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op: BinaryOp::EqEqEq,
        left: Box::new(Expr::Unary(UnaryExpr {
            span: DUMMY_SP,
            op: UnaryOp::TypeOf,
            arg: Box::new(Expr::Ident(var.clone())),
        })),
        right: Box::new(Expr::Lit(Lit::Str(Str {
            span: DUMMY_SP,
            value: "number".into(),
            raw: None,
        }))),
    });
    let shifted = Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op: BinaryOp::ZeroFillRShift,
        left: Box::new(Expr::Ident(var.clone())),
        right: Box::new(num_lit(0)),
    });
    Expr::Paren(ParenExpr {
        span: DUMMY_SP,
        expr: Box::new(Expr::Cond(CondExpr {
            span: DUMMY_SP,
            test: Box::new(is_number),
            cons: Box::new(shifted),
            alt: Box::new(num_lit(0)),
        })),
    })
}

/// Wraps `e` in parentheses.
fn paren(e: Expr) -> Expr {
    Expr::Paren(ParenExpr {
        span: DUMMY_SP,
        expr: Box::new(e),
    })
}

/// `op` applied as a binary expression to `(l) op (r)`, both operands parenthesized.
fn bin(op: BinaryOp, l: Expr, r: Expr) -> Expr {
    Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op,
        left: Box::new(paren(l)),
        right: Box::new(paren(r)),
    })
}

/// `~(e)`.
fn bit_not(e: Expr) -> Expr {
    Expr::Unary(UnaryExpr {
        span: DUMMY_SP,
        op: UnaryOp::Tilde,
        arg: Box::new(paren(e)),
    })
}

/// A pure, deterministic transform `t(w)` of `w = (v >>> 0)`. Each `kind` produces
/// a fresh expression tree (no sharing) so the two operands of a self-cancelling
/// combinator are textually identical yet independently owned.
fn w_transform(kind: usize, var: &Ident) -> Expr {
    let w = u32_of(var);
    match kind {
        0 => w,
        1 => bin(BinaryOp::LShift, w, num_lit(1)),
        2 => bin(BinaryOp::ZeroFillRShift, w, num_lit(1)),
        3 => bit_not(w),
        4 => bin(BinaryOp::BitAnd, u32_of(var), w),
        _ => bin(BinaryOp::Mul, w, num_lit(3)),
    }
}

/// A behavior-preserving additive offset of `0` that is *data-dependent* on a live
/// program value `v` — provably `0` only via a non-trivial self-cancelling
/// invariant (`x ^ x`, `x - x`, `x & ~x` over a pure transform of `v >>> 0`).
fn zero_offset(form: usize, var: &Ident) -> Expr {
    const FORMS: [(BinaryOp, usize, bool); ZERO_OFFSET_FORMS] = [
        (BinaryOp::BitXor, 0, false), // (w) ^ (w)
        (BinaryOp::Sub, 0, false),    // (w) - (w)
        (BinaryOp::BitAnd, 0, true),  // (w) & ~(w)
        (BinaryOp::BitXor, 1, false), // (w<<1) ^ (w<<1)
        (BinaryOp::Sub, 2, false),    // (w>>>1) - (w>>>1)
        (BinaryOp::BitXor, 3, false), // (~w) ^ (~w)
        (BinaryOp::Sub, 3, false),    // (~w) - (~w)
        (BinaryOp::Sub, 5, false),    // (w*3) - (w*3)
    ];
    let (op, kind, negate_right) = FORMS[form % ZERO_OFFSET_FORMS];
    let left = w_transform(kind, var);
    let right = if negate_right {
        bit_not(w_transform(kind, var))
    } else {
        w_transform(kind, var)
    };
    bin(op, left, right)
}

/// The set of wrapper operators `apply_offset_trick` may combine `base` with the
/// provably-zero offset under. Each is the IDENTITY when the right operand is
/// exactly `0` and `base` is a small non-negative int `< 2^31`.
const OFFSET_WRAPPER_OPS: [BinaryOp; 4] = [
    BinaryOp::BitXor,
    BinaryOp::Add,
    BinaryOp::Sub,
    BinaryOp::BitOr,
];

/// Wraps `base` as `base <op> (<self-cancelling zero in v>)`, with `<op>` one of
/// [`OFFSET_WRAPPER_OPS`]. Behavior is preserved (the offset is `0`) while the
/// successor becomes data-dependent on a live value `v` and non-foldable.
fn apply_offset_trick(base: Expr, form: usize, wrapper: usize, var: &Ident) -> Expr {
    let op = OFFSET_WRAPPER_OPS[wrapper % OFFSET_WRAPPER_OPS.len()];
    Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op,
        left: Box::new(Expr::Paren(ParenExpr {
            span: DUMMY_SP,
            expr: Box::new(base),
        })),
        right: Box::new(Expr::Paren(ParenExpr {
            span: DUMMY_SP,
            expr: Box::new(zero_offset(form, var)),
        })),
    })
}

/// Produces the expression for an emitted state `value`, opportunistically
/// applying the offset trick. The per-value decision is drawn from `rng` in a
/// fixed order, so output stays deterministic.
fn value_expr_dd(
    rng: &mut Rng,
    anchor: Option<&OpaqueAnchor>,
    dd: &DataDep<'_>,
    value: usize,
) -> Expr {
    let base = value_expr(rng, anchor, value);
    if !dd.enabled() {
        return base;
    }
    // Draw the apply-decision FIRST, then (only if applying) the var index, the
    // zero-offset form, and the wrapper, so the RNG-consumption pattern is a fixed
    // function of (seed, emission order).
    let apply = rng.random_f32_unit() < dd.rate;
    if !apply {
        return base;
    }
    let idx = rng.pick(dd.vars.len());
    let form = rng.pick(ZERO_OFFSET_FORMS);
    let wrapper = rng.pick(OFFSET_WRAPPER_OPS.len());
    apply_offset_trick(base, form, wrapper, &dd.vars[idx])
}

fn state_ident(name: &str) -> Ident {
    Ident::new(name.into(), DUMMY_SP, SyntaxContext::empty())
}

fn num_lit(n: usize) -> Expr {
    Expr::Lit(Lit::Num(Number {
        span: DUMMY_SP,
        value: n as f64,
        raw: None,
    }))
}

/// `name = <expr>;`
fn assign_stmt(name: &str, value: Expr) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(Expr::Assign(AssignExpr {
            span: DUMMY_SP,
            op: AssignOp::Assign,
            left: AssignTarget::Simple(SimpleAssignTarget::Ident(BindingIdent {
                id: state_ident(name),
                type_ann: None,
            })),
            right: Box::new(value),
        })),
    })
}

fn break_stmt() -> Stmt {
    Stmt::Break(BreakStmt {
        span: DUMMY_SP,
        label: None,
    })
}

/// Statements that move the machine to emitted state value `value`. One assignment
/// for single-var dispatch, two (high/low) for two-var dispatch.
fn set_state(
    rng: &mut Rng,
    anchor: Option<&OpaqueAnchor>,
    dd: &DataDep<'_>,
    dispatch: &Dispatch,
    value: usize,
) -> Vec<Stmt> {
    match dispatch {
        Dispatch::Single { name } => {
            let v = value_expr_dd(rng, anchor, dd, value);
            vec![assign_stmt(name, v)]
        }
        Dispatch::Two { name1, name2, k } => {
            // Evaluate hi then lo in a fixed order for determinism.
            let hi = value_expr_dd(rng, anchor, dd, value / k);
            let lo = value_expr_dd(rng, anchor, dd, value % k);
            vec![assign_stmt(name1, hi), assign_stmt(name2, lo)]
        }
    }
}

/// Builds the statement tail for a block's edge. Transition targets are mapped
/// through `labels` so the emitted state value differs from the block id.
fn edge_tail(
    rng: &mut Rng,
    anchor: Option<&OpaqueAnchor>,
    dd: &DataDep<'_>,
    dispatch: &Dispatch,
    edge: Edge,
    labels: &[usize],
) -> Vec<Stmt> {
    match edge {
        Edge::Sequential(n) => {
            let mut out = set_state(rng, anchor, dd, dispatch, labels[n]);
            out.push(break_stmt());
            out
        }
        Edge::Branch {
            cond,
            then_id,
            else_id,
        } => {
            // Then-branch state first, then else-branch, for deterministic RNG use.
            let then_stmts = set_state(rng, anchor, dd, dispatch, labels[then_id]);
            let else_stmts = set_state(rng, anchor, dd, dispatch, labels[else_id]);
            let set_then = Stmt::Block(BlockStmt {
                span: DUMMY_SP,
                ctxt: SyntaxContext::empty(),
                stmts: then_stmts,
            });
            let set_else = Stmt::Block(BlockStmt {
                span: DUMMY_SP,
                ctxt: SyntaxContext::empty(),
                stmts: else_stmts,
            });
            vec![
                Stmt::If(IfStmt {
                    span: DUMMY_SP,
                    test: cond,
                    cons: Box::new(set_then),
                    alt: Some(Box::new(set_else)),
                }),
                break_stmt(),
            ]
        }
        Edge::Return(arg) => vec![Stmt::Return(ReturnStmt {
            span: DUMMY_SP,
            arg,
        })],
        Edge::Throw(arg) => vec![Stmt::Throw(ThrowStmt {
            span: DUMMY_SP,
            arg,
        })],
    }
}

/// Number of candidate single-variable discriminant envelopes (`name` / `name|0`).
const SINGLE_DISC_FORMS: usize = 2;

/// Number of candidate two-variable discriminant recombination shapes. The first
/// [`TWO_DISC_BASE_FORMS`] are always valid; the remaining shift-based forms are
/// only valid when `k` is a power of two (the selector falls back otherwise).
const TWO_DISC_FORMS: usize = 5;
const TWO_DISC_BASE_FORMS: usize = 3;

/// `| 0` envelope around `e`.
fn or_zero(e: Expr) -> Expr {
    Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op: BinaryOp::BitOr,
        left: Box::new(e),
        right: Box::new(num_lit(0)),
    })
}

/// `name1 * k`.
fn name1_times_k(name1: &str, k: usize) -> Expr {
    Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op: BinaryOp::Mul,
        left: Box::new(Expr::Ident(state_ident(name1))),
        right: Box::new(num_lit(k)),
    })
}

/// `name1 << log2(k)` (valid only when `k` is a power of two).
fn name1_shl(name1: &str, log2k: u32) -> Expr {
    Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op: BinaryOp::LShift,
        left: Box::new(Expr::Ident(state_ident(name1))),
        right: Box::new(num_lit(log2k as usize)),
    })
}

/// `a <op> b`.
fn bin_raw(op: BinaryOp, a: Expr, b: Expr) -> Expr {
    Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op,
        left: Box::new(a),
        right: Box::new(b),
    })
}

/// `log2(k)` if `k` is a power of two and `k >= 1`, else `None`.
fn log2_pow2(k: usize) -> Option<u32> {
    if k != 0 && k.is_power_of_two() {
        Some(k.trailing_zeros())
    } else {
        None
    }
}

/// The two-variable discriminant for the chosen `form`. Every shape evaluates to
/// EXACTLY `name1*k + name2 == V` under the invariants `name1 >= 0`,
/// `0 <= name2 < k`, `V < 2^31`.
fn two_discriminant(name1: &str, name2: &str, k: usize, form: usize) -> Expr {
    let n2 = || Expr::Ident(state_ident(name2));
    match log2_pow2(k) {
        Some(p) => match form % TWO_DISC_FORMS {
            0 => or_zero(bin_raw(BinaryOp::Add, name1_times_k(name1, k), n2())),
            1 => or_zero(bin_raw(BinaryOp::Add, n2(), name1_times_k(name1, k))),
            2 => or_zero(bin_raw(
                BinaryOp::Add,
                paren(or_zero(name1_times_k(name1, k))),
                n2(),
            )),
            3 => or_zero(bin_raw(BinaryOp::Add, paren(name1_shl(name1, p)), n2())),
            _ => bin_raw(BinaryOp::BitOr, name1_shl(name1, p), n2()),
        },
        None => match form % TWO_DISC_BASE_FORMS {
            0 => or_zero(bin_raw(BinaryOp::Add, name1_times_k(name1, k), n2())),
            1 => or_zero(bin_raw(BinaryOp::Add, n2(), name1_times_k(name1, k))),
            _ => or_zero(bin_raw(
                BinaryOp::Add,
                paren(or_zero(name1_times_k(name1, k))),
                n2(),
            )),
        },
    }
}

/// The switch discriminant expression for a dispatch mode. `disc_form` selects a
/// (semantically identical) recombination shape, drawn once per render.
///
/// When `coupling` is `Some`, the recombined value `V` is additionally mixed with
/// an anchor-coupled, provably-`0` offset so the dispatcher cannot be statically
/// resolved without RUNNING the anchor. The mix is an identity on `V`.
fn discriminant(dispatch: &Dispatch, disc_form: usize, coupling: Option<DiscCoupling>) -> Expr {
    let base = match dispatch {
        Dispatch::Single { name } => {
            let id = Expr::Ident(state_ident(name));
            match disc_form % SINGLE_DISC_FORMS {
                0 => id,
                _ => or_zero(id),
            }
        }
        Dispatch::Two { name1, name2, k } => two_discriminant(name1, name2, *k, disc_form),
    };
    match coupling {
        Some(c) => couple_discriminant(base, c),
        None => base,
    }
}

/// Parameters for the decode-coupled discriminant offset, built once per render
/// (only when an anchor is available). The offset itself is an
/// [`crate::opaque::opaque_u32`] of the VALUE `0`, so it evaluates to exactly `0`
/// at runtime but cannot be folded.
struct DiscCoupling {
    /// Index into [`OFFSET_WRAPPER_OPS`]; the wrapper op is the identity when the
    /// right operand is exactly `0` and `base` is a non-negative state int `< 2^31`.
    wrapper: usize,
    /// The anchor-coupled expression that evaluates to exactly `0` at runtime.
    opaque_zero: Expr,
}

/// Builds the discriminant coupling (when an anchor is available), drawing the
/// wrapper-op choice then the anchor-coupled provably-`0` offset from `rng` in a
/// fixed order. Returns `None` when `anchor` is `None`, in which case the
/// discriminant is emitted exactly as before (no RNG consumed).
fn build_disc_coupling(rng: &mut Rng, anchor: Option<&OpaqueAnchor>) -> Option<DiscCoupling> {
    let a = anchor?;
    let wrapper = rng.pick(OFFSET_WRAPPER_OPS.len());
    let opaque_zero = opaque_u32(rng, a, 0);
    Some(DiscCoupling {
        wrapper,
        opaque_zero,
    })
}

/// Wraps the recombined discriminant `base` as `base <op> (opaque_zero)` where
/// `op` is the identity for an exact-`0` right operand.
fn couple_discriminant(base: Expr, c: DiscCoupling) -> Expr {
    let op = OFFSET_WRAPPER_OPS[c.wrapper % OFFSET_WRAPPER_OPS.len()];
    Expr::Bin(BinExpr {
        span: DUMMY_SP,
        op,
        left: Box::new(Expr::Paren(ParenExpr {
            span: DUMMY_SP,
            expr: Box::new(base),
        })),
        right: Box::new(Expr::Paren(ParenExpr {
            span: DUMMY_SP,
            expr: Box::new(c.opaque_zero),
        })),
    })
}

/// The initializing `var` declaration(s) that set the machine to `value`.
fn init_decl(
    rng: &mut Rng,
    anchor: Option<&OpaqueAnchor>,
    dispatch: &Dispatch,
    value: usize,
) -> Stmt {
    let mut decls: Vec<VarDeclarator> = Vec::new();
    let mut push = |name: &str, init: Expr| {
        decls.push(VarDeclarator {
            span: DUMMY_SP,
            name: Pat::Ident(BindingIdent {
                id: state_ident(name),
                type_ann: None,
            }),
            init: Some(Box::new(init)),
            definite: false,
        });
    };
    match dispatch {
        Dispatch::Single { name } => {
            let v = value_expr(rng, anchor, value);
            push(name, v);
        }
        Dispatch::Two { name1, name2, k } => {
            let hi = value_expr(rng, anchor, value / k);
            let lo = value_expr(rng, anchor, value % k);
            push(name1, hi);
            push(name2, lo);
        }
    }
    Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        kind: VarDeclKind::Var,
        declare: false,
        decls,
    })))
}

/// Renders the full `while (1) switch (state) { ... }` block.
///
/// `blocks` are indexed by state ID; `entry` is the starting state. `exit` is the
/// synthetic terminal block (already a `return;`), rendered like any other.
pub fn render(
    rng: &mut Rng,
    blocks: Vec<BasicBlock>,
    entry: usize,
    _exit: usize,
    opts: &RenderOpts<'_>,
) -> FunctionBody {
    let labels = opts.labels;
    let dispatch = &opts.dispatch;
    let anchor = opts.anchor;
    let dd = DataDep {
        vars: opts.inscope_vars,
        rate: opts.data_dep_rate,
    };
    // Draw the dispatcher discriminant recombination shape FIRST — a single,
    // deterministic RNG draw before any other emission. Drawing the max of both
    // form-space sizes keeps the consumed-RNG count independent of dispatch mode.
    let disc_form = rng.pick(SINGLE_DISC_FORMS.max(TWO_DISC_FORMS));
    // Build the init first. The init runs exactly once before the loop, so it keeps
    // the plain constant/opaque form — the offset trick targets in-loop transitions.
    let init = init_decl(rng, anchor, dispatch, labels[entry]);

    let num_live = blocks.len();
    let mut cases: Vec<SwitchCase> = Vec::with_capacity(labels.len());
    for (id, block) in blocks.into_iter().enumerate() {
        let mut body = block.stmts;
        let tail = edge_tail(rng, anchor, &dd, dispatch, block.edge, labels);
        body.extend(tail);
        // Emit the statements directly into the case clause — do NOT wrap them in a
        // nested `{ ... }` block. All cases must share the single switch-block scope
        // so that hoisted function declarations remain visible across cases.
        cases.push(SwitchCase {
            span: DUMMY_SP,
            test: Some(Box::new(num_lit(labels[id]))),
            cons: body,
        });
    }

    // Dead states: labels[num_live..] are never targeted by any live transition.
    // Each transitions unconditionally back to a LIVE state (the entry) and breaks.
    let dead_dd = if dd.enabled() {
        // Force the trick on every dead-state transition (rate 1.0).
        DataDep {
            vars: dd.vars,
            rate: 1.0,
        }
    } else {
        DataDep::disabled()
    };
    for dead_id in num_live..labels.len() {
        let mut body = set_state(rng, anchor, &dead_dd, dispatch, labels[entry]);
        body.push(break_stmt());
        cases.push(SwitchCase {
            span: DUMMY_SP,
            test: Some(Box::new(num_lit(labels[dead_id]))),
            cons: body,
        });
    }

    // After all cases/inits are emitted, build the decode-coupled discriminant
    // offset. This consumes new RNG ONLY when an anchor is present.
    let coupling = build_disc_coupling(rng, anchor);
    let switch = Stmt::Switch(SwitchStmt {
        body_ctxt: Default::default(),
        span: DUMMY_SP,
        discriminant: Box::new(discriminant(dispatch, disc_form, coupling)),
        cases,
    });

    let while_stmt = Stmt::While(WhileStmt {
        span: DUMMY_SP,
        test: Box::new(Expr::Lit(Lit::Num(Number {
            span: DUMMY_SP,
            value: 1.0,
            raw: None,
        }))),
        body: Box::new(switch),
    });

    FunctionBody {
        span: DUMMY_SP,
        stmts: vec![init, while_stmt],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::cfflatten::cfg;
    use crate::passes::cfflatten::test_support::{emit_block, parse_body};

    /// Identity labels (block id == state value) for tests not exercising shuffle.
    fn identity_labels(n: usize) -> Vec<usize> {
        (0..n).collect()
    }

    fn rng(seed: u64) -> Rng {
        Rng::for_pass(seed, "cfflatten")
    }

    #[test]
    fn renders_while_switch_case() {
        let mut r = rng(1);
        let (blocks, entry, exit) =
            build_blocks("a = 1; if (a > 0) { a = 2; } else { a = 3; } b = a;");
        let labels = identity_labels(blocks.len());
        let block = render(
            &mut r,
            blocks,
            entry,
            exit,
            &RenderOpts::single(&labels, "_0xst".into()),
        );
        let out = emit_block(&block.stmts);
        assert!(out.contains("while"), "missing while: {out}");
        assert!(out.contains("switch"), "missing switch: {out}");
        assert!(out.contains("case "), "missing case: {out}");
        assert!(out.contains("_0xst"), "missing state var: {out}");
    }

    #[test]
    fn return_value_is_preserved() {
        let mut r = rng(1);
        let (blocks, entry, exit) = build_blocks("a = 1; b = 2; return a + b;");
        let labels = identity_labels(blocks.len());
        let block = render(
            &mut r,
            blocks,
            entry,
            exit,
            &RenderOpts::single(&labels, "_0xst".into()),
        );
        let out = emit_block(&block.stmts);
        assert!(out.contains("return"), "missing return: {out}");
    }

    #[test]
    fn two_state_vars_emit_both_names_and_computed_key() {
        let mut r = rng(1);
        let (blocks, entry, exit) =
            build_blocks("a = 1; if (a > 0) { a = 2; } else { a = 3; } b = a;");
        let labels = identity_labels(blocks.len());
        let opts = RenderOpts {
            labels: &labels,
            dispatch: Dispatch::Two {
                name1: "_s1".into(),
                name2: "_s2".into(),
                k: 3,
            },
            anchor: None,
            inscope_vars: &[],
            data_dep_rate: 0.0,
        };
        let block = render(&mut r, blocks, entry, exit, &opts);
        let out = emit_block(&block.stmts);
        assert!(out.contains("_s1"), "missing s1: {out}");
        assert!(out.contains("_s2"), "missing s2: {out}");
        assert!(out.contains("switch"), "missing switch: {out}");
    }

    #[test]
    fn opaque_transitions_reference_anchor_and_resist_folding() {
        let mut r = rng(1);
        let (blocks, entry, exit) =
            build_blocks("a = 1; if (a > 0) { a = 2; } else { a = 3; } b = a;");
        let labels = identity_labels(blocks.len());
        let anchor = OpaqueAnchor::decoder("_core");
        let opts = RenderOpts {
            labels: &labels,
            dispatch: Dispatch::Single {
                name: "_0xst".into(),
            },
            anchor: Some(&anchor),
            inscope_vars: &[],
            data_dep_rate: 0.0,
        };
        let block = render(&mut r, blocks, entry, exit, &opts);
        let out = emit_block(&block.stmts);
        assert!(
            out.contains("_core("),
            "opaque transition must call anchor: {out}"
        );
        assert!(
            out.contains(">>>"),
            "opaque transition must use >>>0: {out}"
        );
    }

    #[test]
    fn dead_states_emit_extra_cases_transitioning_to_entry() {
        let mut r = rng(1);
        let (blocks, entry, exit) = build_blocks("a = 1; b = 2; return a + b;");
        let num_live = blocks.len();
        let total = num_live + 2;
        let labels: Vec<usize> = (0..total).collect();
        let opts = RenderOpts {
            labels: &labels,
            dispatch: Dispatch::Single {
                name: "_0xst".into(),
            },
            anchor: None,
            inscope_vars: &[],
            data_dep_rate: 0.0,
        };
        let block = render(&mut r, blocks, entry, exit, &opts);
        let switch = block.stmts.iter().find_map(|s| match s {
            Stmt::While(w) => match &*w.body {
                Stmt::Switch(sw) => Some(sw),
                _ => None,
            },
            _ => None,
        });
        let sw = switch.expect("switch present");
        assert_eq!(
            sw.cases.len(),
            total,
            "must emit a case per live + dead state"
        );
    }

    fn build_blocks(src: &str) -> (Vec<cfg::BasicBlock>, usize, usize) {
        cfg::build(parse_body(src).stmts)
    }

    // -----------------------------------------------------------------------
    // Self-cancelling, data-dependent zero offsets
    // -----------------------------------------------------------------------

    /// A plain `var` ident (empty resolver context) usable as the in-scope value.
    fn var(name: &str) -> Ident {
        Ident::new(name.into(), DUMMY_SP, SyntaxContext::empty())
    }

    /// Render a bare expression to source by wrapping it in a one-statement block.
    fn render_expr(e: Expr) -> String {
        let b = BlockStmt {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            stmts: vec![Stmt::Expr(ExprStmt {
                span: DUMMY_SP,
                expr: Box::new(e),
            })],
        };
        emit_block(&b.stmts)
    }

    /// The offset must NOT carry a `% 1` folding signature, and all
    /// `ZERO_OFFSET_FORMS` shapes must render structurally distinct.
    #[test]
    fn zero_offset_forms_are_distinct_and_carry_no_mod1() {
        let v = var("_n");
        let mut shapes = std::collections::HashSet::new();
        for form in 0..ZERO_OFFSET_FORMS {
            let s = render_expr(zero_offset(form, &v));
            assert!(!s.contains('%'), "offset must not use `% 1`: {s}");
            assert!(s.contains("_n"), "offset must reference the live var: {s}");
            shapes.insert(s);
        }
        assert_eq!(
            shapes.len(),
            ZERO_OFFSET_FORMS,
            "each offset form must render distinctly: {shapes:?}"
        );
    }

    /// Strip the wrapping `{ … ; }` block to a bare expression fragment.
    fn expr_src(e: Expr) -> String {
        let s = render_expr(e);
        let s = s.trim();
        let s = s.strip_prefix('{').unwrap_or(s);
        let s = s.strip_suffix('}').unwrap_or(s).trim();
        s.strip_suffix(';').unwrap_or(s).trim().to_string()
    }

    /// Every offset form evaluates to exactly `0` for a spread of values.
    #[test]
    fn zero_offset_forms_evaluate_to_zero() {
        use mangler_testkit::eval::assert_behaviorally_equal;
        let vals = [
            "0",
            "1",
            "2",
            "255",
            "65535",
            "2147483647",
            "2147483648",
            "4294967295",
            "-1",
            "-2147483648",
            "3.5",
            "1e9",
        ];
        let v = var("_n");
        for form in 0..ZERO_OFFSET_FORMS {
            let inner = expr_src(zero_offset(form, &v));
            for val in &vals {
                assert_behaviorally_equal(
                    "globalThis.__out=String(0);",
                    &format!("var _n={val};globalThis.__out=String(({inner}));"),
                );
            }
        }
    }

    /// The wrapped transition `TARGET ^ offset` must equal `TARGET` for every
    /// offset form and a range of live-var values.
    #[test]
    fn offset_trick_preserves_target() {
        use mangler_testkit::eval::assert_behaviorally_equal;
        let v = var("_n");
        for target in [0usize, 1, 2, 7, 42, 255] {
            for form in 0..ZERO_OFFSET_FORMS {
                for wrapper in 0..OFFSET_WRAPPER_OPS.len() {
                    let wrapped = apply_offset_trick(num_lit(target), form, wrapper, &v);
                    let inner = expr_src(wrapped);
                    for val in ["0", "5", "4294967295", "-1"] {
                        assert_behaviorally_equal(
                            &format!("globalThis.__out=String({target});"),
                            &format!("var _n={val};globalThis.__out=String(({inner}));"),
                        );
                    }
                }
            }
        }
    }

    /// Every two-var discriminant shape must evaluate to exactly
    /// `name1*k + name2 == V`, for both power-of-two and non-power-of-two `k`.
    #[test]
    fn two_discriminant_forms_equal_name1_k_plus_name2() {
        use mangler_testkit::eval::assert_behaviorally_equal;
        let cases = [
            (3usize, 0usize, 0usize),
            (3, 2, 1),
            (3, 5, 2),
            (4, 0, 3),
            (4, 7, 1),
            (5, 3, 4),
            (8, 0, 7),
            (8, 6, 5),
            (8, 1000, 7),
        ];
        for (k, n1, n2) in cases {
            let expected = n1 * k + n2;
            for form in 0..TWO_DISC_FORMS {
                let inner = expr_src(two_discriminant("_a", "_b", k, form));
                assert_behaviorally_equal(
                    &format!("globalThis.__out=String({expected});"),
                    &format!("var _a={n1},_b={n2};globalThis.__out=String(({inner}));"),
                );
            }
        }
    }

    /// Multiple distinct two-var discriminant SHAPES must appear across seeds.
    #[test]
    fn discriminant_shapes_vary_across_seeds() {
        let mut shapes = std::collections::HashSet::new();
        for seed in 0..64u64 {
            let mut r = rng(seed);
            let (blocks, entry, exit) =
                build_blocks("a = 1; if (a > 0) { a = 2; } else { a = 3; } b = a;");
            let labels = identity_labels(blocks.len());
            let opts = RenderOpts {
                labels: &labels,
                dispatch: Dispatch::Two {
                    name1: "_s1".into(),
                    name2: "_s2".into(),
                    k: 4,
                },
                anchor: None,
                inscope_vars: &[],
                data_dep_rate: 0.0,
            };
            let block = render(&mut r, blocks, entry, exit, &opts);
            let out = emit_block(&block.stmts);
            for line in out.lines() {
                if line.contains("switch") {
                    shapes.insert(line.trim().to_string());
                }
            }
        }
        assert!(
            shapes.len() >= 2,
            "expected multiple distinct discriminant shapes across seeds, got {shapes:?}"
        );
    }

    /// The coupled discriminant `base <op> opaque_zero` must equal `base` for
    /// EVERY wrapper op and EVERY decoded string the stub returns.
    #[test]
    fn coupled_discriminant_equals_base_for_all_decoded_data() {
        use mangler_testkit::eval::assert_behaviorally_equal;
        let rets = ["\"\"", "\"a\"", "\"abc\"", "\"\\uD83D\\uDE00\""];
        let anchor = OpaqueAnchor::decoder("_core");
        for base in [0usize, 1, 2, 7, 42, 255, 1000, 65535] {
            for wrapper in 0..OFFSET_WRAPPER_OPS.len() {
                for seed in 0..16u64 {
                    let mut r = rng(seed);
                    let opaque_zero = opaque_u32(&mut r, &anchor, 0);
                    let coupled = couple_discriminant(
                        num_lit(base),
                        DiscCoupling {
                            wrapper,
                            opaque_zero,
                        },
                    );
                    let inner = expr_src(coupled);
                    for ret in &rets {
                        assert_behaviorally_equal(
                            &format!("globalThis.__out=String({base});"),
                            &format!(
                                "var _core=function(_i){{return {ret};}};globalThis.__out=String(({inner}));"
                            ),
                        );
                    }
                }
            }
        }
    }

    /// With an anchor present, the switch HEAD must reference the anchor `core(`
    /// across seeds, and at least one seed's head must be DATA-dependent.
    #[test]
    fn discriminant_is_decode_coupled_with_anchor() {
        let mut saw_core = 0usize;
        let mut saw_charcode = false;
        let n = 64u64;
        let anchor = OpaqueAnchor::decoder("_core");
        for seed in 0..n {
            let mut r = rng(seed);
            let (blocks, entry, exit) =
                build_blocks("a = 1; if (a > 0) { a = 2; } else { a = 3; } b = a;");
            let labels = identity_labels(blocks.len());
            let opts = RenderOpts {
                labels: &labels,
                dispatch: Dispatch::Single {
                    name: "_0xst".into(),
                },
                anchor: Some(&anchor),
                inscope_vars: &[],
                data_dep_rate: 0.0,
            };
            let block = render(&mut r, blocks, entry, exit, &opts);
            let out = emit_block(&block.stmts);
            let head = out
                .lines()
                .find(|l| l.contains("switch"))
                .unwrap_or("")
                .to_string();
            if head.contains("_core(") {
                saw_core += 1;
            }
            if head.contains("charCodeAt") {
                saw_charcode = true;
            }
        }
        assert_eq!(
            saw_core, n as usize,
            "every anchor-enabled discriminant head must reference the anchor"
        );
        assert!(
            saw_charcode,
            "expected at least one decode-coupled (charCodeAt) discriminant head across seeds"
        );
    }

    /// Same seed ⇒ byte-identical decode-coupled machine.
    #[test]
    fn coupled_discriminant_is_deterministic() {
        let anchor = OpaqueAnchor::decoder("_core");
        let render_once = || {
            let mut r = rng(999);
            let (blocks, entry, exit) =
                build_blocks("a = 1; if (a > 0) { a = 2; } else { a = 3; } b = a;");
            let labels = identity_labels(blocks.len());
            let opts = RenderOpts {
                labels: &labels,
                dispatch: Dispatch::Single {
                    name: "_0xst".into(),
                },
                anchor: Some(&anchor),
                inscope_vars: &[],
                data_dep_rate: 0.0,
            };
            emit_block(&render(&mut r, blocks, entry, exit, &opts).stmts)
        };
        assert_eq!(render_once(), render_once());
    }
}
