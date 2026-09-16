//! Lazy destructuring inside suspended expressions. A transparent generator
//! supplies the exception region required by IteratorClose while every source
//! expression remains in the bytecode compilation tree.
use super::*;
use mangler_core::Language;
use mangler_jsast::{Js, ParseOpts};
use swc_core::common::{DUMMY_SP, SyntaxContext};

fn fresh(name: &str) -> Ident {
    Ident::new(
        name.into(),
        DUMMY_SP,
        SyntaxContext::empty().apply_mark(Mark::new()),
    )
}
fn id(value: &Ident) -> Box<Expr> {
    Box::new(Expr::Ident(value.clone()))
}
fn arg(expr: Box<Expr>) -> ExprOrSpread {
    ExprOrSpread { spread: None, expr }
}
fn call(function: &Ident, args: impl IntoIterator<Item = Box<Expr>>) -> Box<Expr> {
    Box::new(Expr::Call(CallExpr {
        callee: Callee::Expr(id(function)),
        args: args.into_iter().map(arg).collect(),
        ..Default::default()
    }))
}
fn boolean(value: bool) -> Box<Expr> {
    Box::new(Expr::Lit(Lit::Bool(Bool {
        span: DUMMY_SP,
        value,
    })))
}
fn undefined() -> Box<Expr> {
    Box::new(Expr::Unary(UnaryExpr {
        span: DUMMY_SP,
        op: UnaryOp::Void,
        arg: Box::new(Expr::Lit(Lit::Num(Number {
            span: DUMMY_SP,
            value: 0.,
            raw: None,
        }))),
    }))
}
fn expr(value: Box<Expr>) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: value,
    })
}
fn assignment(target: AssignTarget, right: Box<Expr>) -> Box<Expr> {
    Box::new(Expr::Assign(AssignExpr {
        span: DUMMY_SP,
        op: AssignOp::Assign,
        left: target,
        right,
    }))
}
fn set(target: &Ident, value: Box<Expr>) -> Box<Expr> {
    assignment(
        AssignTarget::Simple(SimpleAssignTarget::Ident(target.clone().into())),
        value,
    )
}
fn var(name: Ident, init: Option<Box<Expr>>) -> VarDeclarator {
    VarDeclarator {
        span: DUMMY_SP,
        name: Pat::Ident(name.into()),
        init,
        definite: false,
    }
}
fn declaration(decls: Vec<VarDeclarator>) -> Stmt {
    Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        kind: VarDeclKind::Var,
        decls,
        ..Default::default()
    })))
}
fn block(stmts: Vec<Stmt>) -> BlockStmt {
    BlockStmt {
        stmts,
        ..Default::default()
    }
}
fn property(object: Box<Expr>, key: Box<Expr>) -> Box<Expr> {
    Box::new(Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: object,
        prop: MemberProp::Computed(ComputedPropName {
            span: DUMMY_SP,
            expr: key,
        }),
    }))
}
fn string(value: &str) -> Box<Expr> {
    Box::new(Expr::Lit(Lit::Str(Str {
        span: DUMMY_SP,
        value: value.into(),
        raw: None,
    })))
}

struct Helpers {
    open: Ident,
    step: Ident,
    close: Ident,
    rest: Ident,
    object: Ident,
    key: Ident,
    object_rest: Ident,
    prototype: Ident,
}
impl Helpers {
    fn bind(&self, pattern: Pat, value: Box<Expr>, output: &mut Vec<Stmt>) {
        match pattern {
            Pat::Ident(binding) => output.push(expr(assignment(
                AssignTarget::Simple(SimpleAssignTarget::Ident(binding)),
                value,
            ))),
            Pat::Expr(target) => output.push(expr(assignment(
                AssignTarget::Simple(
                    SimpleAssignTarget::try_from(target).expect("destructuring assignment target"),
                ),
                value,
            ))),
            Pat::Rest(rest) => self.bind(*rest.arg, value, output),
            Pat::Assign(default) => {
                let temporary = fresh("_pattern_default");
                output.push(declaration(vec![var(temporary.clone(), None)]));
                let mut right = default.right;
                let mut direct = &*right;
                while let Expr::Paren(p) = direct {
                    direct = &p.expr;
                }
                if let Pat::Ident(binding) = &*default.left
                    && (matches!(direct, Expr::Fn(f) if f.ident.is_none())
                        || matches!(direct, Expr::Arrow(_))
                        || matches!(direct, Expr::Class(c) if c.ident.is_none()))
                {
                    let key = string(binding.id.sym.as_ref());
                    right = property(
                        Box::new(Expr::Object(ObjectLit {
                            span: DUMMY_SP,
                            props: vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(
                                KeyValueProp {
                                    key: PropName::Computed(ComputedPropName {
                                        span: DUMMY_SP,
                                        expr: key.clone(),
                                    }),
                                    value: right,
                                },
                            )))],
                        })),
                        key,
                    );
                }
                let evaluated = Box::new(Expr::Seq(SeqExpr {
                    span: DUMMY_SP,
                    exprs: vec![
                        set(&temporary, value),
                        Box::new(Expr::Cond(CondExpr {
                            span: DUMMY_SP,
                            test: Box::new(Expr::Bin(BinExpr {
                                span: DUMMY_SP,
                                op: BinaryOp::EqEqEq,
                                left: id(&temporary),
                                right: undefined(),
                            })),
                            cons: right,
                            alt: id(&temporary),
                        })),
                    ],
                }));
                self.bind(*default.left, evaluated, output);
            }
            Pat::Array(array) => {
                let record = fresh("_pattern_iterator");
                let abrupt = fresh("_pattern_abrupt");
                let error = fresh("_pattern_error");
                output.push(declaration(vec![
                    var(record.clone(), Some(call(&self.open, [value]))),
                    var(abrupt.clone(), Some(boolean(false))),
                ]));
                let mut body = Vec::new();
                for element in array.elems {
                    match element {
                        Some(Pat::Rest(rest)) => {
                            self.bind(*rest.arg, call(&self.rest, [id(&record)]), &mut body)
                        }
                        Some(pattern) => self.bind(
                            pattern,
                            call(&self.step, [id(&record), boolean(true)]),
                            &mut body,
                        ),
                        None => body.push(expr(call(&self.step, [id(&record), boolean(false)]))),
                    }
                }
                output.push(Stmt::Try(Box::new(TryStmt {
                    span: DUMMY_SP,
                    block: block(body),
                    handler: Some(CatchClause {
                        span: DUMMY_SP,
                        param: Some(Pat::Ident(error.clone().into())),
                        body: block(vec![
                            expr(set(&abrupt, boolean(true))),
                            Stmt::Throw(ThrowStmt {
                                span: DUMMY_SP,
                                arg: id(&error),
                            }),
                        ]),
                    }),
                    finalizer: Some(block(vec![expr(call(
                        &self.close,
                        [id(&record), id(&abrupt)],
                    ))])),
                })));
            }
            Pat::Object(object) => {
                let source = fresh("_pattern_object");
                output.push(declaration(vec![var(
                    source.clone(),
                    Some(call(&self.object, [value])),
                )]));
                let mut excluded = Vec::new();
                for entry in object.props {
                    match entry {
                        ObjectPatProp::KeyValue(entry) => {
                            let value = match entry.key {
                                PropName::Ident(key) => string(key.sym.as_ref()),
                                PropName::Str(key) => Box::new(Expr::Lit(Lit::Str(key))),
                                PropName::Num(key) => Box::new(Expr::Lit(Lit::Num(key))),
                                PropName::BigInt(key) => Box::new(Expr::Lit(Lit::BigInt(key))),
                                PropName::Computed(key) => key.expr,
                            };
                            let key = fresh("_pattern_key");
                            output.push(declaration(vec![var(
                                key.clone(),
                                Some(call(&self.key, [value])),
                            )]));
                            self.bind(*entry.value, property(id(&source), id(&key)), output);
                            excluded.push(Some(arg(id(&key))));
                        }
                        ObjectPatProp::Assign(entry) => {
                            let key = string(entry.key.id.sym.as_ref());
                            let mut pattern = Pat::Ident(entry.key);
                            if let Some(right) = entry.value {
                                pattern = Pat::Assign(AssignPat {
                                    span: entry.span,
                                    left: Box::new(pattern),
                                    right,
                                });
                            }
                            self.bind(pattern, property(id(&source), key.clone()), output);
                            excluded.push(Some(arg(key)));
                        }
                        ObjectPatProp::Rest(rest) => {
                            self.bind(
                                *rest.arg,
                                call(
                                    &self.object_rest,
                                    [
                                        id(&source),
                                        Box::new(Expr::Array(ArrayLit {
                                            span: DUMMY_SP,
                                            elems: std::mem::take(&mut excluded),
                                        })),
                                    ],
                                ),
                                output,
                            );
                        }
                    }
                }
            }
            Pat::Invalid(_) => unreachable!("validated pattern"),
        }
    }
}

pub(super) fn lower(program: &mut Program, unresolved: Mark) -> super::class_names::ClassNames {
    let mut helper_program = Js
        .parse(include_str!("patterns.js"), &ParseOpts::default())
        .expect("pattern helpers parse")
        .into_program();
    resolver(unresolved, Mark::new(), false).process(&mut helper_program);
    struct Generated;
    impl VisitMut for Generated {
        fn visit_mut_ident(&mut self, ident: &mut Ident) {
            ident.span = DUMMY_SP;
        }
        fn visit_mut_function(&mut self, function: &mut Function) {
            function.span = DUMMY_SP;
            function.visit_mut_children_with(self);
        }
    }
    helper_program.visit_mut_with(&mut Generated);
    let Program::Script(mut helper_program) = helper_program else {
        unreachable!()
    };
    let identities: Vec<_> = helper_program
        .body
        .iter()
        .filter_map(|s| match s {
            Stmt::Decl(Decl::Fn(f)) => Some(f.ident.clone()),
            _ => None,
        })
        .collect();
    let helpers = Helpers {
        open: identities[0].clone(),
        step: identities[1].clone(),
        close: identities[2].clone(),
        rest: identities[3].clone(),
        object: identities[4].clone(),
        key: identities[5].clone(),
        object_rest: identities[6].clone(),
        prototype: identities[7].clone(),
    };
    struct Lower<'a> {
        helpers: &'a Helpers,
        active: bool,
        used: bool,
        hoisted: Vec<VarDeclarator>,
        unresolved: Mark,
    }
    impl Lower<'_> {
        fn expression(&mut self, pattern: Pat, mut value: Box<Expr>) -> Box<Expr> {
            let mut pattern = pattern;
            let mut hoister = swc_core::ecma::utils::function::FnEnvHoister::new(
                SyntaxContext::empty().apply_mark(self.unresolved),
            );
            hoister.disable_arguments();
            value.visit_mut_with(&mut hoister);
            pattern.visit_mut_with(&mut hoister);
            self.hoisted.extend(hoister.to_decl());
            let result = fresh("_pattern_result");
            let mut stmts = vec![declaration(vec![var(result.clone(), Some(value))])];
            self.helpers.bind(pattern, id(&result), &mut stmts);
            stmts.push(Stmt::Return(ReturnStmt {
                span: DUMMY_SP,
                arg: Some(id(&result)),
            }));
            let marker = mangler_jsast::span::suspension_entry_span();
            let function = Expr::Fn(FnExpr {
                ident: None,
                function: Box::new(Function {
                    span: marker,
                    body: Some(FunctionBody {
                        span: marker,
                        stmts,
                        ..Default::default()
                    }),
                    is_generator: true,
                    ..Default::default()
                }),
            });
            self.used = true;
            Box::new(Expr::Yield(YieldExpr {
                span: DUMMY_SP,
                delegate: true,
                arg: Some(Box::new(Expr::Call(CallExpr {
                    callee: Callee::Expr(Box::new(function)),
                    ..Default::default()
                }))),
            }))
        }
    }
    impl VisitMut for Lower<'_> {
        fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(expression, self);
        }
        fn visit_mut_function(&mut self, function: &mut Function) {
            let active = std::mem::replace(&mut self.active, false);
            function.params.visit_mut_with(self);
            self.active = function.is_generator;
            let saved = std::mem::take(&mut self.hoisted);
            if let Some(body) = &mut function.body {
                body.visit_mut_with(self);
                if !self.hoisted.is_empty() {
                    let at = body
                        .stmts
                        .iter()
                        .take_while(|s| mangler_jsast::directives::is_directive(s))
                        .count();
                    body.stmts
                        .insert(at, declaration(std::mem::take(&mut self.hoisted)));
                }
            }
            self.hoisted = saved;
            self.active = active;
        }
        fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
            let active = std::mem::replace(&mut self.active, false);
            arrow.params.visit_mut_with(self);
            arrow.body.visit_mut_with(self);
            self.active = active;
        }
        fn visit_mut_constructor(&mut self, constructor: &mut Constructor) {
            let active = std::mem::replace(&mut self.active, false);
            constructor.visit_mut_children_with(self);
            self.active = active;
        }
        fn visit_mut_class_prop(&mut self, property: &mut ClassProp) {
            property.key.visit_mut_with(self);
            let active = std::mem::replace(&mut self.active, false);
            property.value.visit_mut_with(self);
            self.active = active;
        }
        fn visit_mut_private_prop(&mut self, property: &mut PrivateProp) {
            let active = std::mem::replace(&mut self.active, false);
            property.value.visit_mut_with(self);
            self.active = active;
        }
        fn visit_mut_auto_accessor(&mut self, property: &mut AutoAccessor) {
            property.key.visit_mut_with(self);
            let active = std::mem::replace(&mut self.active, false);
            property.value.visit_mut_with(self);
            self.active = active;
        }
        fn visit_mut_static_block(&mut self, block: &mut StaticBlock) {
            let active = std::mem::replace(&mut self.active, false);
            block.body.visit_mut_with(self);
            self.active = active;
        }
        fn visit_mut_var_declarator(&mut self, declaration: &mut VarDeclarator) {
            declaration.visit_mut_children_with(self);
            if !self.active || !matches!(declaration.name, Pat::Array(_) | Pat::Object(_)) {
                return;
            }
            let Some(value) = declaration.init.take() else {
                return;
            };
            struct Names(Vec<Ident>);
            impl Visit for Names {
                fn visit_binding_ident(&mut self, binding: &BindingIdent) {
                    self.0.push(binding.id.clone());
                }
                fn visit_expr(&mut self, _: &Expr) {}
            }
            let mut names = Names(Vec::new());
            declaration.name.visit_with(&mut names);
            self.hoisted
                .extend(names.0.into_iter().map(|name| var(name, None)));
            let pattern = std::mem::replace(
                &mut declaration.name,
                Pat::Ident(fresh("_pattern_declaration").into()),
            );
            declaration.init = Some(self.expression(pattern, value));
        }
        fn visit_mut_expr(&mut self, expression: &mut Expr) {
            expression.visit_mut_children_with(self);
            if !self.active {
                return;
            }
            let Expr::Assign(assignment) = expression else {
                return;
            };
            let pattern = match &assignment.left {
                AssignTarget::Pat(AssignTargetPat::Array(array)) => Pat::Array(array.clone()),
                AssignTarget::Pat(AssignTargetPat::Object(object)) => Pat::Object(object.clone()),
                _ => return,
            };
            let right = std::mem::replace(&mut assignment.right, undefined());
            *expression = *self.expression(pattern, right);
        }
    }
    let mut lower = Lower {
        helpers: &helpers,
        active: false,
        used: false,
        hoisted: Vec::new(),
        unresolved,
    };
    program.visit_mut_with(&mut lower);
    let class_names = super::class_names::prepare(program, &helpers.key, &helpers.prototype);
    if lower.used || class_names.used() {
        super::generated::certify_helpers(&mut helper_program.body);
        match program {
            Program::Script(script) => {
                let at = script
                    .body
                    .iter()
                    .take_while(|s| mangler_jsast::directives::is_directive(s))
                    .count();
                script.body.splice(at..at, helper_program.body);
            }
            Program::Module(module) => {
                let at = module.body.iter().take_while(|s| matches!(s, ModuleItem::Stmt(stmt) if mangler_jsast::directives::is_directive(stmt))).count();
                module.body.splice(
                    at..at,
                    helper_program.body.into_iter().map(ModuleItem::Stmt),
                );
            }
        }
    }
    class_names
}

#[cfg(test)]
mod tests {
    use super::*;
    fn equivalent(source: &str, module: bool) {
        let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
            preset: Some(mangler_config::Intensity::Minify),
            seed: Some(42),
            virtualize: (!module).then(|| "pay".into()),
            require_virtualized: (!module).then(|| "pay".into()),
            virtualize_program: module,
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
        let evaluate = |code: &str| {
            let executable =
                std::env::var_os("MANGLER_TESTKIT_NODE").unwrap_or_else(|| "node".into());
            let (kind, capture) = if module {
                (
                    "--input-type=module",
                    "await new Promise(setImmediate);process.stdout.write(JSON.stringify(globalThis.__out));",
                )
            } else {
                (
                    "--input-type=commonjs",
                    "setImmediate(()=>process.stdout.write(JSON.stringify(globalThis.__out)));",
                )
            };
            let result = std::process::Command::new(executable)
                .args([kind, "-e"])
                .arg(format!("{code};{capture}"))
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            result.stdout
        };
        assert_eq!(evaluate(source), evaluate(&output), "{source}");
    }
    #[test]
    fn suspended_patterns_interleave_steps_defaults_and_close() {
        for source in [
            "let log=[];function* pay(){let it={[Symbol.iterator](){return this},next(){log.push('next');return{value:void 0,done:false}},return(){log.push('close');return{}}};const [a=(log.push('default'),yield 3),b=(log.push('second'),4)]=it;return[a,b]}let g=pay();let first=g.next();let second=g.next(7);globalThis.__out=[first,second,log];",
            "let log=[];async function pay(){let it={[Symbol.iterator](){return this},next(){log.push('next');return{done:false}},return(){log.push('close');throw Error('close')}};try{const [x=await Promise.reject(Error('original'))]=it}catch(e){return[e.message,log]}}pay().then(x=>globalThis.__out=x);",
            "let log=[];function* pay(){function it(name,value){return{[Symbol.iterator](){return this},next(){log.push(name+'next');return{done:false,value}},return(){log.push(name+'close');throw Error(name)}}}try{let [[x=yield 1]]=it('outer',it('inner',void 0))}catch(e){return[e.message,log]}}let g=pay();g.next();globalThis.__out=g.throw(Error('original'));",
            "function* pay(){let rhs=[void 0,2,3],x,rest;let same=([x=yield 1,...rest]=rhs)===rhs;return[same,x,rest]}let g=pay();g.next();globalThis.__out=g.next(7);",
            "let log=[];function* pay(){let rhs={get a(){log.push('a');return void 0},get b(){log.push('b');return 2}};const {a=yield 1,...rest}=rhs;return[a,rest,log]}let g=pay();g.next();globalThis.__out=g.next(7);",
            "function* pay(){try{let [a=a]=[void 0]}catch(e){yield e.name}try{let [a=b,b]=[void 0,2]}catch(e){yield e.name}let [a,b=yield a]=[3];return[a,b]}let g=pay();globalThis.__out=[g.next(),g.next(),g.next(),g.next(4)];",
            "let log=[];function* pay(){let it={[Symbol.iterator](){return this},next(){log.push('next');return{done:false,value:void 0}},return(){log.push('close');return{}}};for(const [x=yield 1] of [it])throw Error('body')}let g=pay();g.next();try{g.throw(Error('default'))}catch(e){log.push(e.message)}globalThis.__out=log;",
            "function* pay(a){let x;[x=(yield 1,arguments={v:7},arguments.v)]=[void 0];return[x,arguments.v,a]}let g=pay(3);g.next();globalThis.__out=g.next();",
        ] {
            equivalent(source, false);
        }
    }
    #[test]
    fn top_level_await_pattern_keeps_original_throw_before_close_error() {
        equivalent(
            "let log=[];const iterator={[Symbol.iterator](){return{next(){log.push('next');return{value:void 0,done:false}},return(){log.push('close');throw Error('close')}}}};let caught;try{const [value=await Promise.reject(Error('original'))]=iterator}catch(e){caught=e.message}globalThis.__out=[caught,log];export{};",
            true,
        );
    }
}
