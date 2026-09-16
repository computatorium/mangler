//! A private brand is lexical syntax, never an evaluated binary left operand.
//! Hide that operand from the state builder, which otherwise caches `#name` in
//! a temporary before a suspended RHS. Restore the operator before bytecode or
//! native lexical-capability lowering sees the transformed program.
use super::*;
use swc_core::common::{BytePos, Span};

pub(super) struct PrivateIn(HashMap<Span, (Span, PrivateName)>);
impl PrivateIn {
    pub(super) fn take(program: &mut Program) -> Self {
        struct Spans(HashSet<Span>);
        impl Visit for Spans {
            fn visit_bin_expr(&mut self, binary: &BinExpr) {
                mangler_jsast::deep::walk_binary(binary, self);
            }
            fn visit_span(&mut self, span: &Span) {
                if span.lo == BytePos(0) {
                    self.0.insert(*span);
                }
            }
        }
        struct Yield(bool);
        impl Visit for Yield {
            fn visit_function(&mut self, _: &Function) {}
            fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
            fn visit_yield_expr(&mut self, _: &YieldExpr) {
                self.0 = true;
            }
            fn visit_bin_expr(&mut self, binary: &BinExpr) {
                mangler_jsast::deep::walk_binary(binary, self);
            }
        }
        struct Take {
            occupied: HashSet<Span>,
            entries: HashMap<Span, (Span, PrivateName)>,
            next: u32,
        }
        impl VisitMut for Take {
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                mangler_jsast::deep::rewrite_expression_spine(expression, self, Self::rewrite);
            }
        }
        impl Take {
            fn rewrite(&mut self, expression: &mut Expr) {
                let Expr::Bin(binary) = expression else {
                    return;
                };
                if binary.op != BinaryOp::In {
                    return;
                }
                let Expr::PrivateName(private) = &*binary.left else {
                    return;
                };
                let mut yielded = Yield(false);
                binary.right.visit_with(&mut yielded);
                if !yielded.0 {
                    return;
                }
                // These identities exist only across the state transform. Avoid
                // every input span, including other compiler protocol markers.
                let marker = loop {
                    let span = Span::new(BytePos(0), BytePos(self.next));
                    self.next -= 1;
                    if self.occupied.insert(span) {
                        break span;
                    }
                };
                self.entries.insert(marker, (binary.span, private.clone()));
                let rhs = std::mem::replace(
                    &mut binary.right,
                    Box::new(Expr::Invalid(Invalid { span: binary.span })),
                );
                *expression = Expr::Unary(UnaryExpr {
                    span: marker,
                    op: UnaryOp::TypeOf,
                    arg: rhs,
                });
            }
        }
        let mut spans = Spans(HashSet::new());
        program.visit_with(&mut spans);
        let mut take = Take {
            occupied: spans.0,
            entries: HashMap::new(),
            next: u32::MAX,
        };
        program.visit_mut_with(&mut take);
        Self(take.entries)
    }
    pub(super) fn restore(self, program: &mut Program) {
        struct Restore(HashMap<Span, (Span, PrivateName)>);
        impl VisitMut for Restore {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                expression.visit_mut_children_with(self);
                let Expr::Unary(unary) = expression else {
                    return;
                };
                let Some((span, private)) = self.0.get(&unary.span) else {
                    return;
                };
                let rhs = std::mem::replace(
                    &mut unary.arg,
                    Box::new(Expr::Invalid(Invalid { span: *span })),
                );
                *expression = Expr::Bin(BinExpr {
                    span: *span,
                    op: BinaryOp::In,
                    left: Box::new(Expr::PrivateName(private.clone())),
                    right: rhs,
                });
            }
        }
        struct Remaining<'a>(&'a HashMap<Span, (Span, PrivateName)>);
        impl Visit for Remaining<'_> {
            fn visit_bin_expr(&mut self, binary: &BinExpr) {
                mangler_jsast::deep::walk_binary(binary, self);
            }
            fn visit_span(&mut self, span: &Span) {
                assert!(
                    !self.0.contains_key(span),
                    "private brand operator restored"
                );
            }
        }
        let mut restore = Restore(self.0);
        program.visit_mut_with(&mut restore);
        // Unreachable branches can disappear; every surviving carrier must be
        // consumed, without assuming prepared and restored node counts match.
        program.visit_with(&mut Remaining(&restore.0));
    }
}
