//! Free-global detection.
//!
//! Sound, pre-resolver detection without per-scope analysis: a name is a *free
//! global* iff it is **never declared anywhere in the file**. We therefore walk
//! the whole program once collecting EVERY binding name introduced by any
//! declaration form, and separately detect the bail conditions (direct `eval(`
//! call or a `with` statement). Any user shadow of `X` anywhere in the file
//! lands `X` in the declared-set, which conservatively disables indirection of
//! `X` file-wide.

use mangler_jsast::analysis::{binding_names, is_direct_eval_callee};
use std::collections::HashSet;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

/// Peel nested `(…)` wrappers off an expression. swc represents a parenthesized
/// write target as `Expr::Paren { expr }`, so `(X) = 1`, `++(X)`, `delete (X)`,
/// `for ((X) of a)`, and `[(X)] = arr` all hide the bare ident behind one or more
/// `Paren` layers. The write-target collectors peel these before testing for a
/// bare `Expr::Ident`, so a parenthesized write still excludes the whole name.
fn peel_parens(e: &Expr) -> &Expr {
    let mut e = e;
    while let Expr::Paren(p) = e {
        e = &p.expr;
    }
    e
}

/// Result of the detection walk.
pub struct Detection {
    /// Every identifier symbol appearing ANYWHERE in the file (the "all
    /// identifiers universe"), collected in the SAME traversal as the
    /// declared/written sets, so injected alias names are guaranteed collision-
    /// free against both declarations and references.
    pub all_idents: HashSet<String>,
    /// Every binding name declared anywhere in the file.
    pub declared: HashSet<String>,
    /// Every bare identifier that appears in *any* write / mutation-target
    /// position anywhere in the file: an assignment target (`X = v`, `X += v`,
    /// destructuring assignment leaves `[X] = a` / `({k: X} = o)`), an update
    /// operand (`++X` / `X--`), a bare `delete X`, or a non-`VarDecl` for-in /
    /// for-of head target (`for (X of a)` / `for (X in o)`).
    ///
    /// Per the **whole-name write-exclusion** rule, a free
    /// global is indirectable only if EVERY occurrence is a read; if a name
    /// lands in this set the ENTIRE name is excluded from indirection (the
    /// hoisted `_Ga = _G["X"]` alias is a load-time snapshot, so indirecting
    /// reads of a name that is also written would desync from the live global).
    pub written: HashSet<String>,
    /// Set if the file contains a direct `eval(...)` call or a `with` statement;
    /// when true the pass must bail entirely (dynamic bindings break the
    /// never-declared soundness argument).
    pub bail: bool,
}

/// Walk `program` collecting all declared binding names, all written names, and
/// the bail flag.
pub fn detect(program: &Program) -> Detection {
    let mut v = Detector {
        all_idents: HashSet::new(),
        declared: HashSet::new(),
        written: HashSet::new(),
        bail: false,
    };
    program.visit_with(&mut v);
    Detection {
        all_idents: v.all_idents,
        declared: v.declared,
        written: v.written,
        bail: v.bail,
    }
}

struct Detector {
    all_idents: HashSet<String>,
    declared: HashSet<String>,
    written: HashSet<String>,
    bail: bool,
}

impl Detector {
    /// Insert into `all_idents` after a cheap `&str` membership check so repeat
    /// identifiers avoid a fresh `to_string()` allocation.
    #[inline]
    fn add_ident(&mut self, sym: &str) {
        if !self.all_idents.contains(sym) {
            self.all_idents.insert(sym.to_string());
        }
    }

    /// Record every binding identifier introduced by a binding pattern,
    /// recursing through destructuring. Delegates to the canonical
    /// [`binding_names`] walk so this matches every other binding collector.
    fn collect_pat(&mut self, pat: &Pat) {
        let declared = &mut self.declared;
        binding_names(pat, &mut |id| {
            declared.insert(id.sym.to_string());
        });
    }

    /// Record every bare identifier in a *write-target* `Pat` (assignment
    /// pattern leaf or non-`VarDecl` for-in/for-of head). Unlike `collect_pat`
    /// (which records *bindings*), a write target's bare-ident leaf may be
    /// encoded by swc as either `Pat::Ident` *or* `Pat::Expr(Expr::Ident)` —
    /// handle both. Nested members / computed sub-expressions (`[a.b] = …`) are
    /// not bare globals being written, so they do not exclude a name.
    fn collect_write_pat(&mut self, pat: &Pat) {
        match pat {
            Pat::Ident(BindingIdent { id, .. }) => {
                self.written.insert(id.sym.to_string());
            }
            Pat::Expr(e) => {
                // The leaf may be parenthesized (`for ((X) of a)`, `[(X)] = arr`).
                if let Expr::Ident(id) = peel_parens(e) {
                    self.written.insert(id.sym.to_string());
                }
            }
            Pat::Array(ArrayPat { elems, .. }) => {
                for e in elems.iter().flatten() {
                    self.collect_write_pat(e);
                }
            }
            Pat::Object(ObjectPat { props, .. }) => {
                for p in props {
                    match p {
                        ObjectPatProp::KeyValue(KeyValuePatProp { value, .. }) => {
                            self.collect_write_pat(value);
                        }
                        ObjectPatProp::Assign(AssignPatProp { key, .. }) => {
                            // `({ X } = o)` / `({ X = d } = o)` — `X` is written.
                            self.written.insert(key.id.sym.to_string());
                        }
                        ObjectPatProp::Rest(RestPat { arg, .. }) => {
                            self.collect_write_pat(arg);
                        }
                    }
                }
            }
            Pat::Rest(RestPat { arg, .. }) => self.collect_write_pat(arg),
            Pat::Assign(AssignPat { left, .. }) => self.collect_write_pat(left),
            Pat::Invalid(_) => {}
        }
    }

    /// Record every bare identifier in an assignment target (`X = v`,
    /// `[X] = a`, `({k: X} = o)`).
    fn collect_assign_target(&mut self, target: &AssignTarget) {
        match target {
            AssignTarget::Simple(SimpleAssignTarget::Ident(b)) => {
                self.written.insert(b.id.sym.to_string());
            }
            AssignTarget::Simple(SimpleAssignTarget::Paren(p)) => {
                // `(X) = 1` / `(X) += 1` — peel parens to find the bare-ident
                // write target. A peeled member/other expr is no bare-global write.
                if let Expr::Ident(id) = peel_parens(&p.expr) {
                    self.written.insert(id.sym.to_string());
                }
            }
            AssignTarget::Pat(AssignTargetPat::Array(a)) => {
                let pat = Pat::Array(a.clone());
                self.collect_write_pat(&pat);
            }
            AssignTarget::Pat(AssignTargetPat::Object(o)) => {
                let pat = Pat::Object(o.clone());
                self.collect_write_pat(&pat);
            }
            // Member targets (`obj.x = …`), TS wrappers, invalid: no bare-global
            // write to exclude.
            _ => {}
        }
    }

    /// Record the bare-ident target(s) of a non-`VarDecl` for-in/for-of head.
    fn collect_for_head(&mut self, head: &ForHead) {
        if let ForHead::Pat(pat) = head {
            self.collect_write_pat(pat);
        }
        // `ForHead::VarDecl` / `ForHead::UsingDecl` are declarations, already
        // handled as bindings by the declared-set walk; they are not writes to a
        // free global.
    }
}

impl Visit for Detector {
    // ── All-identifiers universe ─────────────────────────────────────────────

    fn visit_ident(&mut self, n: &Ident) {
        self.add_ident(n.sym.as_ref());
    }

    fn visit_ident_name(&mut self, n: &IdentName) {
        self.add_ident(n.sym.as_ref());
    }

    fn visit_binding_ident(&mut self, n: &BindingIdent) {
        self.add_ident(n.id.sym.as_ref());
        n.visit_children_with(self);
    }

    // ── Bail conditions ─────────────────────────────────────────────────────

    fn visit_with_stmt(&mut self, n: &WithStmt) {
        self.bail = true;
        n.visit_children_with(self);
    }

    fn visit_call_expr(&mut self, n: &CallExpr) {
        // Direct `eval(...)` (shared paren-peeling policy).
        if is_direct_eval_callee(&n.callee) {
            self.bail = true;
        }
        n.visit_children_with(self);
    }

    // ── Write / mutation-target positions (whole-name write-exclusion) ───────

    fn visit_assign_expr(&mut self, n: &AssignExpr) {
        self.collect_assign_target(&n.left);
        n.visit_children_with(self);
    }

    fn visit_update_expr(&mut self, n: &UpdateExpr) {
        // `++(X)` / `(X)++` wraps the operand in `Expr::Paren`; peel first.
        if let Expr::Ident(id) = peel_parens(&n.arg) {
            self.written.insert(id.sym.to_string());
        }
        n.visit_children_with(self);
    }

    fn visit_unary_expr(&mut self, n: &UnaryExpr) {
        if n.op == UnaryOp::Delete {
            // `delete (X)` wraps the operand in `Expr::Paren`; peel first.
            if let Expr::Ident(id) = peel_parens(&n.arg) {
                self.written.insert(id.sym.to_string());
            }
        }
        n.visit_children_with(self);
    }

    fn visit_for_in_stmt(&mut self, n: &ForInStmt) {
        self.collect_for_head(&n.left);
        n.visit_children_with(self);
    }

    fn visit_for_of_stmt(&mut self, n: &ForOfStmt) {
        self.collect_for_head(&n.left);
        n.visit_children_with(self);
    }

    // ── Binding forms ───────────────────────────────────────────────────────

    fn visit_var_declarator(&mut self, n: &VarDeclarator) {
        self.collect_pat(&n.name);
        n.visit_children_with(self);
    }

    fn visit_fn_decl(&mut self, n: &FnDecl) {
        self.declared.insert(n.ident.sym.to_string());
        n.visit_children_with(self);
    }

    fn visit_class_decl(&mut self, n: &ClassDecl) {
        self.declared.insert(n.ident.sym.to_string());
        n.visit_children_with(self);
    }

    fn visit_fn_expr(&mut self, n: &FnExpr) {
        // Named function expression: the name is bound inside its own scope.
        if let Some(id) = &n.ident {
            self.declared.insert(id.sym.to_string());
        }
        n.visit_children_with(self);
    }

    fn visit_class_expr(&mut self, n: &ClassExpr) {
        // Named class expression: the name is bound inside its own scope.
        if let Some(id) = &n.ident {
            self.declared.insert(id.sym.to_string());
        }
        n.visit_children_with(self);
    }

    fn visit_param(&mut self, n: &Param) {
        self.collect_pat(&n.pat);
        n.visit_children_with(self);
    }

    /// Arrow params are bare `Pat`s (not wrapped in `Param`).
    fn visit_arrow_expr(&mut self, n: &ArrowExpr) {
        for p in &n.params {
            self.collect_pat(p);
        }
        n.visit_children_with(self);
    }

    /// Constructor params are `ParamOrTsParamProp`.
    fn visit_constructor(&mut self, n: &Constructor) {
        for p in &n.params {
            match p {
                ParamOrTsParamProp::Param(param) => self.collect_pat(&param.pat),
                ParamOrTsParamProp::TsParamProp(tp) => match &tp.param {
                    TsParamPropParam::Ident(b) => {
                        self.declared.insert(b.id.sym.to_string());
                    }
                    TsParamPropParam::Assign(a) => self.collect_pat(&Pat::Assign(a.clone())),
                },
            }
        }
        n.visit_children_with(self);
    }

    fn visit_catch_clause(&mut self, n: &CatchClause) {
        if let Some(p) = &n.param {
            self.collect_pat(p);
        }
        n.visit_children_with(self);
    }

    fn visit_import_decl(&mut self, n: &ImportDecl) {
        for spec in &n.specifiers {
            match spec {
                ImportSpecifier::Named(s) => {
                    self.declared.insert(s.local.sym.to_string());
                }
                ImportSpecifier::Default(s) => {
                    self.declared.insert(s.local.sym.to_string());
                }
                ImportSpecifier::Namespace(s) => {
                    self.declared.insert(s.local.sym.to_string());
                }
            }
        }
        n.visit_children_with(self);
    }
}
