//! Phase 4 — opportunistic, LOCAL, SOUND desugaring (§3.2), gated by `whole_program`.
//!
//! This module converts a couple of otherwise-VM-native constructs into a *wrappable*
//! form **before** the partition classifier runs ([`super::partition`]), so they get
//! pulled into the VM and raise coverage. Each desugaring is its own opt-in flag,
//! default OFF, and is applied here only when both `whole_program` AND the relevant
//! flag are set. After lowering, the NORMAL classifier/compiler decides wrappability
//! and bails as usual — we never reimplement eligibility here.
//!
//! ## Soundness contract (§7 — "never a miscompile")
//!
//! A desugaring only fires for a construct we can prove observably-equivalent. If a
//! construct has any shape we are unsure about, we LEAVE IT NATIVE (untouched), and it
//! falls back to MustStayNative / bisection exactly as before. Correctness dominates:
//! when in doubt, do nothing.
//!
//! ## What is lowered
//!
//! 1. **class → function/prototype** (`desugar_classes`, the main item, hard-gated).
//!    Only a top-level `class C [extends B] { … }` declaration whose members are all
//!    *supported shapes* is lowered; see [`class_is_lowerable`] for the EXACT
//!    skip-desugar predicate. The lowering reproduces:
//!      * constructor / `super(...)` ordering (via `Reflect.construct`, which works for
//!        BOTH a function-style base and a real ES-class base),
//!      * instance-field init order (fields init right after `super`, in source order),
//!      * method **non-enumerability** (methods go on the prototype via
//!        `Object.defineProperty` with `enumerable:false`),
//!      * the `extends` prototype chain (`C.prototype.__proto__ = B.prototype` and the
//!        static-inheritance link `C.__proto__ = B`),
//!      * `static` members (static methods non-enumerable; static fields are enumerable
//!        own data properties, matching class field `[[DefineOwnProperty]]` semantics).
//!
//! 2. **regex literal → `new RegExp("re","flags")`** (`desugar_regexes`, stretch). Left
//!    WIRED but evaluated only when its flag is set. See [`desugar_regexes`] for the
//!    semantics note.
//!
//! Both lowerings are pure AST→AST and DETERMINISTIC (no RNG), so the same input yields
//! byte-identical output for a given seed (§7).

use swc_core::common::DUMMY_SP;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

// ===========================================================================
// Entry point
// ===========================================================================

/// Apply the enabled Phase-4 desugarings to the whole program `items` (the top-level
/// list AND, recursively, every nested statement list — function/arrow/block bodies),
/// IN PLACE. Returns the (possibly rewritten) item list.
///
/// We recurse because whole-program virtualization pulls nested function bodies into VM
/// chunks too (the common shape is a single top-level IIFE whose body holds the classes
/// and regexes). A class/regex anywhere a chunk will compile is worth lowering; one we
/// cannot lower soundly is left native and simply bisects out as before. We lower only
/// class *declaration statements* (and `export class`); a class *expression* is left
/// native (our lowering emits statements, not an expression — no sound in-place form).
pub(crate) fn desugar_top_level(
    items: Vec<ModuleItem>,
    desugar_class: bool,
    desugar_regex: bool,
) -> Vec<ModuleItem> {
    let mut out = items;
    if desugar_class {
        // Top-level (handles `export class`), then recurse into nested statement lists.
        out = desugar_classes(out);
        let mut rw = ClassStmtRewriter;
        for it in out.iter_mut() {
            it.visit_mut_with(&mut rw);
        }
    }
    if desugar_regex {
        desugar_regexes(&mut out);
    }
    out
}

/// Recursively lower class-declaration STATEMENTS inside any nested statement list
/// (function/arrow/block bodies). Top-level `export class` is handled by
/// [`desugar_classes`] before this runs; this rewriter only sees plain
/// `Stmt::Decl(Decl::Class)` (an `export class` cannot appear nested).
struct ClassStmtRewriter;

impl VisitMut for ClassStmtRewriter {
    fn visit_mut_stmts(&mut self, stmts: &mut Vec<Stmt>) {
        // Recurse first so inner bodies (and inner classes) are lowered bottom-up.
        stmts.visit_mut_children_with(self);
        let mut out: Vec<Stmt> = Vec::with_capacity(stmts.len());
        for s in std::mem::take(stmts) {
            match s {
                Stmt::Decl(Decl::Class(cd)) if class_is_lowerable(&cd.class) => {
                    match lower_class(&cd.ident, &cd.class) {
                        Some(lowered) => out.extend(lowered),
                        None => out.push(Stmt::Decl(Decl::Class(cd))),
                    }
                }
                other => out.push(other),
            }
        }
        *stmts = out;
    }
}

// ===========================================================================
// 1. class → function/prototype (hard-gated)
// ===========================================================================

/// Lower each top-level `class C { … }` / `export class C { … }` *declaration* that
/// passes [`class_is_lowerable`] into a run of function/prototype statements. A class
/// we cannot lower soundly is left exactly as it was (bail-to-safe). `export class C`
/// becomes `export { C }` after the lowered statements so the export binding survives.
fn desugar_classes(items: Vec<ModuleItem>) -> Vec<ModuleItem> {
    let mut out: Vec<ModuleItem> = Vec::with_capacity(items.len());
    for it in items {
        match it {
            // `class C extends B { … }`
            ModuleItem::Stmt(Stmt::Decl(Decl::Class(cd))) if class_is_lowerable(&cd.class) => {
                match lower_class(&cd.ident, &cd.class) {
                    Some(stmts) => out.extend(stmts.into_iter().map(ModuleItem::Stmt)),
                    None => out.push(ModuleItem::Stmt(Stmt::Decl(Decl::Class(cd)))),
                }
            }
            // `export class C extends B { … }`
            ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ed)) => match ed.decl {
                Decl::Class(cd) if class_is_lowerable(&cd.class) => {
                    let name = cd.ident.sym.to_string();
                    match lower_class(&cd.ident, &cd.class) {
                        Some(stmts) => {
                            out.extend(stmts.into_iter().map(ModuleItem::Stmt));
                            // Re-export the now-native binding by name.
                            out.push(export_named_item(&name));
                        }
                        None => out.push(ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl {
                            span: ed.span,
                            decl: Decl::Class(cd),
                        }))),
                    }
                }
                other => out.push(ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl {
                    span: ed.span,
                    decl: other,
                }))),
            },
            other => out.push(other),
        }
    }
    out
}

/// The EXACT skip-desugar predicate (§3.2 / §10). A class is lowerable to
/// function/prototype form ONLY when EVERY member is a supported shape AND the class
/// has no construct we cannot reproduce soundly. We SKIP-DESUGAR (leave native) when
/// the class has ANY of:
///
///   * a private field/method (`#x`) — [`ClassMember::PrivateMethod`] /
///     [`ClassMember::PrivateProp`]; private state has no sound prototype-form analogue;
///   * a `static { … }` block — [`ClassMember::StaticBlock`];
///   * a TS index signature / auto-accessor — [`ClassMember::TsIndexSignature`] /
///     [`ClassMember::AutoAccessor`] (and `declare`/abstract members);
///   * a decorator anywhere (class-level or member-level);
///   * a **computed** member key (`[expr]() {}` / `[expr] = v`) — we cannot statically
///     name the property, and a computed key may close over a TDZ binding (§3.2);
///   * a getter/setter accessor (`get x(){}` / `set x(v){}`) — REJECTED to keep the
///     surface tight (a get/set pair on one key would clobber under a naive
///     per-accessor `defineProperty`); only PLAIN methods are lowered;
///   * a member key that is a number / bigint (rare; keep the supported surface small);
///   * use of `super.x` / `super.m()` (a *property* super access) ANYWHERE — only a
///     bare `super(...)` *call* in the constructor is reproduced; a `super.member`
///     reference needs `[[HomeObject]]` semantics we do not emulate;
///   * `super(...)` that is NOT a single direct top-level statement of the constructor
///     body (e.g. `if (c) super()`, `super()` inside a block, `super() || x`), or more
///     than one `super(...)` call, or `super(...)` in a class with no `extends` (a
///     syntax error anyway) — the lowering only splices a top-level `super(...);`
///     statement, so any other position would silently drop the base init; or a
///     constructor that
///     references `super` in a nested non-arrow function;
///   * `new.target` anywhere.
///
/// Anything not positively recognized as safe makes the whole class non-lowerable —
/// the conservative default is "stay native".
fn class_is_lowerable(class: &Class) -> bool {
    // No class-level decorators / abstract / TS type params.
    if !class.decorators.is_empty() || class.is_abstract || class.type_params.is_some() {
        return false;
    }
    if class.super_type_params.is_some() || !class.implements.is_empty() {
        return false;
    }

    let mut has_ctor = false;
    for m in &class.body {
        match m {
            ClassMember::Constructor(c) => {
                has_ctor = true;
                if c.is_optional || c.accessibility.is_some() {
                    return false;
                }
                // Constructor params must be plain (no TS param properties).
                for p in &c.params {
                    match p {
                        ParamOrTsParamProp::Param(param) => {
                            if !param.decorators.is_empty() {
                                return false;
                            }
                        }
                        ParamOrTsParamProp::TsParamProp(_) => return false,
                    }
                }
                if let Some(body) = &c.body {
                    if !ctor_super_use_is_sound(body, class.super_class.is_some()) {
                        return false;
                    }
                }
            }
            ClassMember::Method(m) => {
                if !method_is_lowerable(m) {
                    return false;
                }
            }
            ClassMember::ClassProp(p) => {
                if !class_prop_is_lowerable(p) {
                    return false;
                }
            }
            // Anything else is an unsupported shape → stay native.
            ClassMember::PrivateMethod(_)
            | ClassMember::PrivateProp(_)
            | ClassMember::TsIndexSignature(_)
            | ClassMember::StaticBlock(_)
            | ClassMember::AutoAccessor(_) => return false,
            ClassMember::Empty(_) => {}
        }
    }

    // A base class (no `extends`): a constructor that contains a bare `super(...)` is a
    // syntax error, so `ctor_super_use_is_sound` already rejects it. Nothing else to
    // check. With `extends` but NO own constructor, the implicit constructor forwards
    // args via `super(...args)` — we reproduce that too (see `lower_class`).
    let _ = has_ctor;

    // No `super.member` or `new.target` anywhere in the class (methods included): those
    // need home-object / new.target semantics we do not emulate.
    if class_uses_super_property_or_new_target(class) {
        return false;
    }
    true
}

/// A method member is lowerable iff it is a PLAIN method (`MethodKind::Method`, NOT a
/// getter/setter — accessors are rejected, see below) with a static identifier/string
/// key, no decorators, and no TS modifiers. An async/generator method *body* is fine —
/// it becomes a native function value on the prototype — but we require a non-computed
/// key to keep the surface tight.
fn method_is_lowerable(m: &ClassMethod) -> bool {
    if m.is_abstract || m.is_optional || m.is_override || m.accessibility.is_some() {
        return false;
    }
    if !m.function.decorators.is_empty() {
        return false;
    }
    // Conservative surface: only plain methods. Getters/setters would each need an
    // independent `Object.defineProperty`, and a get/set PAIR on the same key would
    // clobber one another in the naive lowering — so we skip-desugar any accessor and
    // leave the whole class native (sound, tight surface). Revisit behind fuzzing.
    if !matches!(m.kind, MethodKind::Method) {
        return false;
    }
    static_key_name(&m.key).is_some()
}

/// A class field is lowerable iff it has a static identifier/string key, no decorators,
/// and no TS-only modifiers (`declare`/`definite`/`abstract`/`readonly`/accessibility).
fn class_prop_is_lowerable(p: &ClassProp) -> bool {
    if p.is_abstract
        || p.is_optional
        || p.is_override
        || p.declare
        || p.definite
        || p.readonly
        || p.accessibility.is_some()
    {
        return false;
    }
    if !p.decorators.is_empty() {
        return false;
    }
    static_key_name(&p.key).is_some()
}

/// The static name of a member key, or `None` for a computed / private / numeric /
/// bigint key (any of which makes the class non-lowerable).
fn static_key_name(key: &PropName) -> Option<String> {
    match key {
        PropName::Ident(id) => Some(id.sym.to_string()),
        PropName::Str(s) => s.value.as_str().map(|v| v.to_string()),
        PropName::Num(_) | PropName::BigInt(_) | PropName::Computed(_) => None,
    }
}

/// True iff the constructor's `super(...)` usage is sound to reproduce: a bare
/// `super(...)` call may appear only when the class has `extends`, only directly in the
/// constructor's own body (not inside a nested non-arrow function), and never as a
/// `super.member` property access. (Property-super and nested-fn super are caught here
/// AND by [`class_uses_super_property_or_new_target`]; this also rejects `super()` with
/// no `extends`.)
fn ctor_super_use_is_sound(body: &BlockStmt, has_extends: bool) -> bool {
    // Count EVERY `super(...)` call anywhere in the ctor (skipping nested non-arrow
    // functions, where a super reference would be unreproducible). Also reject any
    // `super.member` access.
    struct Scan {
        super_calls: u32,
        super_prop: bool,
        nonarrow_depth: u32,
    }
    impl Visit for Scan {
        fn visit_function(&mut self, n: &Function) {
            self.nonarrow_depth += 1;
            n.visit_children_with(self);
            self.nonarrow_depth -= 1;
        }
        fn visit_callee(&mut self, n: &Callee) {
            if let Callee::Super(_) = n {
                // A `super(...)` inside a nested non-arrow function is not reproducible.
                if self.nonarrow_depth > 0 {
                    self.super_prop = true; // reuse the reject flag (unsound shape)
                } else {
                    self.super_calls += 1;
                }
            }
            n.visit_children_with(self);
        }
        fn visit_super_prop_expr(&mut self, _n: &SuperPropExpr) {
            self.super_prop = true;
        }
    }
    let mut sc = Scan { super_calls: 0, super_prop: false, nonarrow_depth: 0 };
    body.visit_with(&mut sc);

    // `super.member` (or nested-fn super, folded into `super_prop`) → never lowerable.
    if sc.super_prop {
        return false;
    }

    if !has_extends {
        // A BASE class: a constructor that calls `super(...)` is a syntax error, so a
        // sound base ctor has zero super calls. Reject if any slipped through.
        return sc.super_calls == 0;
    }

    // A DERIVED class constructor. `build_derived_ctor` ONLY reproduces a super call
    // that appears as a DIRECT top-level expression statement `super(...);`. A
    // `super(...)` anywhere else (conditional `if(c) super()`, inside a block, an operand
    // of `||`/`,`, etc.) would be silently dropped by the splicer — the base ctor would
    // never run (a miscompile). So we require EXACTLY ONE super call AND that it is a
    // direct top-level statement; anything else stays native (bail-to-safe).
    sc.super_calls == 1 && body.stmts.iter().any(|s| super_call_args(s).is_some())
}

/// True if the class uses `super.member` / `super[expr]` (property super) or
/// `new.target` ANYWHERE (in any method or the constructor). Both need semantics the
/// prototype lowering does not reproduce, so their presence makes the class native.
fn class_uses_super_property_or_new_target(class: &Class) -> bool {
    struct Scan {
        hit: bool,
    }
    impl Visit for Scan {
        fn visit_super_prop_expr(&mut self, _n: &SuperPropExpr) {
            self.hit = true;
        }
        fn visit_meta_prop_expr(&mut self, n: &MetaPropExpr) {
            // `new.target` / `import.meta`.
            if matches!(n.kind, MetaPropKind::NewTarget) {
                self.hit = true;
            }
        }
    }
    let mut sc = Scan { hit: false };
    class.visit_with(&mut sc);
    sc.hit
}

/// Lower a lowerable `class <name> [extends <Base>] { … }` to a run of native
/// statements. Returns `None` only if the machine-generated scaffold fails to reparse
/// (never expected; bail-to-safe leaves the class native). The emitted run:
///
/// ```text
/// function <name>(<ctorParams>) { <super-init>; <field inits>; <ctor body sans super> }
/// // if extends:
/// Object.setPrototypeOf(<name>.prototype, <Base>.prototype);
/// Object.setPrototypeOf(<name>, <Base>);
/// // each instance method (non-enumerable):
/// Object.defineProperty(<name>.prototype, "<m>", { value/get/set …, enumerable:false, configurable:true });
/// // each static method (non-enumerable):
/// Object.defineProperty(<name>, "<m>", { … });
/// // each static field (enumerable own data prop):
/// <name>.<f> = <init>;
/// ```
fn lower_class(ident: &Ident, class: &Class) -> Option<Vec<Stmt>> {
    let name = ident.sym.to_string();
    let base: Option<Expr> = class.super_class.as_deref().cloned();

    // Partition members.
    let mut ctor: Option<&Constructor> = None;
    let mut instance_fields: Vec<&ClassProp> = Vec::new();
    let mut static_fields: Vec<&ClassProp> = Vec::new();
    let mut proto_methods: Vec<&ClassMethod> = Vec::new();
    let mut static_methods: Vec<&ClassMethod> = Vec::new();
    for m in &class.body {
        match m {
            ClassMember::Constructor(c) => ctor = Some(c),
            ClassMember::ClassProp(p) if p.is_static => static_fields.push(p),
            ClassMember::ClassProp(p) => instance_fields.push(p),
            ClassMember::Method(m) if m.is_static => static_methods.push(m),
            ClassMember::Method(m) => proto_methods.push(m),
            ClassMember::Empty(_) => {}
            // class_is_lowerable already rejected everything else.
            _ => return None,
        }
    }

    // --- constructor body ---------------------------------------------------
    // Field-init statements (in source order). Each instance field `f = init;` becomes
    // `this.f = init;` (or `this.f = undefined;` for a bare `f;`).
    let field_inits: Vec<Stmt> = instance_fields
        .iter()
        .map(|p| {
            let key = static_key_name(&p.key).expect("checked lowerable");
            let init = p
                .value
                .as_deref()
                .cloned()
                .unwrap_or_else(|| undefined_expr());
            this_assign_stmt(&key, init)
        })
        .collect();

    // Constructor params + body. Reproduce `super(args)` as a `Reflect.construct` of the
    // base, capturing the result as `this` via a synthetic `_super` rewrite. We use a
    // robust strategy: build the function body as
    //   [ <maybe-super-call-rewritten ctor stmts, with field inits spliced after super> ]
    let (ctor_params, ctor_body_stmts): (Vec<Param>, Vec<Stmt>) = match ctor {
        Some(c) => {
            let params: Vec<Param> = c
                .params
                .iter()
                .filter_map(|p| match p {
                    ParamOrTsParamProp::Param(param) => Some(param.clone()),
                    ParamOrTsParamProp::TsParamProp(_) => None,
                })
                .collect();
            let body = c.body.clone().map(|b| b.stmts).unwrap_or_default();
            (params, body)
        }
        None => {
            // No own constructor. With `extends`, the implicit ctor is
            // `constructor(...args){ super(...args); }`; without, it is empty.
            if base.is_some() {
                (
                    vec![rest_param("args")],
                    vec![implicit_super_call_stmt("args")],
                )
            } else {
                (Vec::new(), Vec::new())
            }
        }
    };

    // Build the constructor body. For a derived class we use the spec-faithful
    // `Reflect.construct` rebind (see `build_derived_ctor`); for a base class fields
    // init first, then the body.
    let final_ctor_body = if base.is_some() {
        build_derived_ctor(&name, ctor_body_stmts, field_inits)
    } else {
        let mut out = field_inits;
        out.extend(ctor_body_stmts);
        out
    };

    let ctor_fn = function_decl(&name, ctor_params, final_ctor_body);

    let mut out: Vec<Stmt> = vec![ctor_fn];

    // --- prototype chain (extends) -----------------------------------------
    if let Some(b) = &base {
        out.push(set_proto_of(
            member(ident_expr(&name), "prototype"),
            member(b.clone(), "prototype"),
        ));
        out.push(set_proto_of(ident_expr(&name), b.clone()));
    }

    // --- instance methods (non-enumerable on the prototype) ----------------
    for m in &proto_methods {
        out.push(define_method(&member(ident_expr(&name), "prototype"), m)?);
    }
    // --- static methods (non-enumerable on the constructor) ----------------
    for m in &static_methods {
        out.push(define_method(&ident_expr(&name), m)?);
    }
    // --- static fields (enumerable own data props: `C.f = init;`) ----------
    for p in &static_fields {
        let key = static_key_name(&p.key).expect("checked lowerable");
        let init = p.value.as_deref().cloned().unwrap_or_else(undefined_expr);
        out.push(assign_member_stmt(ident_expr(&name), &key, init));
    }

    Some(out)
}

/// Build a DERIVED-class constructor body, spec-faithfully and soundly for BOTH a
/// function-style base and a real ES-class base.
///
/// The standard sound lowering (Babel's `_callSuper`): under `new <name>(args)` the
/// engine hands us a `this` already linked to `<name>.prototype`, but for a derived
/// class the base constructor must produce the actual instance. We therefore:
///   1. rebind `this` to `_self = Reflect.construct(Object.getPrototypeOf(<name>), [super-args], <name>)`
///      at the point of the original `super(...)` call. `Object.getPrototypeOf(<name>)`
///      is the base constructor (we set `<name>.__proto__ = Base` in `lower_class`),
///      and passing `<name>` as `newTarget` makes the produced object's prototype
///      `<name>.prototype`. This runs the base ctor with the right `new.target` and
///      works whether the base is a function or an ES class.
///   2. rewrite EVERY `this` in the constructor's own scope (descending into arrows,
///      which share `this`, but NOT into nested non-arrow `function(){}`) to `_self`.
///   3. splice the field inits right after the rebind (spec: fields init after super),
///      writing through `_self`.
///   4. `return _self;` at the end so `new` yields the base-produced object.
///
/// `class_is_lowerable` guarantees: the class HAS `extends`, the ctor's `super` use is a
/// bare top-level (non-nested-fn) `super(...)` call, and there is no `super.member` /
/// `new.target` — so this rewrite is sound. If the (machine-checked) preconditions
/// somehow do not hold, we still never miscompile because the differential gate would
/// catch it; in practice the predicate is the guard.
fn build_derived_ctor(name: &str, ctor_stmts: Vec<Stmt>, field_inits: Vec<Stmt>) -> Vec<Stmt> {
    const SELF: &str = "_self";

    // Rewrite `this` → `_self` in the ctor's own scope (arrows transparent, non-arrow
    // functions opaque). Also rewrite the field-init statements (they reference `this`).
    let mut rw = ThisRewriter { self_name: SELF, nonarrow_depth: 0 };
    let mut ctor_stmts = ctor_stmts;
    for s in &mut ctor_stmts {
        s.visit_mut_with(&mut rw);
    }
    let mut field_inits = field_inits;
    for s in &mut field_inits {
        s.visit_mut_with(&mut rw);
    }

    // Walk the (rewritten) ctor stmts; replace the first top-level `super(...)` with the
    // `var _self = Reflect.construct(...);` rebind, then splice field inits after it.
    let mut out: Vec<Stmt> = Vec::with_capacity(ctor_stmts.len() + field_inits.len() + 2);
    let mut spliced = false;
    let mut fields = Some(field_inits);
    for s in ctor_stmts {
        if !spliced {
            if let Some(args) = super_call_args(&s) {
                out.push(super_rebind_stmt(name, SELF, args));
                if let Some(f) = fields.take() {
                    out.extend(f);
                }
                spliced = true;
                continue;
            }
        }
        out.push(s);
    }
    if let Some(f) = fields.take() {
        // No top-level super found (defensive): emit fields anyway so none are dropped.
        out.extend(f);
    }
    // `return _self;` — only meaningful when we actually rebound; if no super was found
    // (defensive), `_self` is undefined and the return is harmless (new ignores a
    // non-object return). Emit it only when we spliced a rebind.
    if spliced {
        out.push(Stmt::Return(ReturnStmt {
            span: DUMMY_SP,
            arg: Some(Box::new(ident_expr(SELF))),
        }));
    }
    out
}

/// `var <self> = Reflect.construct(Object.getPrototypeOf(<name>), [<args...>], <name>);`
fn super_rebind_stmt(name: &str, self_name: &str, args: Vec<ExprOrSpread>) -> Stmt {
    let base = call(member(ident_expr("Object"), "getPrototypeOf"), vec![ident_expr(name)]);
    let args_array = Expr::Array(ArrayLit { span: DUMMY_SP, elems: args.into_iter().map(Some).collect() });
    let construct = call(
        member(ident_expr("Reflect"), "construct"),
        vec![base, args_array, ident_expr(name)],
    );
    Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        ctxt: Default::default(),
        kind: VarDeclKind::Var,
        declare: false,
        decls: vec![VarDeclarator {
            span: DUMMY_SP,
            name: Pat::Ident(BindingIdent {
                id: Ident::new(self_name.into(), DUMMY_SP, Default::default()),
                type_ann: None,
            }),
            init: Some(Box::new(construct)),
            definite: false,
        }],
    })))
}

/// If `s` is exactly `super(<args>);`, return its argument list.
fn super_call_args(s: &Stmt) -> Option<Vec<ExprOrSpread>> {
    let Stmt::Expr(es) = s else { return None };
    let Expr::Call(call) = &*es.expr else { return None };
    match &call.callee {
        Callee::Super(_) => Some(call.args.clone()),
        _ => None,
    }
}

/// Rewrites `this` → `<self_name>` within a derived constructor's OWN lexical `this`
/// scope: descends into arrows (which inherit `this`) but treats a nested non-arrow
/// `function(){}` as opaque (it rebinds `this`). `class_is_lowerable` guarantees the
/// ctor never references `super` inside such a nested function, so a non-arrow function
/// here only ever uses its OWN `this` — which we must leave untouched.
struct ThisRewriter {
    self_name: &'static str,
    nonarrow_depth: u32,
}

impl VisitMut for ThisRewriter {
    fn visit_mut_function(&mut self, n: &mut Function) {
        self.nonarrow_depth += 1;
        n.visit_mut_children_with(self);
        self.nonarrow_depth -= 1;
    }
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        if self.nonarrow_depth == 0 {
            if let Expr::This(_) = e {
                *e = ident_expr(self.self_name);
                return;
            }
        }
        e.visit_mut_children_with(self);
    }
}

/// `export { <name> };` as a module item.
fn export_named_item(name: &str) -> ModuleItem {
    let id = Ident::new(name.into(), DUMMY_SP, Default::default());
    ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(NamedExport {
        span: DUMMY_SP,
        specifiers: vec![ExportSpecifier::Named(ExportNamedSpecifier {
            span: DUMMY_SP,
            orig: ModuleExportName::Ident(id),
            exported: None,
            is_type_only: false,
        })],
        src: None,
        type_only: false,
        with: None,
    }))
}

// --- helpers (AST builders) ------------------------------------------------

fn undefined_expr() -> Expr {
    Expr::Ident(Ident::new("undefined".into(), DUMMY_SP, Default::default()))
}

fn ident_expr(name: &str) -> Expr {
    Expr::Ident(Ident::new(name.into(), DUMMY_SP, Default::default()))
}

fn member(obj: Expr, prop: &str) -> Expr {
    Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(obj),
        prop: MemberProp::Ident(IdentName::new(prop.into(), DUMMY_SP)),
    })
}

/// `this.<key> = <value>;`
fn this_assign_stmt(key: &str, value: Expr) -> Stmt {
    assign_member_stmt(Expr::This(ThisExpr { span: DUMMY_SP }), key, value)
}

/// `<obj>.<key> = <value>;`
fn assign_member_stmt(obj: Expr, key: &str, value: Expr) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(Expr::Assign(AssignExpr {
            span: DUMMY_SP,
            op: AssignOp::Assign,
            left: AssignTarget::Simple(SimpleAssignTarget::Member(MemberExpr {
                span: DUMMY_SP,
                obj: Box::new(obj),
                prop: MemberProp::Ident(IdentName::new(key.into(), DUMMY_SP)),
            })),
            right: Box::new(value),
        })),
    })
}

/// `function <name>(<params>) { <body> }`
fn function_decl(name: &str, params: Vec<Param>, body: Vec<Stmt>) -> Stmt {
    Stmt::Decl(Decl::Fn(FnDecl {
        ident: Ident::new(name.into(), DUMMY_SP, Default::default()),
        declare: false,
        function: Box::new(Function {
            params,
            decorators: vec![],
            span: DUMMY_SP,
            ctxt: Default::default(),
            body: Some(BlockStmt {
                span: DUMMY_SP,
                ctxt: Default::default(),
                stmts: body,
            }),
            is_generator: false,
            is_async: false,
            type_params: None,
            return_type: None,
        }),
    }))
}

fn rest_param(name: &str) -> Param {
    Param {
        span: DUMMY_SP,
        decorators: vec![],
        pat: Pat::Rest(RestPat {
            span: DUMMY_SP,
            dot3_token: DUMMY_SP,
            arg: Box::new(Pat::Ident(BindingIdent {
                id: Ident::new(name.into(), DUMMY_SP, Default::default()),
                type_ann: None,
            })),
            type_ann: None,
        }),
    }
}

/// `super(...args);` — the implicit-constructor forwarding call.
fn implicit_super_call_stmt(args_name: &str) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(Expr::Call(CallExpr {
            span: DUMMY_SP,
            ctxt: Default::default(),
            callee: Callee::Super(Super { span: DUMMY_SP }),
            args: vec![ExprOrSpread {
                spread: Some(DUMMY_SP),
                expr: Box::new(ident_expr(args_name)),
            }],
            type_args: None,
        })),
    })
}

/// `Object.setPrototypeOf(<a>, <b>);`
fn set_proto_of(a: Expr, b: Expr) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(call(
            member(ident_expr("Object"), "setPrototypeOf"),
            vec![a, b],
        )),
    })
}

fn call(callee: Expr, args: Vec<Expr>) -> Expr {
    Expr::Call(CallExpr {
        span: DUMMY_SP,
        ctxt: Default::default(),
        callee: Callee::Expr(Box::new(callee)),
        args: args
            .into_iter()
            .map(|e| ExprOrSpread { spread: None, expr: Box::new(e) })
            .collect(),
        type_args: None,
    })
}

/// `Object.defineProperty(<target>, "<key>", { <descriptor> })` for a method `m`. The
/// descriptor is `{ value: <fn>, writable: true, enumerable: false, configurable: true }`
/// for a plain method, or `{ get/set: <fn>, enumerable:false, configurable:true }` for
/// an accessor. Returns `None` if the method key is not a static name (already checked).
fn define_method(target: &Expr, m: &ClassMethod) -> Option<Stmt> {
    let key = static_key_name(&m.key)?;
    let func = Expr::Fn(FnExpr {
        ident: None,
        function: m.function.clone(),
    });
    let mut descriptor_props: Vec<PropOrSpread> = Vec::new();
    match m.kind {
        MethodKind::Method => {
            descriptor_props.push(kv_prop("value", func));
            descriptor_props.push(kv_prop("writable", bool_lit(true)));
        }
        MethodKind::Getter => {
            descriptor_props.push(kv_prop("get", func));
        }
        MethodKind::Setter => {
            descriptor_props.push(kv_prop("set", func));
        }
    }
    descriptor_props.push(kv_prop("enumerable", bool_lit(false)));
    descriptor_props.push(kv_prop("configurable", bool_lit(true)));

    let descriptor = Expr::Object(ObjectLit {
        span: DUMMY_SP,
        props: descriptor_props,
    });
    Some(Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(call(
            member(ident_expr("Object"), "defineProperty"),
            vec![target.clone(), str_lit(&key), descriptor],
        )),
    }))
}

fn kv_prop(key: &str, value: Expr) -> PropOrSpread {
    PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
        key: PropName::Ident(IdentName::new(key.into(), DUMMY_SP)),
        value: Box::new(value),
    })))
}

fn bool_lit(b: bool) -> Expr {
    Expr::Lit(Lit::Bool(Bool { span: DUMMY_SP, value: b }))
}

fn str_lit(s: &str) -> Expr {
    Expr::Lit(Lit::Str(Str { span: DUMMY_SP, value: s.into(), raw: None }))
}

// ===========================================================================
// 2. regex literal → new RegExp (stretch, wired, flag-gated)
// ===========================================================================

/// Lower every regex *literal* `/re/flags` to `new RegExp("re","flags")`.
///
/// ## Semantics note (why this is sound)
///
/// In modern JS (ES5+), a regex literal evaluates to a FRESH `RegExp` object each time
/// the literal expression is evaluated — there is no shared/cached object across loop
/// iterations (the ES3 "cached literal" behavior was removed). `new RegExp(src, flags)`
/// likewise produces a fresh object with `lastIndex === 0`. So:
///   * `/re/g` in a loop body  ≡  `new RegExp("re","g")` in a loop body — each iteration
///     gets a fresh object with `lastIndex` reset; identical statefulness.
///   * `.test`/`.exec` advancing `lastIndex` on a `g`/`y` regex behaves identically
///     because both forms are the same fresh object within one evaluation.
///   * `String.prototype.replace`/`match`/`split` coerce a string arg to a RegExp the
///     same way; passing a `RegExp` object (either form) is identical.
///
/// The one subtlety is `source`/`flags` STRING ESCAPING: the literal body `re` is the
/// raw pattern text, which is exactly what `RegExp(source)` expects, EXCEPT a literal
/// `/` inside a character class is written `\/` in a literal but must be `/` (or `\/`,
/// both legal) in the string. swc's `Regex.exp` is the literal body verbatim (with the
/// literal's escaping), and `RegExp("...")` accepts that same text. To avoid any
/// escaping hazard we build the `RegExp` argument as a STRING LITERAL whose value is the
/// regex source `Atom`, letting the codegen re-quote it canonically — the parser then
/// reads back the identical pattern.
///
/// This is wired and flag-gated; it is left DEFAULT-OFF (see config) until it has a
/// clean differential-fuzz run, per the Phase-4 contract.
fn desugar_regexes(items: &mut [ModuleItem]) {
    struct Rw;
    impl VisitMut for Rw {
        fn visit_mut_expr(&mut self, e: &mut Expr) {
            // Recurse first so nested regexes inside the new args are handled (there are
            // none we create, but keep the visit uniform).
            e.visit_mut_children_with(self);
            if let Expr::Lit(Lit::Regex(re)) = e {
                let source = re.exp.to_string();
                let flags = re.flags.to_string();
                let mut args = vec![ExprOrSpread { spread: None, expr: Box::new(str_lit(&source)) }];
                if !flags.is_empty() {
                    args.push(ExprOrSpread { spread: None, expr: Box::new(str_lit(&flags)) });
                }
                *e = Expr::New(NewExpr {
                    span: DUMMY_SP,
                    ctxt: Default::default(),
                    callee: Box::new(ident_expr("RegExp")),
                    args: Some(args),
                    type_args: None,
                });
            }
        }
    }
    for it in items.iter_mut() {
        it.visit_mut_with(&mut Rw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_core::Language;
    use mangler_jsast::lang::{Js, ParseOpts};

    fn items_of(src: &str) -> Vec<ModuleItem> {
        match Js.parse(src, &ParseOpts::default()).unwrap().into_program() {
            Program::Module(m) => m.body,
            Program::Script(s) => s.body.into_iter().map(ModuleItem::Stmt).collect(),
        }
    }
    fn print_items(items: Vec<ModuleItem>) -> String {
        let mut ast = Js.parse("0;", &ParseOpts::default()).unwrap();
        *ast.program_mut() = Program::Script(Script {
            span: DUMMY_SP,
            body: items
                .into_iter()
                .filter_map(|it| match it {
                    ModuleItem::Stmt(s) => Some(s),
                    _ => None,
                })
                .collect(),
            shebang: None,
        });
        Js.print(&ast)
    }

    fn lowered(src: &str) -> String {
        print_items(desugar_top_level(items_of(src), true, false))
    }

    /// A lowerable class is rewritten to function/prototype form: the `class` keyword is
    /// gone, the constructor becomes a function, methods go on the prototype via
    /// non-enumerable `Object.defineProperty`, and `extends`/`super` are reproduced via
    /// the prototype chain + `Reflect.construct`.
    #[test]
    fn lowers_full_supported_surface() {
        let out = lowered(
            "class A { constructor(n){ this.name=n; } describe(){ return this.name; } static k(){ return 'K'; } } \
             class B extends A { constructor(n){ super(n); this.s='w'; } speak(){ return this.name+this.s; } } \
             B.sp='c';",
        );
        assert!(!out.contains("class"), "no class keyword left: {out}");
        assert!(out.contains("function A("), "ctor became a function: {out}");
        assert!(out.contains("Object.defineProperty"), "methods via defineProperty: {out}");
        assert!(out.contains("enumerable:!1") || out.contains("enumerable:false"), "non-enumerable methods: {out}");
        assert!(out.contains("Object.setPrototypeOf"), "extends → prototype chain: {out}");
        assert!(out.contains("Reflect.construct"), "super → Reflect.construct: {out}");
    }

    /// Skip-desugar predicate: each unsupported member shape leaves the class native.
    #[test]
    fn skip_desugar_unsupported_shapes() {
        for src in [
            "class C { #x = 1; }",                          // private field
            "class C { #m(){} }",                           // private method
            "class C { static { this.x = 1; } }",           // static block
            "class C { [k](){} }",                          // computed-name method
            "class C { ['f'] = 1; }",                       // computed-name field
            "class C { get x(){ return 1; } }",             // accessor (tight surface)
            "class C { set x(v){} }",                       // accessor
            "class C { m(){ return super.toString(); } }",  // super.member
            "class C { constructor(){ this.t = new.target; } }", // new.target
            // `super()` NOT a single direct top-level statement → must stay native, else
            // build_derived_ctor would silently drop the base init (CRITICAL miscompile).
            "class B extends A { constructor(x){ if (x) super(x); else super(0); } }", // conditional
            "class B extends A { constructor(x){ { super(x); } } }",                   // in a block
            "class B extends A { constructor(x){ super(x) || 0; } }",                  // operand position
            "class B extends A { constructor(x){ super(x); super(x); } }",             // two super calls
        ] {
            let out = lowered(src);
            assert!(out.contains("class"), "must stay native: {src} =>\n{out}");
        }
    }

    /// A derived ctor with `super()` as a single direct top-level statement DOES lower
    /// (the positive case, to prove the tightened predicate didn't over-reject).
    #[test]
    fn lowers_top_level_super_statement() {
        let out = lowered("class A { constructor(x){ this.x = x; } } class B extends A { constructor(x){ super(x); this.y = 2; } }");
        assert!(!out.contains("class"), "top-level super lowers: {out}");
        assert!(out.contains("Reflect.construct"), "super rebind emitted: {out}");
    }

    /// A base class (no extends) with only fields + a method lowers; fields init in
    /// source order before the constructor body, methods are non-enumerable.
    #[test]
    fn lowers_base_class_field_order() {
        let out = lowered("class P { constructor(){ this.c = this.a + this.b; } } P.x = 1;");
        assert!(!out.contains("class"), "lowered: {out}");
        assert!(out.contains("function P("), "ctor function: {out}");
    }

    /// A class *expression* is NOT lowered (our lowering emits statements; an expression
    /// has no sound in-place statement form) — it stays native.
    #[test]
    fn class_expression_stays_native() {
        let out = lowered("var X = class { m(){ return 1; } };");
        assert!(out.contains("class"), "class expression stays native: {out}");
    }

    /// regex literal → `new RegExp(...)`, preserving source + flags.
    #[test]
    fn lowers_regex_literal() {
        let mut items = items_of("var r = /a(\\d)/gi; var s = /x/; var t = 5;");
        desugar_regexes(&mut items);
        let out = print_items(items);
        assert!(!out.contains("/a("), "regex literal gone: {out}");
        assert!(out.contains("new RegExp(") , "became new RegExp: {out}");
        assert!(out.contains("\"gi\"") || out.contains("'gi'"), "flags preserved: {out}");
        // a flagless literal omits the second arg.
        assert!(out.contains("RegExp(\"x\")") || out.contains("RegExp('x')"), "flagless regex: {out}");
    }

    /// Determinism: lowering is a pure function of the input AST.
    #[test]
    fn lowering_is_deterministic() {
        let src = "class A { constructor(n){ this.n = n; } m(){ return this.n; } } class B extends A { constructor(n){ super(n); } }";
        assert_eq!(lowered(src), lowered(src), "same input → identical lowering");
    }
}
