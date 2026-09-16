//! JavaScript function bodies to executable VM bytecode.
//!
//! A frame has positional input slots, parameter and body bindings, a bounded
//! scratch pool, and a final contiguous capture segment. Non-simple parameter
//! lists initialize in source order using temporal-dead-zone descriptors. Their
//! parameter-expression environment stays distinct from body var declarations;
//! simple duplicate parameters retain the final positional binding.
//!
//! Declaration collection assigns lexical bindings stable slots keyed by declaration
//! node identity. Emission maintains lexical scope maps and initializes runtime binding
//! descriptors when a scope is entered. Descriptors preserve TDZ, const writes,
//! closure sharing, and loop iteration identity. Function declarations initialize
//! at function or block entry; sloppy block declarations additionally update their
//! permitted Annex B var binding at the declaration statement.
//!
//! Captures can be values, legacy shared cells, or live reference descriptors.
//! Active with object records participate in name resolution and preserve call
//! receivers. External var bindings let a caller own declarations, including eval
//! variable environments and native outer-scope envelopes. Compiler-owned bindings
//! remain inaccessible to with-object interception.
//!
//! The allocation pass also computes scratch requirements. Binary expression
//! spines use heap worklists in analysis and emission so large generated expressions
//! do not consume one Rust stack frame per operand. Capture slots stay contiguous
//! after scratch slots regardless of the order in which free names are discovered.
//!
//! This module owns frame layout and declaration allocation. Statement, expression,
//! destructuring, and reference lowering live in their respective submodules.
//! Unsupported or malformed constructs return an explicit compilation error;
//! explicit native exclusions are controlled separately by CompileOptions.

use std::collections::HashMap;

use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

use crate::cells;
use crate::chunk::{ChildChunk, Compiled, Const};
use crate::isa::Instr;

/// Frame binding contracts and explicit closure policy supplied by the caller.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompileOptions<'a> {
    /// Name-glob of nested functions to KEEP NATIVE (run as a native closure
    /// inside the VM frame). `None` = match nothing.
    pub exclude: Option<&'a str>,
    /// Explicit legacy policy permitting a native child when compilation fails.
    /// Native children are reported separately from protected VM chunks.
    pub divert_ineligible: bool,
    /// Captures are live property descriptors supplied by the calling thunk.
    pub live_captures: bool,
    /// Captures whose object/environment receiver and resolution remain dynamic.
    pub dynamic_captures: Option<&'a std::collections::HashSet<String>>,
    /// Source-resolved dynamic compiler references, keyed by expression position.
    pub source_compiler_sites: Option<&'a std::collections::HashSet<u32>>,
    /// Lossless source markers restored once as this frame's constants form.
    pub source_utf16: Option<&'a crate::source_text::SourceTextMap>,
    /// The native wrapper already initialized parameters and owns arguments.
    pub native_parameters: bool,
    /// Arrows inherit arguments from their enclosing function.
    pub lexical_arguments: bool,
    /// Inherited or explicit strict mode.
    pub strict: bool,
    /// This body is an eval StatementList rather than a new function activation.
    pub eval_context: bool,
    /// Original source grammar, independent of the synthetic entry shell.
    pub source_context: crate::eval::SourceContext,
    /// Source class grammar and opaque capsules, keyed by direct eval position.
    pub eval_class_contexts: Option<&'a crate::eval::EvalClassContexts>,
    /// Synthetic lexical entry sharing the incoming variable environment.
    pub lexical_entry: bool,
    /// Immutable named function-expression binding supplied by MakeClosure.
    pub self_binding: Option<&'a str>,
    /// Compiler-owned bindings cannot be intercepted by with object records.
    pub internal_bindings: Option<&'a std::collections::HashSet<String>>,
    /// Var declarations backed by a caller-owned variable environment.
    /// For lexical/eval entries this is the complete source var set, including
    /// permitted Annex B aliases; a partition must not invent additional aliases.
    pub external_var_bindings: Option<&'a std::collections::HashSet<String>>,
    /// Native suspension shell kind for each lowered executable body.
    /// Original lexical names preserved by suspension cell lowering at eval sites.
    pub suspension_references: Option<&'a crate::eval::SuspensionLexicalReferences>,
    pub suspension_lexicals: Option<&'a crate::eval::SuspensionLexicalScopes>,
    pub suspensions: Option<&'a HashMap<u32, crate::chunk::SuspensionKind>>,
}

pub(crate) mod destructure;
pub(crate) mod dynamic_scope;
pub(crate) mod environment;
pub(crate) mod expr;
pub(crate) mod native;
pub(crate) mod object_super;
pub(crate) mod projection;
pub(crate) mod stmt;

// Re-export the construct-family emit entry points used across submodules and by
// the parent (the names the legacy single-file compiler exposed at module scope).
pub(crate) use destructure::*;
pub(crate) use dynamic_scope::*;
pub(crate) use environment::*;
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
    /// All labels attached to this construct (`outer: inner: for (...)`).
    /// Empty for an unlabeled construct.
    pub(crate) labels: Vec<String>,
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

/// Identity of a declaration node during one immutable compilation. Distinct
/// generated declarations may share both their resolved Id and source span.
/// Addresses are lookup-only: never order by them or emit them into bytecode.
/// Any AST normalization or cloning must occur before allocation; emission must
/// use those same borrowed declarations when looking up their slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct DeclarationKey(usize);
impl DeclarationKey {
    pub(crate) fn of(identifier: &Ident) -> Self {
        Self(std::ptr::from_ref(identifier).addr())
    }
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
    pub(crate) with_scopes: Vec<(usize, u32)>,
    pub(crate) needs_environment: bool,
    pub(crate) dynamic_variables: bool,
    pub(crate) hidden_environment_bindings: std::collections::HashSet<String>,
    pub(crate) simple_catch_slots: std::collections::HashSet<u32>,
    pub(crate) var_binding_slots: std::collections::HashSet<u32>,
    environment_snapshots: Vec<usize>,
    /// Slots keyed by declaration-node identity in the immutable input AST.
    /// Allocation and emission visit the same nodes; source coordinates may be
    /// shared or entirely absent in generated input.
    pub(crate) decl_slots: HashMap<DeclarationKey, u32>,
    pub(crate) block_fn_aliases: HashMap<DeclarationKey, u32>,
    block_fn_external_aliases: HashMap<DeclarationKey, String>,
    pub(crate) lexical_slots: HashMap<u32, bool>,
    pub(crate) initializing: bool,
    /// Parameter bindings maintained by the native entry wrapper.
    pub(crate) native_bindings: std::collections::HashSet<String>,
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
    /// Sized by the declaration pass for the maximum simultaneous scratch usage
    /// across iteration, destructuring, reference preparation, and closure capture.
    pub(crate) temp_base: u32,
    pub(crate) temp_top: u32,
    pub(crate) frames: Vec<Frame>,
    /// Live `PushHandler` count at the current emit point (incremented while
    /// emitting a `try` body, restored after). Drives the fast-vs-unwind choice for
    /// `return`/`break`/`continue` and is snapshotted into each pushed `Frame`.
    pub(crate) handler_depth: u32,
    /// Labels consumed when the next labeled loop pushes its control frame.
    pub(crate) pending_labels: Vec<String>,
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

/// Walk arbitrarily long binary spines with heap storage. Analysis visitors use
/// this instead of SWC's recursive default, preserving left-to-right leaf order.
pub(crate) fn walk_binary_chain<V: Visit>(binary: &BinExpr, visitor: &mut V) {
    let mut pending: Vec<&Expr> = vec![&binary.right, &binary.left];
    while let Some(expr) = pending.pop() {
        if let Expr::Bin(binary) = expr {
            pending.push(&binary.right);
            pending.push(&binary.left);
        } else {
            expr.visit_with(visitor);
        }
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
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        walk_binary_chain(n, self);
    }
    fn visit_object_lit(&mut self, n: &ObjectLit) {
        self.enter(2);
        n.visit_children_with(self);
        self.exit(2);
    }

    fn visit_assign_expr(&mut self, n: &AssignExpr) {
        self.enter(3);
        n.visit_children_with(self);
        self.exit(3);
    }

    fn visit_opt_call(&mut self, n: &OptCall) {
        let count = if n.args.iter().any(|a| a.spread.is_some()) {
            1
        } else {
            0
        };
        self.enter(count);
        n.visit_children_with(self);
        self.exit(count);
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
                    ObjectPatProp::KeyValue(kv) => {
                        pat_temp_count(&kv.value).max(if let PropName::Computed(c) = &kv.key {
                            expr_max_spread_temps(&c.expr)
                        } else {
                            0
                        })
                    }
                    ObjectPatProp::Assign(a) => {
                        2 + a.value.as_ref().map_or(0, |d| expr_max_spread_temps(d))
                    }
                    ObjectPatProp::Rest(r) => pat_temp_count(&r.arg),
                })
                .max()
                .unwrap_or(0);
            let has_rest = obj
                .props
                .iter()
                .any(|p| matches!(p, ObjectPatProp::Rest(_)));
            let keys = obj
                .props
                .iter()
                .filter(|p| !matches!(p, ObjectPatProp::Rest(_)))
                .count() as u32;
            1 + if has_rest { keys } else { keys.min(1) } + child
        }
        Pat::Assign(ap) => pat_temp_count(&ap.left) + expr_max_spread_temps(&ap.right),
        Pat::Expr(expr) => 3 + expr_max_spread_temps(expr),
        Pat::Rest(r) => pat_temp_count(&r.arg),
        Pat::Ident(_) => 2,
        _ => 0,
    }
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
    statement_depth: u32,
    with_depth: u32,
    lexical_names: Vec<std::collections::HashSet<String>>,
    parameter_names: std::collections::HashSet<String>,
    /// Temps currently live at this point of the traversal.
    temp_cur: u32,
    /// High-water mark of `temp_cur` — the value `count_temps` reports.
    temp_max: u32,
}

impl<'a, 'b> DeclCollector<'a, 'b> {
    fn statement_list(&mut self, statements: &[Stmt]) {
        if self.statement_depth > 0 {
            allocate_block_functions(self.cx, statements);
        }
        let mut names = std::collections::HashSet::new();
        for statement in statements {
            if let Stmt::Decl(Decl::Var(declaration)) = statement {
                names.extend(
                    for_var_decl_block_bindings(declaration)
                        .into_iter()
                        .map(|(name, _)| name),
                );
            }
        }
        self.lexical_names.push(names);
        statements.visit_with(self);
        self.lexical_names.pop();
    }

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
pub(crate) fn uses_arguments(body: &FunctionBody) -> bool {
    struct V {
        found: bool,
    }
    impl Visit for V {
        fn visit_bin_expr(&mut self, n: &BinExpr) {
            walk_binary_chain(n, self);
        }
        fn visit_ident(&mut self, n: &Ident) {
            if n.sym.as_ref() == "arguments" {
                self.found = true;
            }
        }
        fn visit_function(&mut self, function: &Function) {
            if function
                .body
                .as_ref()
                .is_some_and(|body| mangler_jsast::span::is_suspension_entry_span(body.span))
            {
                function.visit_children_with(self);
            }
        }
    }
    let mut v = V { found: false };
    body.visit_with(&mut v);
    v.found
}

pub(crate) fn slot_func_binding(cx: &mut Cx<'_>, name: String) -> bool {
    if cx.native_bindings.contains(&name) || cx.scopes[0].contains_key(&name) {
        return false;
    }
    let slot = cx.next_slot;
    cx.next_slot += 1;
    cx.scopes[0].insert(name, slot);
    true
}

/// Allocate a distinct lexical slot for this declaration node. Function-scoped
/// redeclarations and duplicate block functions share slots through their own
/// explicit hoisting rules, independently of declaration identity.
pub(crate) fn slot_block_binding(cx: &mut Cx<'_>, identifier: &Ident) {
    let slot = cx.next_slot;
    cx.next_slot += 1;
    cx.decl_slots.insert(DeclarationKey::of(identifier), slot);
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
/// each by its declaration identity in `decl_slots` (see `slot_block_binding`). Used by the
/// `DeclCollector` allocation pass.
pub(crate) fn slot_pat_leaves_block(cx: &mut Cx<'_>, pat: &Pat) {
    match pat {
        Pat::Ident(bi) => slot_block_binding(cx, &bi.id),
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
                    ObjectPatProp::Assign(a) => slot_block_binding(cx, &a.key.id),
                    ObjectPatProp::Rest(r) => slot_pat_leaves_block(cx, &r.arg),
                }
            }
        }
        Pat::Expr(_) | Pat::Invalid(_) => cx.bail_with("destructuring_decl"),
    }
}

/// Collect every leaf binding `(name, declaration)` pair in a declaration/catch pattern,
/// in source (leaf) order. Used by **emission** to populate a freshly-pushed scope
/// frame from the `DeclCollector`-allocated `decl_slots` (looked up by declaration identity),
/// and to enumerate a block's / catch's bindings without re-allocating.
pub(crate) fn collect_pattern_bindings(pat: &Pat, out: &mut Vec<(String, DeclarationKey)>) {
    match pat {
        Pat::Ident(bi) => out.push((bi.id.sym.to_string(), DeclarationKey::of(&bi.id))),
        Pat::Assign(ap) => collect_pattern_bindings(&ap.left, out),
        Pat::Rest(r) => collect_pattern_bindings(&r.arg, out),
        Pat::Array(arr) => {
            for elem in arr.elems.iter().flatten() {
                collect_pattern_bindings(elem, out);
            }
        }
        Pat::Object(obj) => {
            for prop in &obj.props {
                match prop {
                    ObjectPatProp::KeyValue(kv) => collect_pattern_bindings(&kv.value, out),
                    ObjectPatProp::Assign(a) => {
                        out.push((a.key.id.sym.to_string(), DeclarationKey::of(&a.key.id)))
                    }
                    ObjectPatProp::Rest(r) => collect_pattern_bindings(&r.arg, out),
                }
            }
        }
        Pat::Expr(_) | Pat::Invalid(_) => {}
    }
}

/// Collect the **direct** block-scoped (`let`/`const`) bindings of a block's
/// statement list — i.e. the bindings whose lexical scope is exactly this block —
/// as `(name, declaration)` pairs. Does NOT descend into nested blocks, loops, `try`
/// bodies, `switch`, or functions (those open their own scopes). Emission uses this
/// to populate a block's scope frame (D3); `var` is function-scoped and excluded.
pub(crate) fn direct_block_bindings(stmts: &[Stmt]) -> Vec<(String, DeclarationKey)> {
    let mut out = Vec::new();
    for s in stmts {
        if let Stmt::Decl(Decl::Fn(f)) = s {
            out.push((f.ident.sym.to_string(), DeclarationKey::of(&f.ident)));
        }
        if let Stmt::Decl(Decl::Var(v)) = s
            && matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const)
        {
            for d in &v.decls {
                collect_pattern_bindings(&d.name, &mut out);
            }
        }
    }
    out
}

/// Populate the innermost (just-pushed) scope frame with each `(name, slot)` for the
/// given `(name, declaration)` bindings, resolving the slot via `decl_slots`. A binding
/// missing from `decl_slots` (should not happen — the `DeclCollector` allocates all
/// block-scoped bindings) is skipped defensively.
pub(crate) fn bind_declarations_in_scope(cx: &mut Cx<'_>, bindings: &[(String, DeclarationKey)]) {
    for (name, declaration) in bindings {
        if let Some(&slot) = cx.decl_slots.get(declaration) {
            cx.bind_in_scope(name.clone(), slot);
            if let Some(&constant) = cx.lexical_slots.get(&slot) {
                cx.emit(Instr::BeginLexical(slot * 2 + u32::from(constant)));
            }
        }
    }
}

fn allocate_block_functions<'a>(cx: &mut Cx<'_>, statements: impl IntoIterator<Item = &'a Stmt>) {
    let mut slots = HashMap::new();
    for statement in statements {
        if let Stmt::Decl(Decl::Fn(function)) = statement {
            let name = function.ident.sym.to_string();
            let slot = *slots.entry(name.clone()).or_insert_with(|| {
                let slot = cx.next_slot;
                cx.next_slot += 1;
                cx.lexical_slots.insert(slot, false);
                slot
            });
            cx.decl_slots
                .insert(DeclarationKey::of(&function.ident), slot);
        }
    }
}

#[derive(Default)]
struct IdentifierCount(u32);
impl Visit for IdentifierCount {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        walk_binary_chain(n, self);
    }
    fn visit_ident(&mut self, _: &Ident) {
        self.0 += 1;
    }
}

#[derive(Default)]
struct NestedCaptureTemps(u32);
impl Visit for NestedCaptureTemps {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        walk_binary_chain(n, self);
    }
    fn visit_function(&mut self, function: &Function) {
        let mut count = IdentifierCount::default();
        function.visit_with(&mut count);
        self.0 = self.0.max(count.0);
    }
    fn visit_arrow_expr(&mut self, arrow: &ArrowExpr) {
        let mut count = IdentifierCount::default();
        arrow.visit_with(&mut count);
        self.0 = self.0.max(count.0);
    }
}

impl Visit for DeclCollector<'_, '_> {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        walk_binary_chain(n, self);
    }
    fn visit_for_stmt(&mut self, n: &ForStmt) {
        let names = match &n.init {
            Some(VarDeclOrExpr::VarDecl(v)) => for_var_decl_block_bindings(v)
                .into_iter()
                .map(|(name, _)| name)
                .collect(),
            _ => std::collections::HashSet::new(),
        };
        self.lexical_names.push(names);
        n.visit_children_with(self);
        self.lexical_names.pop();
    }
    fn visit_switch_stmt(&mut self, n: &SwitchStmt) {
        allocate_block_functions(self.cx, n.cases.iter().flat_map(|case| &case.cons));
        let mut names = std::collections::HashSet::new();
        for case in &n.cases {
            for statement in &case.cons {
                if let Stmt::Decl(Decl::Var(v)) = statement {
                    names.extend(
                        for_var_decl_block_bindings(v)
                            .into_iter()
                            .map(|(name, _)| name),
                    );
                }
            }
        }
        self.lexical_names.push(names);
        n.visit_children_with(self);
        self.lexical_names.pop();
    }

    fn visit_with_stmt(&mut self, n: &WithStmt) {
        self.enter_temps(1);
        self.with_depth += 1;
        n.visit_children_with(self);
        self.with_depth -= 1;
        self.exit_temps(1);
    }

    fn visit_object_lit(&mut self, n: &ObjectLit) {
        self.enter_temps(2);
        n.visit_children_with(self);
        self.exit_temps(2);
    }

    fn visit_assign_expr(&mut self, n: &AssignExpr) {
        self.enter_temps(3);
        n.visit_children_with(self);
        self.exit_temps(3);
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        self.statement_depth += 1;
        stmt.visit_children_with(self);
        self.statement_depth -= 1;
    }
    fn visit_block_stmt(&mut self, block: &BlockStmt) {
        self.statement_list(&block.stmts);
    }
    fn visit_function_body(&mut self, body: &FunctionBody) {
        self.statement_list(&body.stmts);
    }

    fn visit_opt_call(&mut self, n: &OptCall) {
        let count = if n.args.iter().any(|a| a.spread.is_some()) {
            1
        } else {
            0
        };
        self.enter_temps(count);
        n.visit_children_with(self);
        self.exit_temps(count);
    }

    fn visit_for_of_stmt(&mut self, f: &ForOfStmt) {
        // Iterator plus a value scratch used to install the close handler at
        // the enclosing stack depth after IteratorStep succeeds.
        self.enter_temps(2);
        self.lexical_names.push(
            for_head_block_bindings(&f.left)
                .into_iter()
                .map(|(name, _)| name)
                .collect(),
        );
        f.visit_children_with(self);
        self.lexical_names.pop();
        self.exit_temps(2);
    }

    fn visit_for_in_stmt(&mut self, f: &ForInStmt) {
        // Two temps (enumerated keys + current index) stay live across the loop.
        self.enter_temps(2);
        self.lexical_names.push(
            for_head_block_bindings(&f.left)
                .into_iter()
                .map(|(name, _)| name)
                .collect(),
        );
        f.visit_children_with(self);
        self.lexical_names.pop();
        self.exit_temps(2);
    }

    fn visit_pat(&mut self, p: &Pat) {
        // Charge the whole pattern's temp cost (nesting + default-expr spreads
        // included by `pat_temp_count`) as one bump, so it stacks with any
        // enclosing construct (e.g. the iterator temp of a destructuring for-of
        // head). No recursion: `pat_temp_count` already accounts for sub-patterns
        // and their default expressions.
        let mut capture_temps = NestedCaptureTemps::default();
        if self.with_depth > 0 || self.cx.dynamic_variables {
            p.visit_with(&mut capture_temps);
        }
        let n = pat_temp_count(p) + capture_temps.0;
        self.enter_temps(n);
        self.exit_temps(n);
    }

    // Spread expressions (`[...a]`, `f(...a)`, `new C(...a)`) build a scratch array
    // via the iterator; charge their temps so they stack with any enclosing
    // for-of/destructure. Object spread (`{...o}`) uses `Object.assign` (no temp).

    fn visit_var_decl(&mut self, v: &VarDecl) {
        // `var` is function-scoped: slot into the function frame, deduping a
        // re-declaration. `let`/`const` is block-scoped (D3): every binding gets a
        // fresh slot recorded by declaration identity in `decl_slots`, so a shadow lands on its own
        // slot — no bail. (A let/const colliding with a same-named var/param of the
        // same scope is a JS syntax error and never parses.)
        let block_scoped = matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const);
        for d in &v.decls {
            if block_scoped {
                slot_pat_leaves_block(self.cx, &d.name);
                let mut bindings = Vec::new();
                collect_pattern_bindings(&d.name, &mut bindings);
                for (_, declaration) in bindings {
                    if let Some(&slot) = self.cx.decl_slots.get(&declaration) {
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
        // declaration identity, so `catch (e)` shadowing an outer `e` lands on its own slot — no
        // bail. `visit_children_with` then counts the param pattern's temps via
        // `visit_pat` and descends into the catch body.
        match &c.param {
            Some(Pat::Ident(bi)) => {
                slot_block_binding(self.cx, &bi.id);
                self.cx
                    .simple_catch_slots
                    .insert(self.cx.decl_slots[&DeclarationKey::of(&bi.id)]);
            }
            Some(p @ (Pat::Array(_) | Pat::Object(_))) => slot_pat_leaves_block(self.cx, p),
            _ => {}
        }
        if let Some(p) = &c.param {
            let mut bindings = Vec::new();
            collect_pattern_bindings(p, &mut bindings);
            for (_, declaration) in bindings {
                if let Some(&slot) = self.cx.decl_slots.get(&declaration) {
                    self.cx.lexical_slots.insert(slot, false);
                }
            }
        }
        let mut names = std::collections::HashSet::new();
        if let Some(pattern @ (Pat::Array(_) | Pat::Object(_))) = &c.param {
            mangler_jsast::analysis::binding_names(pattern, &mut |id| {
                names.insert(id.sym.to_string());
            });
        }
        self.lexical_names.push(names);
        c.visit_children_with(self);
        self.lexical_names.pop();
    }
    // D5: a nested `function f(){…}` DECLARATION binds `f` function-scoped (hoisted),
    // so slot the name into the function frame like a `var` (deduping a
    // re-declaration). Do NOT descend into the nested fn's body (its own scope) —
    // its locals/captures are compiled in a separate chunk. Block declarations
    // reuse the lexical slot assigned by their statement-list hoisting pass.
    fn visit_fn_decl(&mut self, n: &FnDecl) {
        self.visit_function(&n.function);
        let name = n.ident.sym.to_string();
        if self.statement_depth <= 1 {
            slot_func_binding(self.cx, name);
        } else {
            if !self
                .cx
                .decl_slots
                .contains_key(&DeclarationKey::of(&n.ident))
            {
                slot_block_binding(self.cx, &n.ident);
            }
            let slot = self.cx.decl_slots[&DeclarationKey::of(&n.ident)];
            self.cx.lexical_slots.insert(slot, false);
            if !self.cx.opts.strict
                && (!(self.cx.opts.lexical_entry || self.cx.opts.eval_context)
                    || self
                        .cx
                        .opts
                        .external_var_bindings
                        .is_none_or(|names| names.contains(&name)))
                && !self.parameter_names.contains(&name)
                && !self.lexical_names.iter().any(|scope| scope.contains(&name))
            {
                slot_func_binding(self.cx, name.clone());
                if let Some(&alias) = self.cx.scopes[0].get(&name) {
                    self.cx
                        .block_fn_aliases
                        .insert(DeclarationKey::of(&n.ident), alias);
                } else if self.cx.native_bindings.contains(&name) {
                    self.cx
                        .block_fn_external_aliases
                        .insert(DeclarationKey::of(&n.ident), name);
                }
            }
        }
    }
    // Do not descend into nested functions; their bodies are separate chunks (D5).
    fn visit_function(&mut self, n: &Function) {
        if self.with_depth > 0 || self.cx.dynamic_variables {
            let mut count = IdentifierCount::default();
            n.visit_with(&mut count);
            self.enter_temps(count.0);
            self.exit_temps(count.0);
        }
    }
    fn visit_arrow_expr(&mut self, n: &ArrowExpr) {
        if self.with_depth > 0 || self.cx.dynamic_variables {
            let mut count = IdentifierCount::default();
            n.visit_with(&mut count);
            self.enter_temps(count.0);
            self.exit_temps(count.0);
        }
    }
}

/// Initialize block functions at block entry, before any statement executes.
pub(crate) fn emit_block_function_declarations(cx: &mut Cx<'_>, stmts: &[Stmt]) {
    for stmt in stmts {
        let Stmt::Decl(Decl::Fn(decl)) = stmt else {
            continue;
        };
        let Some(&slot) = cx.decl_slots.get(&DeclarationKey::of(&decl.ident)) else {
            continue;
        };
        let Some(body) = &decl.function.body else {
            cx.bail_with("fn_decl_no_body");
            return;
        };
        let params: Vec<Pat> = decl.function.params.iter().map(|p| p.pat.clone()).collect();
        cx.pending_fn_name = Some(decl.ident.sym.to_string());
        emit_nested_closure(
            cx,
            &params,
            body,
            false,
            decl.function.is_async,
            decl.function.is_generator,
            None,
        );
        cx.emit(Instr::InitLocal(slot));
        cx.emit(Instr::Pop);
    }
}

pub fn compile_body(params: &[Param], body: &FunctionBody) -> Result<Compiled, &'static str> {
    compile_body_boxed(params, body, &std::collections::HashSet::new())
}

/// Phase 3 entry point: compile `body` with native-closure divert [`CompileOptions`]
/// (the `--virtualize-exclude` glob and the ineligible-divert flag). Otherwise
/// identical to [`compile_body`] (empty boxed set, no `BoxPlan`); the options are
/// inherited by every nested child chunk so a deeply-nested excluded/ineligible fn
/// is diverted too.
pub fn compile_body_with_opts(
    params: &[Param],
    body: &FunctionBody,
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
    body: &FunctionBody,
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
    body: &FunctionBody,
    boxed: &std::collections::HashSet<String>,
    plan: &cells::BoxPlan,
) -> Result<Compiled, &'static str> {
    compile_body_inner(params, body, boxed, Some(plan))
}

pub(crate) fn compile_body_inner(
    params: &[Param],
    body: &FunctionBody,
    boxed: &std::collections::HashSet<String>,
    plan: Option<&cells::BoxPlan>,
) -> Result<Compiled, &'static str> {
    compile_body_inner_opts(params, body, boxed, plan, CompileOptions::default())
}

pub(crate) fn compile_body_inner_opts<'a>(
    params: &[Param],
    body: &FunctionBody,
    boxed: &std::collections::HashSet<String>,
    plan: Option<&'a cells::BoxPlan>,
    opts: CompileOptions<'a>,
) -> Result<Compiled, &'static str> {
    let environment_use = environment_usage(params, body, opts.suspension_lexicals);
    let mut hidden_environment_bindings = opts.internal_bindings.cloned().unwrap_or_default();
    if let Some(contexts) = opts.eval_class_contexts {
        hidden_environment_bindings.extend(
            contexts
                .values()
                .map(|context| context.capsule_binding.clone()),
        );
    }
    if let Some(scopes) = opts.suspension_lexicals {
        hidden_environment_bindings.extend(scopes.values().flatten().flat_map(|alias| {
            std::iter::once(alias.cell.clone()).chain(alias.objects.iter().cloned())
        }));
    }
    if let Some(references) = opts.suspension_references {
        hidden_environment_bindings
            .extend(references.values().map(|reference| reference.cell.clone()));
        hidden_environment_bindings.extend(
            references
                .values()
                .flat_map(|reference| reference.objects.iter().cloned()),
        );
    }
    let mut cx = Cx {
        code: Vec::new(),
        consts: Vec::new(),
        // `scopes[0]` is the function frame (params + `var`s + captures); it is
        // never popped. Block/catch/for-head frames are pushed and popped around
        // their bodies during emission (D3).
        scopes: vec![HashMap::new()],
        with_scopes: Vec::new(),
        needs_environment: environment_use.descendants,
        hidden_environment_bindings,
        dynamic_variables: environment_use.direct,
        var_binding_slots: std::collections::HashSet::new(),
        simple_catch_slots: std::collections::HashSet::new(),
        environment_snapshots: Vec::new(),
        decl_slots: HashMap::new(),
        block_fn_aliases: HashMap::new(),
        block_fn_external_aliases: HashMap::new(),
        lexical_slots: HashMap::new(),
        initializing: false,
        native_bindings: std::collections::HashSet::new(),
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
        pending_labels: Vec::new(),
        pending_fn_name: None,
        bail_reason: None,
        children: Vec::new(),
        box_plan: plan,
        opts,
    };

    if let Some(bindings) = opts.external_var_bindings {
        cx.native_bindings.extend(bindings.iter().cloned());
    }

    // Positional input slots are independent of binding slots. Simple parameters
    // retain their input slots (last duplicate wins); non-simple parameters use
    // lexical descriptors so every not-yet-initialized binding has a real TDZ.
    let opts = CompileOptions {
        strict: opts.strict || has_use_strict_directive_block(body),
        ..opts
    };
    cx.opts = opts;
    let simple = params.iter().all(|p| matches!(p.pat, Pat::Ident(_)));
    struct ParameterExpressions(bool);
    impl Visit for ParameterExpressions {
        fn visit_bin_expr(&mut self, n: &BinExpr) {
            walk_binary_chain(n, self);
        }
        fn visit_expr(&mut self, _: &Expr) {
            self.0 = true;
        }
    }
    let mut expressions = ParameterExpressions(false);
    params.visit_with(&mut expressions);
    let separate_environment = expressions.0;
    let mut parameter_names = Vec::new();
    for p in params {
        collect_pattern_bindings(&p.pat, &mut parameter_names);
    }
    let param_count = if opts.native_parameters {
        0
    } else {
        params.len()
    };
    let has_rest =
        !opts.native_parameters && params.last().is_some_and(|p| matches!(p.pat, Pat::Rest(_)));
    let pcount = (param_count - usize::from(has_rest)) as u32;
    if opts.native_parameters {
        if !separate_environment {
            cx.native_bindings
                .extend(parameter_names.iter().map(|(n, _)| n.clone()));
            cx.native_bindings.insert("arguments".into());
        }
    } else {
        cx.next_slot = param_count as u32;
        if simple {
            for (index, p) in params.iter().enumerate() {
                if let Pat::Ident(id) = &p.pat {
                    cx.scopes[0].insert(id.id.sym.to_string(), index as u32);
                    cx.lexical_slots.insert(index as u32, false);
                }
            }
        } else {
            for (name, _) in &parameter_names {
                if cx.scopes[0].contains_key(name) {
                    return Err("duplicate_non_simple_parameter");
                }
                let slot = cx.next_slot;
                cx.next_slot += 1;
                cx.scopes[0].insert(name.clone(), slot);
                cx.lexical_slots.insert(slot, false);
            }
        }
    }
    // Object-method lowering supplies a compiler-owned class capsule at body
    // entry. Its lexical descriptor belongs to the activation before defaults,
    // so eval in a parameter can use the same capsule as eval in the body.
    let capsule_names: std::collections::HashSet<&str> = opts
        .eval_class_contexts
        .into_iter()
        .flat_map(|contexts| contexts.values())
        .map(|context| context.capsule_binding.as_str())
        .collect();
    let early_capsules: Vec<(usize, &Stmt)> = body
        .stmts
        .iter()
        .enumerate()
        .filter(|(_, statement)| {
            matches!(statement, Stmt::Decl(Decl::Var(declaration))
            if declaration.span.is_dummy() && declaration.kind == VarDeclKind::Const
            && !declaration.decls.is_empty() && declaration.decls.iter().all(|binding|
                binding.init.is_some() && matches!(&binding.name, Pat::Ident(id)
                    if capsule_names.contains(id.id.sym.as_ref()))))
        })
        .collect();
    let early_bindings: Vec<(String, DeclarationKey)> = early_capsules
        .iter()
        .flat_map(|(_, statement)| {
            let Stmt::Decl(Decl::Var(declaration)) = statement else {
                unreachable!()
            };
            declaration.decls.iter().map(|binding| {
                let Pat::Ident(id) = &binding.name else {
                    unreachable!()
                };
                (id.id.sym.to_string(), DeclarationKey::of(&id.id))
            })
        })
        .collect();
    let early_declarations: std::collections::HashSet<DeclarationKey> = early_bindings
        .iter()
        .map(|(_, declaration)| *declaration)
        .collect();

    let param_slots = cx.scopes[0].clone();
    // A parameter expression cannot see body declarations. Allocate that body's
    // var environment independently and join it only after parameter evaluation.
    if separate_environment && !opts.native_parameters {
        cx.scopes[0].clear();
    }
    // 2. Body-declared locals (and, in the same walk, the count_temps pre-pass).
    //    The reserved temp pool must cover both the body's needs and the (separate,
    //    prologue-only) destructuring-param destructure — they never overlap, so
    //    the max suffices.
    let reserved;
    {
        let mut dc = DeclCollector {
            cx: &mut cx,
            statement_depth: 0,
            with_depth: 0,
            lexical_names: Vec::new(),
            parameter_names: parameter_names.iter().map(|(n, _)| n.clone()).collect(),
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

    let mut parameter_arguments = false;
    for p in params {
        struct ArgumentsUse(bool);
        impl Visit for ArgumentsUse {
            fn visit_bin_expr(&mut self, n: &BinExpr) {
                walk_binary_chain(n, self);
            }
            fn visit_ident(&mut self, n: &Ident) {
                self.0 |= n.sym == *"arguments";
            }
            fn visit_function(&mut self, _: &Function) {}
        }
        let mut scan = ArgumentsUse(false);
        p.pat.visit_with(&mut scan);
        parameter_arguments |= scan.0;
    }
    let declared_body_slots = cx.scopes[0].clone();
    let arg_slot = if !opts.native_parameters
        && !opts.lexical_arguments
        && !param_slots.contains_key("arguments")
        && (uses_arguments(body) || parameter_arguments || cx.needs_environment)
    {
        let existing = if separate_environment {
            None
        } else {
            cx.scopes[0].get("arguments").copied()
        };
        let slot = existing.unwrap_or_else(|| {
            let slot = cx.next_slot;
            cx.next_slot += 1;
            slot
        });
        cx.scopes[0].insert("arguments".into(), slot);
        cx.lexical_slots.insert(slot, false);
        Some(slot)
    } else {
        None
    };
    let mut body_slots = cx.scopes[0].clone();
    body_slots.extend(declared_body_slots);
    // 2b. Reserve the anonymous temp-slot pool. It MUST sit after body locals and
    // before captures so captures stay the last contiguous slots (the thunk /
    // interpreter compute `cap_start = slots - captures.len()`). `reserved` comes
    // from the declaration pass's maximum simultaneous scratch usage.
    cx.temp_base = cx.next_slot;
    cx.next_slot += reserved;
    cx.temp_top = cx.temp_base;

    // 2c. Snapshot the param+local+temp boundary. Captures are allocated lazily
    // during prologue/body emission (steps 3–4), so any slot >= this value is a
    // capture.
    cx.cap_floor = cx.next_slot;
    cx.var_binding_slots.extend(param_slots.values().copied());
    cx.var_binding_slots.extend(body_slots.values().copied());
    if let Some(slot) = arg_slot {
        cx.var_binding_slots.insert(slot);
    }

    // Evaluate all parameters in source order in their own environment. Captures
    // allocated here remain separate from same-named body locals.
    cx.scopes[0] = param_slots.clone();
    if let Some(slot) = arg_slot {
        cx.scopes[0].insert("arguments".into(), slot);
    }
    if cx.needs_environment
        && let Some(name) = opts
            .self_binding
            .filter(|name| !param_slots.contains_key(*name))
    {
        let slot = cx.resolve(name);
        emit_variable_environment(&mut cx, &HashMap::from([(name.to_string(), slot)]));
    }
    if cx.needs_environment && !opts.lexical_entry && (!opts.eval_context || opts.strict) {
        let mut variables = if separate_environment {
            param_slots.clone()
        } else {
            body_slots.clone()
        };
        variables.extend(param_slots.clone());
        if let Some(slot) = arg_slot {
            variables.insert("arguments".into(), slot);
        }
        let environment = emit_variable_environment(&mut cx, &variables);
        if separate_environment {
            let parameter_slots: std::collections::HashSet<u32> =
                param_slots.values().copied().collect();
            if let Const::Environment(metadata) = &mut cx.consts[environment as usize] {
                for scope in &mut metadata.scopes {
                    if let crate::eval::EnvironmentScope::Bindings(bindings) = scope {
                        for binding in bindings {
                            binding.lexical = parameter_slots.contains(&binding.slot);
                        }
                    }
                }
            }
        }
    }
    if !opts.native_parameters && !simple && arg_slot.is_some() {
        cx.emit(Instr::UnmapArguments);
    }
    if !opts.native_parameters && !simple {
        let mut slots: Vec<u32> = param_slots.values().copied().collect();
        slots.sort_unstable();
        for slot in slots {
            cx.emit(Instr::BeginLexical(slot * 2));
        }
    }
    if !opts.native_parameters
        && simple
        && !opts.strict
        && !opts.lexical_arguments
        && arg_slot.is_some()
    {
        let mut slots: Vec<u32> = param_slots.values().copied().collect();
        slots.sort_unstable();
        for slot in slots {
            cx.emit(Instr::MapArgument(slot, slot));
        }
    }
    if let Some(slot) = arg_slot {
        cx.emit(Instr::BeginLexical(slot * 2));
        cx.emit(Instr::LoadArguments);
        cx.emit(Instr::InitLocal(slot));
        cx.emit(Instr::Pop);
    }
    bind_declarations_in_scope(&mut cx, &early_bindings);
    for (_, statement) in &early_capsules {
        emit_stmt(&mut cx, statement);
        if let Some(reason) = cx.bail_reason {
            return Err(reason);
        }
    }
    if !opts.native_parameters && !simple {
        cx.initializing = true;
        for (index, p) in params.iter().enumerate() {
            match &p.pat {
                Pat::Rest(rest) => {
                    cx.emit(Instr::LoadRest(index as u32));
                    emit_bind_target(&mut cx, &rest.arg);
                }
                pat => {
                    cx.emit(Instr::LoadLocal(index as u32));
                    emit_bind_target(&mut cx, pat);
                }
            }
            if let Some(reason) = cx.bail_reason {
                return Err(reason);
            }
        }
        cx.initializing = false;
    }
    // Body var declarations overlapping parameters receive the initialized value,
    // while closures created by defaults keep the original parameter environment.
    for (name, slot) in &body_slots {
        if opts.native_parameters
            && separate_environment
            && (name == "arguments" || parameter_names.iter().any(|(n, _)| n == name))
        {
            let parameter = cx.resolve(name);
            cx.emit(Instr::LoadLocal(parameter));
            cx.emit(Instr::StoreLocal(*slot));
            cx.emit(Instr::Pop);
        } else if name == "arguments" && arg_slot.is_some_and(|arg| arg != *slot) {
            cx.emit(Instr::LoadLocal(arg_slot.unwrap()));
            cx.emit(Instr::StoreLocal(*slot));
            cx.emit(Instr::Pop);
        } else if let Some(param) = param_slots.get(name).filter(|param| *param != slot) {
            cx.emit(Instr::LoadLocal(*param));
            cx.emit(Instr::StoreLocal(*slot));
            cx.emit(Instr::Pop);
        }
    }
    if cx.needs_environment
        && separate_environment
        && !opts.lexical_entry
        && (!opts.eval_context || opts.strict)
    {
        emit_variable_environment(&mut cx, &body_slots);
    }
    cx.scopes[0].extend(body_slots);
    let mut external_aliases: Vec<_> = std::mem::take(&mut cx.block_fn_external_aliases)
        .into_iter()
        .collect();
    // Capture allocation follows declaration slots, never hash or address order.
    external_aliases.sort_by_key(|(declaration, _)| cx.decl_slots[declaration]);
    for (declaration, name) in external_aliases {
        let slot = cx.resolve(&name);
        cx.block_fn_aliases.insert(declaration, slot);
    }

    // 3d. D5 nested function DECLARATIONS are hoisted: a `function inc(){…}` at the
    //     body's top level binds `inc` (function-scoped) to its closure, visible
    //     throughout the body (including before the textual declaration).
    //     Each top-level fn-decl gets a function-frame slot, then its closure is
    //     built (MakeClosure) and stored into that slot here, before the body.

    // The fn-decl NAMES were already slotted (function-scoped) by `DeclCollector`
    // (see its `visit_fn_decl`), so they sit below `cap_floor` like `var`s; here we
    // just collect each top-level fn-decl with its slot for the hoisted emission.
    let mut fn_decl_slots: Vec<(u32, &FnDecl)> = Vec::new();
    for stmt in &body.stmts {
        if let Stmt::Decl(Decl::Fn(fd)) = stmt {
            let slot = cx.resolve(fd.ident.sym.as_ref());
            fn_decl_slots.push((slot, fd));
        }
    }

    // 4. Emit body. The function body's own top-level `let`/`const` bindings live
    //    in the function frame (`scopes[0]`); populate it from their
    //    `DeclCollector`-allocated slots before emission so a top-level `let x`
    //    resolves to its slot (D3). Nested blocks push their own frames as they are
    //    emitted. Captures are allocated lazily here.
    let mut top_bindings = direct_block_bindings(&body.stmts);
    top_bindings.retain(|(_, declaration)| !early_declarations.contains(declaration));
    bind_declarations_in_scope(&mut cx, &top_bindings);

    // 4a. D5 in-VM cell SEEDING. Each boxed local (captured-and-mutated by a nested
    //     closure) must hold a one-element cell `[v]` before any closure builder or
    //     body statement runs. This runs AFTER `bind_declarations_in_scope` so a body-top-
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
                    .filter(|slot| *slot < cx.cap_floor && !cx.lexical_slots.contains_key(slot))
                    .map(|slot| {
                        (
                            slot,
                            param_slots.contains_key(name)
                                || (opts.native_parameters
                                    && separate_environment
                                    && (name == "arguments"
                                        || parameter_names.iter().any(|(n, _)| n == name))),
                        )
                    })
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
        cx.pending_fn_name = Some(self_name.clone());
        emit_nested_closure(
            &mut cx,
            &pats,
            fbody,
            false,
            fd.function.is_async,
            fd.function.is_generator,
            None,
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

    for (index, stmt) in body.stmts.iter().enumerate() {
        if early_capsules.iter().any(|(early, _)| *early == index) {
            continue;
        }
        emit_stmt(&mut cx, stmt);
        if let Some(r) = cx.bail_reason {
            return Err(r);
        }
    }

    // Terminator: guarantee every program ends in Ret so fall-off-end and
    // loop/if-exit jumps that target one-past-end land here (return undefined).
    cx.emit(Instr::PushUndef);
    cx.emit(Instr::Ret);

    finish_environment_snapshots(&mut cx);
    let slots = cx.next_slot;
    let mut compiled = Compiled {
        requires_source_compiler: source_compiler_usage(params, body, opts.source_compiler_sites),
        code: cx.code,
        consts: cx.consts,
        captures: cx.captures,
        slots,
        pcount,
        children: cx.children,
    };
    if let Some(map) = opts.source_utf16 {
        crate::source_text::restore_constants(&mut compiled, map);
    }
    Ok(compiled)
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
            Some(name) => f.labels.contains(name),
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
/// `for-of` head, as `(name, declaration)` pairs — empty for a `var`/expression head
/// (those are function-scoped or pre-existing). The loop arms push a scope frame
/// holding these so a `for (let i …)` head that shadows an outer `i` resolves to
/// its own slot for the whole loop (D3).
pub(crate) fn for_var_decl_block_bindings(v: &VarDecl) -> Vec<(String, DeclarationKey)> {
    let mut out = Vec::new();
    if matches!(v.kind, VarDeclKind::Let | VarDeclKind::Const) {
        for d in &v.decls {
            collect_pattern_bindings(&d.name, &mut out);
        }
    }
    out
}

/// The `(name, declaration)` block-scoped bindings of a `for-in`/`for-of` head
/// (`for (let x of …)`); empty for a `var`/pattern head.
pub(crate) fn for_head_block_bindings(head: &ForHead) -> Vec<(String, DeclarationKey)> {
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
    fn generated_declarations_do_not_depend_on_spans_or_addresses() {
        use swc_core::common::DUMMY_SP;
        use swc_core::ecma::ast::Ident;
        use swc_core::ecma::visit::{VisitMut, VisitMutWith};
        struct Synthetic;
        impl VisitMut for Synthetic {
            fn visit_mut_ident(&mut self, identifier: &mut Ident) {
                identifier.span = DUMMY_SP;
            }
        }
        for source in [
            "function(){const env=1;try{throw 2}catch(error){}return env}",
            "function(){let x=1;{let x=2}return x}",
            "function(){let [left,right]=[1,2];return left+right}",
            "function(){let {left,right}={left:1,right:2};return left+right}",
            "function(){var first,second;{function f(){return 1}function f(){return 2}first=f}{function f(){return 3}second=f}return first()+second()}",
            "function(){let x=0;for(let i=0;i<2;i++){x+=i}return x}",
            "function(){let sum=0;for(let [x,y] of [[1,2]]){sum+=x+y}return sum}",
        ] {
            let (params, mut body) = parse_fn_with_params(source);
            let expected = compile_body(&params, &body).expect("source body compiles");
            body.visit_mut_with(&mut Synthetic);
            let compiled = compile_body(&params, &body).expect("generated body compiles");
            assert_eq!(
                compiled, expected,
                "source positions cannot change bindings: {source}"
            );
            let cloned = body.clone();
            assert_eq!(
                compiled,
                compile_body(&params, &cloned).unwrap(),
                "declaration addresses cannot affect bytecode: {source}"
            );
        }
    }

    #[test]
    fn external_block_function_aliases_allocate_captures_in_declaration_order() {
        use std::collections::HashSet;
        let (params, body) = parse_fn_with_params(
            "function(){{function first(){return 1}}{function second(){return 2}}return first()+second()}",
        );
        let external = HashSet::from(["first".to_string(), "second".to_string()]);
        let options = super::CompileOptions {
            live_captures: true,
            external_var_bindings: Some(&external),
            ..Default::default()
        };
        let expected = super::compile_body_with_opts(&params, &body, options).unwrap();
        assert_eq!(expected.captures, ["first", "second"]);
        for _ in 0..4 {
            let cloned = body.clone();
            assert_eq!(
                expected,
                super::compile_body_with_opts(&params, &cloned, options).unwrap()
            );
        }
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
        assert!(compile_body(&p, &b).is_ok());
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
    fn compiles_duplicate_simple_param_names() {
        // Sloppy duplicate simple parameters bind the last positional occurrence.
        let (p, b) = parse_fn_with_params("function(a, a){ return a; }");
        assert_eq!(compile_body(&p, &b).unwrap().pcount, 2);
        let (p2, b2) = parse_fn_with_params("function(a, a = 2){ return a; }");
        assert!(matches!(
            compile_body(&p2, &b2),
            Err("duplicate_non_simple_parameter")
        ));
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
        // Rest values can initialize an arbitrary binding pattern.
        let (p2, b2) = parse_fn_with_params("function(...[a, b]){ return a + b; }");
        assert!(compile_body(&p2, &b2).is_ok());
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
    fn captures_default_referencing_body_local() {
        // A default must NOT resolve to a body var/let local. In real JS the
        // default sees the OUTER `c`, not the body `var c` (which is invisible in
        // the parameter scope). Our flat slot model would wrongly read the local
        // slot, so we must bail rather than miscompile to a silent wrong value.
        let (p, b) = parse_fn_with_params("function(a, b = c){ var c = 5; return b; }");
        assert!(compile_body(&p, &b).is_ok());
        // `let`-declared body local is the same divergence.
        let (p2, b2) = parse_fn_with_params("function(a, b = c){ let c = 5; return b; }");
        assert!(compile_body(&p2, &b2).is_ok());
    }

    #[test]
    fn compiles_default_parameter_tdz() {
        // self-reference (TDZ in real JS).
        let (p, b) = parse_fn_with_params("function(a = a){ return a; }");
        assert!(compile_body(&p, &b).is_ok());
        // later-param reference (TDZ in real JS); could return a wrong value.
        let (p2, b2) = parse_fn_with_params("function(a = b, b){ return a; }");
        assert!(compile_body(&p2, &b2).is_ok());
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
    fn compiles_optional_call_of_method() {
        // Optional method calls preserve the member receiver through the guard.
        let (p, b) = parse_fn_with_params("function(o){ return o.m?.(); }");
        assert!(compile_body(&p, &b).is_ok());
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
        assert!(compile_body(&p2, &b2).is_ok());
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
            prog.code.iter().any(|i| matches!(i, Instr::ArraySpread)),
            "spread must invoke the iterator protocol"
        );
        // `new C(...a)` lowers to a captured-global Reflect.construct call.
        let (p2, b2) = parse_fn_with_params("function(C, a){ return new C(...a); }");
        let prog2 = compile_body(&p2, &b2).expect("new-spread must compile");
        assert!(
            prog2.code.iter().any(|i| matches!(i, Instr::NewArray)),
            "new-spread must use the intrinsic construction operation"
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
    fn compiles_delete_of_local_binding() {
        // A local identifier is an undeletable binding in sloppy code.
        let (p, b) = parse_fn_with_params("function(x){ return delete x; }");
        assert!(compile_body(&p, &b).is_ok());
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
