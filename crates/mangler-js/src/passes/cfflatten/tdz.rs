//! TDZ-aware `let`/`const` → guarded `var` rewrite.
//!
//! The flattener (see [`super::cfg`] / [`super::emit`]) dissolves block scopes
//! into a flat state machine. Raw `let`/`const` would lose their block scoping
//! and Temporal-Dead-Zone semantics in the process, so before flattening a body
//! that uses them we lower each binding into a function-scoped `var` pair:
//!
//!   * `V`  — holds the value (hoisted `var V;`, initially `undefined`)
//!   * `T`  — a TDZ flag (hoisted `var T = 1;`, `1` while uninitialized)
//!
//! and rewrite every site (the guard's name argument is the hoisted *mangled*
//! var name `V`, never the original source name — see [`guard`]):
//!
//!   * declaration `let x = e;`  → `V = e; T = 0;`   (`let x;` → `T = 0;`)
//!   * read `x`                  → `(T ? __throwTdz("V") : V)`
//!   * write `x = e` (let)       → `(T ? __throwTdz("V") : (V = e))`
//!   * write `x = e` (const)     → `(e, (T ? __throwTdz("V") : __throwConst("V")))`
//!   * write `x op= e` (const)   → `(T ? __throwTdz("V") : (e, __throwConst("V")))`
//!   * update `x++` (let)        → `(T ? __throwTdz("V") : V++)`
//!   * update `x++` (const)      → `(T ? __throwTdz("V") : __throwConst("V"))`
//!
//! Bindings are identified by `(name, SyntaxContext)` using the resolver-assigned
//! mark, so same-named bindings in different (possibly nested) scopes map to
//! distinct hoisted vars and block-scoped shadowing is preserved exactly. The CF
//! pass therefore runs *after* the resolver (it declares
//! [`Resource::resolved_scopes`](mangler_passgraph::Resource::resolved_scopes) as a
//! read).
//!
//! Soundness is conservative: the flattener's fused gate scan
//! ([`super::eligibility::scan_gates`]) rejects constructs this lowering does not
//! model, and [`loop_let_captured`] rejects loop-scoped bindings captured by a
//! closure. Rejected bodies are simply left unflattened.

use std::collections::{HashMap, HashSet};

use swc_core::common::{DUMMY_SP, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

use super::helpers::TdzHelpers;
use crate::config::FileConfig;

/// Key identifying a binding: its source name plus its resolver mark.
type Key = (String, SyntaxContext);

struct Binding {
    val: String,
    tdz: String,
    is_const: bool,
}

// ---------------------------------------------------------------------------
// Safety gate
// ---------------------------------------------------------------------------
//
// The structural safety checks and loop-declared let/const name collection are
// computed by the flattener's fused gate scan
// ([`super::eligibility::scan_gates`]). Only the conditional closure-capture scan
// lives here — unlike every other gate walk it descends INTO nested
// functions/arrows.

/// Returns whether any nested closure references one of `names` (name-based,
/// over-approximate — a false positive only causes an extra skip). `names` are
/// the `let`/`const` bindings declared inside loops; a captured one would need
/// per-iteration freshness the lowering does not emulate, so the caller must skip
/// the body.
pub fn loop_let_captured(body: &BlockStmt, names: &[String]) -> bool {
    let names: HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    let mut cs = CaptureScan {
        names: &names,
        found: false,
    };
    body.visit_with(&mut cs);
    cs.found
}

/// Finds whether any nested closure references one of `names`.
struct CaptureScan<'a> {
    names: &'a HashSet<&'a str>,
    found: bool,
}
impl<'a> Visit for CaptureScan<'a> {
    fn visit_function(&mut self, n: &Function) {
        let mut r = NameRefScan {
            names: self.names,
            found: false,
        };
        n.visit_with(&mut r);
        if r.found {
            self.found = true;
        }
    }
    fn visit_arrow_expr(&mut self, n: &ArrowExpr) {
        let mut r = NameRefScan {
            names: self.names,
            found: false,
        };
        n.visit_with(&mut r);
        if r.found {
            self.found = true;
        }
    }
}

struct NameRefScan<'a> {
    names: &'a HashSet<&'a str>,
    found: bool,
}
impl<'a> Visit for NameRefScan<'a> {
    fn visit_ident(&mut self, n: &Ident) {
        if self.names.contains(n.sym.as_ref()) {
            self.found = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Rewrite
// ---------------------------------------------------------------------------

/// Lowers all `let`/`const` bindings in `body` to guarded vars, prepending the
/// hoisted `var V…;` / `var T = 1;` declarations. Returns whether any binding
/// was rewritten (so the caller can arrange helper injection). After this the
/// body contains only `var` declarations and is ready for flattening.
///
/// Caller must have checked the TDZ-safety gates first.
pub fn rewrite(body: &mut BlockStmt, cfg: &FileConfig, helpers: &TdzHelpers) -> bool {
    let mut collector = BindingCollector { order: Vec::new() };
    body.visit_with(&mut collector);
    if collector.order.is_empty() {
        return false;
    }

    let mut map: HashMap<Key, Binding> = HashMap::with_capacity(collector.order.len());
    let mut order: Vec<Key> = Vec::with_capacity(collector.order.len());
    for (key, is_const) in collector.order {
        if map.contains_key(&key) {
            continue;
        }
        let val = cfg.fresh_name();
        let tdz = cfg.fresh_name();
        order.push(key.clone());
        map.insert(key, Binding { val, tdz, is_const });
    }

    let mut rw = Rewriter { map: &map, helpers };
    body.visit_mut_with(&mut rw);

    // Prepend hoist declarations in deterministic (collection) order.
    let mut val_decls: Vec<VarDeclarator> = Vec::with_capacity(order.len());
    let mut tdz_decls: Vec<VarDeclarator> = Vec::with_capacity(order.len());
    for key in &order {
        let b = &map[key];
        val_decls.push(declarator(&b.val, None));
        tdz_decls.push(declarator(&b.tdz, Some(num(1.0))));
    }
    let hoist = vec![
        Stmt::Decl(Decl::Var(Box::new(VarDecl {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            kind: VarDeclKind::Var,
            declare: false,
            decls: val_decls,
        }))),
        Stmt::Decl(Decl::Var(Box::new(VarDecl {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            kind: VarDeclKind::Var,
            declare: false,
            decls: tdz_decls,
        }))),
    ];
    let at = mangler_jsast::directives::leading_directive_count(&body.stmts);
    body.stmts.splice(at..at, hoist);
    true
}

/// Collects every `let`/`const` simple binding in the body (including C-style
/// for-headers), not descending into nested functions.
struct BindingCollector {
    order: Vec<(Key, bool)>,
}
impl Visit for BindingCollector {
    fn visit_var_decl(&mut self, n: &VarDecl) {
        if n.kind != VarDeclKind::Var {
            let is_const = n.kind == VarDeclKind::Const;
            for d in &n.decls {
                if let Pat::Ident(bi) = &d.name {
                    let key = (bi.id.sym.to_string(), bi.id.ctxt);
                    self.order.push((key, is_const));
                }
            }
        }
        n.visit_children_with(self);
    }
    fn visit_function(&mut self, _n: &Function) {}
    fn visit_arrow_expr(&mut self, _n: &ArrowExpr) {}
    fn visit_class(&mut self, _n: &Class) {}
}

struct Rewriter<'a> {
    map: &'a HashMap<Key, Binding>,
    helpers: &'a TdzHelpers,
}

/// Owned copy of the bits of a [`Binding`] needed to build a rewrite, taken so
/// the `map` borrow ends before the AST is mutated in place.
struct BindingSnap {
    val: String,
    tdz: String,
    is_const: bool,
}

impl<'a> Rewriter<'a> {
    fn snapshot(&self, key: &Key) -> Option<BindingSnap> {
        self.map.get(key).map(|b| BindingSnap {
            val: b.val.clone(),
            tdz: b.tdz.clone(),
            is_const: b.is_const,
        })
    }

    /// Expands a `let`/`const` declaration statement into guarded assignments.
    fn expand_decl(&mut self, v: VarDecl) -> Vec<Stmt> {
        let mut out = Vec::new();
        for d in v.decls {
            // The initializer may reference outer (mapped) bindings even when this
            // declarator's own binding is not mapped. Visit it in every branch so
            // those references are rewritten.
            let mut init = d.init;
            if let Some(init) = init.as_deref_mut() {
                init.visit_mut_with(self);
            }
            let bi = match d.name {
                Pat::Ident(bi) => bi,
                // tdz_safe guarantees Ident patterns; keep defensively as a var.
                other => {
                    out.push(decl_stmt(VarDeclKind::Var, other, init));
                    continue;
                }
            };
            let key = (bi.id.sym.to_string(), bi.id.ctxt);
            let (val, tdz) = match self.map.get(&key) {
                Some(b) => (b.val.clone(), b.tdz.clone()),
                None => {
                    out.push(decl_stmt(VarDeclKind::Var, Pat::Ident(bi), init));
                    continue;
                }
            };
            if let Some(init) = init {
                out.push(assign_stmt(&val, AssignOp::Assign, *init));
            }
            out.push(assign_stmt(&tdz, AssignOp::Assign, num(0.0)));
        }
        out
    }
}

impl<'a> VisitMut for Rewriter<'a> {
    fn visit_mut_stmts(&mut self, stmts: &mut Vec<Stmt>) {
        let mut out = Vec::with_capacity(stmts.len());
        for s in std::mem::take(stmts) {
            match s {
                Stmt::Decl(Decl::Var(v)) if v.kind != VarDeclKind::Var => {
                    out.extend(self.expand_decl(*v));
                }
                mut other => {
                    other.visit_mut_with(self);
                    out.push(other);
                }
            }
        }
        *stmts = out;
    }

    fn visit_mut_for_stmt(&mut self, n: &mut ForStmt) {
        // Lower a `let`/`const` C-for header into an assignment sequence so the
        // binding becomes a hoisted guarded var like any other.
        if matches!(&n.init, Some(VarDeclOrExpr::VarDecl(v)) if v.kind != VarDeclKind::Var)
            && let Some(VarDeclOrExpr::VarDecl(v)) = n.init.take()
        {
            let mut exprs: Vec<Box<Expr>> = Vec::new();
            for d in v.decls {
                if let Pat::Ident(bi) = d.name
                    && let Some(b) = self.map.get(&(bi.id.sym.to_string(), bi.id.ctxt))
                {
                    let (val, tdz) = (b.val.clone(), b.tdz.clone());
                    if let Some(init) = d.init {
                        exprs.push(Box::new(assign_expr(&val, AssignOp::Assign, *init)));
                    }
                    exprs.push(Box::new(assign_expr(&tdz, AssignOp::Assign, num(0.0))));
                }
            }
            n.init = match exprs.len() {
                0 => None,
                1 => Some(VarDeclOrExpr::Expr(exprs.pop().unwrap())),
                _ => Some(VarDeclOrExpr::Expr(Box::new(Expr::Seq(SeqExpr {
                    span: DUMMY_SP,
                    exprs,
                })))),
            };
        }
        n.visit_mut_children_with(self);
    }

    fn visit_mut_prop(&mut self, p: &mut Prop) {
        // An object-literal shorthand property `{ x }` is BOTH the key `x` and a
        // value-reference to the binding `x` — but the value reference is NOT an
        // `Expr::Ident` node, so `visit_mut_expr` never sees it. If `x` is a mapped
        // let/const binding we must expand the shorthand to `{ x: <guarded read> }`
        // so the value reads the hoisted `V` (through the TDZ guard) instead of the
        // now-undeclared source name. Non-mapped shorthands are left untouched.
        if let Prop::Shorthand(id) = p {
            if let Some(b) = self.snapshot(&(id.sym.to_string(), id.ctxt)) {
                let key = PropName::Ident(IdentName {
                    span: id.span,
                    sym: id.sym.clone(),
                });
                let value = guard(&b.tdz, &self.helpers.throw_tdz, &b.val, ident_expr(&b.val));
                *p = Prop::KeyValue(KeyValueProp {
                    key,
                    value: Box::new(value),
                });
                return;
            }
            // Not mapped: nothing inside a bare ident to descend into.
            return;
        }
        // KeyValue / method / getter / setter / assign / spread: descend so any
        // mapped reads in computed keys or values are rewritten as usual.
        p.visit_mut_children_with(self);
    }

    fn visit_mut_expr(&mut self, e: &mut Expr) {
        match e {
            // Write: `x = rhs` / `x op= rhs`.
            Expr::Assign(a) => {
                let snap = simple_target_key(&a.left).and_then(|k| self.snapshot(&k));
                if let Some(b) = snap {
                    let op = a.op;
                    a.right.visit_mut_with(self);
                    let rhs = std::mem::replace(&mut a.right, Box::new(undefined_expr()));
                    *e = if b.is_const {
                        let throw_const = call_throw(&self.helpers.throw_const, &b.val);
                        if op == AssignOp::Assign {
                            // Plain `x = rhs`: real JS evaluates rhs first, then
                            // the write throws.
                            // (rhs, (T ? __throwTdz("x") : __throwConst("x")))
                            Expr::Seq(SeqExpr {
                                span: DUMMY_SP,
                                exprs: vec![
                                    rhs,
                                    Box::new(guard(
                                        &b.tdz,
                                        &self.helpers.throw_tdz,
                                        &b.val,
                                        throw_const,
                                    )),
                                ],
                            })
                        } else {
                            // Compound `x op= rhs` reads `x` BEFORE evaluating rhs.
                            // (T ? __throwTdz("x") : (rhs, __throwConst("x")))
                            guard(
                                &b.tdz,
                                &self.helpers.throw_tdz,
                                &b.val,
                                Expr::Seq(SeqExpr {
                                    span: DUMMY_SP,
                                    exprs: vec![rhs, Box::new(throw_const)],
                                }),
                            )
                        }
                    } else {
                        let inner = Expr::Assign(AssignExpr {
                            span: DUMMY_SP,
                            op,
                            left: ident_target(&b.val),
                            right: rhs,
                        });
                        guard(&b.tdz, &self.helpers.throw_tdz, &b.val, inner)
                    };
                    return;
                }
                e.visit_mut_children_with(self);
            }
            // Update: `x++` / `--x`.
            Expr::Update(u) => {
                let snap = match &*u.arg {
                    Expr::Ident(id) => self.snapshot(&(id.sym.to_string(), id.ctxt)),
                    _ => None,
                };
                if let Some(b) = snap {
                    *e = if b.is_const {
                        // `x++` reads `x` first.
                        guard(
                            &b.tdz,
                            &self.helpers.throw_tdz,
                            &b.val,
                            call_throw(&self.helpers.throw_const, &b.val),
                        )
                    } else {
                        let upd = Expr::Update(UpdateExpr {
                            span: DUMMY_SP,
                            op: u.op,
                            prefix: u.prefix,
                            arg: Box::new(ident_expr(&b.val)),
                        });
                        guard(&b.tdz, &self.helpers.throw_tdz, &b.val, upd)
                    };
                    return;
                }
                e.visit_mut_children_with(self);
            }
            // Read: bare `x`.
            Expr::Ident(id) => {
                if let Some(b) = self.snapshot(&(id.sym.to_string(), id.ctxt)) {
                    *e = guard(&b.tdz, &self.helpers.throw_tdz, &b.val, ident_expr(&b.val));
                }
            }
            _ => e.visit_mut_children_with(self),
        }
    }

    // Don't rewrite inside nested functions' bodies that re-bind — but they share
    // this body's hoisted vars when they reference an outer binding. The default
    // traversal already enters nested functions; an inner reference to an outer let
    // resolves to the same (name, ctxt) key and is rewritten to the shared `V`,
    // which is correct. Inner re-bindings have a different ctxt and are left alone.
}

// ---------------------------------------------------------------------------
// AST construction helpers
// ---------------------------------------------------------------------------

fn ident(name: &str) -> Ident {
    Ident::new(name.into(), DUMMY_SP, SyntaxContext::empty())
}

fn ident_expr(name: &str) -> Expr {
    Expr::Ident(ident(name))
}

fn ident_target(name: &str) -> AssignTarget {
    AssignTarget::Simple(SimpleAssignTarget::Ident(BindingIdent {
        id: ident(name),
        type_ann: None,
    }))
}

fn num(v: f64) -> Expr {
    Expr::Lit(Lit::Num(Number {
        span: DUMMY_SP,
        value: v,
        raw: None,
    }))
}

fn undefined_expr() -> Expr {
    Expr::Ident(ident("undefined"))
}

fn declarator(name: &str, init: Option<Expr>) -> VarDeclarator {
    VarDeclarator {
        span: DUMMY_SP,
        name: Pat::Ident(BindingIdent {
            id: ident(name),
            type_ann: None,
        }),
        init: init.map(Box::new),
        definite: false,
    }
}

fn decl_stmt(kind: VarDeclKind, name: Pat, init: Option<Box<Expr>>) -> Stmt {
    Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        kind,
        declare: false,
        decls: vec![VarDeclarator {
            span: DUMMY_SP,
            name,
            init,
            definite: false,
        }],
    })))
}

fn assign_expr(name: &str, op: AssignOp, rhs: Expr) -> Expr {
    Expr::Assign(AssignExpr {
        span: DUMMY_SP,
        op,
        left: ident_target(name),
        right: Box::new(rhs),
    })
}

fn assign_stmt(name: &str, op: AssignOp, rhs: Expr) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(assign_expr(name, op, rhs)),
    })
}

fn call_throw(fn_name: &str, arg: &str) -> Expr {
    Expr::Call(CallExpr {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        callee: Callee::Expr(Box::new(ident_expr(fn_name))),
        args: vec![ExprOrSpread {
            spread: None,
            expr: Box::new(Expr::Lit(Lit::Str(Str {
                span: DUMMY_SP,
                value: arg.into(),
                raw: None,
            }))),
        }],
        type_args: None,
    })
}

/// `(T ? __throwTdz("V") : <value>)`
///
/// `name` is the hoisted *mangled* var name (`b.val`), never the original
/// source identifier — embedding the source name here would leak a cleartext
/// rename map (the false branch is the same `V`, so this reveals nothing).
fn guard(tdz: &str, throw_tdz: &str, name: &str, value: Expr) -> Expr {
    Expr::Cond(CondExpr {
        span: DUMMY_SP,
        test: Box::new(ident_expr(tdz)),
        cons: Box::new(call_throw(throw_tdz, name)),
        alt: Box::new(value),
    })
}

fn simple_target_key(t: &AssignTarget) -> Option<Key> {
    match t {
        AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) => {
            Some((bi.id.sym.to_string(), bi.id.ctxt))
        }
        _ => None,
    }
}
