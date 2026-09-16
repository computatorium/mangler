//! Keep source directive prologues on generator activations. SWC's state builder
//! otherwise places directives inside switch cases, where they lose semantics.
use super::*;
use swc_core::common::SyntaxContext;

#[derive(Default)]
pub(super) struct GeneratorDirectives {
    bodies: HashMap<SyntaxContext, (SyntaxContext, Vec<Stmt>)>,
}

impl GeneratorDirectives {
    pub(super) fn take(program: &mut Program) -> Self {
        let mut directives = Self::default();
        program.visit_mut_with(&mut directives);
        directives
    }

    pub(super) fn restore(mut self, program: &mut Program) {
        struct Restore<'a>(&'a mut GeneratorDirectives);
        impl VisitMut for Restore<'_> {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_function(&mut self, function: &mut Function) {
                if let Some((context, directives)) = self.0.bodies.remove(&function.ctxt) {
                    function.ctxt = context;
                    function
                        .body
                        .as_mut()
                        .expect("generator body retained")
                        .stmts
                        .splice(0..0, directives);
                }
                function.visit_mut_children_with(self);
            }
        }
        program.visit_mut_with(&mut Restore(&mut self));
        assert!(
            self.bodies.is_empty(),
            "generator directives survive state lowering"
        );
    }
}

impl VisitMut for GeneratorDirectives {
    fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(binary, self);
    }
    fn visit_mut_function(&mut self, function: &mut Function) {
        function.visit_mut_children_with(self);
        if !function.is_generator {
            return;
        }
        let Some(body) = &mut function.body else {
            return;
        };
        let count = mangler_jsast::directives::leading_directive_count(&body.stmts);
        if count == 0 {
            return;
        }
        let marker = SyntaxContext::empty().apply_mark(Mark::new());
        let context = std::mem::replace(&mut function.ctxt, marker);
        self.bodies
            .insert(marker, (context, body.stmts.drain(..count).collect()));
    }
}

/// Async compatibility lowering moves the source body into a generator. Its
/// public entry must keep the source's own strict receiver/arguments semantics.
/// Arrows retain lexical receiver/arguments; their body directives are preserved
/// by GeneratorDirectives on the generated state driver.
pub(super) struct AsyncStrictness(HashSet<u32>);
impl AsyncStrictness {
    pub(super) fn capture(program: &Program) -> Self {
        struct Own(HashSet<u32>);
        impl Visit for Own {
            fn visit_bin_expr(&mut self, binary: &BinExpr) {
                mangler_jsast::deep::walk_binary(binary, self);
            }
            fn visit_function(&mut self, function: &Function) {
                if function.is_async
                    && function
                        .body
                        .as_ref()
                        .is_some_and(|body| mangler_jsast::directives::has_use_strict(&body.stmts))
                {
                    self.0.insert(function.span.lo.0);
                }
                function.visit_children_with(self);
            }
        }
        let mut own = Own(HashSet::new());
        program.visit_with(&mut own);
        Self(own.0)
    }

    pub(super) fn restore(self, program: &mut Program) {
        struct Restore {
            source: HashSet<u32>,
            found: HashSet<u32>,
        }
        impl Restore {
            fn body(&mut self, span: u32, body: &mut FunctionBody) {
                if !self.source.contains(&span) {
                    return;
                }
                self.found.insert(span);
                if !mangler_jsast::directives::has_use_strict(&body.stmts) {
                    body.stmts.insert(
                        0,
                        Stmt::Expr(ExprStmt {
                            span: swc_core::common::DUMMY_SP,
                            expr: Box::new(mangler_jsast::build::str_lit("use strict")),
                        }),
                    );
                }
            }
        }
        impl VisitMut for Restore {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_function(&mut self, function: &mut Function) {
                if let Some(body) = &mut function.body {
                    self.body(function.span.lo.0, body);
                }
                function.visit_mut_children_with(self);
            }
        }
        let mut restore = Restore {
            source: self.0,
            found: HashSet::new(),
        };
        program.visit_mut_with(&mut restore);
        assert_eq!(
            restore.source, restore.found,
            "async source entries survive lowering"
        );
    }
}
