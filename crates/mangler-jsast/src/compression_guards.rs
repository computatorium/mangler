//! Preserve host JavaScript operator semantics across SWC compression.
//!
//! SWC unconditionally treats strict null/undefined pairs as loose null checks,
//! and treats `typeof value === 'undefined'` as a strict undefined comparison.
//! Those identities do not hold for HTML's IsHTMLDDA objects. Temporarily opaque
//! operator calls keep operand references visible to liveness and scope analysis;
//! restoration removes every marker before hygiene/code generation.
use swc_core::common::{DUMMY_SP, Mark, SyntaxContext};
use swc_core::ecma::{
    ast::*,
    visit::{VisitMut, VisitMutWith},
};

pub(crate) struct Operators {
    eq_null: Id,
    ne_null: Id,
    typeof_value: Id,
    empty_array: Id,
    empty_object: Id,
    named_value: Id,
    lexical_initializer: Id,
}
impl Operators {
    pub fn new(unresolved: Mark) -> Self {
        let context = SyntaxContext::empty().apply_mark(unresolved);
        Self {
            eq_null: ("\0mangler_strict_null".into(), context),
            ne_null: ("\0mangler_strict_not_null".into(), context),
            typeof_value: ("\0mangler_typeof".into(), context),
            empty_array: ("\0mangler_array_elisions".into(), context),
            empty_object: ("\0mangler_empty_object".into(), context),
            named_value: ("\0mangler_named_value".into(), context),
            lexical_initializer: ("\0mangler_lexical_initializer".into(), context),
        }
    }

    pub fn protect(&self, program: &mut Program) -> bool {
        let mut constants = ConstBindings::default();
        program.visit_mut_with(&mut constants);
        let mut protect = Protect(self, &constants.0, false);
        program.visit_mut_with(&mut protect);
        protect.2
    }

    pub fn restore(&self, program: &mut Program) {
        program.visit_mut_with(&mut Restore(self));
    }
}

fn empty_name() -> Str {
    Str {
        span: DUMMY_SP,
        value: "".into(),
        raw: None,
    }
}

fn canonical_reference(expression: &mut Box<Expr>) -> Option<SimpleAssignTarget> {
    use crate::assignment_target::{Reference, expression_reference};
    if !matches!(
        expression_reference(expression),
        Reference::Ident(_) | Reference::Member(_) | Reference::Super(_)
    ) {
        return None;
    }
    let mut value = std::mem::replace(
        expression,
        Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
    );
    while let Expr::Paren(paren) = *value {
        value = paren.expr;
    }
    Some(match *value {
        Expr::Ident(id) => SimpleAssignTarget::Ident(id.into()),
        Expr::Member(member) => SimpleAssignTarget::Member(member),
        Expr::SuperProp(property) => SimpleAssignTarget::SuperProp(property),
        _ => unreachable!("validated assignment reference"),
    })
}

fn reference_expression(target: SimpleAssignTarget) -> Expr {
    match target {
        SimpleAssignTarget::Ident(binding) => Expr::Ident(binding.id),
        SimpleAssignTarget::Member(member) => Expr::Member(member),
        SimpleAssignTarget::SuperProp(property) => Expr::SuperProp(property),
        _ => unreachable!("canonical assignment reference"),
    }
}

fn named_guard(operators: &Operators, key: Str, value: Box<Expr>) -> Expr {
    Expr::Call(CallExpr {
        span: DUMMY_SP,
        callee: Callee::Expr(Box::new(Expr::Ident(Ident::new(
            operators.named_value.0.clone(),
            DUMMY_SP,
            operators.named_value.1,
        )))),
        args: vec![
            ExprOrSpread {
                spread: None,
                expr: Box::new(Expr::Lit(Lit::Str(key))),
            },
            ExprOrSpread {
                spread: None,
                expr: value,
            },
        ],
        ..Default::default()
    })
}

/// The shared NamedEvaluation helper is also an ordinary source idiom. Match
/// its exact one-property shape, preserving UTF-16 keys without converting them
/// through Rust strings. Member bases with getters/spreads cannot match.
fn named_value_parts(expression: &mut Expr) -> Option<(Str, Box<Expr>)> {
    let Expr::Member(member) = expression else {
        return None;
    };
    let MemberProp::Computed(key) = &member.prop else {
        return None;
    };
    let Expr::Lit(Lit::Str(key)) = key.expr.as_ref() else {
        return None;
    };
    let mut object = member.obj.as_mut();
    while let Expr::Paren(paren) = object {
        object = paren.expr.as_mut();
    }
    let Expr::Object(object) = object else {
        return None;
    };
    let [PropOrSpread::Prop(property)] = object.props.as_mut_slice() else {
        return None;
    };
    let Prop::KeyValue(property) = property.as_mut() else {
        return None;
    };
    let PropName::Computed(name) = &property.key else {
        return None;
    };
    let Expr::Lit(Lit::Str(name)) = name.expr.as_ref() else {
        return None;
    };
    (name.value == key.value).then(|| {
        let inferred = if crate::assignment_target::is_anonymous_definition(&property.value) {
            name.clone()
        } else {
            // Folding a conditional/call result into an anonymous definition
            // must not invent NamedEvaluation absent from the original syntax.
            empty_name()
        };
        let value = std::mem::replace(
            &mut property.value,
            Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
        );
        (inferred, value)
    })
}

#[derive(Default)]
struct ConstBindings(std::collections::HashSet<Id>);
impl VisitMut for ConstBindings {
    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        crate::deep::rewrite_expression_spine(expression, self, |_, _| {});
    }

    fn visit_mut_var_decl(&mut self, declaration: &mut VarDecl) {
        if declaration.kind == VarDeclKind::Const {
            for variable in &declaration.decls {
                crate::analysis::binding_names(&variable.name, &mut |id| {
                    self.0.insert(id.to_id());
                });
            }
        }
        declaration.visit_mut_children_with(self);
    }
    fn visit_mut_using_decl(&mut self, declaration: &mut UsingDecl) {
        for variable in &declaration.decls {
            crate::analysis::binding_names(&variable.name, &mut |id| {
                self.0.insert(id.to_id());
            });
        }
        declaration.visit_mut_children_with(self);
    }
}

fn constant_reference(
    reference: crate::assignment_target::Reference<'_>,
    constants: &std::collections::HashSet<Id>,
) -> bool {
    use crate::assignment_target::Reference;
    match reference {
        Reference::Ident(id) => constants.contains(&id.to_id()),
        Reference::Array(array) => array
            .elems
            .iter()
            .flatten()
            .any(|pattern| constant_pattern(pattern, constants)),
        Reference::Object(object) => object.props.iter().any(|property| match property {
            ObjectPatProp::KeyValue(property) => constant_pattern(&property.value, constants),
            ObjectPatProp::Assign(property) => constants.contains(&property.key.id.to_id()),
            ObjectPatProp::Rest(property) => constant_pattern(&property.arg, constants),
        }),
        _ => false,
    }
}
fn constant_pattern(pattern: &Pat, constants: &std::collections::HashSet<Id>) -> bool {
    use crate::assignment_target::{Reference, expression_reference};
    match pattern {
        Pat::Ident(binding) => constants.contains(&binding.id.to_id()),
        Pat::Array(array) => constant_reference(Reference::Array(array), constants),
        Pat::Object(object) => constant_reference(Reference::Object(object), constants),
        Pat::Assign(default) => constant_pattern(&default.left, constants),
        Pat::Rest(rest) => constant_pattern(&rest.arg, constants),
        Pat::Expr(expression) => constant_reference(expression_reference(expression), constants),
        Pat::Invalid(_) => false,
    }
}

struct Protect<'a>(&'a Operators, &'a std::collections::HashSet<Id>, bool);
impl VisitMut for Protect<'_> {
    fn visit_mut_private_prop(&mut self, property: &mut PrivateProp) {
        property.visit_mut_children_with(self);
        if let Some(value) = property.value.take() {
            let name = if crate::assignment_target::is_anonymous_definition(&value) {
                Str {
                    span: DUMMY_SP,
                    value: format!("#{}", property.key.name).into(),
                    raw: None,
                }
            } else {
                empty_name()
            };
            // Preserve the source NamedEvaluation decision before private-key
            // mangling or folding a conditional into an anonymous definition.
            property.value = Some(Box::new(named_guard(self.0, name, value)));
        }
    }

    fn visit_mut_for_head(&mut self, head: &mut ForHead) {
        if let ForHead::Pat(pattern) = head {
            self.2 |= constant_pattern(pattern, self.1);
        }
        // Iteration binding heads have no initializer position; their TDZ ends
        // in the native iteration protocol, rather than a declaration statement.
        match head {
            ForHead::VarDecl(declaration) => declaration.visit_mut_children_with(self),
            _ => head.visit_mut_children_with(self),
        }
    }

    fn visit_mut_var_decl(&mut self, declaration: &mut VarDecl) {
        declaration.visit_mut_children_with(self);
        if declaration.kind != VarDeclKind::Let {
            return;
        }
        for variable in &mut declaration.decls {
            // SWC's collapse_vars_without_init moves bare lets before earlier
            // statements, ending their TDZ too soon. An initialized let may
            // become bare during compression, so preserve every let's explicit
            // initialization boundary until the final restoration pass.
            let value = variable.init.take().unwrap_or_else(|| {
                Box::new(Expr::Unary(UnaryExpr {
                    span: DUMMY_SP,
                    op: UnaryOp::Void,
                    arg: Box::new(Expr::Lit(Lit::Num(Number {
                        span: DUMMY_SP,
                        value: 0.0,
                        raw: None,
                    }))),
                }))
            });
            let value = if let Pat::Ident(binding) = &variable.name {
                let name = if crate::assignment_target::is_anonymous_definition(&value) {
                    Str {
                        span: DUMMY_SP,
                        value: binding.id.sym.clone().into(),
                        raw: None,
                    }
                } else {
                    empty_name()
                };
                // Preserve the original NamedEvaluation decision even if
                // compression folds a conditional into an anonymous definition.
                Box::new(named_guard(self.0, name, value))
            } else {
                value
            };
            variable.init = Some(Box::new(Expr::Call(CallExpr {
                span: variable.span,
                callee: Callee::Expr(Box::new(Expr::Ident(Ident::new(
                    self.0.lexical_initializer.0.clone(),
                    DUMMY_SP,
                    self.0.lexical_initializer.1,
                )))),
                args: vec![ExprOrSpread {
                    spread: None,
                    expr: value,
                }],
                ..Default::default()
            })));
        }
    }

    fn visit_mut_update_expr(&mut self, update: &mut UpdateExpr) {
        self.2 |= constant_reference(
            crate::assignment_target::expression_reference(&update.arg),
            self.1,
        );
        update.visit_mut_children_with(self);
        if let Expr::Paren(paren) = update.arg.as_mut()
            && let Some(target) = canonical_reference(&mut paren.expr)
        {
            update.arg = Box::new(reference_expression(target));
        }
    }

    fn visit_mut_pat(&mut self, pattern: &mut Pat) {
        // Parenthesized destructuring/default targets are assignment references,
        // never declaration bindings. Removing their grouping must not create
        // NamedEvaluation for an anonymous default initializer.
        if let Pat::Assign(default) = pattern
            && matches!(default.left.as_ref(), Pat::Expr(e) if matches!(e.as_ref(), Expr::Paren(_)))
        {
            let right = std::mem::replace(
                &mut default.right,
                Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
            );
            default.right = Box::new(named_guard(self.0, empty_name(), right));
        }
        pattern.visit_mut_children_with(self);
        if let Pat::Expr(expression) = pattern
            && let Expr::Paren(paren) = expression.as_mut()
            && let Some(target) = canonical_reference(&mut paren.expr)
        {
            *pattern = match target {
                SimpleAssignTarget::Ident(binding) => Pat::Ident(binding),
                target => Pat::Expr(Box::new(reference_expression(target))),
            };
        }
    }

    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        crate::deep::rewrite_expression_spine(expression, self, Self::rewrite_expression);
    }
}

impl Protect<'_> {
    fn rewrite_expression(&mut self, expression: &mut Expr) {
        // Keep syntax-dependent naming opaque while compression may inline
        // property values or turn conditional values into anonymous definitions.
        if let Some((key, value)) = named_value_parts(expression) {
            *expression = named_guard(self.0, key, value);
            return;
        }
        // Even binding-free patterns open/close iterators or require an object.
        // SWC's unconditional empty-assignment simplifier drops those effects.
        if let Expr::Assign(assignment) = expression {
            let immutable = constant_reference(
                crate::assignment_target::reference(&assignment.left),
                self.1,
            );
            self.2 |= immutable;
            let inferred = if matches!(
                assignment.op,
                AssignOp::Assign
                    | AssignOp::AndAssign
                    | AssignOp::OrAssign
                    | AssignOp::NullishAssign
            ) && crate::assignment_target::is_anonymous_definition(
                &assignment.right,
            ) {
                crate::assignment_target::inferred_name(&assignment.left).map(|name| Str {
                    span: DUMMY_SP,
                    value: name.into(),
                    raw: None,
                })
            } else {
                None
            };
            let grouped = if let AssignTarget::Simple(SimpleAssignTarget::Paren(paren)) =
                &mut assignment.left
                && let Some(target) = canonical_reference(&mut paren.expr)
            {
                assignment.left = AssignTarget::Simple(target);
                true
            } else {
                false
            };
            if grouped || immutable {
                let right = std::mem::replace(
                    &mut assignment.right,
                    Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
                );
                assignment.right = Box::new(named_guard(
                    self.0,
                    inferred.unwrap_or_else(empty_name),
                    right,
                ));
            }
            if immutable {
                // SWC's ignored-value inliner and return-termination pass can
                // discard a const write independently of `unused`. Keep the
                // assignment in value position until all compression finishes.
                let value =
                    std::mem::replace(expression, Expr::Invalid(Invalid { span: DUMMY_SP }));
                *expression = named_guard(self.0, empty_name(), Box::new(value));
                return;
            }
            let (marker, holes) = match &assignment.left {
                AssignTarget::Pat(AssignTargetPat::Array(array))
                    if array.elems.iter().all(Option::is_none) =>
                {
                    (&self.0.empty_array, array.elems.len())
                }
                AssignTarget::Pat(AssignTargetPat::Object(object)) if object.props.is_empty() => {
                    (&self.0.empty_object, 0)
                }
                _ => return,
            };
            let right = std::mem::replace(
                &mut assignment.right,
                Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
            );
            *expression = Expr::Call(CallExpr {
                span: assignment.span,
                callee: Callee::Expr(Box::new(Expr::Ident(Ident::new(
                    marker.0.clone(),
                    DUMMY_SP,
                    marker.1,
                )))),
                args: vec![
                    ExprOrSpread {
                        spread: None,
                        expr: right,
                    },
                    ExprOrSpread {
                        spread: None,
                        expr: Box::new(Expr::Lit(Lit::Num(Number {
                            span: DUMMY_SP,
                            value: holes as f64,
                            raw: None,
                        }))),
                    },
                ],
                ..Default::default()
            });
            return;
        }
        let (marker, operand) = match expression {
            Expr::Bin(binary) if matches!(binary.op, BinaryOp::EqEqEq | BinaryOp::NotEqEq) => {
                let (operand, discarded_null) = if matches!(
                    crate::assignment_target::unparen(&binary.left),
                    Expr::Lit(Lit::Null(_))
                ) {
                    (&mut binary.right, &mut binary.left)
                } else if matches!(
                    crate::assignment_target::unparen(&binary.right),
                    Expr::Lit(Lit::Null(_))
                ) {
                    (&mut binary.left, &mut binary.right)
                } else {
                    return;
                };
                // The null side is pure, but its discarded grouping can be
                // arbitrarily deep. Move through it before replacing the Bin;
                // the derived Box drop must never recurse down that spine.
                let mut discarded = std::mem::replace(
                    discarded_null,
                    Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
                );
                while let Expr::Paren(paren) = *discarded {
                    discarded = paren.expr;
                }
                (
                    if binary.op == BinaryOp::EqEqEq {
                        &self.0.eq_null
                    } else {
                        &self.0.ne_null
                    },
                    operand,
                )
            }
            Expr::Unary(unary) if unary.op == UnaryOp::TypeOf => {
                (&self.0.typeof_value, &mut unary.arg)
            }
            _ => return,
        };
        let operand =
            std::mem::replace(operand, Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })));
        *expression = Expr::Call(CallExpr {
            span: swc_core::common::Spanned::span(expression),
            callee: Callee::Expr(Box::new(Expr::Ident(Ident::new(
                marker.0.clone(),
                DUMMY_SP,
                marker.1,
            )))),
            args: vec![ExprOrSpread {
                spread: None,
                expr: operand,
            }],
            ..Default::default()
        });
    }
}

struct Restore<'a>(&'a Operators);
impl VisitMut for Restore<'_> {
    fn visit_mut_import_named_specifier(&mut self, specifier: &mut ImportNamedSpecifier) {
        let Some(ModuleExportName::Ident(remote)) = &specifier.imported else {
            return;
        };
        // SWC's import merger rebuilds string export names as identifiers.
        // Preserve the merged import while restoring names requiring quotes.
        let mut chars = remote.sym.chars();
        if !chars.next().is_some_and(Ident::is_valid_start) || !chars.all(Ident::is_valid_continue)
        {
            specifier.imported = Some(ModuleExportName::Str(Str {
                span: remote.span,
                value: remote.sym.clone().into(),
                raw: None,
            }));
        }
    }

    fn visit_mut_var_decl(&mut self, declaration: &mut VarDecl) {
        declaration.visit_mut_children_with(self);
        if declaration.kind == VarDeclKind::Let {
            for variable in &mut declaration.decls {
                if matches!(variable.name, Pat::Ident(_))
                    && matches!(variable.init.as_deref(), Some(Expr::Unary(UnaryExpr { op: UnaryOp::Void, arg, .. })) if matches!(arg.as_ref(), Expr::Lit(Lit::Num(_))))
                {
                    // Compression has finished: bare let now keeps this exact
                    // initialization position without emitting a redundant RHS.
                    variable.init = None;
                }
            }
        }
    }

    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        crate::deep::rewrite_expression_spine(expression, self, Self::rewrite_expression);
    }
}

impl Restore<'_> {
    fn rewrite_expression(&mut self, expression: &mut Expr) {
        let Expr::Call(call) = expression else { return };
        let Callee::Expr(callee) = &call.callee else {
            return;
        };
        let Expr::Ident(identifier) = &**callee else {
            return;
        };
        let id = identifier.to_id();
        if id == self.0.lexical_initializer {
            assert_eq!(call.args.len(), 1, "lexical initializer retains its value");
            *expression = *call.args.pop().unwrap().expr;
            return;
        }
        if id == self.0.named_value {
            assert_eq!(
                call.args.len(),
                2,
                "named value guard retains name and value"
            );
            let value = call.args.pop().unwrap().expr;
            let Expr::Lit(Lit::Str(key)) = *call.args.pop().unwrap().expr else {
                panic!("named value guard retains literal key");
            };
            *expression = if crate::assignment_target::is_anonymous_definition(&value) {
                *crate::assignment_target::named_value_key(key, value)
            } else {
                *value
            };
            return;
        }
        if id == self.0.empty_array || id == self.0.empty_object {
            assert_eq!(
                call.args.len(),
                2,
                "pattern guard retains RHS and elision count"
            );
            let count = call.args.pop().unwrap().expr;
            let Expr::Lit(Lit::Num(count)) = *count else {
                panic!("pattern guard retains its numeric elision count");
            };
            let left = if id == self.0.empty_array {
                AssignTargetPat::Array(ArrayPat {
                    span: call.span,
                    elems: vec![None; count.value as usize],
                    optional: false,
                    type_ann: None,
                })
            } else {
                AssignTargetPat::Object(ObjectPat {
                    span: call.span,
                    props: Vec::new(),
                    optional: false,
                    type_ann: None,
                })
            };
            *expression = Expr::Assign(AssignExpr {
                span: call.span,
                op: AssignOp::Assign,
                left: AssignTarget::Pat(left),
                right: call.args.pop().unwrap().expr,
            });
            return;
        }
        let typeof_value = id == self.0.typeof_value;
        let operator = if id == self.0.eq_null {
            BinaryOp::EqEqEq
        } else if id == self.0.ne_null {
            BinaryOp::NotEqEq
        } else if typeof_value {
            BinaryOp::EqEqEq
        } else {
            return;
        };
        assert_eq!(call.args.len(), 1, "compression guard retains its operand");
        let operand = call.args.pop().unwrap().expr;
        *expression = if typeof_value {
            Expr::Unary(UnaryExpr {
                span: call.span,
                op: UnaryOp::TypeOf,
                arg: operand,
            })
        } else {
            Expr::Bin(BinExpr {
                span: call.span,
                op: operator,
                left: operand,
                right: Box::new(Expr::Lit(Lit::Null(Null { span: DUMMY_SP }))),
            })
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Js, ParseOpts};
    use mangler_core::Language;
    use swc_core::ecma::visit::{Visit, VisitWith};

    #[test]
    fn immutable_write_guard_uses_resolved_binding_and_target_positions() {
        for (source, expected) in [
            ("const x=1;(x)=2", true),
            ("const x=1;x++", true),
            ("const x=1;[(x)]=[2]", true),
            ("const x=1;for((x) of [2]){}", true),
            ("const x=1;function write(){x=2}", true),
            ("const x=1;function shadow(x){(x)=2}", false),
            ("const x=1;{let x;(x)=2}", false),
            ("const x={};(x.value)=2", false),
            ("const x='key';let value;({[x]:value}={})", false),
            ("const x=1;let value;[value=x]=[]", false),
        ] {
            Js::with_globals(|| {
                let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
                let (unresolved, _) = Js::resolve(&mut ast);
                let guards = Operators::new(unresolved);
                assert_eq!(guards.protect(ast.program_mut()), expected, "{source}");
                guards.restore(ast.program_mut());
                Js::reparse(&Js.print(&ast), &ParseOpts::default()).unwrap();
            });
        }
    }

    #[test]
    fn naming_markers_restore_utf16_keys_and_leave_no_runtime_calls() {
        Js::with_globals(|| {
            let source = "let x;(x)=class{static observed=this.name};let y=({['\\ud800']:function(){}})['\\ud800'];";
            let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
            let (unresolved, _) = Js::resolve(&mut ast);
            let guards = Operators::new(unresolved);
            guards.protect(ast.program_mut());
            guards.restore(ast.program_mut());
            let output = Js.print(&ast);
            assert!(!output.contains("mangler_named_value"), "{output}");
            assert!(
                output.contains("\\uD800") || output.contains("\\ud800"),
                "{output}"
            );
            Js::reparse(&output, &ParseOpts::default()).unwrap();
        });
    }

    #[test]
    fn compression_preserves_strict_null_and_typeof_operators() {
        for mangle in [false, true] {
            for source in [
                "globalThis.check=function(x){return [x===null||x===void 0,typeof x==='undefined']}",
                "globalThis.check=function(x){return [null!==x&&void 0!==x,'undefined'!==typeof x]}",
                "globalThis.check=function(x,a){return [a&&(x===null||x===void 0),typeof x==='undefined']}",
                "globalThis.check=function(x,a){return [(x===void 0||null===x)&&a,'undefined'===typeof x]}",
                "globalThis.check=function(x,a){return [a||(x!==null&&void 0!==x),typeof x!=='undefined']}",
                "globalThis.check=function(x,a){return [((null)!==x&&x!==void 0)||a,'undefined'!==typeof(x)]}",
                "globalThis.check=function(x,a,b){return [a&&(b||(x===(null)||x===void 0)),typeof(x)==='undefined']}",
                "globalThis.check=function(x,a,b){return [(void 0===x||((null)===x))&&(a||b),'undefined'!==typeof x]}",
            ] {
                Js::with_globals(|| {
                    let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
                    let marks = Js::resolve(&mut ast);
                    let output = Js::print_optimized(ast, marks, mangle, &[]);
                    assert!(!output.contains("mangler_strict"), "{output}");
                    assert!(!output.contains("mangler_typeof"), "{output}");
                    struct Count {
                        strict_null: usize,
                        loose_null: usize,
                        typeof_value: usize,
                    }
                    impl Visit for Count {
                        fn visit_bin_expr(&mut self, binary: &BinExpr) {
                            if matches!(&*binary.left, Expr::Lit(Lit::Null(_)))
                                || matches!(&*binary.right, Expr::Lit(Lit::Null(_)))
                            {
                                self.strict_null += usize::from(matches!(
                                    binary.op,
                                    BinaryOp::EqEqEq | BinaryOp::NotEqEq
                                ));
                                self.loose_null += usize::from(matches!(
                                    binary.op,
                                    BinaryOp::EqEq | BinaryOp::NotEq
                                ));
                            }
                            binary.visit_children_with(self);
                        }
                        fn visit_unary_expr(&mut self, unary: &UnaryExpr) {
                            self.typeof_value += usize::from(unary.op == UnaryOp::TypeOf);
                            unary.visit_children_with(self);
                        }
                    }
                    let transformed = Js.parse(&output, &ParseOpts::default()).unwrap();
                    let mut count = Count {
                        strict_null: 0,
                        loose_null: 0,
                        typeof_value: 0,
                    };
                    transformed.program().visit_with(&mut count);
                    assert_eq!(count.strict_null, 1, "{output}");
                    assert_eq!(count.loose_null, 0, "{output}");
                    assert_eq!(count.typeof_value, 1, "{output}");
                });
            }
        }
    }

    #[test]
    fn operator_guards_cover_deep_binary_and_parenthesized_spines() {
        std::thread::spawn(|| {
            Js::with_globals(|| {
                const DEPTH: usize = 12_000;
                let mut expression = Expr::Ident(Ident::new_no_ctxt("x".into(), DUMMY_SP));
                for index in 0..DEPTH {
                    let value = Box::new(Expr::Ident(Ident::new_no_ctxt("x".into(), DUMMY_SP)));
                    let null = Box::new(Expr::Paren(ParenExpr {
                        span: DUMMY_SP,
                        expr: Box::new(Expr::Lit(Lit::Null(Null { span: DUMMY_SP }))),
                    }));
                    let (left, right) = if index % 2 == 0 { (value, null) } else { (null, value) };
                    let comparison = Expr::Bin(BinExpr {
                        span: DUMMY_SP,
                        op: if index % 2 == 0 { BinaryOp::EqEqEq } else { BinaryOp::NotEqEq },
                        left,
                        right,
                    });
                    expression = Expr::Bin(BinExpr {
                        span: DUMMY_SP,
                        op: BinaryOp::LogicalOr,
                        left: Box::new(Expr::Paren(ParenExpr { span: DUMMY_SP, expr: Box::new(expression) })),
                        right: Box::new(comparison),
                    });
                }
                // A pure parenthesized root must be bounded during constant
                // discovery as well as during protection and restoration.
                for _ in 0..DEPTH {
                    expression = Expr::Paren(ParenExpr { span: DUMMY_SP, expr: Box::new(expression) });
                }
                let mut program = Program::Script(Script {
                    body: vec![Stmt::Expr(ExprStmt { span: DUMMY_SP, expr: Box::new(expression) })],
                    ..Default::default()
                });
                let operators = Operators::new(Mark::new());
                operators.protect(&mut program);
                #[derive(Default)]
                struct Count { guards: usize, strict: usize }
                impl VisitMut for Count {
                    fn visit_mut_expr(&mut self, expression: &mut Expr) {
                        crate::deep::rewrite_expression_spine(expression, self, |count, node| {
                            match node {
                                Expr::Call(call) if matches!(&call.callee,
                                    Callee::Expr(callee) if matches!(callee.as_ref(),
                                        Expr::Ident(ident) if ident.sym.starts_with("\0mangler_strict"))) => count.guards += 1,
                                Expr::Bin(binary) if matches!(binary.op, BinaryOp::EqEqEq | BinaryOp::NotEqEq) => count.strict += 1,
                                _ => {}
                            }
                        });
                    }
                }
                let mut protected = Count::default();
                program.visit_mut_with(&mut protected);
                assert_eq!(protected.guards, DEPTH);
                assert_eq!(protected.strict, 0);
                operators.restore(&mut program);
                let mut restored = Count::default();
                program.visit_mut_with(&mut restored);
                assert_eq!(restored.guards, 0);
                assert_eq!(restored.strict, DEPTH);
                crate::deep::drop_program(program);
            });
        }).join().unwrap();
    }

    #[test]
    fn deeply_parenthesized_null_operands_are_discarded_without_recursive_drop() {
        std::thread::spawn(|| {
            Js::with_globals(|| {
                for reversed in [false, true] {
                    let mut null = Expr::Lit(Lit::Null(Null { span: DUMMY_SP }));
                    for _ in 0..20_000 {
                        null = Expr::Paren(ParenExpr {
                            span: DUMMY_SP,
                            expr: Box::new(null),
                        });
                    }
                    let value = Box::new(Expr::Ident(Ident::new_no_ctxt("value".into(), DUMMY_SP)));
                    let (left, right) = if reversed {
                        (Box::new(null), value)
                    } else {
                        (value, Box::new(null))
                    };
                    let mut program = Program::Script(Script {
                        body: vec![Stmt::Expr(ExprStmt {
                            span: DUMMY_SP,
                            expr: Box::new(Expr::Bin(BinExpr {
                                span: DUMMY_SP,
                                op: if reversed {
                                    BinaryOp::NotEqEq
                                } else {
                                    BinaryOp::EqEqEq
                                },
                                left,
                                right,
                            })),
                        })],
                        ..Default::default()
                    });
                    let operators = Operators::new(Mark::new());
                    operators.protect(&mut program);
                    let Program::Script(script) = &program else {
                        unreachable!()
                    };
                    let Stmt::Expr(statement) = &script.body[0] else {
                        unreachable!()
                    };
                    assert!(matches!(statement.expr.as_ref(), Expr::Call(_)));
                    operators.restore(&mut program);
                    let Program::Script(script) = &program else {
                        unreachable!()
                    };
                    let Stmt::Expr(statement) = &script.body[0] else {
                        unreachable!()
                    };
                    let Expr::Bin(binary) = statement.expr.as_ref() else {
                        panic!("guard not restored")
                    };
                    assert_eq!(
                        binary.op,
                        if reversed {
                            BinaryOp::NotEqEq
                        } else {
                            BinaryOp::EqEqEq
                        }
                    );
                    assert!(
                        matches!(binary.left.as_ref(), Expr::Ident(ident) if ident.sym == "value")
                    );
                    assert!(matches!(binary.right.as_ref(), Expr::Lit(Lit::Null(_))));
                    crate::deep::drop_program(program);
                }
            });
        })
        .join()
        .unwrap();
    }
}
