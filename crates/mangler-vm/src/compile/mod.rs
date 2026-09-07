//! AST → bytecode compiler (bail-to-safe), decomposed per construct family.
//!
//! Compiles one eligible function body into a [`Compiled`] chunk — a flat
//! stack-machine program ([`Instr`] stream + [`Const`] pool) that the generated
//! interpreter ([`crate::emit`]) executes. The entry points are [`compile_body`] /
//! [`compile_body_boxed`] / [`compile_body_with_plan`]; each returns `Err(reason)`
//! to bail (the function then stays un-virtualized — **a bail is never a
//! miscompile**).
//!
//! ## Flat-slot frame model
//! The VM has no scope objects: every binding lives in a numbered slot of a single
//! flat local array `L`. The slot layout for a frame is contiguous and ordered:
//!   * params and body locals (`var` / hoisted fn-decl / block-scoped `let`/`const`)
//!     occupy slots `< cap_floor`;
//!   * an anonymous temp pool (`temp_base..cap_floor`) sits after locals;
//!   * captures (free upvalues, allocated lazily on first reference) are the LAST
//!     contiguous slots, `>= cap_floor`.
//!
//! `cap_floor` is frozen once params + locals are allocated, so the `< cap_floor`
//! test cleanly separates "param/local" from "capture", and `cap_start =
//! slots - captures.len()` is where the thunk threads the captured values in.
//!
//! ## Two-pass slot allocation (single source of truth)
//! Because virtualize runs pre-resolver, the AST carries NO resolver marks; slot
//! identity is instead pinned to source position. A `DeclCollector` pre-pass walks
//! the body ONCE and allocates exactly one slot per block-scoped binding, keyed by
//! the declaration ident's `BytePos` (`decl_slots`). Emission then never invents
//! slots: on entering a block / catch / for-head it looks each binding's slot up by
//! `BytePos` and populates a fresh lexical scope frame (`scopes`, walked
//! innermost-first by `lookup`/`resolve`). The two passes share `decl_slots` so
//! they can never disagree on which slot a name owns.
//!
//! ## Decomposition
//! The compiler is split per construct family: this module holds the frame model
//! (`Cx`), slot allocation (`DeclCollector`), and the entry points; `stmt` holds
//! the statement compiler; `expr` the expression compiler; and `destructure` the
//! destructuring / spread / iterator helpers. All the `emit_*` free functions take
//! `&mut Cx` and are `pub(crate)` so they compose across the submodules exactly as
//! the legacy single-file compiler did.

use std::collections::HashMap;

use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

use crate::cells;
use crate::chunk::{ChildChunk, Compiled, Const};
use crate::isa::Instr;

/// Compile-time options that steer the Phase-3 native-closure escape hatch (§4).
/// Threaded from the virtualize pass (which owns the `--virtualize-exclude` glob)
/// down into [`emit_nested_closure`], where the divert decision is made. The
/// default (`exclude: None`, `divert_ineligible: false`) is byte-for-byte the
/// pre-Phase-3 behavior: a nested fn always becomes a child chunk and an
/// ineligible one bails the parent.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompileOptions<'a> {
    /// Name-glob of nested functions to KEEP NATIVE (run as a native closure
    /// inside the VM frame). `None` = match nothing.
    pub exclude: Option<&'a str>,
    /// When true, a nested function that is async/generator/`"use strict"`/
    /// structurally-ineligible/otherwise un-virtualizable is diverted to a native
    /// closure instead of bailing the whole parent (coverage maximization, §4.1).
    /// When false (the default), such a nested fn bails the parent as before.
    pub divert_ineligible: bool,
    /// Captures are live property descriptors supplied by the calling thunk.
    pub live_captures: bool,
    /// The native wrapper already initialized parameters and owns arguments.
    pub native_parameters: bool,
}

pub(crate) mod destructure;
pub(crate) mod expr;
pub(crate) mod native;
pub(crate) mod stmt;

// Re-export the construct-family emit entry points used across submodules and by
// the parent (the names the legacy single-file compiler exposed at module scope).
pub(crate) use destructure::*;
pub(crate) use expr::*;
pub(crate) use stmt::*;

/// The kind of control-flow frame on the frame stack. `Loop` is pushed by the
/// loop arms, `Switch` by `emit_switch`, and `Block` by the labeled-statement arm
/// for a labeled non-loop construct (so `break label` can target its end).
pub(crate) enum FrameKind {
    Loop,
    Switch,
    Block,
}

/// A control-flow frame tracking the pending break/continue jumps that must be
/// patched when the construct's exit / continuation point is known.
pub(crate) struct Frame {
    pub(crate) kind: FrameKind,
    /// The label attached to the construct, if any (e.g. `outer: for(...)`).
    /// Set from `pending_label` (loops) or directly (labeled blocks); `None` for
    /// an unlabeled construct.
    pub(crate) label: Option<String>,
    /// The live `PushHandler` count a `break` to this frame must unwind down to.
    /// For ordinary loops this is the depth at frame entry; for `for-of` it is the
    /// depth OUTSIDE the close handler, so `break` runs the iterator close.
    pub(crate) handler_depth: u32,
    /// The live `PushHandler` count a `continue` to this frame must unwind down to.
    /// Equal to `handler_depth` for ordinary loops; for `for-of` it is one deeper
    /// (INSIDE the close handler) so `continue` does NOT close the iterator.
    pub(crate) continue_handler_depth: u32,
    pub(crate) break_jumps: Vec<usize>,
    pub(crate) continue_jumps: Vec<usize>,
}

pub(crate) struct Cx<'a> {
    pub(crate) code: Vec<Instr>,
    pub(crate) consts: Vec<Const>,
    /// Lexical scope stack of `name -> slot` frames (D3). `scopes[0]` is the
    /// **function frame** (params + function-scoped `var`s + lazily-allocated
    /// captures) and is never popped. Nested blocks / `catch` clauses / `for`-heads
    /// push a frame on entry and pop it on exit, so a block-scoped `let`/`const`/
    /// `catch` binding that shadows an outer name resolves to its own slot while in
    /// scope and the outer binding is restored on block exit. Resolution
    /// (`resolve`) walks frames innermost-first; a name found in no frame is a free
    /// capture allocated into the function frame.
    pub(crate) scopes: Vec<HashMap<String, u32>>,
    /// Authoritative slot for each **block-scoped** binding declaration, keyed by
    /// the declared identifier's span (`BytePos.0`). The `DeclCollector` pre-pass
    /// is the single source of truth: it allocates one fresh slot per block-scoped
    /// binding (v1 never recycles slots across sibling blocks) and records it here;
    /// emission looks the slot up by span when it enters the binding's block and
    /// populates the new frame — so the two passes never disagree on a slot.
    pub(crate) decl_slots: HashMap<u32, u32>,
    pub(crate) lexical_slots: HashMap<u32, bool>,
    pub(crate) initializing: bool,
    /// Names declared by a body `let`/`const`/`catch` (block-scoped) binding,
    /// collected during the `DeclCollector` pass. Used only by the default-param
    /// scope-soundness check (`default_scope_ok`) to keep its conservative bail when
    /// a default references a name the body also declares block-scoped.
    pub(crate) block_local_names: std::collections::HashSet<String>,
    /// captures in first-encounter order.
    pub(crate) captures: Vec<String>,
    /// D1 boxed-capture names: the subset of free names that the enclosing-scope
    /// pre-pass (`cells.rs`) has boxed into one-element cells `[v]`. A read of such
    /// a name compiles to `LoadCell` and a write to `StoreCell` (instead of
    /// `LoadLocal`/`StoreLocal`), so the slot holds the shared cell array and a
    /// write mutates the array the enclosing scope also holds — the mutation
    /// propagates. Names NOT in this set keep the read-only `LoadLocal` capture
    /// (no box, no regression); a write to such a name still bails `mutable_capture`.
    pub(crate) boxed_caps: std::collections::HashSet<String>,
    /// D5 in-VM boxed LOCALS: this frame's OWN bindings (param / `var` / top-level
    /// `let` / fn-decl) that are captured-and-mutated by a nested closure compiled as
    /// a child of this frame. Such a local's slot holds a one-element cell `[v]`
    /// (created by `MakeCell` at its declaration / param entry); the parent
    /// reads/writes it via `LoadCell`/`StoreCell`, and `MakeClosure` threads the
    /// SLOT VALUE (the cell array) to the child as an upvalue — so the parent VM
    /// frame and every closure over it share one mutable cell, exactly like D1 boxing
    /// but with the enclosing scope being a VM frame rather than plain JS.
    pub(crate) boxed_locals: std::collections::HashSet<String>,
    pub(crate) next_slot: u32,
    /// First slot index that may be a capture (set after param + local allocation,
    /// before body emission). Slots `< cap_floor` are params or body locals;
    /// slots `>= cap_floor` are captures. Enables O(1) `is_param_or_local` check.
    pub(crate) cap_floor: u32,
    /// Reserved anonymous temp-slot pool, sitting AFTER body locals and BEFORE
    /// captures (so captures stay the last contiguous slots). `temp_base` is the
    /// first temp slot; `temp_top` is the next free temp slot (a LIFO bump
    /// pointer). Temps are never recorded in any scope frame or `captures`.
    /// Sized by `count_temps` (the max simultaneously-live temps); 0 today since
    /// no in-scope construct allocates temps yet (for-of/for-in/destructuring
    /// targets, added by later tasks, are the only consumers).
    pub(crate) temp_base: u32,
    pub(crate) temp_top: u32,
    pub(crate) frames: Vec<Frame>,
    /// Live `PushHandler` count at the current emit point (incremented while
    /// emitting a `try` body, restored after). Drives the fast-vs-unwind choice for
    /// `return`/`break`/`continue` and is snapshotted into each pushed `Frame`.
    pub(crate) handler_depth: u32,
    /// Label pending attachment to the next pushed frame. Set by the labeled-
    /// statement arm when the labeled body is a loop, and consumed via `take()`
    /// when that loop pushes its frame; `None` otherwise.
    pub(crate) pending_label: Option<String>,
    /// Phase 3 (§4.3): the binding name inferred for the next function/arrow
    /// EXPRESSION emitted in a value position (`const render = () => …`,
    /// `obj.render = function(){}`, `{ render: () => … }`). Set by the emit site
    /// that holds the binding context just before `emit_expr` on the initializer/
    /// RHS/value, and consumed (taken) by the `Expr::Fn`/`Expr::Arrow` arms of
    /// `emit_expr` so the divert can match the exclude glob on it. A function's own
    /// `ident` (`function render(){}`) takes precedence and does not use this.
    pub(crate) pending_fn_name: Option<String>,
    pub(crate) bail_reason: Option<&'static str>,
    /// D5: nested-function chunks compiled while emitting this body, in
    /// `MakeClosure` `child` index order. A nested `function`/`arrow` is compiled
    /// to its own `ChildChunk` here and referenced by index from a `MakeClosure`.
    pub(crate) children: Vec<ChildChunk>,
    /// D5: the boxing plan from `cells.rs`, consulted to compile each nested chunk
    /// with its own boxed-capture set (so a mutated shared upvalue lowers to
    /// `LoadCell`/`StoreCell`). `None` when compiling standalone (tests / the
    /// no-nesting path) — nested fns then compile with an empty boxed set.
    pub(crate) box_plan: Option<&'a cells::BoxPlan>,
    /// Phase 3 (§4): the native-closure divert options (exclude glob +
    /// ineligible-divert flag), threaded down so `emit_nested_closure` can decide
    /// whether a nested function stays native. Inherited unchanged by every child
    /// chunk compiled while emitting this body.
    pub(crate) opts: CompileOptions<'a>,
}

/// Phase 3: glob match for the `--virtualize-exclude` name test. An invalid glob
/// matches nothing (mirrors `cells::glob_matches`).
pub(crate) fn glob_matches(glob: &str, name: &str) -> bool {
    glob::Pattern::new(glob)
        .map(|p| p.matches(name))
        .unwrap_or(false)
}

impl<'a> Cx<'a> {
    /// Generic bail (reason "unsupported"). Prefer `bail_with` where a more
    /// specific reason aids the `--verbose` skip diagnostic.
    pub(crate) fn bail(&mut self) {
        self.bail_with("unsupported");
    }

    /// Bail with a specific reason; the first reason recorded wins.
    pub(crate) fn bail_with(&mut self, reason: &'static str) {
        if self.bail_reason.is_none() {
            self.bail_reason = Some(reason);
        }
    }

    pub(crate) fn bailed(&self) -> bool {
        self.bail_reason.is_some()
    }

    pub(crate) fn here(&self) -> u32 {
        self.code.len() as u32
    }

    pub(crate) fn emit(&mut self, i: Instr) {
        if self.code.len() >= u32::MAX as usize {
            self.bail_with("too_large");
            return;
        }
        self.code.push(i);
    }

    pub(crate) fn const_num(&mut self, v: f64) -> u32 {
        if self.consts.len() >= u32::MAX as usize {
            self.bail_with("too_large");
            return 0;
        }
        let idx = self.consts.len() as u32;
        self.consts.push(Const::Num(v));
        idx
    }

    pub(crate) fn const_str(&mut self, v: String) -> u32 {
        if self.consts.len() >= u32::MAX as usize {
            self.bail_with("too_large");
            return 0;
        }
        let idx = self.consts.len() as u32;
        self.consts.push(Const::Str(v));
        idx
    }

    pub(crate) fn const_bool(&mut self, v: bool) -> u32 {
        if self.consts.len() >= u32::MAX as usize {
            self.bail_with("too_large");
            return 0;
        }
        let idx = self.consts.len() as u32;
        self.consts.push(Const::Bool(v));
        idx
    }

    /// D4: register a tagged-template's template object as a const. The object is
    /// built (frozen, `.raw`-bearing) once at runtime and cached in this slot, so
    /// the call site reuses the SAME object on every evaluation (identity-stable).
    pub(crate) fn const_template(&mut self, cooked: Vec<Option<String>>, raw: Vec<String>) -> u32 {
        if self.consts.len() >= u32::MAX as usize {
            self.bail_with("too_large");
            return 0;
        }
        let idx = self.consts.len() as u32;
        self.consts.push(Const::TemplateObject { cooked, raw });
        idx
    }

    /// Look up a name in the lexical scope stack, innermost frame first (D3).
    /// Returns the slot of the lexically-nearest in-scope binding, or `None` if the
    /// name is bound in no frame (a free reference -> capture candidate).
    pub(crate) fn lookup(&self, name: &str) -> Option<u32> {
        self.scopes.iter().rev().find_map(|f| f.get(name).copied())
    }

    /// Resolve an identifier name to a slot. A binding in any live scope frame =>
    /// its slot (innermost wins, matching JS lexical scoping); a free name =>
    /// allocate a capture slot in the function frame (recording the name in order).
    pub(crate) fn resolve(&mut self, name: &str) -> u32 {
        if let Some(slot) = self.lookup(name) {
            return slot;
        }
        // free => capture (captures live in the function frame, slots >= cap_floor)
        let slot = self.next_slot;
        self.next_slot += 1;
        self.scopes[0].insert(name.to_string(), slot);
        self.captures.push(name.to_string());
        slot
    }

    /// True if name resolves to a param/local (not a fresh capture). A name bound
    /// in any live scope frame at a slot below `cap_floor` is a param/local; a name
    /// bound only at/above `cap_floor` (or unbound) is a capture / free.
    pub(crate) fn is_param_or_local(&self, name: &str) -> bool {
        matches!(self.lookup(name), Some(slot) if slot < self.cap_floor)
    }

    /// D1: true if `name` is a BOXED capture — a free name the enclosing-scope
    /// pre-pass cell-ified. A boxed capture is never a param/local (it has no
    /// in-scope frame binding below `cap_floor`); its slot holds the shared cell.
    /// Reads/writes use `LoadCell`/`StoreCell`. A boxed name that happens to ALSO
    /// be a param/local of THIS function (a shadow) is NOT treated as boxed here —
    /// the local binding wins, exactly as JS lexical scoping requires.
    pub(crate) fn is_boxed_capture(&self, name: &str) -> bool {
        self.boxed_caps.contains(name) && !self.is_param_or_local(name)
    }

    /// D5: true if `name` is an in-VM BOXED LOCAL of THIS frame — a param/var/let
    /// whose slot holds a cell because a nested closure captures-and-mutates it. Its
    /// reads/writes go through `LoadCell`/`StoreCell` and the slot value (the cell) is
    /// threaded to the child by `MakeClosure`. Only meaningful for a name that
    /// resolves to a local of this frame (not a free capture / upvalue).
    pub(crate) fn is_boxed_local(&self, name: &str) -> bool {
        self.boxed_locals.contains(name)
            && self.is_param_or_local(name)
            && !self
                .lookup(name)
                .is_some_and(|slot| self.lexical_slots.contains_key(&slot))
    }

    /// D1+D5: true if a read/write of `name` must go through a cell — either an
    /// in-VM boxed local of this frame (D5) or a boxed upvalue capture (D1/D5). The
    /// single predicate the emit paths consult so a cell binding is uniformly
    /// lowered to `LoadCell`/`StoreCell`.
    pub(crate) fn is_celled(&self, name: &str) -> bool {
        self.is_boxed_local(name) || self.is_boxed_capture(name)
    }

    /// Push a fresh (empty) lexical scope frame for a block / catch / for-head.
    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    /// Pop the innermost lexical scope frame (block exit), restoring outer bindings.
    pub(crate) fn pop_scope(&mut self) {
        debug_assert!(self.scopes.len() > 1, "popped the function frame");
        self.scopes.pop();
    }

    /// Bind a name to a pre-allocated slot in the innermost (current) scope frame.
    /// Used by emission when it enters a block to populate the frame from the
    /// `DeclCollector`-allocated `decl_slots`.
    pub(crate) fn bind_in_scope(&mut self, name: String, slot: u32) {
        if let Some(top) = self.scopes.last_mut() {
            top.insert(name, slot);
        }
    }

    /// Allocate the next anonymous temp slot from the reserved pool (LIFO bump).
    /// The pool was sized by `count_temps`, so this never exceeds `cap_floor`.
    pub(crate) fn alloc_temp(&mut self) -> u32 {
        let s = self.temp_top;
        self.temp_top += 1;
        debug_assert!(
            self.temp_top <= self.cap_floor,
            "temp pool overflow: top {} > cap_floor {} (count_temps under-sized?)",
            self.temp_top,
            self.cap_floor
        );
        s
    }

    /// Release the most recently allocated temp slot (LIFO with `alloc_temp`).
    pub(crate) fn free_temp(&mut self) {
        debug_assert!(
            self.temp_top > self.temp_base,
            "free_temp underflow: temp_top already at base"
        );
        self.temp_top -= 1;
    }
}

/// Temp slots an array-destructuring level holds live: the iterator and a `done`
/// flag always, plus a 2-slot scratch (accumulator array + value) when the level
/// has a `...rest` element. The source itself is NOT a temp here — array
/// destructuring takes the iterator off the stack value immediately (see
/// `emit_destructure_array`), so no source slot is reserved.
pub(crate) fn array_level_temps(arr: &ArrayPat) -> u32 {
    let has_rest = arr.elems.iter().any(|e| matches!(e, Some(Pat::Rest(_))));
    2 + if has_rest { 2 } else { 0 }
}

/// Temps a spread level holds. An array/`new` spread level needs 3 (the
/// accumulator array + iterator + value scratch); 0 with no spread.
pub(crate) fn array_spread_charge(n: &ArrayLit) -> u32 {
    if n.elems.iter().flatten().any(|e| e.spread.is_some()) {
        3
    } else {
        0
    }
}
/// A spread CALL needs 4 — the 3 of `array_spread_charge` plus one for a method
/// receiver held across the args build (`o.m(...a)` -> `o.m.apply(recv, ARR)`).
pub(crate) fn call_spread_charge(n: &CallExpr) -> u32 {
    if n.args.iter().any(|a| a.spread.is_some()) {
        4
    } else {
        0
    }
}
pub(crate) fn new_spread_charge(n: &NewExpr) -> u32 {
    let has = n
        .args
        .as_ref()
        .is_some_and(|a| a.iter().any(|x| x.spread.is_some()));
    if has { 3 } else { 0 }
}

/// Max simultaneously-live spread temps in an expression subtree (the scratch the
/// `emit_spread_array` path needs). Used to size the temp pool for spreads in
/// param defaults / pattern defaults, where the body `DeclCollector` does not run.
pub(crate) fn expr_max_spread_temps(e: &Expr) -> u32 {
    let mut s = SpreadTempScan { cur: 0, max: 0 };
    e.visit_with(&mut s);
    s.max
}

pub(crate) struct SpreadTempScan {
    cur: u32,
    max: u32,
}
impl SpreadTempScan {
    fn enter(&mut self, n: u32) {
        self.cur += n;
        if self.cur > self.max {
            self.max = self.cur;
        }
    }
    fn exit(&mut self, n: u32) {
        self.cur -= n;
    }
}
impl Visit for SpreadTempScan {
    fn visit_array_lit(&mut self, n: &ArrayLit) {
        let c = array_spread_charge(n);
        self.enter(c);
        n.visit_children_with(self);
        self.exit(c);
    }
    fn visit_call_expr(&mut self, n: &CallExpr) {
        let c = call_spread_charge(n);
        self.enter(c);
        n.visit_children_with(self);
        self.exit(c);
    }
    fn visit_new_expr(&mut self, n: &NewExpr) {
        let c = new_spread_charge(n);
        self.enter(c);
        n.visit_children_with(self);
        self.exit(c);
    }
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
}

/// Maximum number of simultaneously-live temp slots a destructuring pattern needs
/// (a safe over-approximation; unused reserved slots are harmless). An object
/// level costs 1 (the source slot read once per property); an array level costs
/// `array_level_temps`. A default expression's spread temps are folded in. Nested
/// sub-patterns are processed one at a time, so a level's cost is its own plus the
/// *max* (not sum) over its children. This is the standalone form used for
/// parameter patterns; the body's `DeclCollector` computes the same value
/// incrementally so for-of/for-in/spread temps stack correctly.
pub(crate) fn pat_temp_count(pat: &Pat) -> u32 {
    match pat {
        Pat::Array(arr) => {
            let child = arr
                .elems
                .iter()
                .flatten()
                .map(pat_temp_count)
                .max()
                .unwrap_or(0);
            array_level_temps(arr) + child
        }
        Pat::Object(obj) => {
            let child = obj
                .props
                .iter()
                .map(|p| match p {
                    ObjectPatProp::KeyValue(kv) => pat_temp_count(&kv.value),
                    ObjectPatProp::Assign(a) => {
                        a.value.as_ref().map_or(0, |d| expr_max_spread_temps(d))
                    }
                    ObjectPatProp::Rest(r) => pat_temp_count(&r.arg),
                })
                .max()
                .unwrap_or(0);
            1 + child
        }
        Pat::Assign(ap) => pat_temp_count(&ap.left).max(expr_max_spread_temps(&ap.right)),
        Pat::Rest(r) => pat_temp_count(&r.arg),
        _ => 0,
    }
}

/// Default-param scope soundness: a default expression evaluates in *parameter*
/// scope, seeing earlier params but not later ones (TDZ), not itself, and not body
/// locals / destructuring-param leaves. A reference our flat slot model would
/// resolve to the wrong binding bails rather than miscompile. `own_slot` is the
/// defaulted param's slot; a referenced param at slot `>= own_slot` is itself or a
/// later param.
pub(crate) fn default_scope_ok(
    default: &Expr,
    own_slot: u32,
    param_slots: &HashMap<String, u32>,
    func_locals: &HashMap<String, u32>,
    block_locals: &std::collections::HashSet<String>,
) -> Result<(), &'static str> {
    let mut rc = RefNameCollector { names: Vec::new() };
    default.visit_with(&mut rc);
    for name in &rc.names {
        if let Some(&pslot) = param_slots.get(name) {
            if pslot >= own_slot {
                return Err("default_refs_later_param");
            }
        } else if func_locals.contains_key(name) || block_locals.contains(name) {
            // A body-declared `var`/`let`/`const` shares the name: the default sees
            // the OUTER binding (param scope excludes the body scope), but our model
            // would resolve it to the body local — bail conservatively.
            return Err("default_refs_body_local");
        }
    }
    Ok(())
}

/// Visitor: collect var/let/const declared identifier names in the body
/// (incl. nested blocks/loops/if but NOT nested functions — eligibility
/// already rejected those), descending through destructuring patterns to slot
/// each leaf binding (`collect_pat_locals`).
///
/// It also doubles as the `count_temps` pre-pass: in the *same* traversal it
/// computes the maximum number of anonymous temp slots that are simultaneously
/// live, so `compile_body` can reserve exactly that many below the captures.
/// Cost model: `for-of` = 1 (the iterator), `for-in` = 2 (the keys array +
/// index), each object-destructure level = 1, each array-destructure level =
/// `array_level_temps`. Costs nest: a `temp_cur` running counter is bumped on
/// entry and restored on exit, and `temp_max` tracks the high-water mark — so an
/// inner construct's temps stack on top of an outer construct's still-live ones
/// (e.g. a destructuring for-of head on top of the loop's iterator).
pub(crate) struct DeclCollector<'a, 'b> {
    cx: &'a mut Cx<'b>,
    /// Temps currently live at this point of the traversal.
    temp_cur: u32,
    /// High-water mark of `temp_cur` — the value `count_temps` reports.
    temp_max: u32,
}

impl<'a, 'b> DeclCollector<'a, 'b> {
    /// Enter a construct that holds `n` temps live across its subtree.
    fn enter_temps(&mut self, n: u32) {
        self.temp_cur += n;
        if self.temp_cur > self.temp_max {
            self.temp_max = self.temp_cur;
        }
    }

    /// Leave a construct previously entered with `enter_temps(n)`.
    fn exit_temps(&mut self, n: u32) {
        self.temp_cur -= n;
    }
}

/// Slot a function-scoped `var` (or destructuring-param leaf) binding into the
/// **function frame** (`scopes[0]`). A re-declaration of an already-seen name
/// reuses the existing slot (the same hoisted binding). Returns `true` if this was
/// a fresh allocation, `false` if the name already existed (a re-declaration) — the
/// param path uses the latter to detect a duplicate param name.
/// True if `body` references the identifier `arguments` (a free reference to the
/// implicit arguments object). Used by the D2 prologue to decide whether to
/// materialize the actual arguments object. A non-computed member property (`o.arguments`)
/// is an `IdentName`, not an `Ident`, so it does not trip this — only genuine
/// identifier references do.
pub(crate) fn uses_arguments(body: &BlockStmt) -> bool {
    struct V {
        found: bool,
    }
    impl Visit for V {
        fn visit_ident(&mut self, n: &Ident) {
            if n.sym.as_ref() == "arguments" {
                self.found = true;
            }
        }
    }
    let mut v = V { found: false };
    body.visit_with(&mut v);
    v.found
}

/// True if the body explicitly declares a binding named `arguments` via
/// `var`/`let`/`const` (a leaf of any declaration pattern). A param named
/// `arguments` is handled separately (it occupies a positional slot the
/// interpreter fills, so resolving references to it is correct). A `var`/`let`/
/// `const arguments` shadow is a sloppy-mode corner case whose initial value the
/// flat-slot model can't reproduce, so the caller bails rather than miscompile.
pub(crate) fn declares_arguments(body: &BlockStmt) -> bool {
    struct V {
        found: bool,
    }
    fn pat_has(pat: &Pat, found: &mut bool) {
        match pat {
            Pat::Ident(bi) => {
                if bi.id.sym.as_ref() == "arguments" {
                    *found = true;
                }
            }
            Pat::Assign(ap) => pat_has(&ap.left, found),
            Pat::Rest(r) => pat_has(&r.arg, found),
            Pat::Array(arr) => {
                for el in arr.elems.iter().flatten() {
                    pat_has(el, found);
                }
            }
            Pat::Object(obj) => {
                for prop in &obj.props {
                    match prop {
                        ObjectPatProp::KeyValue(kv) => pat_has(&kv.value, found),
                        ObjectPatProp::Assign(a) => {
                            if a.key.id.sym.as_ref() == "arguments" {
                                *found = true;
                            }
                        }
                        ObjectPatProp::Rest(r) => pat_has(&r.arg, found),
                    }
                }
            }
            Pat::Expr(_) | Pat::Invalid(_) => {}
        }
    }
    impl Visit for V {
        fn visit_var_declarator(&mut self, d: &VarDeclarator) {
            pat_has(&d.name, &mut self.found);
            d.visit_children_with(self);
        }
    }
    let mut v = V { found: false };
    body.visit_with(&mut v);
    v.found
}

pub(crate) fn slot_func_binding(cx: &mut Cx<'_>, name: String) -> bool {
    if cx.scopes[0].contains_key(&name) {
        return false;
    }
    let slot = cx.next_slot;
    cx.next_slot += 1;
    cx.scopes[0].insert(name, slot);
    true
}

/// Slot a single block-scoped (`let`/`const`/`catch`) binding (D3). v1 NEVER reuses
/// slots across sibling blocks: every block-scoped binding gets a brand-new slot,
/// recorded by the declared identifier's span in `decl_slots` so emission can look
/// it up when it enters the binding's block. A shadow therefore lands on its own
/// slot and never clobbers the outer binding. `lo` is the binding ident's span
/// `BytePos.0` (a stable per-binding key across the decl/emit passes); `name` is
/// recorded in `block_local_names` for the default-param scope check.
pub(crate) fn slot_block_binding(cx: &mut Cx<'_>, name: &str, lo: u32) {
    let slot = cx.next_slot;
    cx.next_slot += 1;
    cx.decl_slots.insert(lo, slot);
    cx.block_local_names.insert(name.to_string());
}

/// Recursively slot every leaf binding identifier in a `var` declaration pattern or
/// a destructuring **param** pattern into the function frame — simple idents,
/// defaults (`= d`), nested array/object patterns, and rest targets. The matching
/// emit-time logic lives in `emit_destructure_*`.
pub(crate) fn slot_pat_leaves_func(cx: &mut Cx<'_>, pat: &Pat) {
    match pat {
        Pat::Ident(bi) => {
            slot_func_binding(cx, bi.id.sym.to_string());
        }
        Pat::Assign(ap) => slot_pat_leaves_func(cx, &ap.left),
        Pat::Rest(r) => slot_pat_leaves_func(cx, &r.arg),
        Pat::Array(arr) => {
            for elem in arr.elems.iter().flatten() {
                slot_pat_leaves_func(cx, elem);
            }
        }
        Pat::Object(obj) => {
            for prop in &obj.props {
                match prop {
                    ObjectPatProp::KeyValue(kv) => slot_pat_leaves_func(cx, &kv.value),
                    ObjectPatProp::Assign(a) => {
                        slot_func_binding(cx, a.key.id.sym.to_string());
                    }
                    ObjectPatProp::Rest(r) => slot_pat_leaves_func(cx, &r.arg),
                }
            }
        }
        // `Pat::Expr` only appears in assignment patterns (existing bindings, not
        // declarations); `Pat::Invalid` never reaches a successful parse.
        Pat::Expr(_) | Pat::Invalid(_) => cx.bail_with("destructuring_decl"),
    }
}

/// Recursively allocate a fresh block-scoped slot for every leaf binding identifier
/// in a `let`/`const` declaration or destructuring `catch` pattern (D3), recording
/// each by its span in `decl_slots` (see `slot_block_binding`). Used by the
/// `DeclCollector` allocation pass.
pub(crate) fn slot_pat_leaves_block(cx: &mut Cx<'_>, pat: &Pat) {
    match pat {
        Pat::Ident(bi) => slot_block_binding(cx, bi.id.sym.as_ref(), bi.id.span.lo.0),
        Pat::Assign(ap) => slot_pat_leaves_block(cx, &ap.left),
        Pat::Rest(r) => slot_pat_leaves_block(cx, &r.arg),
        Pat::Array(arr) => {
            for elem in arr.elems.iter().flatten() {
                slot_pat_leaves_block(cx, elem);
            }
        }
        Pat::Object(obj) => {
            for prop in &obj.props {
                match prop {
                    ObjectPatProp::KeyValue(kv) => slot_pat_leaves_block(cx, &kv.value),
                    ObjectPatProp::Assign(a) => {
                        slot_block_binding(cx, a.key.id.sym.as_ref(), a.key.id.span.lo.0)
                    }
                    ObjectPatProp::Rest(r) => slot_pat_leaves_block(cx, &r.arg),
                }
            }
        }
        Pat::Expr(_) | Pat::Invalid(_) => cx.bail_with("destructuring_decl"),
    }
}

/// Collect every leaf binding `(name, span_lo)` pair in a declaration/catch pattern,
/// in source (leaf) order. Used by **emission** to populate a freshly-pushed scope
/// frame from the `DeclCollector`-allocated `decl_slots` (looked up by `span_lo`),
/// and to enumerate a block's / catch's bindings without re-allocating.
pub(crate) fn collect_pat_binding_lows(pat: &Pat, out: &mut Vec<(String, u32)>) {
    match pat {
        Pat::Ident(bi) => out.push((bi.id.sym.to_string(), bi.id.span.lo.0)),
        Pat::Assign(ap) => collect_pat_binding_lows(&ap.left, out),
        Pat::Rest(r) => collect_pat_binding_lows(&r.arg, out),
        Pat::Array(arr) => {
            for elem in arr.elems.iter().flatten() {
                collect_pat_binding_lows(elem, out);
            }
        }
        Pat::Object(obj) => {
            for prop in &obj.props {
                match prop {
                    ObjectPatProp::KeyValue(kv) => collect_pat_binding_lows(&kv.value, out),
                    ObjectPatProp::Assign(a) => {
                        out.push((a.key.id.sym.to_string(), a.key.id.span.lo.0))
                    }
                    ObjectPatProp::Rest(r) => collect_pat_binding_lows(&r.arg, out),
                }
            }
        }
        Pat::Expr(_) | Pat::Invalid(_) => {}
    }
}

/// Collect the **direct** block-scoped (`let`/`const`) bindings of a block's
/// statement list — i.e. the bindings whose lexical scope is exactly this block —
/// as `(name, span_lo)` pairs. Does NOT descend into nested blocks, loops, `try`
/// bodies, `switch`, or functions (those open their own scopes). Emission uses this
/// to populate a block's scope frame (D3); `var` is function-scoped and excluded.
pub(crate) fn direct_block_bindings(stmts: &[Stmt]) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for s in stmts {
        if let Stmt::Decl(Decl::Var(v)) = s
            && matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const)
        {
            for d in &v.decls {
                collect_pat_binding_lows(&d.name, &mut out);
            }
        }
    }
    out
}

/// Populate the innermost (just-pushed) scope frame with each `(name, slot)` for the
/// given `(name, span_lo)` bindings, resolving the slot via `decl_slots`. A binding
/// missing from `decl_slots` (should not happen — the `DeclCollector` allocates all
/// block-scoped bindings) is skipped defensively.
pub(crate) fn bind_lows_in_scope(cx: &mut Cx<'_>, lows: &[(String, u32)]) {
    for (name, lo) in lows {
        if let Some(&slot) = cx.decl_slots.get(lo) {
            cx.bind_in_scope(name.clone(), slot);
            if let Some(&constant) = cx.lexical_slots.get(&slot) {
                cx.emit(Instr::BeginLexical(slot * 2 + u32::from(constant)));
            }
        }
    }
}

impl Visit for DeclCollector<'_, '_> {
    fn visit_for_of_stmt(&mut self, f: &ForOfStmt) {
        // One temp holds the iterator object for the whole loop, including its
        // body — so it stacks with any temps the body needs.
        self.enter_temps(1);
        f.visit_children_with(self);
        self.exit_temps(1);
    }

    fn visit_for_in_stmt(&mut self, f: &ForInStmt) {
        // Two temps (enumerated keys + current index) stay live across the loop.
        self.enter_temps(2);
        f.visit_children_with(self);
        self.exit_temps(2);
    }

    fn visit_pat(&mut self, p: &Pat) {
        // Charge the whole pattern's temp cost (nesting + default-expr spreads
        // included by `pat_temp_count`) as one bump, so it stacks with any
        // enclosing construct (e.g. the iterator temp of a destructuring for-of
        // head). No recursion: `pat_temp_count` already accounts for sub-patterns
        // and their default expressions.
        let n = pat_temp_count(p);
        self.enter_temps(n);
        self.exit_temps(n);
    }

    // Spread expressions (`[...a]`, `f(...a)`, `new C(...a)`) build a scratch array
    // via the iterator; charge their temps so they stack with any enclosing
    // for-of/destructure. Object spread (`{...o}`) uses `Object.assign` (no temp).
    fn visit_array_lit(&mut self, n: &ArrayLit) {
        let c = array_spread_charge(n);
        self.enter_temps(c);
        n.visit_children_with(self);
        self.exit_temps(c);
    }
    fn visit_call_expr(&mut self, n: &CallExpr) {
        let c = call_spread_charge(n);
        self.enter_temps(c);
        n.visit_children_with(self);
        self.exit_temps(c);
    }
    fn visit_new_expr(&mut self, n: &NewExpr) {
        let c = new_spread_charge(n);
        self.enter_temps(c);
        n.visit_children_with(self);
        self.exit_temps(c);
    }

    fn visit_var_decl(&mut self, v: &VarDecl) {
        // `var` is function-scoped: slot into the function frame, deduping a
        // re-declaration. `let`/`const` is block-scoped (D3): every binding gets a
        // fresh slot recorded by span in `decl_slots`, so a shadow lands on its own
        // slot — no bail. (A let/const colliding with a same-named var/param of the
        // same scope is a JS syntax error and never parses.)
        let block_scoped = matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const);
        for d in &v.decls {
            if block_scoped {
                slot_pat_leaves_block(self.cx, &d.name);
                let mut lows = Vec::new();
                collect_pat_binding_lows(&d.name, &mut lows);
                for (_, lo) in lows {
                    if let Some(&slot) = self.cx.decl_slots.get(&lo) {
                        self.cx
                            .lexical_slots
                            .insert(slot, v.kind == VarDeclKind::Const);
                    }
                }
            } else {
                slot_pat_leaves_func(self.cx, &d.name);
            }
        }
        // Keep descending so nested-block decls + destructuring temps are counted.
        v.visit_children_with(self);
    }
    fn visit_catch_clause(&mut self, c: &CatchClause) {
        // The catch binding is block-scoped (D3): allocate a fresh slot recorded by
        // span, so `catch (e)` shadowing an outer `e` lands on its own slot — no
        // bail. `visit_children_with` then counts the param pattern's temps via
        // `visit_pat` and descends into the catch body.
        match &c.param {
            Some(Pat::Ident(bi)) => {
                slot_block_binding(self.cx, bi.id.sym.as_ref(), bi.id.span.lo.0)
            }
            Some(p @ (Pat::Array(_) | Pat::Object(_))) => slot_pat_leaves_block(self.cx, p),
            _ => {}
        }
        if let Some(p) = &c.param {
            let mut lows = Vec::new();
            collect_pat_binding_lows(p, &mut lows);
            for (_, lo) in lows {
                if let Some(&slot) = self.cx.decl_slots.get(&lo) {
                    self.cx.lexical_slots.insert(slot, false);
                }
            }
        }
        c.visit_children_with(self);
    }
    // D5: a nested `function f(){…}` DECLARATION binds `f` function-scoped (hoisted),
    // so slot the name into the function frame like a `var` (deduping a
    // re-declaration). Do NOT descend into the nested fn's body (its own scope) —
    // its locals/captures are compiled in a separate chunk. A block-nested fn-decl is
    // bailed separately by `check_no_nested_block_fn_decls`, so an unused slot here is
    // harmless.
    fn visit_fn_decl(&mut self, n: &FnDecl) {
        slot_func_binding(self.cx, n.ident.sym.to_string());
    }
    // Do not descend into nested functions; their bodies are separate chunks (D5).
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
}

/// True if a top-level statement list contains a `function`-declaration nested
/// inside a block / loop / `if` / `try` / `switch` (i.e. NOT a direct top-level
/// statement of the function body). Such a declaration's hoisting is mode-dependent
/// (sloppy block-function semantics), which the flat-slot VM cannot reproduce, so
/// the caller bails. Direct top-level fn-decls are fine (fully hoisted). Does not
/// descend into nested function bodies (their own chunks handle their decls).
pub(crate) fn check_no_nested_block_fn_decls(stmts: &[Stmt]) -> Result<(), &'static str> {
    struct V {
        depth: u32,
        bad: bool,
    }
    impl Visit for V {
        fn visit_fn_decl(&mut self, n: &FnDecl) {
            if self.depth > 0 {
                self.bad = true;
            }
            // Do not descend into the nested fn's own body.
            let _ = n;
        }
        fn visit_stmt(&mut self, s: &Stmt) {
            match s {
                // A fn-decl as a direct child of the CURRENT statement list is fine;
                // anything that opens a nested statement context bumps depth.
                Stmt::Decl(Decl::Fn(_)) if self.depth == 0 => {}
                Stmt::Block(_)
                | Stmt::If(_)
                | Stmt::For(_)
                | Stmt::ForIn(_)
                | Stmt::ForOf(_)
                | Stmt::While(_)
                | Stmt::DoWhile(_)
                | Stmt::Try(_)
                | Stmt::Switch(_)
                | Stmt::Labeled(_)
                | Stmt::With(_) => {
                    self.depth += 1;
                    s.visit_children_with(self);
                    self.depth -= 1;
                    return;
                }
                _ => {}
            }
            s.visit_children_with(self);
        }
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
    }
    let mut v = V {
        depth: 0,
        bad: false,
    };
    for s in stmts {
        s.visit_with(&mut v);
    }
    if v.bad {
        Err("nested_block_fn_decl")
    } else {
        Ok(())
    }
}

/// Collects the value-position identifier names an expression *reads* — used to
/// validate default-param expressions against the parameter scope. Skips
/// member-property and object-key identifiers (not bindings) and nested
/// function/arrow bodies (their own scope).
pub(crate) struct RefNameCollector {
    names: Vec<String>,
}

impl Visit for RefNameCollector {
    fn visit_ident(&mut self, id: &Ident) {
        self.names.push(id.sym.to_string());
    }
    fn visit_member_expr(&mut self, m: &MemberExpr) {
        m.obj.visit_with(self);
        if let MemberProp::Computed(c) = &m.prop {
            c.visit_with(self);
        }
    }
    fn visit_prop_name(&mut self, p: &PropName) {
        if let PropName::Computed(c) = p {
            c.visit_with(self);
        }
    }
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
}

/// Compile a function body to bytecode, or `None` to skip anything we can't
/// prove safe.
///
/// Compile `body` with no boxed captures (the common, no-capture-mutation path).
/// Equivalent to `compile_body_boxed` with an empty boxed set; kept as the public
/// entry point used by tests and the non-D1 call sites.
pub fn compile_body(params: &[Param], body: &BlockStmt) -> Result<Compiled, &'static str> {
    compile_body_boxed(params, body, &std::collections::HashSet::new())
}

/// Phase 3 entry point: compile `body` with native-closure divert [`CompileOptions`]
/// (the `--virtualize-exclude` glob and the ineligible-divert flag). Otherwise
/// identical to [`compile_body`] (empty boxed set, no `BoxPlan`); the options are
/// inherited by every nested child chunk so a deeply-nested excluded/ineligible fn
/// is diverted too.
pub fn compile_body_with_opts(
    params: &[Param],
    body: &BlockStmt,
    opts: CompileOptions<'_>,
) -> Result<Compiled, &'static str> {
    compile_body_inner_opts(params, body, &std::collections::HashSet::new(), None, opts)
}

/// Compile `body`, treating every free name in `boxed` as a BOXED mutable capture
/// (D1): its slot holds a one-element cell `[v]` shared by reference with the
/// enclosing scope, so reads compile to `LoadCell` and writes to `StoreCell` and a
/// VM write propagates to the enclosing scope's array. Names NOT in `boxed` keep
/// the read-only `LoadLocal` capture and still bail `mutable_capture` if written.
///
/// Without a `BoxPlan`, a nested `function`/`arrow` in the body compiles its own
/// chunk with an *empty* boxed set — sound, but a mutated shared upvalue would then
/// bail `mutable_capture` inside the child. The D5 driver uses
/// `compile_body_with_plan` to thread the plan so nested boxed upvalues lower to
/// cells.
pub fn compile_body_boxed(
    params: &[Param],
    body: &BlockStmt,
    boxed: &std::collections::HashSet<String>,
) -> Result<Compiled, &'static str> {
    compile_body_inner(params, body, boxed, None)
}

/// D5 entry point: compile `body` with the `cells.rs` boxing `plan` available, so
/// nested-function chunks compiled while emitting this body look up their own
/// boxed-capture sets (keyed by the nested body's start `BytePos`) and lower a
/// mutated shared upvalue to `LoadCell`/`StoreCell`.
pub fn compile_body_with_plan(
    params: &[Param],
    body: &BlockStmt,
    boxed: &std::collections::HashSet<String>,
    plan: &cells::BoxPlan,
) -> Result<Compiled, &'static str> {
    compile_body_inner(params, body, boxed, Some(plan))
}

pub(crate) fn compile_body_inner(
    params: &[Param],
    body: &BlockStmt,
    boxed: &std::collections::HashSet<String>,
    plan: Option<&cells::BoxPlan>,
) -> Result<Compiled, &'static str> {
    compile_body_inner_opts(params, body, boxed, plan, CompileOptions::default())
}

pub(crate) fn compile_body_inner_opts<'a>(
    params: &[Param],
    body: &BlockStmt,
    boxed: &std::collections::HashSet<String>,
    plan: Option<&'a cells::BoxPlan>,
    opts: CompileOptions<'a>,
) -> Result<Compiled, &'static str> {
    let mut cx = Cx {
        code: Vec::new(),
        consts: Vec::new(),
        // `scopes[0]` is the function frame (params + `var`s + captures); it is
        // never popped. Block/catch/for-head frames are pushed and popped around
        // their bodies during emission (D3).
        scopes: vec![HashMap::new()],
        decl_slots: HashMap::new(),
        lexical_slots: HashMap::new(),
        initializing: false,
        block_local_names: std::collections::HashSet::new(),
        captures: Vec::new(),
        boxed_caps: boxed.clone(),
        // D5: this body's own locals that a nested closure captures-and-mutates must
        // hold cells (computed lexically, set before any read/write lowering chooses
        // LoadCell/StoreCell). A boxed local is never also a boxed capture (a name is
        // either bound here or free, not both), so the two sets are disjoint.
        boxed_locals: compute_boxed_locals(params, body),
        next_slot: 0,
        cap_floor: 0,
        temp_base: 0,
        temp_top: 0,
        frames: Vec::new(),
        handler_depth: 0,
        pending_label: None,
        pending_fn_name: None,
        bail_reason: None,
        children: Vec::new(),
        box_plan: plan,
        opts,
    };

    // 1. Params first. Record default-value params so their init prologue can
    //    be emitted (in order) once all params and body locals are slotted —
    //    this keeps captures the last slots (cap_start math holds), since a
    //    capture is only ever allocated during prologue/body emission.
    let mut defaults: Vec<(u32, &Expr)> = Vec::new();
    // Destructuring params `function f([a], {b})`: each takes one positional slot
    // (the raw arg, copied by the interpreter) and is destructured in the prologue.
    // `(positional_slot, pattern, optional outer default)`.
    let mut destructure_params: Vec<(u32, &Pat, Option<&Expr>)> = Vec::new();
    // Trailing `...rest` param: its slot is filled by a `LoadRest` prologue (not
    // the positional arg copy), so it is recorded here and emitted after defaults.
    let mut rest_slot: Option<u32> = None;
    let param_count = params.len();
    for (pi, p) in params.iter().enumerate() {
        match &p.pat {
            Pat::Ident(bi) => {
                let name = bi.id.sym.to_string();
                // Duplicate param name (sloppy-mode `function(a, a)`): JS binds
                // the LAST occurrence, and the flat slot model maps the name to
                // the FIRST slot, so reads diverge — and it breaks the
                // params.len()==param-slots invariant the arg-copy cap relies on.
                // Bail; the function stays un-virtualized (runs as normal JS).
                if !slot_func_binding(&mut cx, name) {
                    return Err("dup_param");
                }
            }
            Pat::Rest(rp) => {
                // Only a TRAILING rest param with a simple-ident target is modeled.
                // A non-trailing rest is a syntax error (never parses), but guard
                // anyway; a destructuring rest target is out of scope.
                if pi != param_count - 1 {
                    return Err("rest_pattern");
                }
                let bi = match &*rp.arg {
                    Pat::Ident(bi) => bi,
                    _ => return Err("rest_pattern"),
                };
                let name = bi.id.sym.to_string();
                if cx.scopes[0].contains_key(&name) {
                    return Err("dup_param");
                }
                let slot = cx.next_slot;
                cx.next_slot += 1;
                cx.scopes[0].insert(name, slot);
                rest_slot = Some(slot);
            }
            Pat::Assign(ap) => {
                // Default param `target = <expr>`. A simple-ident target gets a
                // binding slot + a simple-default prologue; a destructuring target
                // (`[a] = d` / `{a} = d`) takes a positional slot and is destructured
                // in the prologue after applying the outer default.
                match &*ap.left {
                    Pat::Ident(bi) => {
                        let name = bi.id.sym.to_string();
                        if cx.scopes[0].contains_key(&name) {
                            // Duplicate param name (e.g. `function(a, a=2)`): same
                            // hazard as the simple-ident case above. Bail.
                            return Err("dup_param");
                        }
                        let s = cx.next_slot;
                        cx.next_slot += 1;
                        cx.scopes[0].insert(name, s);
                        defaults.push((s, &ap.right));
                    }
                    p @ (Pat::Array(_) | Pat::Object(_)) => {
                        let s = cx.next_slot;
                        cx.next_slot += 1;
                        destructure_params.push((s, p, Some(&ap.right)));
                    }
                    _ => return Err("default_destructure"),
                }
            }
            // Destructuring param without an outer default.
            p @ (Pat::Array(_) | Pat::Object(_)) => {
                let s = cx.next_slot;
                cx.next_slot += 1;
                destructure_params.push((s, p, None));
            }
            _ => {
                // `using` / other unmodeled param shapes: out of scope.
                return Err("param_pattern");
            }
        }
    }
    // Snapshot of param-name -> slot, taken before body locals AND destructuring-
    // param leaves are slotted, so the default-scope check below treats a leaf
    // reference as a body-local reference (conservative bail, never a miscompile).
    let param_slots: HashMap<String, u32> = cx.scopes[0].clone();

    // Slot destructuring-param leaf bindings now — after the contiguous positional
    // slots (0..pcount) and before body locals, into the function frame so a body
    // `var` of the same name reuses the leaf's slot.
    for (_, pat, _) in &destructure_params {
        slot_pat_leaves_func(&mut cx, pat);
    }
    if let Some(r) = cx.bail_reason {
        return Err(r);
    }
    // 2. Body-declared locals (and, in the same walk, the count_temps pre-pass).
    //    The reserved temp pool must cover both the body's needs and the (separate,
    //    prologue-only) destructuring-param destructure — they never overlap, so
    //    the max suffices.
    let reserved;
    {
        let mut dc = DeclCollector {
            cx: &mut cx,
            temp_cur: 0,
            temp_max: 0,
        };
        body.visit_with(&mut dc);
        // Param prologue temps (destructuring params + spreads in any param default)
        // run before the body, so the pool just needs to cover the larger of the two.
        let param_temp = params
            .iter()
            .map(|p| pat_temp_count(&p.pat))
            .max()
            .unwrap_or(0);
        reserved = dc.temp_max.max(param_temp);
    }
    if let Some(r) = cx.bail_reason {
        return Err(r);
    }

    // 2a'. D2 `arguments` materialization. If the body references `arguments` and it
    //      is NOT shadowed by an explicit param/`var` binding of that name (which the
    //      steps above would have slotted into the function frame), allocate a fresh
    //      function-frame slot for it and remember to load the arguments object.
    //      Binding the name `"arguments"` here (a local slot, < cap_floor) makes every
    //      `arguments` read resolve via `lookup` to this slot — never a capture. The
    //      sloppy-aliasing soundness bail lives in `eligibility::classify_body`; by the
    //      time we get here the function is known not to observe aliasing.
    let arg_slot: Option<u32> = if !opts.native_parameters && uses_arguments(body) {
        if cx.scopes[0].contains_key("arguments") {
            // An explicit `arguments` binding shadows the implicit object. A PARAM
            // named `arguments` is a genuine, soundly-modeled shadow (its slot is
            // filled by the interpreter's positional copy), so references resolve
            // to it and we emit no implicit arguments binding. A `var`/`let`/`const arguments` shadow,
            // by contrast, has a sloppy-mode initial value (the arguments object)
            // that the flat-slot model can't reproduce — bail rather than diverge.
            if declares_arguments(body) {
                return Err("arguments_var_shadow");
            }
            None
        } else {
            let s = cx.next_slot;
            cx.next_slot += 1;
            cx.scopes[0].insert("arguments".to_string(), s);
            Some(s)
        }
    } else {
        None
    };

    // 2b. Reserve the anonymous temp-slot pool. It MUST sit after body locals and
    // before captures so captures stay the last contiguous slots (the thunk /
    // interpreter compute `cap_start = slots - captures.len()`). `reserved` comes
    // from count_temps (0 today; non-zero once for-of/for-in/destructuring land).
    cx.temp_base = cx.next_slot;
    cx.next_slot += reserved;
    cx.temp_top = cx.temp_base;

    // 2c. Snapshot the param+local+temp boundary. Captures are allocated lazily
    // during prologue/body emission (steps 3–4), so any slot >= this value is a
    // capture.
    cx.cap_floor = cx.next_slot;

    // 2d. Default-param scope soundness. A default expression is evaluated in
    //     the *parameter* scope: it sees earlier params but NOT body var/let
    //     bindings (which live in the body scope) and NOT its own / later params
    //     (TDZ). Our flat slot model would otherwise resolve such a reference to
    //     the wrong binding (e.g. a body local's uninitialized slot instead of
    //     the outer binding it should capture), silently diverging. Bail on
    //     those rather than miscompile.
    for (own_slot, default) in &defaults {
        default_scope_ok(
            default,
            *own_slot,
            &param_slots,
            &cx.scopes[0],
            &cx.block_local_names,
        )?;
    }
    for (own_slot, _, default) in &destructure_params {
        if let Some(default) = default {
            default_scope_ok(
                default,
                *own_slot,
                &param_slots,
                &cx.scopes[0],
                &cx.block_local_names,
            )?;
        }
    }

    // 3. Default-param init prologue: `if (L[s] === undefined) L[s] = <default>`.
    //    Runs before the body and after all param/local slots exist. Captures
    //    referenced by a default are allocated here (still after locals).
    for (slot, default) in &defaults {
        cx.emit(Instr::LoadLocal(*slot));
        cx.emit(Instr::PushUndef);
        cx.emit(Instr::Bin(7)); // ===
        let skip = cx.code.len();
        cx.emit(Instr::JumpIfFalse(u32::MAX));
        emit_expr(&mut cx, default);
        cx.emit(Instr::StoreLocal(*slot));
        cx.emit(Instr::Pop);
        let here = cx.here();
        patch(&mut cx, skip, here);
        if let Some(r) = cx.bail_reason {
            return Err(r);
        }
    }

    // 3a. Destructuring-param prologue: destructure each `[..]`/`{..}` param from
    //     its positional slot (the interpreter already copied the raw arg there).
    //     With an outer default (`[a] = d`), apply it first. Runs after simple
    //     defaults so a destructure referencing an earlier simple param sees it.
    for (slot, pat, default) in &destructure_params {
        match default {
            Some(default) => {
                // Apply the outer default to the arg, then bind the (defaulted) value.
                cx.emit(Instr::LoadLocal(*slot));
                emit_value_default(&mut cx, default);
                emit_bind_target(&mut cx, pat);
            }
            None => match pat {
                // Destructure straight from the positional slot (no source temp).
                Pat::Array(arr) => {
                    cx.emit(Instr::LoadLocal(*slot));
                    emit_destructure_array(&mut cx, arr);
                }
                Pat::Object(obj) => emit_destructure_object(&mut cx, obj, *slot),
                _ => unreachable!("destructure_params holds only array/object patterns"),
            },
        }
        if let Some(r) = cx.bail_reason {
            return Err(r);
        }
    }

    // 3b. Rest-param prologue: `L[restSlot] = arguments.slice(fixedCount)`. Runs
    //     after the default prologue (a rest param can follow defaulted params)
    //     and before the body. `fixedCount` is the positional param count.
    let pcount = (param_count - if rest_slot.is_some() { 1 } else { 0 }) as u32;
    if let Some(rslot) = rest_slot {
        cx.emit(Instr::LoadRest(pcount));
        cx.emit(Instr::StoreLocal(rslot));
        cx.emit(Instr::Pop);
    }

    // The actual arguments object retains its identity and property descriptors.
    if let Some(aslot) = arg_slot {
        cx.emit(Instr::LoadArguments);
        cx.emit(Instr::StoreLocal(aslot));
        cx.emit(Instr::Pop);
    }

    // 3d. D5 nested function DECLARATIONS are hoisted: a `function inc(){…}` at the
    //     body's top level binds `inc` (function-scoped) to its closure, visible
    //     throughout the body (including before the textual declaration). We bail on
    //     a `function`-decl nested inside a block/loop (its scoping is mode-dependent
    //     and the flat-slot model can't reproduce sloppy block-hoisting precisely).
    //     Each top-level fn-decl gets a function-frame slot, then its closure is
    //     built (MakeClosure) and stored into that slot here, before the body.
    check_no_nested_block_fn_decls(&body.stmts)?;
    // The fn-decl NAMES were already slotted (function-scoped) by `DeclCollector`
    // (see its `visit_fn_decl`), so they sit below `cap_floor` like `var`s; here we
    // just collect each top-level fn-decl with its slot for the hoisted emission.
    let mut fn_decl_slots: Vec<(u32, &FnDecl)> = Vec::new();
    for stmt in &body.stmts {
        if let Stmt::Decl(Decl::Fn(fd)) = stmt {
            let slot = *cx.scopes[0]
                .get(fd.ident.sym.as_ref())
                .expect("fn-decl slotted");
            fn_decl_slots.push((slot, fd));
        }
    }

    // 4. Emit body. The function body's own top-level `let`/`const` bindings live
    //    in the function frame (`scopes[0]`); populate it from their
    //    `DeclCollector`-allocated slots before emission so a top-level `let x`
    //    resolves to its slot (D3). Nested blocks push their own frames as they are
    //    emitted. Captures are allocated lazily here.
    let top_lows = direct_block_bindings(&body.stmts);
    bind_lows_in_scope(&mut cx, &top_lows);

    // 4a. D5 in-VM cell SEEDING. Each boxed local (captured-and-mutated by a nested
    //     closure) must hold a one-element cell `[v]` before any closure builder or
    //     body statement runs. This runs AFTER `bind_lows_in_scope` so a body-top-
    //     level `let` (slotted in `decl_slots`, bound just above) is resolvable:
    //       * a boxed PARAM is boxed in place — `MakeCell(slot)` wraps the already-
    //         copied argument (`L[slot] = [L[slot]]`);
    //       * a boxed `var`/`let`/fn-decl local is seeded to `[undefined]`
    //         (`PushUndef; StoreLocal; MakeCell`) so its cell exists before its
    //         declaration; its `= init` then writes THROUGH the cell (`StoreCell`).
    //     `MakeClosure` later threads the SLOT VALUE (the cell) to each capturing
    //     child, so the parent frame and every closure share one mutable array.
    if !cx.boxed_locals.is_empty() {
        let boxed_names: Vec<String> = cx.boxed_locals.iter().cloned().collect();
        // Stable, deterministic order: resolve each name's slot, sort by slot.
        let mut seeds: Vec<(u32, bool)> = boxed_names
            .iter()
            .filter_map(|name| {
                cx.lookup(name)
                    .filter(|slot| !cx.lexical_slots.contains_key(slot))
                    .map(|slot| (slot, param_slots.contains_key(name)))
            })
            .collect();
        seeds.sort_by_key(|(slot, _)| *slot);
        for (slot, is_param) in seeds {
            if is_param {
                cx.emit(Instr::MakeCell(slot));
            } else {
                cx.emit(Instr::PushUndef);
                cx.emit(Instr::StoreLocal(slot));
                cx.emit(Instr::Pop);
                cx.emit(Instr::MakeCell(slot));
            }
        }
    }

    // Hoisted fn-decl closures: build each closure and store it into its slot before
    // the body runs. Live descriptors preserve reads after later initialization.
    for (slot, fd) in &fn_decl_slots {
        let Some(fbody) = &fd.function.body else {
            return Err("fn_decl_no_body");
        };
        let self_name = fd.ident.sym.to_string();
        let pats: Vec<Pat> = fd.function.params.iter().map(|p| p.pat.clone()).collect();
        // If the fn-decl name is itself a boxed local (a sibling closure captures and
        // reassigns it), store the closure THROUGH the cell so the sibling sees it.
        let celled = cx.is_boxed_local(&self_name);
        emit_nested_closure(
            &mut cx,
            &pats,
            fbody,
            false,
            fd.function.is_async,
            fd.function.is_generator,
            Some(&self_name),
        );
        cx.emit(if celled {
            Instr::StoreCell(*slot)
        } else {
            Instr::StoreLocal(*slot)
        });
        cx.emit(Instr::Pop);
        if let Some(r) = cx.bail_reason {
            return Err(r);
        }
    }

    for stmt in &body.stmts {
        emit_stmt(&mut cx, stmt);
        if let Some(r) = cx.bail_reason {
            return Err(r);
        }
    }

    // Terminator: guarantee every program ends in Ret so fall-off-end and
    // loop/if-exit jumps that target one-past-end land here (return undefined).
    cx.emit(Instr::PushUndef);
    cx.emit(Instr::Ret);

    let slots = cx.next_slot;
    Ok(Compiled {
        code: cx.code,
        consts: cx.consts,
        captures: cx.captures,
        slots,
        pcount,
        children: cx.children,
    })
}

/// Scan the frame stack from innermost outward, returning the index of the first
/// frame whose kind satisfies `kind_ok` AND whose label matches `label`.
///
/// For an unlabeled jump (`label == None`) any in-scope frame matches on label,
/// so this returns the nearest frame of an accepted kind. For a labeled jump it
/// returns the nearest frame carrying exactly that label that also satisfies
/// `kind_ok`. Returns `None` if nothing matches.
///
/// `find_break_target` and `find_continue_target` share this one scan (DRY); the
/// only difference is which kinds are acceptable for the jump.
pub(crate) fn find_target(
    cx: &Cx<'_>,
    label: &Option<String>,
    kind_ok: impl Fn(&FrameKind) -> bool,
) -> Option<usize> {
    cx.frames.iter().enumerate().rev().find_map(|(i, f)| {
        let label_ok = match label {
            Some(_) => f.label.as_ref() == label.as_ref(),
            None => true,
        };
        if kind_ok(&f.kind) && label_ok {
            Some(i)
        } else {
            None
        }
    })
}

/// Target frame for a `break`: per JS, an unlabeled `break` targets the nearest
/// enclosing `Loop` OR `Switch`. (`Block` frames are only break targets via an
/// explicit label, handled by the label-aware match below.)
pub(crate) fn find_break_target(cx: &Cx<'_>, label: &Option<String>) -> Option<usize> {
    match label {
        // Labeled break may target any labeled construct (loop/switch/block).
        Some(_) => find_target(cx, label, |_| true),
        // Unlabeled break: nearest loop or switch.
        None => find_target(cx, label, |k| {
            matches!(k, FrameKind::Loop | FrameKind::Switch)
        }),
    }
}

/// Target frame for a `continue`: per JS, `continue` targets a `Loop` only
/// (never a `Switch`). A labeled continue must still resolve to a loop carrying
/// that label.
pub(crate) fn find_continue_target(cx: &Cx<'_>, label: &Option<String>) -> Option<usize> {
    find_target(cx, label, |k| matches!(k, FrameKind::Loop))
}

/// Emit a statement list as a fresh **lexical block scope** (D3): push a scope
/// frame, populate it with this block's direct `let`/`const` bindings (resolved to
/// their `DeclCollector`-allocated slots), emit each statement, then pop the frame —
/// restoring any outer binding the block shadowed. The bindings are hoisted into
/// the frame up front (not at each decl point) so a reference earlier in the block
/// resolves to the inner slot; its accessor throws until declaration initialization.
/// The block-scoped (`let`/`const`) bindings introduced by a `for`/`for-in`/
/// `for-of` head, as `(name, span_lo)` pairs — empty for a `var`/expression head
/// (those are function-scoped or pre-existing). The loop arms push a scope frame
/// holding these so a `for (let i …)` head that shadows an outer `i` resolves to
/// its own slot for the whole loop (D3).
pub(crate) fn for_var_decl_block_bindings(v: &VarDecl) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    if matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const) {
        for d in &v.decls {
            collect_pat_binding_lows(&d.name, &mut out);
        }
    }
    out
}

/// The `(name, span_lo)` block-scoped bindings of a `for-in`/`for-of` head
/// (`for (let x of …)`); empty for a `var`/pattern head.
pub(crate) fn for_head_block_bindings(head: &ForHead) -> Vec<(String, u32)> {
    match head {
        ForHead::VarDecl(v) => for_var_decl_block_bindings(v),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{compile_body, compile_body_boxed};
    use crate::isa::Instr;
    use crate::test_support::parse_fn_with_params;

    #[test]
    fn compiles_arithmetic_return() {
        let (params, body) = parse_fn_with_params("function(a,b){ var c = a + b; return c * 2; }");
        let prog = compile_body(&params, &body).expect("eligible");
        assert!(
            prog.code
                .iter()
                .filter(|i| matches!(i, Instr::LoadLocal(_)))
                .count()
                >= 3
        );
        assert!(prog.code.iter().any(|i| matches!(i, Instr::Ret)));
    }

    #[test]
    fn captures_outer_readonly_binding() {
        let (params, body) = parse_fn_with_params("function(x){ return x + k; }");
        let prog = compile_body(&params, &body).expect("eligible");
        assert_eq!(prog.captures, vec!["k".to_string()]);
    }

    #[test]
    fn compiles_let_const_shadowing() {
        // D3: a `let` in a nested block shadowing an outer `let` of the same name is
        // now eligible — each binding gets its own slot via the scope stack, so the
        // inner shadow never clobbers the outer. The function must compile (was
        // previously bailed `let_const_shadow`).
        let (params, body) =
            parse_fn_with_params("function(){ let x = 1; { let x = 2; } return x; }");
        let prog = compile_body(&params, &body).expect("shadowing must now compile (D3)");
        // The outer and inner `x` occupy DISTINCT slots (no reuse in v1), so the
        // function frame holds at least two locals.
        assert!(
            prog.slots >= 2,
            "shadow must allocate a distinct slot, got {}",
            prog.slots
        );
    }

    #[test]
    fn compiles_single_let_and_var_redecl() {
        // single let -> compiles
        let (p1, b1) = parse_fn_with_params("function(n){ let s = 0; return s + n; }");
        assert!(compile_body(&p1, &b1).is_ok());
        // var re-declaration of same name is fine (hoisted, same binding)
        let (p2, b2) = parse_fn_with_params("function(){ var i = 0; var i = 1; return i; }");
        assert!(compile_body(&p2, &b2).is_ok());
    }

    #[test]
    fn skips_write_to_captured_binding() {
        // `counter` is a free (captured) identifier; assigning to it cannot be
        // modelled (no write-back), so the function must be skipped.
        let (p, b) = parse_fn_with_params("function(){ counter = counter + 1; return counter; }");
        assert!(compile_body(&p, &b).is_err());
    }

    #[test]
    fn skips_compound_and_update_to_captured_binding() {
        let (p1, b1) = parse_fn_with_params("function(){ total += 5; return total; }");
        assert!(compile_body(&p1, &b1).is_err());
        let (p2, b2) = parse_fn_with_params("function(){ k++; return k; }");
        assert!(compile_body(&p2, &b2).is_err());
    }

    #[test]
    fn still_compiles_writes_to_params_and_locals() {
        // Writing params and locals is sound and must still compile.
        let (p1, b1) = parse_fn_with_params("function(a){ a = a + 1; return a; }");
        assert!(compile_body(&p1, &b1).is_ok());
        let (p2, b2) = parse_fn_with_params(
            "function(n){ var s = 0; for (var i = 0; i < n; i++) { s += i; } return s; }",
        );
        assert!(compile_body(&p2, &b2).is_ok());
    }

    #[test]
    fn frame_refactor_preserves_loop_compilation() {
        // while + for + do/while + nested break/continue still compile unchanged.
        let (p, b) = parse_fn_with_params(
            "function(n){var s=0;for(var i=0;i<n;i++){if(i==2){continue;}if(i==5){break;}s+=i;}\
             var k=0;while(k<n){k++;}do{s++;}while(s<3);return s;}",
        );
        assert!(compile_body(&p, &b).is_ok());
    }

    #[test]
    fn bails_on_exponential_numeric_object_key() {
        // {1e21: 1} would stringify differently in Rust vs JS — must bail, not miscompile.
        let (p, b) = parse_fn_with_params("function(){ var o = { 1e21: 1 }; return o; }");
        assert!(matches!(compile_body(&p, &b), Err("numeric_prop_key")));
        // A safe small-integer key still compiles.
        let (p2, b2) = parse_fn_with_params("function(){ var o = { 42: 1, 0: 2 }; return o[42]; }");
        assert!(compile_body(&p2, &b2).is_ok());
    }

    #[test]
    fn op_tables_total_for_supported_ops() {
        use swc_core::ecma::ast::{BinaryOp, UnaryOp};
        assert_eq!(crate::isa::bin_op_code(BinaryOp::Add), Some(0));
        assert_eq!(crate::isa::bin_op_code(BinaryOp::ZeroFillRShift), Some(19));
        assert_eq!(crate::isa::bin_op_code(BinaryOp::LogicalAnd), None);
        assert_eq!(crate::isa::un_op_code(UnaryOp::TypeOf), Some(3));
        assert_eq!(crate::isa::un_op_code(UnaryOp::Plus), Some(5));
        assert_eq!(crate::isa::un_op_code(UnaryOp::Delete), None);
    }

    // ---- Phase-8 coverage batch: each construct compiles (positive) and the
    // out-of-scope sibling still bails with a clean reason (negative). ----

    #[test]
    fn compiles_default_params() {
        let (p, b) = parse_fn_with_params("function(a, b = 2){ return a + b; }");
        assert!(compile_body(&p, &b).is_ok());
        // default referencing an earlier param is allowed.
        let (p2, b2) = parse_fn_with_params("function(a, b = a + 1){ return b; }");
        assert!(compile_body(&p2, &b2).is_ok());
    }

    #[test]
    fn rejects_duplicate_param_names() {
        // sloppy-mode `function(a, a)`: JS binds the last `a`; the flat slot model
        // maps to the first and would mis-cap the arg copy. Must bail.
        let (p, b) = parse_fn_with_params("function(a, a){ return a; }");
        assert!(matches!(compile_body(&p, &b), Err("dup_param")));
        let (p2, b2) = parse_fn_with_params("function(a, a = 2){ return a; }");
        assert!(matches!(compile_body(&p2, &b2), Err("dup_param")));
    }

    #[test]
    fn compiles_destructured_default_param() {
        // A destructuring param with an outer default (`{a} = {}`) is now lowered:
        // apply the default to the arg, then destructure it.
        let (p, b) = parse_fn_with_params("function({a} = {}){ return a; }");
        assert!(compile_body(&p, &b).is_ok());
        // A destructuring param without a default lowers too.
        let (p2, b2) = parse_fn_with_params("function([a, b]){ return a + b; }");
        assert!(compile_body(&p2, &b2).is_ok());
    }

    #[test]
    fn compiles_rest_param() {
        // `function(first, ...rest)` lowers to a LoadRest prologue; pcount excludes
        // the rest slot so the positional arg copy fills only the fixed params.
        let (p, b) = parse_fn_with_params(
            "function(first, ...rest){ var t = first; \
             for (var i = 0; i < rest.length; i++) { t += rest[i]; } return t; }",
        );
        let prog = compile_body(&p, &b).expect("rest param must compile");
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::LoadRest(1))),
            "rest param must emit LoadRest(fixedCount=1)"
        );
        assert_eq!(prog.pcount, 1, "pcount must exclude the rest slot");
        // A rest param with a destructuring target is out of scope -> bail.
        let (p2, b2) = parse_fn_with_params("function(...[a, b]){ return a + b; }");
        assert!(matches!(compile_body(&p2, &b2), Err("rest_pattern")));
        // A plain function (no rest) reports pcount == params.len().
        let (p3, b3) = parse_fn_with_params("function(a, b){ return a + b; }");
        assert_eq!(compile_body(&p3, &b3).unwrap().pcount, 2);
    }

    #[test]
    fn default_referencing_outer_binding_captures_not_local() {
        // `k` is a free outer binding -> default captures it (read-only). Sound.
        let (p, b) = parse_fn_with_params("function(a, b = k){ return a + b; }");
        let prog = compile_body(&p, &b).expect("default capturing outer is fine");
        assert!(prog.captures.contains(&"k".to_string()));
    }

    #[test]
    fn rejects_default_referencing_body_local() {
        // A default must NOT resolve to a body var/let local. In real JS the
        // default sees the OUTER `c`, not the body `var c` (which is invisible in
        // the parameter scope). Our flat slot model would wrongly read the local
        // slot, so we must bail rather than miscompile to a silent wrong value.
        let (p, b) = parse_fn_with_params("function(a, b = c){ var c = 5; return b; }");
        assert!(matches!(
            compile_body(&p, &b),
            Err("default_refs_body_local")
        ));
        // `let`-declared body local is the same divergence.
        let (p2, b2) = parse_fn_with_params("function(a, b = c){ let c = 5; return b; }");
        assert!(matches!(
            compile_body(&p2, &b2),
            Err("default_refs_body_local")
        ));
    }

    #[test]
    fn rejects_default_referencing_self_or_later_param() {
        // self-reference (TDZ in real JS).
        let (p, b) = parse_fn_with_params("function(a = a){ return a; }");
        assert!(matches!(
            compile_body(&p, &b),
            Err("default_refs_later_param")
        ));
        // later-param reference (TDZ in real JS); could return a wrong value.
        let (p2, b2) = parse_fn_with_params("function(a = b, b){ return a; }");
        assert!(matches!(
            compile_body(&p2, &b2),
            Err("default_refs_later_param")
        ));
    }

    #[test]
    fn default_using_body_local_name_only_as_property_is_fine() {
        // `c` here is a property key, not a binding read, so it must NOT trigger
        // the body-local bail even though a body local named `c` exists.
        let (p, b) = parse_fn_with_params("function(o, b = o.c){ var c = 5; return b + c; }");
        assert!(compile_body(&p, &b).is_ok());
    }

    #[test]
    fn captures_resolve_without_quadratic_scan() {
        // Compile a fn with several free vars and assert it compiles + captures
        // are recorded correctly. This is the behavioral guard for the cap_floor
        // O(1) refactor: the result must be identical to the old linear scan.
        let (p, b) = parse_fn_with_params("function(a){ return a + x + y + z + w + v; }");
        let prog = compile_body(&p, &b).expect("should compile");
        // x, y, z, w, v are free vars → 5 captures
        assert_eq!(prog.captures.len(), 5);
        // 'a' is a param → must NOT appear in captures
        assert!(!prog.captures.contains(&"a".to_string()));
        // All capture names present
        for name in &["x", "y", "z", "w", "v"] {
            assert!(
                prog.captures.contains(&name.to_string()),
                "missing capture {name}"
            );
        }
    }

    #[test]
    fn compiles_template_literal() {
        let (p, b) = parse_fn_with_params("function(x){ return `v=${x}!`; }");
        assert!(compile_body(&p, &b).is_ok());
        // empty template.
        let (p2, b2) = parse_fn_with_params("function(){ return ``; }");
        assert!(compile_body(&p2, &b2).is_ok());
    }

    #[test]
    fn compiles_tagged_template() {
        // D4: tagged templates are now virtualized — lowered to a cached
        // `Const::TemplateObject` pushed as the first arg + each substitution +
        // a call on the tag. Both bare and member-tag forms compile.
        let (p, b) = parse_fn_with_params("function(t,x){ return t`a${x}b`; }");
        let prog = compile_body(&p, &b).expect("bare tag eligible");
        assert!(
            prog.consts
                .iter()
                .any(|c| matches!(c, crate::chunk::Const::TemplateObject { .. })),
            "a TemplateObject const must be emitted"
        );
        // member tag `o.t`…`` (receiver-threaded via CallResolved).
        let (p2, b2) = parse_fn_with_params("function(o,x){ return o.t`a${x}`; }");
        assert!(compile_body(&p2, &b2).is_ok());
        // tag with no substitutions.
        let (p3, b3) = parse_fn_with_params("function(t){ return t`only`; }");
        assert!(compile_body(&p3, &b3).is_ok());
    }

    #[test]
    fn compiles_do_while() {
        let (p, b) = parse_fn_with_params(
            "function(n){ var s = 0; var i = 0; do { s = s + i; i = i + 1; } while (i < n); return s; }",
        );
        assert!(compile_body(&p, &b).is_ok());
    }

    #[test]
    fn compiles_switch() {
        // The `classify` body: fall-through (case 1 -> case 2), a `break`
        // targeting the switch, and a `default` in the MIDDLE of the cases.
        let (p, b) = parse_fn_with_params(
            "function(x){ var out = \"\"; switch (x) { case 1: out += \"one\"; \
             case 2: out += \"<=2\"; break; default: out += \"def\"; \
             case 3: out += \"three\"; break; } return out; }",
        );
        assert!(compile_body(&p, &b).is_ok());
        // Empty switch and default-only switch must compile gracefully.
        let (p2, b2) = parse_fn_with_params("function(x){ switch (x) {} return x; }");
        assert!(compile_body(&p2, &b2).is_ok());
        let (p3, b3) = parse_fn_with_params(
            "function(x){ var r = 0; switch (x) { default: r = 9; } return r; }",
        );
        assert!(compile_body(&p3, &b3).is_ok());
        // A `break` inside a switch nested in a loop must target the switch (the
        // loop keeps iterating); a `continue` must skip the switch and reach the
        // loop. Both must compile.
        let (p4, b4) = parse_fn_with_params(
            "function(n){ var s = 0; for (var i = 0; i < n; i++) { switch (i) { \
             case 0: continue; case 1: break; default: s += i; } s += 100; } return s; }",
        );
        assert!(compile_body(&p4, &b4).is_ok());
    }

    #[test]
    fn compiles_labeled() {
        // The `f` body from the vm_labeled fixture: a labeled outer loop with a
        // nested loop doing `continue outer`/`break outer`, plus a labeled block
        // with a `break blk`. All label targets must resolve (no bail).
        let (p, b) = parse_fn_with_params(
            "function(){ var out = []; \
             outer: for (var i = 0; i < 3; i++) { \
               for (var j = 0; j < 3; j++) { \
                 if (j === 1) continue outer; \
                 if (i === 2) break outer; \
                 out.push(i + \":\" + j); \
               } \
             } \
             blk: { out.push(\"A\"); if (out.length) break blk; out.push(\"B\"); } \
             return out.join(\",\"); }",
        );
        assert!(compile_body(&p, &b).is_ok());
    }

    #[test]
    fn compiles_sequential_labeled_constructs() {
        // Two labeled constructs in sequence: the loop must consume its own label and
        // NOT leave a stale pending_label that the later block (or any later loop) absorbs.
        let (p, b) = parse_fn_with_params(
            "function(n){ var s=0; a: for(var i=0;i<n;i++){ if(i===1) continue a; s+=i; } \
             b: { s+=10; if(s>0) break b; s+=100; } return s; }",
        );
        assert!(compile_body(&p, &b).is_ok());
    }

    #[test]
    fn compiles_nullish_sequence_shorthand_unary_plus() {
        let (p, b) = parse_fn_with_params("function(a, b){ return a ?? b; }");
        assert!(compile_body(&p, &b).is_ok());
        let (p2, b2) = parse_fn_with_params("function(a){ return (a, a + 1); }");
        assert!(compile_body(&p2, &b2).is_ok());
        let (p3, b3) = parse_fn_with_params("function(a, b){ return { a, b }; }");
        assert!(compile_body(&p3, &b3).is_ok());
        let (p4, b4) = parse_fn_with_params("function(a){ return +a; }");
        assert!(compile_body(&p4, &b4).is_ok());
    }

    #[test]
    fn compiles_optional_chain() {
        // The `f` body from the vm_optional_chain fixture: an optional member
        // chain (`o?.a?.b`), an optional method call whose args must not run when
        // the base is nullish (`o?.m(log.push("x"))`), and a loose-null compare.
        let (p, b) = parse_fn_with_params(
            "function(o){ var log = []; var a = o?.a?.b; var c = o?.m(log.push(\"x\")); \
             var d = (o == null) ? \"skip\" : \"ran\"; \
             return [String(a), String(c), d, log.join(\",\")].join(\"|\"); }",
        );
        assert!(compile_body(&p, &b).is_ok(), "optional chain must compile");
        // Common sub-shapes each compile.
        for src in &[
            "function(o){ return o?.a; }",
            "function(o){ return o?.a.b; }",
            "function(o,k){ return o?.[k]; }",
            "function(o){ return o.a?.b; }",
            "function(f){ return f?.(); }",
            "function(o){ return o?.m(1); }",
        ] {
            let (pp, bb) = parse_fn_with_params(src);
            assert!(compile_body(&pp, &bb).is_ok(), "should compile: {src}");
        }
    }

    #[test]
    fn bails_on_optional_call_of_method() {
        // `o.m?.()` would strand the receiver under the short-circuited
        // `undefined` at END; we bail rather than miscompile.
        let (p, b) = parse_fn_with_params("function(o){ return o.m?.(); }");
        assert!(matches!(
            compile_body(&p, &b),
            Err("optional_chain_unsupported")
        ));
    }

    #[test]
    fn compiles_delete() {
        // `delete o.k` and `delete o[k]` both lower to DeleteProp.
        let (p, b) = parse_fn_with_params(
            "function(o){ var a = delete o.k; var b = delete o[\"m\"]; return [a, b]; }",
        );
        let prog = compile_body(&p, &b).expect("delete of member must compile");
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::DeleteProp)),
            "delete must emit a DeleteProp opcode"
        );
    }

    #[test]
    fn compiles_throw() {
        let (p, b) = parse_fn_with_params("function(x){ if (x < 0) { throw \"neg\"; } return x; }");
        let prog = compile_body(&p, &b).expect("throw must compile");
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::Throw)),
            "throw must emit a Throw opcode"
        );
    }

    #[test]
    fn compiles_for_in() {
        // `for (var k in o)` lowers to an EnumKeys snapshot + indexed loop, with a
        // `break`/`continue` inside the body resolving to the loop frame.
        let (p, b) = parse_fn_with_params(
            "function(o){ var out = \"\"; for (var k in o) { if (k === \"x\") continue; \
             if (k === \"z\") break; out += k; } return out; }",
        );
        let prog = compile_body(&p, &b).expect("for-in must compile");
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::EnumKeys)),
            "for-in must emit an EnumKeys opcode"
        );
        // A plain-ident head writing a captured outer binding is unsound -> bail.
        let (p2, b2) = parse_fn_with_params("function(o){ for (k in o) { } return k; }");
        assert!(matches!(compile_body(&p2, &b2), Err("mutable_capture")));
        // A destructuring head (each key string is itself destructured) now lowers
        // in both the `var`-decl and bare assignment-target forms.
        let (p3, b3) = parse_fn_with_params("function(o){ for (var [a] in o) { } return a; }");
        assert!(compile_body(&p3, &b3).is_ok());
        let (p4, b4) = parse_fn_with_params("function(o){ var a; for ([a] in o) { } return a; }");
        assert!(compile_body(&p4, &b4).is_ok());
    }

    #[test]
    fn compiles_try_catch() {
        // try/catch lowers to a PushHandler/PopHandler pair with the catch binding
        // slotted as a local.
        let (p, b) = parse_fn_with_params(
            "function(x){ var out = \"\"; try { out += \"t\"; if (x < 0) { throw \"e\"; } } \
             catch (e) { out += e; } return out; }",
        );
        let prog = compile_body(&p, &b).expect("try/catch must compile");
        assert!(
            prog.code
                .iter()
                .any(|i| matches!(i, Instr::PushHandler(_, _))),
            "try must emit a PushHandler"
        );
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::PopHandler)),
            "try normal completion must emit a PopHandler"
        );
        // try/finally and try/catch/finally compile, emitting a finally
        // (EndFinally) terminal.
        let (p2, b2) = parse_fn_with_params(
            "function(){ var r; try { r = 1; } finally { r = 2; } return r; }",
        );
        let prog2 = compile_body(&p2, &b2).expect("try/finally must compile");
        assert!(
            prog2.code.iter().any(|i| matches!(i, Instr::EndFinally)),
            "try/finally must emit an EndFinally"
        );
        let (pf, bf) = parse_fn_with_params(
            "function(x){ var o=\"\"; try { o+=\"t\"; } catch(e){ o+=\"c\"; } finally { o+=\"f\"; } return o; }",
        );
        assert!(
            compile_body(&pf, &bf).is_ok(),
            "try/catch/finally must compile"
        );
        // D3: a catch binding shadowing an outer param `e` is now eligible — the
        // catch param gets its own block-scoped slot, distinct from the param's, so
        // the shadow is sound (was previously bailed `catch_shadow`).
        let (p3, b3) = parse_fn_with_params("function(e){ try { f(); } catch (e) { return e; } }");
        let prog3 = compile_body(&p3, &b3).expect("catch shadowing must now compile (D3)");
        assert!(
            prog3.slots >= 2,
            "catch shadow must allocate a distinct slot, got {}",
            prog3.slots
        );
    }

    #[test]
    fn compiles_for_of() {
        // for-of lowers to GetIter + an IterStep loop wrapped in a close handler.
        let (p, b) = parse_fn_with_params(
            "function(arr){ var s = 0; for (var v of arr) { if (v < 0) break; s += v; } return s; }",
        );
        let prog = compile_body(&p, &b).expect("for-of must compile");
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::GetIter)),
            "for-of must emit GetIter"
        );
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::IterStep)),
            "for-of must emit IterStep"
        );
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::IterClose)),
            "for-of must emit IterClose (close-on-abrupt handler)"
        );
        // A `break` inside for-of crosses the close handler -> BreakUnwind.
        assert!(
            prog.code
                .iter()
                .any(|i| matches!(i, Instr::BreakUnwind(_, _))),
            "break inside for-of must unwind the close handler"
        );
        // A destructuring loop head binds each value via the array destructure.
        let (p2, b2) =
            parse_fn_with_params("function(arr){ for (var [a, b] of arr) { f(a, b); } }");
        assert!(compile_body(&p2, &b2).is_ok());
    }

    #[test]
    fn compiles_obj_destructure() {
        // Object destructuring (rename + default + rest) lowers to GetProp reads,
        // an Object.assign(+DeleteProp) rest copy, and the assignment-target form.
        let (p, b) = parse_fn_with_params(
            "function(s){ var { a, b: bb, e = 9, ...rest } = s; var x; ({ c: x } = s); \
             return a + bb + e + x + rest.d; }",
        );
        let prog = compile_body(&p, &b).expect("object destructuring must compile");
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::RestProps)),
            "object-rest must exclude taken keys before reading getters"
        );
        // A computed pattern key is out of scope (read-once temp not reserved).
        let (p2, b2) = parse_fn_with_params("function(s, k){ var { [k]: v } = s; return v; }");
        assert!(matches!(
            compile_body(&p2, &b2),
            Err("destructure_computed_key")
        ));
    }

    #[test]
    fn compiles_arr_destructure() {
        // Array destructuring (holes + default + rest + nesting) lowers to the
        // iterator opcodes wrapped in a close-on-abrupt handler.
        let (p, b) = parse_fn_with_params(
            "function(a){ var [x, , y = 7, ...rest] = a; var [[u], w] = a; \
             return x + y + rest.length + u + w; }",
        );
        let prog = compile_body(&p, &b).expect("array destructuring must compile");
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::GetIter)),
            "array destructure must take an iterator"
        );
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::IterClose)),
            "array destructure must close the iterator (on abrupt / non-exhaustion)"
        );
        // A destructuring PARAM lowers from its positional slot.
        let (p2, b2) = parse_fn_with_params("function([a, b]){ return a + b; }");
        let prog2 = compile_body(&p2, &b2).expect("destructuring param must compile");
        assert_eq!(
            prog2.pcount, 1,
            "the array param occupies one positional slot"
        );
    }

    #[test]
    fn compiles_spread() {
        // Array spread iterates each spread source (GetIter), then `.apply`s the
        // built array for a call.
        let (p, b) = parse_fn_with_params("function(g, a){ var xs = [1, ...a]; return g(...xs); }");
        let prog = compile_body(&p, &b).expect("spread must compile");
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::GetIter)),
            "spread must iterate its sources"
        );
        // `new C(...a)` lowers to a captured-global Reflect.construct call.
        let (p2, b2) = parse_fn_with_params("function(C, a){ return new C(...a); }");
        let prog2 = compile_body(&p2, &b2).expect("new-spread must compile");
        assert!(
            prog2.captures.iter().any(|c| c == "Reflect"),
            "new-spread must capture the global Reflect"
        );
        // Object spread lowers to a captured-global Object.assign call.
        let (p3, b3) = parse_fn_with_params("function(o){ return { a: 1, ...o, b: 2 }; }");
        let prog3 = compile_body(&p3, &b3).expect("object-spread must compile");
        assert!(
            prog3.code.iter().any(|i| matches!(i, Instr::CopyProps)),
            "object-spread must copy own data properties"
        );
    }

    #[test]
    fn return_inside_try_uses_unwind() {
        // A `return` while a handler is active must emit RetUnwind (not the fast
        // Ret), so any enclosing finally/close can run.
        let (p, b) =
            parse_fn_with_params("function(){ try { return 1; } catch (e) { return 2; } }");
        let prog = compile_body(&p, &b).expect("compiles");
        assert!(
            prog.code.iter().any(|i| matches!(i, Instr::RetUnwind)),
            "return inside try must emit RetUnwind"
        );
    }

    #[test]
    fn bails_on_delete_of_non_member() {
        // `delete x` (a non-member target) is a sloppy-mode no-op we don't model.
        let (p, b) = parse_fn_with_params("function(x){ return delete x; }");
        assert!(matches!(compile_body(&p, &b), Err("delete_target")));
    }

    #[test]
    fn temp_pool_sits_below_captures() {
        // A fn with a free var (capture) `K` and a body local `s`. Reserving temps
        // must keep captures the LAST slots: cap_start == slots - captures.len().
        let (p, b) = parse_fn_with_params("function(a){ var s = a + K; return s; }");
        let prog = compile_body(&p, &b).expect("compiles");
        assert_eq!(prog.captures, vec!["K".to_string()]);
        let cap_start = prog.slots - prog.captures.len() as u32;
        // The lone capture must occupy the LAST slot (captures are the final contiguous block).
        assert_eq!(cap_start, prog.slots - 1, "capture must be the last slot");
        assert!(prog.slots >= 3, "params(a)+local(s)+capture(K) at least");
    }

    // ---- D1 boxed mutable capture (compile_body_boxed) ----

    #[test]
    fn d1_unboxed_capture_write_still_bails() {
        // No-regression contract: writing a captured outer binding that is NOT in
        // the boxed set still bails `mutable_capture` (read-only capture only).
        for src in [
            "function(){ c = c + 1; return c; }", // simple assign
            "function(){ c += 1; return c; }",    // compound assign
            "function(){ c++; return c; }",       // update
        ] {
            let (p, b) = parse_fn_with_params(src);
            assert!(
                matches!(compile_body(&p, &b), Err("mutable_capture")),
                "unboxed write must bail: {src}"
            );
        }
    }

    #[test]
    fn d1_boxed_capture_write_compiles_with_cell_ops() {
        // With `c` boxed, the same writes compile and emit LoadCell/StoreCell
        // (instead of LoadLocal/StoreLocal) for the capture, so a VM write
        // propagates to the shared cell.
        let mut boxed = std::collections::HashSet::new();
        boxed.insert("c".to_string());
        for src in [
            "function(){ c = c + 1; return c; }",
            "function(){ c += 1; return c; }",
            "function(){ c++; return c; }",
        ] {
            let (p, b) = parse_fn_with_params(src);
            let prog = compile_body_boxed(&p, &b, &boxed)
                .unwrap_or_else(|e| panic!("boxed `{src}` must compile, got {e}"));
            assert_eq!(
                prog.captures,
                vec!["c".to_string()],
                "`c` is the capture: {src}"
            );
            assert!(
                prog.code.iter().any(|i| matches!(i, Instr::StoreCell(_))),
                "boxed write must emit StoreCell: {src}"
            );
            assert!(
                prog.code.iter().any(|i| matches!(i, Instr::LoadCell(_))),
                "boxed read must emit LoadCell: {src}"
            );
            // No plain LoadLocal/StoreLocal of the capture slot (it's the last slot).
            let cap_slot = prog.slots - 1;
            assert!(
                !prog.code.iter().any(
                    |i| matches!(i, Instr::LoadLocal(s) | Instr::StoreLocal(s) if *s == cap_slot)
                ),
                "boxed capture slot must never use plain Load/StoreLocal: {src}"
            );
        }
    }

    #[test]
    fn d1_boxed_set_only_affects_captures_not_locals() {
        // A boxed name that is ALSO a local (a shadow) must NOT be cell-ified: the
        // local binding wins (is_boxed_capture excludes params/locals).
        let mut boxed = std::collections::HashSet::new();
        boxed.insert("c".to_string());
        // `c` here is a body local (`var c`), not a capture — writes stay StoreLocal.
        let (p, b) = parse_fn_with_params("function(){ var c = 0; c = c + 1; return c; }");
        let prog = compile_body_boxed(&p, &b, &boxed).expect("local-c must compile");
        assert!(prog.captures.is_empty(), "`c` is a local, not a capture");
        assert!(
            !prog
                .code
                .iter()
                .any(|i| matches!(i, Instr::StoreCell(_) | Instr::LoadCell(_))),
            "a boxed name that is a local must not use cell ops"
        );
    }
}
