//! Suspension adapters retain the native method target and its HomeObject.
use super::*;
use swc_core::common::Spanned;

const INSTALLED: swc_core::common::Span = swc_core::common::Span {
    lo: swc_core::common::BytePos(0),
    hi: swc_core::common::BytePos(4),
};

fn helper_call(helper: &str, method: &str, arguments: impl IntoIterator<Item = Box<Expr>>) -> Expr {
    let mut expression = call(helper, arguments);
    let Expr::Call(call) = &mut expression else {
        unreachable!()
    };
    call.span = mangler_jsast::span::runtime_span();
    call.callee = Callee::Expr(Box::new(Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(ident(helper)),
        prop: MemberProp::Ident(IdentName::new(method.into(), DUMMY_SP)),
    })));
    expression
}

fn number(value: u32) -> Box<Expr> {
    Box::new(Expr::Lit(Lit::Num(Number {
        span: DUMMY_SP,
        value: value as f64,
        raw: None,
    })))
}
fn string(value: &str) -> Box<Expr> {
    Box::new(Expr::Lit(Lit::Str(Str {
        span: DUMMY_SP,
        value: value.into(),
        raw: None,
    })))
}

impl Virtualizer<'_> {
    pub(in crate::passes::virtualize) fn class_suspension_methods(
        &mut self,
        class: &mut swc_core::ecma::ast::Class,
    ) {
        if class.body.iter().any(
            |member| matches!(member,ClassMember::StaticBlock(block) if block.span == INSTALLED),
        ) {
            return;
        }
        let kind = |function: &Function| -> u32 {
            if self.outcomes.get(&function.span.lo.0) != Some(&None) {
                return 0;
            }
            match self.suspensions.get(&function.span.lo.0) {
                Some(mangler_vm::SuspensionKind::Async) => 1,
                Some(mangler_vm::SuspensionKind::Generator) => 2,
                Some(mangler_vm::SuspensionKind::AsyncGenerator) => 3,
                None => 0,
            }
        };
        if !class.body.iter().any(|member| match member {
            ClassMember::Method(method) => kind(&method.function) != 0,
            ClassMember::PrivateMethod(method) => kind(&method.function) != 0,
            _ => false,
        }) {
            return;
        }

        let helper = if let Some(helper) = self.class_method_helper.as_ref() {
            helper.clone()
        } else {
            let helper = self.cfg.fresh_name();
            let source = include_str!("method_shell.js").replacen("__MCLASS", &helper, 1);
            let Program::Script(mut program) = Js
                .parse(&source, &ParseOpts::default())
                .expect("class method adapters parse")
                .into_program()
            else {
                unreachable!()
            };
            program.visit_mut_with(&mut GeneratedSpans);
            self.shell_intrinsics.extend(program.body);
            *self.class_method_helper = Some(helper.clone());
            helper
        };

        let mut members = Vec::with_capacity(class.body.len() + 2);
        let mut private_names: std::collections::HashSet<String> = class
            .body
            .iter()
            .filter_map(|member| match member {
                ClassMember::PrivateMethod(method) => Some(method.key.name.to_string()),
                ClassMember::PrivateProp(property) => Some(property.key.name.to_string()),
                _ => None,
            })
            .collect();
        members.push(ClassMember::StaticBlock(StaticBlock {
            span: INSTALLED,
            body: BlockStmt {
                span: DUMMY_SP,
                stmts: vec![Stmt::Expr(ExprStmt {
                    span: DUMMY_SP,
                    expr: Box::new(helper_call(
                        &helper,
                        "install",
                        vec![Box::new(Expr::This(ThisExpr { span: DUMMY_SP }))],
                    )),
                })],
                ..Default::default()
            },
        }));
        for mut member in std::mem::take(&mut class.body) {
            match &mut member {
                ClassMember::Method(method) => {
                    let method_kind = kind(&method.function);
                    let accessor = match method.kind {
                        MethodKind::Getter => 1,
                        MethodKind::Setter => 2,
                        MethodKind::Method => 0,
                    };
                    let protected_key = !matches!(&method.key, PropName::Computed(key)
                        if !generated_expression(key.expr.span()));
                    let key = match &method.key {
                        PropName::Ident(id) => string(id.sym.as_ref()),
                        PropName::Str(value) => Box::new(Expr::Lit(Lit::Str(value.clone()))),
                        PropName::Num(value) => Box::new(Expr::Lit(Lit::Num(value.clone()))),
                        PropName::BigInt(value) => Box::new(Expr::Lit(Lit::BigInt(value.clone()))),
                        PropName::Computed(key) => key.expr.clone(),
                    };
                    let original_name = self
                        .source_names
                        .get(&method.function.span.lo.0)
                        .cloned()
                        .or_else(|| static_prop_key_name(&method.key))
                        .unwrap_or_else(|| "<computed>".into());
                    method.key = PropName::Computed(ComputedPropName {
                        span: DUMMY_SP,
                        expr: Box::new(helper_call(
                            &helper,
                            "key",
                            vec![
                                key,
                                number(method_kind),
                                number(accessor),
                                Box::new(Expr::Lit(Lit::Bool(Bool {
                                    span: DUMMY_SP,
                                    value: method.is_static,
                                }))),
                                string(&original_name),
                                Box::new(Expr::Lit(Lit::Bool(Bool {
                                    span: DUMMY_SP,
                                    value: protected_key,
                                }))),
                            ],
                        )),
                    });
                }
                ClassMember::PrivateMethod(method) if kind(&method.function) != 0 => {
                    let method_kind = kind(&method.function);
                    let name = method.key.clone();
                    let mut implementation = self.cfg.fresh_name();
                    while !private_names.insert(implementation.clone()) {
                        implementation = self.cfg.fresh_name();
                    }
                    method.key = PrivateName {
                        span: DUMMY_SP,
                        name: implementation.clone().into(),
                    };
                    let mut getter = method.clone();
                    getter.span = DUMMY_SP;
                    getter.key = name.clone();
                    getter.kind = MethodKind::Getter;
                    getter.function = Box::new(Function {
                        span: DUMMY_SP,
                        body: Some(FunctionBody {
                            span: DUMMY_SP,
                            stmts: vec![Stmt::Return(ReturnStmt {
                                span: DUMMY_SP,
                                arg: Some(Box::new(helper_call(
                                    &helper,
                                    "wrap",
                                    vec![
                                        Box::new(Expr::Member(MemberExpr {
                                            span: DUMMY_SP,
                                            obj: Box::new(Expr::This(ThisExpr { span: DUMMY_SP })),
                                            prop: MemberProp::PrivateName(PrivateName {
                                                span: DUMMY_SP,
                                                name: implementation.into(),
                                            }),
                                        })),
                                        number(method_kind),
                                        string(&format!("#{}", name.name)),
                                        number(0),
                                    ],
                                ))),
                            })],
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                    members.push(ClassMember::PrivateMethod(getter));
                }
                _ => {}
            }
            members.push(member);
        }
        class.body = members;
    }
}

/// Preserve explicit exclusions after a public key becomes a generated marker.
pub(super) fn original_method_name(key: &PropName) -> Option<String> {
    let PropName::Computed(key) = key else {
        return None;
    };
    let Expr::Call(call) = &*key.expr else {
        return None;
    };
    if !mangler_jsast::span::is_runtime_span(call.span) {
        return None;
    }
    let Expr::Lit(Lit::Str(name)) = &*call.args.get(4)?.expr else {
        return None;
    };
    name.value.as_str().map(str::to_string)
}

/// A structural adapter must not conceal a source key that failed compilation.
pub(super) fn original_key_is_protected(key: &PropName) -> Option<bool> {
    original_method_name(key)?;
    let PropName::Computed(key) = key else {
        return None;
    };
    let Expr::Call(call) = &*key.expr else {
        return None;
    };
    let Expr::Lit(Lit::Bool(protected)) = &*call.args.get(5)?.expr else {
        return None;
    };
    Some(protected.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_key_cannot_hide_unprotected_source_header() {
        let key = PropName::Computed(ComputedPropName {
            span: DUMMY_SP,
            expr: Box::new(helper_call(
                "adapter",
                "key",
                vec![
                    Box::new(call("sourceBusinessLogic", vec![])),
                    number(1),
                    number(0),
                    Box::new(Expr::Lit(Lit::Bool(Bool {
                        span: DUMMY_SP,
                        value: false,
                    }))),
                    string("<computed>"),
                    Box::new(Expr::Lit(Lit::Bool(Bool {
                        span: DUMMY_SP,
                        value: false,
                    }))),
                ],
            )),
        });
        let class = swc_core::ecma::ast::Class {
            body: vec![ClassMember::Method(ClassMethod {
                key,
                function: Box::new(Function {
                    body: Some(FunctionBody {
                        span: DUMMY_SP,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            })],
            ..Default::default()
        };
        assert!(!protected_class(&class, None));
    }
}
