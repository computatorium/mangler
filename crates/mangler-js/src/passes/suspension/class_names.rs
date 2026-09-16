//! Carry computed-property NamedEvaluation through compatibility transforms.
//! Key conversion stays before class evaluation; the class itself never crosses
//! a function boundary, including suspended heritage and computed class keys.
use super::*;
use swc_core::common::{DUMMY_SP, SyntaxContext};

const MARKER: &str = "\0mangler_class_property_name";
const CALLABLE: &str = "\0mangler_callable_property_name";
const PROTOTYPE: &str = "\0mangler_object_prototype";

pub(super) struct ClassNames {
    objects: super::objects::Objects,
    names: usize,
    prototypes: usize,
    prototype: Ident,
    key: Ident,
}
impl ClassNames {
    pub(super) fn used(&self) -> bool {
        self.names != 0 || self.prototypes != 0 || self.objects.used()
    }

    // Restore while helper calls still have ordinary arguments. Generator
    // lowering may split their argument arrays around a suspended class value.
    pub(super) fn restore_prototypes(&self, program: &mut Program) {
        self.objects.restore(program);
        struct Restore<'a> {
            count: usize,
            helper: &'a Ident,
        }
        impl VisitMut for Restore<'_> {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
                call.visit_mut_children_with(self);
                if call.args.len() == 3
                    && matches!(&*call.args[1].expr, Expr::Lit(Lit::Str(key)) if key.span == DUMMY_SP && key.value.as_str() == Some(PROTOTYPE))
                {
                    call.callee = Callee::Expr(Box::new(Expr::Ident(self.helper.clone())));
                    call.args.remove(1);
                    self.count += 1;
                }
            }
        }
        let mut restore = Restore {
            count: 0,
            helper: &self.prototype,
        };
        program.visit_mut_with(&mut restore);
        assert_eq!(
            restore.count, self.prototypes,
            "object prototype setters survive compatibility lowering"
        );
    }

    pub(super) fn restore(self, program: &mut Program) {
        struct Restore {
            key: Ident,
        }
        impl VisitMut for Restore {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                expression.visit_mut_children_with(self);
                if let Expr::Call(call) = expression {
                    if call.args.len() == 3
                        && matches!(&call.callee, Callee::Expr(callee) if matches!(&**callee, Expr::Ident(id) if id.to_id() == self.key.to_id()))
                        && matches!(&*call.args[1].expr, Expr::Lit(Lit::Str(marker)) if marker.span == DUMMY_SP && marker.value.as_str() == Some(CALLABLE))
                    {
                        let mut arguments = std::mem::take(&mut call.args).into_iter();
                        let value = arguments.next().unwrap().expr;
                        arguments.next();
                        let key = arguments.next().unwrap().expr;
                        *expression =
                            *mangler_jsast::assignment_target::named_value_expression(key, value);
                        return;
                    }
                }
                let Expr::Class(class) = expression else {
                    return;
                };
                let Some(ClassMember::StaticBlock(block)) = class.class.body.first() else {
                    return;
                };
                if block.span != DUMMY_SP {
                    return;
                }
                let [Stmt::Expr(statement)] = block.body.stmts.as_slice() else {
                    return;
                };
                let Expr::Array(array) = &*statement.expr else {
                    return;
                };
                let [Some(marker), Some(key)] = array.elems.as_slice() else {
                    return;
                };
                if !matches!(&*marker.expr,Expr::Lit(Lit::Str(marker)) if marker.value.as_str()==Some(MARKER))
                {
                    return;
                }
                let key = key.expr.clone();
                class.class.body.remove(0);
                let value = Box::new(std::mem::replace(
                    expression,
                    Expr::Invalid(Invalid { span: DUMMY_SP }),
                ));
                *expression = *mangler_jsast::assignment_target::named_value_expression(key, value);
            }
        }
        program.visit_mut_with(&mut Restore { key: self.key });
        // Unreachable class expressions may have been removed by the state
        // machine builder; only surviving expressions need their names restored.
        struct Remaining;
        impl Visit for Remaining {
            fn visit_bin_expr(&mut self, binary: &BinExpr) {
                mangler_jsast::deep::walk_binary(binary, self);
            }
            fn visit_str(&mut self, value: &Str) {
                assert!(
                    value.span != DUMMY_SP
                        || !matches!(value.value.as_str(), Some(MARKER | CALLABLE)),
                    "property name marker restored"
                );
            }
        }
        program.visit_with(&mut Remaining);
    }
}

pub(super) fn prepare(
    program: &mut Program,
    property_key: &Ident,
    prototype: &Ident,
) -> ClassNames {
    let objects = super::objects::prepare(program, property_key);
    struct Prepare<'a> {
        key: &'a Ident,
        active: bool,
        temporaries: Vec<Ident>,
        count: usize,
        prototypes: usize,
        split_object: bool,
    }
    impl Prepare<'_> {
        fn declare(&mut self, body: &mut FunctionBody) {
            if self.temporaries.is_empty() {
                return;
            }
            let at = mangler_jsast::directives::leading_directive_count(&body.stmts);
            body.stmts.insert(
                at,
                Stmt::Decl(Decl::Var(Box::new(VarDecl {
                    span: DUMMY_SP,
                    kind: VarDeclKind::Var,
                    decls: std::mem::take(&mut self.temporaries)
                        .into_iter()
                        .map(|name| VarDeclarator {
                            span: DUMMY_SP,
                            name: Pat::Ident(name.into()),
                            init: None,
                            definite: false,
                        })
                        .collect(),
                    ..Default::default()
                }))),
            );
        }
    }
    impl VisitMut for Prepare<'_> {
        fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(binary, self);
        }
        fn visit_mut_function(&mut self, function: &mut Function) {
            let active = self.active;
            self.active |= function.is_generator;
            let temporaries = std::mem::take(&mut self.temporaries);
            // GeneratorPrerequisites shields parameters from compatibility
            // transforms; body variables must never leak into parameter scope.
            if let Some(body) = &mut function.body {
                body.visit_mut_with(self);
                self.declare(body);
            }
            self.temporaries = temporaries;
            self.active = false;
            function.params.visit_mut_with(self);
            self.active = active;
        }
        fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
            let temporaries = std::mem::take(&mut self.temporaries);
            arrow.body.visit_mut_with(self);
            if !self.temporaries.is_empty() {
                if let ArrowFunctionBody::Expr(value) = &mut *arrow.body {
                    let result = std::mem::replace(
                        value,
                        Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
                    );
                    *arrow.body = ArrowFunctionBody::FunctionBody(FunctionBody {
                        span: DUMMY_SP,
                        stmts: vec![Stmt::Return(ReturnStmt {
                            span: DUMMY_SP,
                            arg: Some(result),
                        })],
                    });
                }
                if let ArrowFunctionBody::FunctionBody(body) = &mut *arrow.body {
                    self.declare(body);
                }
            }
            self.temporaries = temporaries;
        }
        fn visit_mut_object_lit(&mut self, object: &mut ObjectLit) {
            let split = object.props.iter().any(|property| {
                let PropOrSpread::Prop(property) = property else {
                    return false;
                };
                let key = match &**property {
                    Prop::KeyValue(property) => &property.key,
                    Prop::Method(property) => &property.key,
                    Prop::Getter(property) => &property.key,
                    Prop::Setter(property) => &property.key,
                    _ => return false,
                };
                matches!(key, PropName::Computed(_))
            });
            let previous = std::mem::replace(&mut self.split_object, split);
            object.visit_mut_children_with(self);
            self.split_object = previous;
        }
        fn visit_mut_key_value_prop(&mut self, property: &mut KeyValueProp) {
            property.visit_mut_children_with(self);
            if !self.active {
                return;
            }
            if matches!(&property.key,PropName::Ident(name) if name.sym=="__proto__")
                || matches!(&property.key,PropName::Str(name) if name.value.as_str()==Some("__proto__"))
            {
                // Plain objects retain their native method home objects. Do
                // not force them through computed-property compatibility lowering.
                if !self.split_object {
                    return;
                }
                // Carry source prototype-setter provenance through the same
                // object splitting that turns ordinary properties into helper
                // calls. The constant key cannot suspend or be cached away.
                property.key = PropName::Computed(ComputedPropName {
                    span: DUMMY_SP,
                    expr: Box::new(Expr::Lit(Lit::Str(Str {
                        span: DUMMY_SP,
                        value: PROTOTYPE.into(),
                        raw: None,
                    }))),
                });
                self.prototypes += 1;
                return;
            }
            let mut value = &mut *property.value;
            while let Expr::Paren(parentheses) = value {
                value = &mut parentheses.expr;
            }
            let callable = matches!(value, Expr::Fn(function) if function.ident.is_none())
                || matches!(value, Expr::Arrow(_));
            if !callable && !matches!(value, Expr::Class(_)) {
                return;
            }
            let key = match &property.key {
                PropName::Ident(name) => Expr::Lit(Lit::Str(Str {
                    span: DUMMY_SP,
                    value: name.sym.clone().into(),
                    raw: None,
                })),
                PropName::Str(name) => Expr::Lit(Lit::Str(name.clone())),
                PropName::Num(name) => Expr::Lit(Lit::Num(name.clone())),
                PropName::BigInt(name) => Expr::Lit(Lit::BigInt(name.clone())),
                PropName::Computed(name) => *name.expr.clone(),
            };
            let marker_key = if matches!(property.key, PropName::Computed(_)) {
                let temporary = Ident::new(
                    "_class_property_key".into(),
                    DUMMY_SP,
                    SyntaxContext::empty().apply_mark(Mark::new()),
                );
                property.key = PropName::Computed(ComputedPropName {
                    span: DUMMY_SP,
                    expr: Box::new(Expr::Assign(AssignExpr {
                        span: DUMMY_SP,
                        op: AssignOp::Assign,
                        left: AssignTarget::Simple(SimpleAssignTarget::Ident(
                            temporary.clone().into(),
                        )),
                        right: Box::new(Expr::Call(CallExpr {
                            callee: Callee::Expr(Box::new(Expr::Ident(self.key.clone()))),
                            args: vec![ExprOrSpread {
                                spread: None,
                                expr: Box::new(key),
                            }],
                            ..Default::default()
                        })),
                    })),
                });
                self.temporaries.push(temporary.clone());
                self.count += 1;
                Expr::Ident(temporary)
            } else {
                // Static property keys need no evaluated temporary. Retaining
                // their syntax also retains native object method home objects.
                key
            };
            if callable {
                let original = Box::new(std::mem::replace(
                    value,
                    Expr::Invalid(Invalid { span: DUMMY_SP }),
                ));
                *value = Expr::Call(CallExpr {
                    callee: Callee::Expr(Box::new(Expr::Ident(self.key.clone()))),
                    args: vec![
                        ExprOrSpread {
                            spread: None,
                            expr: original,
                        },
                        ExprOrSpread {
                            spread: None,
                            expr: Box::new(Expr::Lit(Lit::Str(Str {
                                span: DUMMY_SP,
                                value: CALLABLE.into(),
                                raw: None,
                            }))),
                        },
                        ExprOrSpread {
                            spread: None,
                            expr: Box::new(marker_key),
                        },
                    ],
                    ..Default::default()
                });
                return;
            }
            let Expr::Class(class) = value else {
                unreachable!()
            };
            class.class.body.insert(
                0,
                ClassMember::StaticBlock(StaticBlock {
                    span: DUMMY_SP,
                    body: BlockStmt {
                        span: DUMMY_SP,
                        stmts: vec![Stmt::Expr(ExprStmt {
                            span: DUMMY_SP,
                            expr: Box::new(Expr::Array(ArrayLit {
                                span: DUMMY_SP,
                                elems: vec![
                                    Some(ExprOrSpread {
                                        spread: None,
                                        expr: Box::new(Expr::Lit(Lit::Str(Str {
                                            span: DUMMY_SP,
                                            value: MARKER.into(),
                                            raw: None,
                                        }))),
                                    }),
                                    Some(ExprOrSpread {
                                        spread: None,
                                        expr: Box::new(marker_key),
                                    }),
                                ],
                            })),
                        })],
                        ..Default::default()
                    },
                }),
            );
        }
    }
    let mut prepare = Prepare {
        key: property_key,
        active: false,
        temporaries: Vec::new(),
        count: 0,
        prototypes: 0,
        split_object: false,
    };
    program.visit_mut_with(&mut prepare);
    ClassNames {
        objects,
        names: prepare.count,
        prototypes: prepare.prototypes,
        prototype: prototype.clone(),
        key: property_key.clone(),
    }
}
