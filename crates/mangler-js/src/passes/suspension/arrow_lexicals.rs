//! Preserve lexical capabilities when an async arrow becomes an ordinary state
//! callback. Its original wrapper owns the lazy callable; the state callback
//! only evaluates source operands. LexicalBridges supplies each callable through
//! the existing native/eval class-operation protocol.
use super::*;
use mangler_vm::eval_class::Operation;
use swc_core::common::{DUMMY_SP, SyntaxContext};

pub(super) fn capture(body: &mut ArrowFunctionBody) -> Vec<Stmt> {
    struct References {
        construct: Ident,
        receiver: Ident,
        uses_construct: bool,
        uses_receiver: bool,
    }
    impl VisitMut for References {
        fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(binary, self);
        }
        fn visit_mut_function(&mut self, _: &mut Function) {}
        fn visit_mut_class(&mut self, class: &mut Class) {
            class.super_class.visit_mut_with(self);
            for member in &mut class.body {
                match member {
                    ClassMember::Method(method) => method.key.visit_mut_with(self),
                    ClassMember::ClassProp(property) => property.key.visit_mut_with(self),
                    ClassMember::AutoAccessor(accessor) => accessor.key.visit_mut_with(self),
                    _ => {}
                }
            }
        }
        fn visit_mut_expr(&mut self, expression: &mut Expr) {
            if let Expr::This(this) = expression {
                *expression = Expr::Call(CallExpr {
                    span: this.span,
                    callee: Callee::Expr(Box::new(Expr::Ident(self.receiver.clone()))),
                    ..Default::default()
                });
                self.uses_receiver = true;
            } else {
                expression.visit_mut_children_with(self);
            }
        }
        fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
            if matches!(call.callee, Callee::Super(_)) {
                call.callee = Callee::Expr(Box::new(Expr::Ident(self.construct.clone())));
                self.uses_construct = true;
            }
            call.visit_mut_children_with(self);
        }
    }
    fn binding(name: &str) -> Ident {
        Ident::new(
            name.into(),
            DUMMY_SP,
            SyntaxContext::empty().apply_mark(Mark::new()),
        )
    }
    let mut references = References {
        construct: binding("_lexical_super_call"),
        receiver: binding("_lexical_this"),
        uses_construct: false,
        uses_receiver: false,
    };
    body.visit_mut_with(&mut references);
    [
        (
            references.uses_construct,
            references.construct,
            Operation::Construct,
        ),
        (
            references.uses_receiver,
            references.receiver,
            Operation::This,
        ),
    ]
    .into_iter()
    .filter_map(|(used, binding, operation)| {
        used.then(|| {
            Stmt::Decl(Decl::Var(Box::new(VarDecl {
                decls: vec![VarDeclarator {
                    span: DUMMY_SP,
                    name: Pat::Ident(binding.into()),
                    init: Some(reference(operation)),
                    definite: false,
                }],
                ..Default::default()
            })))
        })
    })
    .collect()
}

/// SWC may synthesize a receiver capture for an implicit super-method receiver
/// even when the source contains no ThisExpression. Keep that generated binding
/// lazy too: constructing an async arrow in a derived constructor cannot read it.
pub(super) fn defer_generated_receivers(body: &mut ArrowFunctionBody) {
    struct Capture(HashSet<Id>);
    impl VisitMut for Capture {
        fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(binary, self);
        }
        fn visit_mut_function(&mut self, _: &mut Function) {}
        fn visit_mut_arrow_expr(&mut self, _: &mut ArrowExpr) {}
        fn visit_mut_class(&mut self, _: &mut Class) {}
        fn visit_mut_var_declarator(&mut self, declaration: &mut VarDeclarator) {
            if let Pat::Ident(binding) = &declaration.name
                && binding.id.span.is_dummy()
                && matches!(declaration.init.as_deref(), Some(Expr::This(_)))
            {
                self.0.insert(binding.id.to_id());
                declaration.init = Some(reference(Operation::This));
            } else {
                declaration.visit_mut_children_with(self);
            }
        }
    }
    let mut capture = Capture(HashSet::new());
    body.visit_mut_with(&mut capture);
    struct Reads(HashSet<Id>);
    impl VisitMut for Reads {
        fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(binary, self);
        }
        fn visit_mut_expr(&mut self, expression: &mut Expr) {
            if let Expr::Ident(name) = expression
                && self.0.contains(&name.to_id())
            {
                *expression = Expr::Call(CallExpr {
                    callee: Callee::Expr(Box::new(Expr::Ident(name.clone()))),
                    ..Default::default()
                });
            } else {
                expression.visit_mut_children_with(self);
            }
        }
    }
    if !capture.0.is_empty() {
        body.visit_mut_with(&mut Reads(capture.0));
    }
}

fn reference(operation: Operation) -> Box<Expr> {
    Box::new(Expr::Call(CallExpr {
        span: mangler_jsast::span::lexical_class_reference_span(),
        callee: Callee::Super(Super { span: DUMMY_SP }),
        args: vec![ExprOrSpread {
            spread: None,
            expr: Box::new(Expr::Lit(Lit::Num(Number {
                span: DUMMY_SP,
                value: f64::from(operation.id()),
                raw: None,
            }))),
        }],
        ..Default::default()
    }))
}
