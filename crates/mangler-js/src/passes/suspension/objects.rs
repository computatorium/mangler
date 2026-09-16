//! Preserve object literal evaluation and native method home objects while the
//! suspension prerequisites lower computed properties and object spread.
use super::*;
use swc_core::common::{DUMMY_SP, SyntaxContext};
use swc_core::ecma::visit::{Visit, VisitWith};

const MARKER: &str = "\0mangler_suspended_object_";
#[derive(Clone)]
enum Property {
    Key(PropName),
    Spread,
}
#[derive(Default)]
pub(super) struct Objects {
    properties: HashMap<String, Property>,
}
impl Objects {
    pub(super) fn used(&self) -> bool {
        !self.properties.is_empty()
    }
    pub(super) fn restore(&self, program: &mut Program) {
        struct Restore<'a>(&'a mut HashMap<String, Property>);
        impl VisitMut for Restore<'_> {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_prop_or_spread(&mut self, property: &mut PropOrSpread) {
                property.visit_mut_children_with(self);
                let PropOrSpread::Prop(prop) = property else {
                    return;
                };
                let key = match &mut **prop {
                    Prop::KeyValue(prop) => &mut prop.key,
                    Prop::Method(prop) => &mut prop.key,
                    Prop::Getter(prop) => &mut prop.key,
                    Prop::Setter(prop) => &mut prop.key,
                    _ => return,
                };
                let PropName::Str(name) = key else { return };
                if name.span != DUMMY_SP {
                    return;
                }
                let Some(name) = name.value.as_str() else {
                    return;
                };
                let Some(original) = self.0.remove(name) else {
                    return;
                };
                match original {
                    Property::Key(original) => *key = original,
                    Property::Spread => {
                        let Prop::KeyValue(prop) = &mut **prop else {
                            unreachable!()
                        };
                        *property = PropOrSpread::Spread(SpreadElement {
                            dot3_token: DUMMY_SP,
                            expr: std::mem::replace(&mut prop.value, invalid()),
                        });
                    }
                }
            }
        }
        let mut properties = self.properties.clone();
        program.visit_mut_with(&mut Restore(&mut properties));
        assert!(
            properties.is_empty(),
            "suspended object properties survive prerequisites"
        );
    }
}
fn invalid() -> Box<Expr> {
    Box::new(Expr::Invalid(Invalid { span: DUMMY_SP }))
}
fn id(name: &Ident) -> Box<Expr> {
    Box::new(Expr::Ident(name.clone()))
}
fn key_value(key: PropName, value: Box<Expr>) -> PropOrSpread {
    PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp { key, value })))
}
fn key_expression(key: &PropName) -> Box<Expr> {
    Box::new(match key {
        PropName::Computed(key) => *key.expr.clone(),
        PropName::Ident(key) => Expr::Lit(Lit::Str(Str {
            span: DUMMY_SP,
            value: key.sym.clone().into(),
            raw: None,
        })),
        PropName::Str(key) => Expr::Lit(Lit::Str(key.clone())),
        PropName::Num(key) => Expr::Lit(Lit::Num(key.clone())),
        PropName::BigInt(key) => Expr::Lit(Lit::BigInt(key.clone())),
    })
}

pub(super) fn prepare(program: &mut Program, property_key: &Ident) -> Objects {
    struct Prepare<'a> {
        key: &'a Ident,
        active: bool,
        temporaries: Vec<Ident>,
        objects: Objects,
    }
    impl Prepare<'_> {
        fn cache(&mut self, value: Box<Expr>, evaluation: &mut Vec<Box<Expr>>) -> Ident {
            let name = Ident::new(
                "_object_value".into(),
                DUMMY_SP,
                SyntaxContext::empty().apply_mark(Mark::new()),
            );
            self.temporaries.push(name.clone());
            evaluation.push(Box::new(Expr::Assign(AssignExpr {
                span: DUMMY_SP,
                op: AssignOp::Assign,
                left: AssignTarget::Simple(SimpleAssignTarget::Ident(name.clone().into())),
                // Generated variable names must never become source function names.
                right: Box::new(Expr::Seq(SeqExpr {
                    span: DUMMY_SP,
                    exprs: vec![
                        Box::new(Expr::Lit(Lit::Num(Number {
                            span: DUMMY_SP,
                            value: 0.,
                            raw: None,
                        }))),
                        value,
                    ],
                })),
            })));
            name
        }
        fn marker(&mut self, original: Property) -> PropName {
            let marker = format!("{MARKER}{}", self.objects.properties.len());
            self.objects.properties.insert(marker.clone(), original);
            PropName::Str(Str {
                span: DUMMY_SP,
                value: marker.into(),
                raw: None,
            })
        }
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
            if let Some(body) = &mut function.body {
                body.visit_mut_with(self);
                self.declare(body);
            }
            self.temporaries = temporaries;
            // Source parameters are protected from prerequisite lowering.
            self.active = false;
            function.params.visit_mut_with(self);
            self.active = active;
        }
        fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
            let temporaries = std::mem::take(&mut self.temporaries);
            arrow.body.visit_mut_with(self);
            if !self.temporaries.is_empty() {
                if let ArrowFunctionBody::Expr(value) = &mut *arrow.body {
                    *arrow.body = ArrowFunctionBody::FunctionBody(FunctionBody {
                        span: DUMMY_SP,
                        stmts: vec![Stmt::Return(ReturnStmt {
                            span: DUMMY_SP,
                            arg: Some(std::mem::replace(value, invalid())),
                        })],
                    });
                }
                if let ArrowFunctionBody::FunctionBody(body) = &mut *arrow.body {
                    self.declare(body);
                }
            }
            self.temporaries = temporaries;
        }
        fn visit_mut_expr(&mut self, expression: &mut Expr) {
            expression.visit_mut_children_with(self);
            if !self.active {
                return;
            }
            let Expr::Object(object) = expression else {
                return;
            };
            let complex = object.props.iter().any(|property| match property {
                PropOrSpread::Spread(_) => true,
                PropOrSpread::Prop(property) => match &**property {
                    Prop::KeyValue(p) => matches!(p.key, PropName::Computed(_)),
                    Prop::Method(p) => matches!(p.key, PropName::Computed(_)),
                    Prop::Getter(p) => matches!(p.key, PropName::Computed(_)),
                    Prop::Setter(p) => matches!(p.key, PropName::Computed(_)),
                    _ => false,
                },
            });
            struct Suspends(bool);
            impl Visit for Suspends {
                fn visit_yield_expr(&mut self, _: &YieldExpr) {
                    self.0 = true;
                }
                fn visit_function(&mut self, _: &Function) {}
                fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
                fn visit_bin_expr(&mut self, binary: &BinExpr) {
                    mangler_jsast::deep::walk_binary(binary, self);
                }
            }
            let mut suspends = Suspends(false);
            object.visit_with(&mut suspends);
            if !complex && !suspends.0 {
                return;
            }
            let mut evaluation = Vec::new();
            for property in &mut object.props {
                if let PropOrSpread::Spread(spread) = property {
                    // Copy now: proxy enumeration and getters precede later keys.
                    let value = Box::new(Expr::Object(ObjectLit {
                        span: DUMMY_SP,
                        props: vec![PropOrSpread::Spread(spread.clone())],
                    }));
                    let cache = self.cache(value, &mut evaluation);
                    *property = key_value(self.marker(Property::Spread), id(&cache));
                    continue;
                }
                let PropOrSpread::Prop(property) = property else {
                    unreachable!()
                };
                if let Prop::Shorthand(name) = &**property {
                    let cache = self.cache(id(name), &mut evaluation);
                    **property = Prop::KeyValue(KeyValueProp {
                        key: PropName::Ident(name.clone().into()),
                        value: id(&cache),
                    });
                    continue;
                }
                let key = match &mut **property {
                    Prop::KeyValue(p) => &mut p.key,
                    Prop::Method(p) => &mut p.key,
                    Prop::Getter(p) => &mut p.key,
                    Prop::Setter(p) => &mut p.key,
                    _ => continue,
                };
                let actual_key = if let PropName::Computed(computed) = key {
                    let value = std::mem::replace(&mut computed.expr, invalid());
                    let converted = Box::new(Expr::Call(CallExpr {
                        callee: Callee::Expr(id(self.key)),
                        args: vec![ExprOrSpread {
                            spread: None,
                            expr: value,
                        }],
                        ..Default::default()
                    }));
                    let cache = self.cache(converted, &mut evaluation);
                    let actual = PropName::Computed(ComputedPropName {
                        span: DUMMY_SP,
                        expr: id(&cache),
                    });
                    *key = self.marker(Property::Key(actual.clone()));
                    actual
                } else {
                    key.clone()
                };
                if let Prop::KeyValue(value) = &mut **property {
                    let mut original = std::mem::replace(&mut value.value, invalid());
                    let prototype = matches!(&actual_key, PropName::Ident(key) if key.sym == "__proto__")
                        || matches!(&actual_key, PropName::Str(key) if key.value.as_str()==Some("__proto__"));
                    if !prototype
                        && mangler_jsast::assignment_target::is_anonymous_definition(&original)
                    {
                        original = mangler_jsast::assignment_target::named_value_expression(
                            key_expression(&actual_key),
                            original,
                        );
                    }
                    let cache = self.cache(original, &mut evaluation);
                    value.value = id(&cache);
                }
            }
            evaluation.push(Box::new(std::mem::replace(
                expression,
                Expr::Invalid(Invalid { span: DUMMY_SP }),
            )));
            *expression = Expr::Seq(SeqExpr {
                span: DUMMY_SP,
                exprs: evaluation,
            });
        }
    }
    let mut prepare = Prepare {
        key: property_key,
        active: false,
        temporaries: Vec::new(),
        objects: Objects::default(),
    };
    program.visit_mut_with(&mut prepare);
    prepare.objects
}
