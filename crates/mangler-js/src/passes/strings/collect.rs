//! Two-pass AST walker: collect safe `Lit::Str` literals (and template quasis),
//! emit placeholder `core(idx)` calls, then a finalizer rewrites each placeholder
//! into the planned `callee(index_expr)` form.
//!
//! Ported from the legacy `strings::collect` + `strings::replace` rewriter. The
//! [`SkipContext`] tracks AST positions that are UNSAFE to rewrite (directive
//! prologues, import/export sources, property keys, JSX, TS types) so directives
//! like `"use strict"` are never encoded.

use std::collections::HashMap;
use swc_core::common::{SyntaxContext, DUMMY_SP};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

/// Skip-position flags maintained on the rewriter visitor.
#[derive(Default, Clone, Copy)]
pub struct SkipContext {
    pub in_directive: bool,
    pub in_import_src: bool,
    pub in_export_src: bool,
    pub in_prop_key: bool,
    pub in_jsx: bool,
    pub in_ts_type: bool,
}

impl SkipContext {
    pub fn any(&self) -> bool {
        self.in_directive
            || self.in_import_src
            || self.in_export_src
            || self.in_prop_key
            || self.in_jsx
            || self.in_ts_type
    }
}

/// One recorded call site needing rewrite, captured during the first walk.
#[derive(Debug, Clone, Copy)]
pub struct PendingCall {
    /// Logical entry index after dedup.
    pub logical_idx: usize,
}

/// Per-call-site rewrite plan filled after encoding/sharding is decided.
pub struct DispatchPlan {
    /// The callee expression to invoke at each call site (length == call sites).
    pub call_callees: Vec<Expr>,
    /// The index expression to pass at each call site.
    pub call_index: Vec<Expr>,
}

/// First-pass collector: interns plaintexts (deduped), records each call site, and
/// replaces each safe `Lit::Str` (and template quasi) with a placeholder
/// `core(logical_idx)` call.
pub struct StringCollector {
    ctx_stack: SkipContext,
    pub plaintexts: Vec<String>,
    dedup: HashMap<String, usize>,
    pub pending: Vec<PendingCall>,
    core_name: String,
}

impl StringCollector {
    pub fn new(core_name: String) -> Self {
        StringCollector {
            ctx_stack: SkipContext::default(),
            plaintexts: Vec::new(),
            dedup: HashMap::new(),
            pending: Vec::new(),
            core_name,
        }
    }

    fn intern(&mut self, s: String) -> usize {
        if let Some(&i) = self.dedup.get(&s) {
            return i;
        }
        let i = self.plaintexts.len();
        self.plaintexts.push(s.clone());
        self.dedup.insert(s, i);
        i
    }

    fn build_placeholder_call(&self, idx: usize) -> Expr {
        Expr::Call(CallExpr {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            callee: Callee::Expr(Box::new(Expr::Ident(Ident::new(
                self.core_name.clone().into(),
                DUMMY_SP,
                SyntaxContext::empty(),
            )))),
            args: vec![ExprOrSpread {
                spread: None,
                expr: Box::new(Expr::Lit(Lit::Num(Number {
                    span: DUMMY_SP,
                    value: idx as f64,
                    raw: None,
                }))),
            }],
            type_args: None,
        })
    }

    /// Rewrite an untagged template literal: each static quasi is encoded like a
    /// string literal; substitutions are visited in place. Reproduces the original
    /// exactly while preserving template `ToString` coercion.
    fn rewrite_tpl(&mut self, expr: &mut Expr) {
        let Expr::Tpl(tpl) = expr else { return };

        // Bail (leave intact) if any cooked quasi has a lone surrogate.
        if tpl
            .quasis
            .iter()
            .any(|q| q.cooked.as_ref().is_some_and(|c| c.as_str().is_none()))
        {
            return;
        }

        let exprs = std::mem::take(&mut tpl.exprs);
        let quasis = std::mem::take(&mut tpl.quasis);

        let encode_quasi = |this: &mut Self, q: TplElement| -> Expr {
            let cooked = q
                .cooked
                .as_ref()
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string();
            let i = this.intern(cooked);
            this.pending.push(PendingCall { logical_idx: i });
            this.build_placeholder_call(i)
        };

        if exprs.is_empty() {
            let only = quasis.into_iter().next().unwrap_or(TplElement {
                span: DUMMY_SP,
                tail: true,
                cooked: Some("".into()),
                raw: "".into(),
            });
            *expr = encode_quasi(self, only);
            return;
        }

        let mut subs: Vec<Box<Expr>> = Vec::with_capacity(quasis.len() + exprs.len());
        let mut exprs_iter = exprs.into_iter();
        for q in quasis {
            subs.push(Box::new(encode_quasi(self, q)));
            if let Some(mut e) = exprs_iter.next() {
                e.visit_mut_with(self);
                subs.push(e);
            }
        }
        let quasis = empty_quasis(subs.len() + 1);
        *expr = Expr::Tpl(Tpl { span: DUMMY_SP, exprs: subs, quasis });
    }
}

/// Build `count` empty template quasis (`tail` on the last) — an all-substitution
/// template with no visible static text.
fn empty_quasis(count: usize) -> Vec<TplElement> {
    (0..count)
        .map(|i| TplElement {
            span: DUMMY_SP,
            tail: i + 1 == count,
            cooked: Some("".into()),
            raw: "".into(),
        })
        .collect()
}

impl VisitMut for StringCollector {
    fn visit_mut_expr(&mut self, expr: &mut Expr) {
        if matches!(expr, Expr::Tpl(_)) && !self.ctx_stack.any() {
            self.rewrite_tpl(expr);
            return;
        }

        expr.visit_mut_children_with(self);

        if self.ctx_stack.any() {
            return;
        }
        if let Expr::Lit(Lit::Str(s)) = expr {
            // Lone surrogates: leave unencoded (to_string_lossy would corrupt them).
            let Some(plaintext) = s.value.as_str() else {
                return;
            };
            let i = self.intern(plaintext.to_string());
            self.pending.push(PendingCall { logical_idx: i });
            *expr = self.build_placeholder_call(i);
        }
    }

    fn visit_mut_module(&mut self, m: &mut Module) {
        let n = leading_directive_count_module(&m.body);
        for (idx, item) in m.body.iter_mut().enumerate() {
            self.ctx_stack.in_directive = idx < n;
            item.visit_mut_with(self);
        }
        self.ctx_stack.in_directive = false;
    }

    fn visit_mut_script(&mut self, s: &mut Script) {
        let n = leading_directive_count_script(&s.body);
        for (idx, stmt) in s.body.iter_mut().enumerate() {
            self.ctx_stack.in_directive = idx < n;
            stmt.visit_mut_with(self);
        }
        self.ctx_stack.in_directive = false;
    }

    fn visit_mut_function(&mut self, f: &mut Function) {
        for p in &mut f.params {
            p.visit_mut_with(self);
        }
        for d in &mut f.decorators {
            d.visit_mut_with(self);
        }
        if let Some(body) = f.body.as_mut() {
            let n = leading_directive_count_script(&body.stmts);
            for (idx, stmt) in body.stmts.iter_mut().enumerate() {
                self.ctx_stack.in_directive = idx < n;
                stmt.visit_mut_with(self);
            }
            self.ctx_stack.in_directive = false;
        }
    }

    fn visit_mut_import_decl(&mut self, n: &mut ImportDecl) {
        for s in &mut n.specifiers {
            s.visit_mut_with(self);
        }
        let prev = self.ctx_stack.in_import_src;
        self.ctx_stack.in_import_src = true;
        n.src.visit_mut_with(self);
        self.ctx_stack.in_import_src = prev;
        if let Some(asserts) = n.with.as_mut() {
            asserts.visit_mut_with(self);
        }
    }

    fn visit_mut_named_export(&mut self, n: &mut NamedExport) {
        for s in &mut n.specifiers {
            s.visit_mut_with(self);
        }
        if let Some(src) = n.src.as_mut() {
            let prev = self.ctx_stack.in_export_src;
            self.ctx_stack.in_export_src = true;
            src.visit_mut_with(self);
            self.ctx_stack.in_export_src = prev;
        }
        if let Some(asserts) = n.with.as_mut() {
            asserts.visit_mut_with(self);
        }
    }

    fn visit_mut_export_all(&mut self, n: &mut ExportAll) {
        let prev = self.ctx_stack.in_export_src;
        self.ctx_stack.in_export_src = true;
        n.src.visit_mut_with(self);
        self.ctx_stack.in_export_src = prev;
        if let Some(asserts) = n.with.as_mut() {
            asserts.visit_mut_with(self);
        }
    }

    fn visit_mut_prop_name(&mut self, n: &mut PropName) {
        match n {
            PropName::Str(_) => {
                let prev = self.ctx_stack.in_prop_key;
                self.ctx_stack.in_prop_key = true;
                n.visit_mut_children_with(self);
                self.ctx_stack.in_prop_key = prev;
            }
            _ => n.visit_mut_children_with(self),
        }
    }

    fn visit_mut_jsx_element(&mut self, n: &mut JSXElement) {
        let prev = self.ctx_stack.in_jsx;
        self.ctx_stack.in_jsx = true;
        n.visit_mut_children_with(self);
        self.ctx_stack.in_jsx = prev;
    }

    fn visit_mut_jsx_fragment(&mut self, n: &mut JSXFragment) {
        let prev = self.ctx_stack.in_jsx;
        self.ctx_stack.in_jsx = true;
        n.visit_mut_children_with(self);
        self.ctx_stack.in_jsx = prev;
    }

    fn visit_mut_ts_type(&mut self, n: &mut TsType) {
        let prev = self.ctx_stack.in_ts_type;
        self.ctx_stack.in_ts_type = true;
        n.visit_mut_children_with(self);
        self.ctx_stack.in_ts_type = prev;
    }
}

/// Second pass: replace each placeholder `core(N)` call with `callee(index_expr)`
/// drawn from the dispatch plan, in source (post-order) sequence.
pub struct RewriteFinalizer<'a> {
    core_name: &'a str,
    plan: &'a DispatchPlan,
    cursor: usize,
}

impl<'a> RewriteFinalizer<'a> {
    pub fn new(core_name: &'a str, plan: &'a DispatchPlan) -> Self {
        RewriteFinalizer { core_name, plan, cursor: 0 }
    }
}

impl VisitMut for RewriteFinalizer<'_> {
    fn visit_mut_expr(&mut self, expr: &mut Expr) {
        expr.visit_mut_children_with(self);

        let is_placeholder = match expr {
            Expr::Call(c) => match &c.callee {
                Callee::Expr(e) => {
                    matches!(&**e, Expr::Ident(id) if id.sym.as_ref() == self.core_name)
                }
                _ => false,
            },
            _ => false,
        };
        if !is_placeholder {
            return;
        }
        let i = self.cursor;
        if i >= self.plan.call_callees.len() {
            return;
        }
        let callee = self.plan.call_callees[i].clone();
        let arg_expr = self.plan.call_index[i].clone();
        self.cursor += 1;
        *expr = Expr::Call(CallExpr {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            callee: Callee::Expr(Box::new(callee)),
            args: vec![ExprOrSpread { spread: None, expr: Box::new(arg_expr) }],
            type_args: None,
        });
    }
}

fn is_directive_stmt(stmt: &Stmt) -> bool {
    matches!(stmt, Stmt::Expr(ExprStmt { expr, .. }) if matches!(&**expr, Expr::Lit(Lit::Str(_))))
}

fn leading_directive_count_script(stmts: &[Stmt]) -> usize {
    stmts.iter().take_while(|s| is_directive_stmt(s)).count()
}

fn leading_directive_count_module(body: &[ModuleItem]) -> usize {
    body.iter()
        .take_while(|it| matches!(it, ModuleItem::Stmt(s) if is_directive_stmt(s)))
        .count()
}
