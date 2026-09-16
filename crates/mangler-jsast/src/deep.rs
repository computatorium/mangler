//! Heap-backed operations for expression chains produced by generated JavaScript.
use swc_core::{
    common::{BytePos, DUMMY_SP, Span},
    ecma::{
        ast::*,
        visit::{Visit, VisitMut, VisitMutWith, VisitWith},
    },
};

/// Copy a program without recursively cloning binary or parenthesized spines.
/// Source positions and resolver identities are preserved in both trees.
pub fn clone_program(program: &mut Program) -> Program {
    clone_node(program)
}

/// Tear down a temporary program without recursive drops along expression spines.
pub fn drop_program(mut program: Program) {
    let mut detach = Detach { arena: Vec::new() };
    program.visit_mut_with(&mut detach);
}

/// Copy a function without recursively cloning its binary/parenthesized spines.
/// The temporary arena preserves every source span and resolved binding identity.
pub fn clone_function(function: &mut Function) -> Function {
    clone_node(function)
}

/// Copy an expression while retaining its spans and resolved identifier contexts.
pub fn clone_expr(expression: &mut Expr) -> Expr {
    clone_node(expression)
}

/// Copy a statement block without recursively cloning binary expression spines.
pub fn clone_function_body(block: &mut FunctionBody) -> FunctionBody {
    clone_node(block)
}

fn clone_node<N>(node: &mut N) -> N
where
    N: Clone + VisitMutWith<Detach> + VisitMutWith<Restore>,
{
    let mut detach = Detach { arena: Vec::new() };
    node.visit_mut_with(&mut detach);
    let mut copy = node.clone();
    let copy_arena = detach.arena.clone();
    restore(node, detach.arena);
    restore(&mut copy, copy_arena);
    copy
}

/// Run a scope/identity visitor with binary and parenthesized spines temporarily
/// represented as flat sequences. The callback must preserve sequence order and
/// arity; the original operators and spans are restored even during unwinding.
/// Leaf expressions remain inside their original lexical scopes throughout.
pub fn with_flattened_spines<R>(program: &mut Program, visit: impl FnOnce(&mut Program) -> R) -> R {
    let mut flatten = FlattenSpines { shapes: Vec::new() };
    program.visit_mut_with(&mut flatten);
    let guard = SpineGuard {
        program,
        shapes: flatten.shapes,
    };
    visit(&mut *guard.program)
}

enum ShapePart {
    Leaf,
    Binary(Span, BinaryOp),
    Paren(Span),
}
struct FlattenSpines {
    shapes: Vec<Option<Vec<ShapePart>>>,
}
impl VisitMut for FlattenSpines {
    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        if !matches!(expression, Expr::Bin(_) | Expr::Paren(_)) {
            expression.visit_mut_children_with(self);
            return;
        }
        enum Task {
            Expr(Expr),
            Binary(Span, BinaryOp),
            Paren(Span),
        }
        let mut tasks = vec![Task::Expr(std::mem::replace(
            expression,
            Expr::Invalid(Invalid { span: DUMMY_SP }),
        ))];
        let mut parts = Vec::new();
        let mut leaves = Vec::new();
        while let Some(task) = tasks.pop() {
            match task {
                Task::Expr(Expr::Bin(binary)) => {
                    tasks.push(Task::Binary(binary.span, binary.op));
                    tasks.push(Task::Expr(*binary.right));
                    tasks.push(Task::Expr(*binary.left));
                }
                Task::Expr(Expr::Paren(paren)) => {
                    tasks.push(Task::Paren(paren.span));
                    tasks.push(Task::Expr(*paren.expr));
                }
                Task::Expr(mut leaf) => {
                    leaf.visit_mut_children_with(self);
                    parts.push(ShapePart::Leaf);
                    leaves.push(Box::new(leaf));
                }
                Task::Binary(span, op) => parts.push(ShapePart::Binary(span, op)),
                Task::Paren(span) => parts.push(ShapePart::Paren(span)),
            }
        }
        let index =
            u32::try_from(self.shapes.len()).expect("expression shapes exceed address space");
        self.shapes.push(Some(parts));
        *expression = Expr::Seq(SeqExpr {
            span: Span {
                lo: BytePos(index),
                hi: BytePos(u32::MAX),
            },
            exprs: leaves,
        });
    }
}

struct SpineGuard<'a> {
    program: &'a mut Program,
    shapes: Vec<Option<Vec<ShapePart>>>,
}
impl Drop for SpineGuard<'_> {
    fn drop(&mut self) {
        self.program.visit_mut_with(&mut RestoreSpines {
            shapes: &mut self.shapes,
        });
    }
}
struct RestoreSpines<'a> {
    shapes: &'a mut [Option<Vec<ShapePart>>],
}
impl VisitMut for RestoreSpines<'_> {
    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        expression.visit_mut_children_with(self);
        let Expr::Seq(sequence) = expression else {
            return;
        };
        if sequence.span.hi != BytePos(u32::MAX) {
            return;
        }
        let parts = self.shapes[sequence.span.lo.0 as usize]
            .take()
            .expect("each expression shape is restored once");
        let mut leaves = std::mem::take(&mut sequence.exprs).into_iter();
        let mut values = Vec::new();
        for part in parts {
            let value = match part {
                ShapePart::Leaf => *leaves
                    .next()
                    .expect("scope visitor preserves expression leaves"),
                ShapePart::Binary(span, op) => {
                    let right = Box::new(values.pop().unwrap());
                    let left = Box::new(values.pop().unwrap());
                    Expr::Bin(BinExpr {
                        span,
                        op,
                        left,
                        right,
                    })
                }
                ShapePart::Paren(span) => {
                    let expr = Box::new(values.pop().unwrap());
                    Expr::Paren(ParenExpr { span, expr })
                }
            };
            values.push(value);
        }
        assert!(
            leaves.next().is_none(),
            "scope visitor preserves expression leaves"
        );
        *expression = values.pop().unwrap();
        assert!(values.is_empty());
    }
}

struct Detach {
    arena: Vec<Expr>,
}
impl Detach {
    fn save(&mut self, expression: Expr) -> Expr {
        let index =
            u32::try_from(self.arena.len()).expect("expression arena exceeds address space");
        self.arena.push(expression);
        Expr::Invalid(Invalid {
            span: Span {
                lo: BytePos(index),
                hi: BytePos(u32::MAX),
            },
        })
    }
}
impl VisitMut for Detach {
    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        if !matches!(expression, Expr::Bin(_) | Expr::Paren(_)) {
            expression.visit_mut_children_with(self);
            return;
        }
        enum Task {
            Expr(Expr),
            Binary(Span, BinaryOp),
            Paren(Span),
        }
        let mut tasks = vec![Task::Expr(std::mem::replace(
            expression,
            Expr::Invalid(Invalid { span: DUMMY_SP }),
        ))];
        let mut values = Vec::new();
        while let Some(task) = tasks.pop() {
            match task {
                Task::Expr(Expr::Bin(binary)) => {
                    tasks.push(Task::Binary(binary.span, binary.op));
                    tasks.push(Task::Expr(*binary.right));
                    tasks.push(Task::Expr(*binary.left));
                }
                Task::Expr(Expr::Paren(paren)) => {
                    tasks.push(Task::Paren(paren.span));
                    tasks.push(Task::Expr(*paren.expr));
                }
                Task::Expr(mut leaf) => {
                    leaf.visit_mut_children_with(self);
                    values.push(self.save(leaf));
                }
                Task::Binary(span, op) => {
                    let right = Box::new(values.pop().unwrap());
                    let left = Box::new(values.pop().unwrap());
                    values.push(self.save(Expr::Bin(BinExpr {
                        span,
                        op,
                        left,
                        right,
                    })));
                }
                Task::Paren(span) => {
                    let expr = Box::new(values.pop().unwrap());
                    values.push(self.save(Expr::Paren(ParenExpr { span, expr })));
                }
            }
        }
        *expression = values.pop().unwrap();
    }
}

struct Restore {
    arena: Vec<Option<Expr>>,
}
impl VisitMut for Restore {
    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        if let Expr::Invalid(Invalid { span }) = expression
            && span.hi == BytePos(u32::MAX)
        {
            *expression = self.arena[span.lo.0 as usize].take().unwrap();
            return;
        }
        expression.visit_mut_children_with(self);
    }
}
fn restore<N: VisitMutWith<Restore>>(node: &mut N, arena: Vec<Expr>) {
    let mut restore = Restore {
        arena: Vec::with_capacity(arena.len()),
    };
    for mut expression in arena {
        expression.visit_mut_with(&mut restore);
        restore.arena.push(Some(expression));
    }
    node.visit_mut_with(&mut restore);
}

/// Visit a binary spine in source evaluation order without recursive frames.
pub fn walk_binary_mut<V: VisitMut>(binary: &mut BinExpr, visitor: &mut V) {
    binary.span.visit_mut_with(visitor);
    binary.op.visit_mut_with(visitor);
    let mut pending = vec![&mut *binary.right, &mut *binary.left];
    while let Some(expression) = pending.pop() {
        match expression {
            Expr::Bin(binary) => {
                binary.span.visit_mut_with(visitor);
                binary.op.visit_mut_with(visitor);
                pending.push(&mut binary.right);
                pending.push(&mut binary.left);
            }
            Expr::Paren(paren) => {
                paren.span.visit_mut_with(visitor);
                pending.push(&mut paren.expr);
            }
            expression => expression.visit_mut_with(visitor),
        }
    }
}

/// Rewrite every expression in a binary/parenthesized spine in postorder,
/// without recursive descent along that spine. Leaf children use the visitor's
/// ordinary hooks; `rewrite` then runs once for each original expression,
/// including reconstructed binary and parenthesized nodes. Replacements are
/// not revisited, matching an ordinary postorder expression visitor.
pub fn rewrite_expression_spine<V: VisitMut>(
    expression: &mut Expr,
    visitor: &mut V,
    rewrite: impl FnMut(&mut V, &mut Expr),
) {
    rewrite_spine(expression, visitor, LeafVisit::Children, rewrite);
}

/// Rewrite binary and parenthesized nodes after their children, delegating
/// complete leaf expressions to the visitor. Call this from a spine branch;
/// non-spine roots are sent back to the ordinary expression hook. Only original
/// spine nodes receive `rewrite`; leaf replacements are not revisited.
pub fn rewrite_binary_spine<V: VisitMut>(
    expression: &mut Expr,
    visitor: &mut V,
    rewrite: impl FnMut(&mut V, &mut Expr),
) {
    rewrite_spine(expression, visitor, LeafVisit::Expression, rewrite);
}

#[derive(Clone, Copy)]
enum LeafVisit {
    Children,
    Expression,
}

fn rewrite_spine<V: VisitMut>(
    expression: &mut Expr,
    visitor: &mut V,
    leaf_visit: LeafVisit,
    mut rewrite: impl FnMut(&mut V, &mut Expr),
) {
    if !matches!(expression, Expr::Bin(_) | Expr::Paren(_)) {
        match leaf_visit {
            LeafVisit::Children => {
                expression.visit_mut_children_with(visitor);
                rewrite(visitor, expression);
            }
            LeafVisit::Expression => expression.visit_mut_with(visitor),
        }
        return;
    }
    enum Task {
        Expr(Expr),
        Binary(Span, BinaryOp),
        Paren(Span),
    }
    let mut tasks = vec![Task::Expr(std::mem::replace(
        expression,
        Expr::Invalid(Invalid { span: DUMMY_SP }),
    ))];
    let mut values = Vec::new();
    while let Some(task) = tasks.pop() {
        let mut value = match task {
            Task::Expr(Expr::Bin(mut binary)) => {
                binary.span.visit_mut_with(visitor);
                binary.op.visit_mut_with(visitor);
                tasks.push(Task::Binary(binary.span, binary.op));
                tasks.push(Task::Expr(*binary.right));
                tasks.push(Task::Expr(*binary.left));
                continue;
            }
            Task::Expr(Expr::Paren(mut paren)) => {
                paren.span.visit_mut_with(visitor);
                tasks.push(Task::Paren(paren.span));
                tasks.push(Task::Expr(*paren.expr));
                continue;
            }
            Task::Expr(mut leaf) => match leaf_visit {
                LeafVisit::Children => {
                    leaf.visit_mut_children_with(visitor);
                    leaf
                }
                LeafVisit::Expression => {
                    leaf.visit_mut_with(visitor);
                    values.push(leaf);
                    continue;
                }
            },
            Task::Binary(span, op) => {
                let right = Box::new(values.pop().unwrap());
                let left = Box::new(values.pop().unwrap());
                Expr::Bin(BinExpr {
                    span,
                    op,
                    left,
                    right,
                })
            }
            Task::Paren(span) => {
                let expr = Box::new(values.pop().unwrap());
                Expr::Paren(ParenExpr { span, expr })
            }
        };
        rewrite(visitor, &mut value);
        values.push(value);
    }
    *expression = values.pop().unwrap();
    debug_assert!(values.is_empty());
}

/// Walk a binary spine for scanners that only inspect its leaves.
pub fn walk_binary<V: Visit>(binary: &BinExpr, visitor: &mut V) {
    binary.span.visit_with(visitor);
    binary.op.visit_with(visitor);
    let mut pending = vec![&*binary.right, &*binary.left];
    while let Some(expression) = pending.pop() {
        match expression {
            Expr::Bin(binary) => {
                binary.span.visit_with(visitor);
                binary.op.visit_with(visitor);
                pending.push(&binary.right);
                pending.push(&binary.left);
            }
            Expr::Paren(paren) => {
                paren.span.visit_with(visitor);
                pending.push(&paren.expr);
            }
            expression => expression.visit_with(visitor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Js, ParseOpts};
    use mangler_core::Language;

    #[test]
    fn flattened_resolution_retains_lexical_scopes_and_restores_shapes() {
        Js::with_globals(|| {
            let mut ast = Js
                .parse(
                    "function outer(x){return (x+(function inner(x){return x+1})())+(function capture(){return x+3})()}",
                    &ParseOpts::default(),
                )
                .unwrap();
            let before = ast.program().clone();
            with_flattened_spines(ast.program_mut(), |_| {});
            assert_eq!(ast.program(), &before);
            let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_flattened_spines(ast.program_mut(), |_| panic!("test unwinding"));
            }));
            assert!(unwind.is_err());
            assert_eq!(ast.program(), &before);
            with_flattened_spines(ast.program_mut(), |program| {
                program.visit_mut_with(&mut swc_core::ecma::transforms::base::resolver(
                    swc_core::common::Mark::new(),
                    swc_core::common::Mark::new(),
                    false,
                ));
                Js::repair_resolver_scopes(program);
            });
            #[derive(Default)]
            struct Names(Vec<Id>);
            impl Visit for Names {
                fn visit_ident(&mut self, ident: &Ident) {
                    if ident.sym == "x" {
                        self.0.push(ident.to_id());
                    }
                }
            }
            let mut names = Names::default();
            ast.program().visit_with(&mut names);
            assert_eq!(names.0.len(), 5);
            assert_eq!(names.0[0], names.0[1]);
            assert_eq!(names.0[2], names.0[3]);
            assert_ne!(names.0[0], names.0[2]);
            assert_eq!(names.0[0], names.0[4]);
        });
    }

    #[test]
    fn clone_preserves_spans_bindings_and_nested_expression_shapes() {
        Js::with_globals(|| {
            let mut ast = Js
                .parse(
                    "function f(a){return a+(b=>b*(a+1))(2)+(a?3:4)}",
                    &ParseOpts::default(),
                )
                .unwrap();
            Js::resolve(&mut ast);
            let Program::Script(script) = ast.program_mut() else {
                panic!()
            };
            let Stmt::Decl(Decl::Fn(declaration)) = &mut script.body[0] else {
                panic!()
            };
            let before = declaration.function.clone();
            let after = clone_function(&mut declaration.function);
            assert_eq!(*before, after);
            assert_eq!(*declaration.function, after);
        });
    }

    #[test]
    fn clones_twenty_thousand_operand_spines_on_an_ordinary_thread_stack() {
        std::thread::spawn(|| {
            let mut expression = Expr::Ident(Ident::new_no_ctxt("x".into(), DUMMY_SP));
            for _ in 1..20_000 {
                expression = Expr::Bin(BinExpr {
                    span: DUMMY_SP,
                    op: BinaryOp::Add,
                    left: Box::new(expression),
                    right: Box::new(Expr::Ident(Ident::new_no_ctxt("x".into(), DUMMY_SP))),
                });
            }
            let mut copy = clone_expr(&mut expression);
            for tree in [&mut expression, &mut copy] {
                // Detaching also permits teardown without the derived recursive Drop.
                let mut detached = Detach { arena: Vec::new() };
                tree.visit_mut_with(&mut detached);
                assert_eq!(detached.arena.len(), 39_999);
                assert_eq!(
                    detached
                        .arena
                        .iter()
                        .filter(|e| matches!(e, Expr::Ident(_)))
                        .count(),
                    20_000
                );
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn program_copy_preserves_source_identity_and_supports_iterative_teardown() {
        Js::with_globals(|| {
            let mut ast = Js
                .parse(
                    "export const x=1;export function f(a){return a+x}",
                    &ParseOpts {
                        module: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            Js::resolve(&mut ast);
            let before = ast.program().clone();
            let copy = clone_program(ast.program_mut());
            assert_eq!(&copy, &before);
            assert_eq!(ast.program(), &before);
            drop_program(copy);
        });
        std::thread::spawn(|| {
            Js::with_globals(|| {
                let expression = (1..20_000).fold(
                    Expr::Ident(Ident::new_no_ctxt("x".into(), DUMMY_SP)),
                    |left, _| {
                        Expr::Bin(BinExpr {
                            span: DUMMY_SP,
                            op: BinaryOp::Add,
                            left: Box::new(left),
                            right: Box::new(Expr::Ident(Ident::new_no_ctxt("x".into(), DUMMY_SP))),
                        })
                    },
                );
                let mut program = Program::Script(Script {
                    body: vec![Stmt::Expr(ExprStmt {
                        span: DUMMY_SP,
                        expr: Box::new(expression),
                    })],
                    ..Default::default()
                });
                let mut copy = clone_program(&mut program);
                with_flattened_spines(&mut copy, |program| {
                    program.visit_mut_with(&mut swc_core::ecma::transforms::base::resolver(
                        swc_core::common::Mark::new(),
                        swc_core::common::Mark::new(),
                        false,
                    ));
                    Js::repair_resolver_scopes(program);
                });
                drop_program(copy);
                drop_program(program);
            })
        })
        .join()
        .unwrap();
    }

    #[test]
    fn postorder_spine_rewrite_visits_leaves_once_and_exposes_child_rewrites() {
        #[derive(Default)]
        struct Reduce {
            leaves: Vec<String>,
            binary: usize,
        }
        impl VisitMut for Reduce {
            fn visit_mut_ident(&mut self, ident: &mut Ident) {
                self.leaves.push(ident.sym.to_string());
            }
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                rewrite_expression_spine(expression, self, |reduce, node| {
                    let value = match node {
                        Expr::Ident(_) => 1.0,
                        Expr::Bin(binary) => {
                            let Expr::Lit(Lit::Num(left)) =
                                crate::assignment_target::unparen(&binary.left)
                            else {
                                panic!("left child not rewritten")
                            };
                            let Expr::Lit(Lit::Num(right)) =
                                crate::assignment_target::unparen(&binary.right)
                            else {
                                panic!("right child not rewritten")
                            };
                            reduce.binary += 1;
                            left.value + right.value
                        }
                        _ => return,
                    };
                    *node = Expr::Lit(Lit::Num(Number {
                        span: DUMMY_SP,
                        value,
                        raw: None,
                    }));
                });
            }
        }
        let mut ast = Js.parse("a+(b*c)+(d-e)", &ParseOpts::default()).unwrap();
        let mut reduce = Reduce::default();
        ast.program_mut().visit_mut_with(&mut reduce);
        assert_eq!(reduce.leaves, ["a", "b", "c", "d", "e"]);
        assert_eq!(reduce.binary, 4);
        assert_eq!(Js.print(&ast), "5;");
    }

    #[test]
    fn binary_spine_rewrite_preserves_complete_leaf_hooks_and_scope_boundaries() {
        #[derive(Default)]
        struct Rewrite {
            functions: usize,
            identifiers: Vec<String>,
            binaries: usize,
            parens: usize,
        }
        impl VisitMut for Rewrite {
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                if matches!(expression, Expr::Bin(_) | Expr::Paren(_)) {
                    rewrite_binary_spine(expression, self, |visitor, node| match node {
                        Expr::Bin(binary) => {
                            visitor.binaries += 1;
                            binary.op = BinaryOp::Sub;
                        }
                        Expr::Paren(_) => visitor.parens += 1,
                        _ => panic!("only original spine nodes receive the callback"),
                    });
                    return;
                }
                match expression {
                    Expr::Fn(_) => {
                        // A complete leaf hook may stop traversal at an activation.
                        self.functions += 1;
                        return;
                    }
                    Expr::Ident(ident) => {
                        self.identifiers.push(ident.sym.to_string());
                        ident.sym = "visited".into();
                    }
                    _ => {}
                }
                expression.visit_mut_children_with(self);
            }
        }
        let mut ast = Js
            .parse(
                "a+(b*c)+(function(){return hidden+inside})(d+e)",
                &ParseOpts::default(),
            )
            .unwrap();
        let mut visitor = Rewrite::default();
        ast.program_mut().visit_mut_with(&mut visitor);
        assert_eq!(visitor.identifiers, ["a", "b", "c", "d", "e"]);
        assert_eq!(visitor.functions, 1);
        assert_eq!(visitor.binaries, 4);
        assert_eq!(visitor.parens, 2);
        assert_eq!(
            Js.print(&ast),
            "visited-(visited-visited)-(function(){return hidden+inside;})(visited-visited);"
        );
    }
}
