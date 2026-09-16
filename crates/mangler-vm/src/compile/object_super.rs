//! Home-object references for methods created by the object-expression compiler.
use mangler_core::Language;
use mangler_jsast::{Js, ParseOpts};
use swc_core::common::{DUMMY_SP, Span};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

pub(crate) fn lower_params(params: &[Pat], home: &str, strict: bool) -> Vec<Pat> {
    let mut params = params.to_vec();
    params.visit_mut_with(&mut Lower { home, strict });
    params
}

pub(crate) fn lower(body: &FunctionBody, home: &str, strict: bool) -> FunctionBody {
    let mut body = body.clone();
    body.visit_mut_with(&mut Lower { home, strict });
    body
}
struct Lower<'a> {
    home: &'a str,
    strict: bool,
}
impl Lower<'_> {
    fn host_operation(
        &mut self,
        member: &SuperPropExpr,
        mode: u32,
        rhs: Option<Box<Expr>>,
    ) -> Expr {
        let mut key = match &member.prop {
            SuperProp::Ident(i) => Box::new(Expr::Lit(Lit::Str(Str {
                span: DUMMY_SP,
                value: i.sym.clone().into(),
                raw: None,
            }))),
            SuperProp::Computed(c) => c.expr.clone(),
        };
        key.visit_mut_with(self);
        let sentinel = if rhs.is_some() {
            "\0mangler_super_assign"
        } else {
            "\0mangler_super_update"
        };
        let mut args = vec![
            Box::new(Expr::Lit(Lit::Num(Number {
                span: DUMMY_SP,
                value: (mode | if self.strict { 32 } else { 0 }) as f64,
                raw: None,
            }))),
            Box::new(Expr::Ident(Ident::new_no_ctxt(self.home.into(), DUMMY_SP))),
            key,
            Box::new(Expr::This(ThisExpr { span: DUMMY_SP })),
        ];
        if let Some(mut rhs) = rhs {
            rhs.visit_mut_with(self);
            args.push(Box::new(Expr::Arrow(ArrowExpr {
                span: DUMMY_SP,
                body: Box::new(ArrowFunctionBody::Expr(rhs)),
                ..Default::default()
            })));
        }
        Expr::Call(CallExpr {
            span: DUMMY_SP,
            callee: Callee::Expr(Box::new(Expr::Ident(Ident::new_no_ctxt(
                sentinel.into(),
                DUMMY_SP,
            )))),
            args: args
                .into_iter()
                .map(|expr| ExprOrSpread { expr, spread: None })
                .collect(),
            ..Default::default()
        })
    }
    fn reference(&mut self, member: &SuperPropExpr, callable: bool, optional: bool) -> Expr {
        let mut key = match &member.prop {
            SuperProp::Ident(i) => Box::new(Expr::Lit(Lit::Str(Str {
                span: DUMMY_SP,
                value: i.sym.clone().into(),
                raw: None,
            }))),
            SuperProp::Computed(c) => c.expr.clone(),
        };
        key.visit_mut_with(self);
        let source = if callable {
            let guard = if optional { "if(f==null)return f;" } else { "" };
            format!(
                "((k,o)=>{{const f=Reflect.get(Object.getPrototypeOf(_HOME),k,o);{guard}return (...a)=>Reflect.apply(f,o,a)}})(_KEY,this)"
            )
        } else {
            // Capture a Reference once: key conversion and the prototype lookup
            // precede the RHS, including for compound/logical assignments.
            let set = if self.strict {
                "if(!Reflect.set(b,k,v,o))throw new TypeError('Cannot set super property')"
            } else {
                "Reflect.set(b,k,v,o)"
            };
            "((k,o)=>{k=Reflect.ownKeys({[k]:0})[0];const b=Object.getPrototypeOf(_HOME);return {get value(){return Reflect.get(b,k,o)},set value(v){_SET}}})(_KEY,this).value".replace("_SET", set)
        };
        let ast = Js
            .parse(
                &format!("function _(){{return {source}}}"),
                &ParseOpts::default(),
            )
            .expect("generated home-object bridge parses");
        let mut result = match ast.into_program() {
            Program::Script(s) => match s.body.into_iter().next().unwrap() {
                Stmt::Decl(Decl::Fn(f)) => {
                    match f.function.body.unwrap().stmts.into_iter().next().unwrap() {
                        Stmt::Return(r) => *r.arg.unwrap(),
                        _ => unreachable!(),
                    }
                }
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        struct Substitute<'a> {
            home: &'a str,
            key: Box<Expr>,
        }
        impl VisitMut for Substitute<'_> {
            fn visit_mut_span(&mut self, span: &mut Span) {
                *span = DUMMY_SP;
            }
            fn visit_mut_expr(&mut self, e: &mut Expr) {
                if let Expr::Ident(i) = e {
                    if i.sym.as_ref() == "_HOME" {
                        *e = Expr::Ident(Ident::new_no_ctxt(self.home.into(), DUMMY_SP));
                        return;
                    }
                    if i.sym.as_ref() == "_KEY" {
                        *e = *self.key.clone();
                        return;
                    }
                }
                e.visit_mut_children_with(self);
            }
        }
        result.visit_mut_with(&mut Substitute {
            home: self.home,
            key,
        });
        result
    }
}
fn super_reference(expr: &Expr) -> Option<&SuperPropExpr> {
    match expr {
        Expr::SuperProp(s) => Some(s),
        Expr::Paren(p) => super_reference(&p.expr),
        _ => None,
    }
}

impl VisitMut for Lower<'_> {
    fn visit_mut_function(&mut self, _: &mut Function) {}
    fn visit_mut_class(&mut self, _: &mut Class) {}
    fn visit_mut_unary_expr(&mut self, unary: &mut UnaryExpr) {
        if unary.op == UnaryOp::Delete
            && let Some(member) = super_reference(&unary.arg)
        {
            let mut key = match &member.prop {
                SuperProp::Ident(name) => Box::new(Expr::Lit(Lit::Str(Str {
                    span: DUMMY_SP,
                    value: name.sym.clone().into(),
                    raw: None,
                }))),
                SuperProp::Computed(key) => key.expr.clone(),
            };
            key.visit_mut_with(self);
            let call = |callee: Box<Expr>, args: Vec<Box<Expr>>| {
                Box::new(Expr::Call(CallExpr {
                    span: DUMMY_SP,
                    callee: Callee::Expr(callee),
                    args: args
                        .into_iter()
                        .map(|expr| ExprOrSpread { spread: None, expr })
                        .collect(),
                    ..Default::default()
                }))
            };
            let number = |value: u32| {
                Box::new(Expr::Lit(Lit::Num(Number {
                    span: DUMMY_SP,
                    value: value as f64,
                    raw: None,
                })))
            };
            let provider = call(
                Box::new(Expr::Ident(Ident::new_no_ctxt(
                    "\0mangler_object_super_provider".into(),
                    DUMMY_SP,
                ))),
                vec![
                    number(u32::from(self.strict)),
                    Box::new(Expr::Ident(Ident::new_no_ctxt(self.home.into(), DUMMY_SP))),
                    Box::new(Expr::This(ThisExpr { span: DUMMY_SP })),
                ],
            );
            let operation = call(
                provider,
                vec![number(crate::eval_class::Operation::Delete.id())],
            );
            unary.arg = call(operation, vec![key]);
            return;
        }
        unary.visit_mut_children_with(self);
    }
    fn visit_mut_simple_assign_target(&mut self, target: &mut SimpleAssignTarget) {
        if let SimpleAssignTarget::SuperProp(s) = target {
            let Expr::Member(m) = self.reference(s, false, false) else {
                unreachable!()
            };
            *target = SimpleAssignTarget::Member(m);
        } else {
            target.visit_mut_children_with(self);
        }
    }
    fn visit_mut_expr(&mut self, expr: &mut Expr) {
        match expr {
            Expr::Call(call)
                if call.args.is_empty()
                    && matches!(&call.callee, Callee::Expr(callee) if matches!(&**callee, Expr::Ident(name) if name.sym == *"\0mangler_object_super_provider")) =>
            {
                call.args = vec![
                    ExprOrSpread {
                        spread: None,
                        expr: Box::new(Expr::Lit(Lit::Num(Number {
                            span: DUMMY_SP,
                            value: u32::from(self.strict) as f64,
                            raw: None,
                        }))),
                    },
                    ExprOrSpread {
                        spread: None,
                        expr: Box::new(Expr::Ident(Ident::new_no_ctxt(self.home.into(), DUMMY_SP))),
                    },
                    ExprOrSpread {
                        spread: None,
                        expr: Box::new(Expr::This(ThisExpr { span: DUMMY_SP })),
                    },
                ];
                return;
            }
            Expr::Assign(a) => {
                if let AssignTarget::Simple(SimpleAssignTarget::SuperProp(member)) = &a.left {
                    let mode = match a.op {
                        AssignOp::Assign => 0,
                        AssignOp::AddAssign => 1,
                        AssignOp::SubAssign => 2,
                        AssignOp::MulAssign => 3,
                        AssignOp::DivAssign => 4,
                        AssignOp::ModAssign => 5,
                        AssignOp::ExpAssign => 6,
                        AssignOp::LShiftAssign => 7,
                        AssignOp::RShiftAssign => 8,
                        AssignOp::ZeroFillRShiftAssign => 9,
                        AssignOp::BitOrAssign => 10,
                        AssignOp::BitXorAssign => 11,
                        AssignOp::BitAndAssign => 12,
                        AssignOp::AndAssign => 13,
                        AssignOp::OrAssign => 14,
                        AssignOp::NullishAssign => 15,
                    };
                    *expr = self.host_operation(member, mode, Some(a.right.clone()));
                    return;
                }
            }
            Expr::Update(update) => {
                if let Some(member) = super_reference(&update.arg) {
                    let mode = if update.op == UpdateOp::PlusPlus {
                        0
                    } else {
                        2
                    } | if update.prefix { 1 } else { 0 };
                    *expr = self.host_operation(member, mode, None);
                    return;
                }
            }
            Expr::Call(c) => {
                if let Callee::Expr(callee) = &mut c.callee {
                    let source = super_reference(callee);
                    if let Some(s) = source {
                        **callee = self.reference(s, true, false);
                        c.args.visit_mut_with(self);
                        return;
                    }
                }
            }
            Expr::OptChain(c) => {
                if let OptChainBase::Call(call) = &mut *c.base
                    && let Some(s) = super_reference(&call.callee)
                {
                    *call.callee = self.reference(s, true, c.optional);
                    call.args.visit_mut_with(self);
                    return;
                }
            }
            Expr::TaggedTpl(t) => {
                if let Some(s) = super_reference(&t.tag) {
                    *t.tag = self.reference(s, true, false);
                    t.tpl.visit_mut_with(self);
                    return;
                }
            }
            Expr::SuperProp(s) => {
                *expr = self.reference(s, false, false);
                return;
            }
            _ => {}
        }
        expr.visit_mut_children_with(self);
    }
}
