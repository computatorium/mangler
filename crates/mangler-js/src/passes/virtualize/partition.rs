//! Preserve native declaration/module boundaries and group executable statements.
//!
//! Native lexical and variable bindings are authoritative. VM chunks capture their
//! live descriptors; initializer chunks run at the original declaration position.
//! There is no parallel cell representation or silent unsupported-statement escape.

use std::collections::HashSet;

use swc_core::common::{DUMMY_SP, Spanned};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

/// One top-level item paired with its §3.1 classification.
pub(crate) enum Class {
    /// Preserve a binding/module envelope or a top-level suspension boundary.
    /// The wrapper pass compiles initializer expressions and await state producers.
    Native(ModuleItem),
    /// Executable statement compiled with the adjacent statements in its run.
    /// Compilation errors propagate to the caller and prevent artifact emission.
    Wrappable(Stmt),
}

/// Classify statements, keeping native binding envelopes and any declaration of a
/// `protected` name at its top level — used to keep the strings-decoder stub
/// (`var <core> = (function(){…})()`) out of the VM. Virtualized user code may still
/// CALL `core(idx)` (it captures `core` as a free global); only the declaration must
/// remain a native module binding so that capture resolves.
pub(crate) fn classify_with_protected(items: Vec<ModuleItem>, protected: &[String]) -> Vec<Class> {
    items
        .into_iter()
        .map(|it| match it {
            ModuleItem::ModuleDecl(_) => Class::Native(it),
            ModuleItem::Stmt(s) => {
                // Native binding envelopes preserve global object exposure, lexical
                // TDZ and module live bindings. Their initializer expressions and
                // callable bodies receive independent VM chunks in the wrapper pass.
                if matches!(
                    s,
                    Stmt::Decl(Decl::Var(_) | Decl::Fn(_) | Decl::Class(_) | Decl::Using(_))
                ) || matches!(&s, Stmt::Expr(expression)
                        if mangler_jsast::span::is_runtime_span(expression.expr.span()))
                    || stmt_has_top_level_await(&s)
                    || stmt_declares_any(&s, protected)
                {
                    Class::Native(ModuleItem::Stmt(s))
                } else {
                    Class::Wrappable(s)
                }
            }
        })
        .collect()
}

/// True if `s` declares (at its own top level: `var`/`let`/`const`/`function`) any
/// name in `names`. Does not descend into nested functions/blocks (a protected name
/// is declared at module top by the strings stub).
fn stmt_declares_any(s: &Stmt, names: &[String]) -> bool {
    if names.is_empty() {
        return false;
    }
    match s {
        Stmt::Decl(Decl::Var(v)) => v.decls.iter().any(|d| {
            let mut hit = false;
            mangler_jsast::analysis::binding_names(&d.name, &mut |id| {
                if names.iter().any(|n| n == id.sym.as_ref()) {
                    hit = true;
                }
            });
            hit
        }),
        Stmt::Decl(Decl::Fn(f)) => names.iter().any(|n| n == f.ident.sym.as_ref()),
        _ => false,
    }
}

/// True if `s` contains an `await` expression that is NOT nested inside a function or
/// arrow. The wrapper handles these with a bytecode state producer and a native
/// module await driver. `for await` heads count too.
pub(super) fn stmt_has_top_level_await(s: &Stmt) -> bool {
    struct Scan {
        found: bool,
    }
    impl Visit for Scan {
        fn visit_bin_expr(&mut self, binary: &BinExpr) {
            mangler_jsast::deep::walk_binary(binary, self);
        }
        // Do not descend into nested functions/arrows: their `await` belongs to an
        // async function, not the module top level.
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
        fn visit_await_expr(&mut self, _: &AwaitExpr) {
            self.found = true;
        }
        fn visit_for_of_stmt(&mut self, n: &ForOfStmt) {
            if n.is_await {
                self.found = true;
            }
            n.visit_children_with(self);
        }
    }
    let mut sc = Scan { found: false };
    s.visit_with(&mut sc);
    sc.found
}

/// A maximal contiguous run of wrappable statements, recorded by its position in the
/// classified item list. Native envelopes and suspension drivers remain between runs.
pub(crate) enum Segment {
    Native(ModuleItem),
    /// A contiguous run of wrappable statements (in program order).
    Run(Vec<Stmt>),
}

/// Group the classified items into native segments and maximal wrappable runs, in
/// program order.
pub(crate) fn segment(classes: Vec<Class>) -> Vec<Segment> {
    let mut segs: Vec<Segment> = Vec::new();
    let mut run: Vec<Stmt> = Vec::new();
    for c in classes {
        match c {
            Class::Wrappable(s) => run.push(s),
            Class::Native(it) => {
                if !run.is_empty() {
                    segs.push(Segment::Run(std::mem::take(&mut run)));
                }
                segs.push(Segment::Native(it));
            }
        }
    }
    if !run.is_empty() {
        segs.push(Segment::Run(run));
    }
    segs
}

/// Program var bindings keep their native storage so host observers and module
/// live bindings see updates made inside independently compiled statement runs.
pub(super) fn program_var_bindings(program: &Program, strict: bool) -> HashSet<String> {
    mangler_jsast::analysis::declarations::program_var_declarations(program, strict)
        .into_iter()
        .collect()
}

pub(super) fn var_hoist(names: &HashSet<String>) -> Option<Stmt> {
    if names.is_empty() {
        return None;
    }
    let mut names: Vec<_> = names.iter().collect();
    names.sort();
    Some(Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        kind: VarDeclKind::Var,
        decls: names
            .into_iter()
            .map(|name| VarDeclarator {
                span: DUMMY_SP,
                name: Pat::Ident(Ident::new_no_ctxt(name.as_str().into(), DUMMY_SP).into()),
                init: None,
                definite: false,
            })
            .collect(),
        ..Default::default()
    }))))
}
