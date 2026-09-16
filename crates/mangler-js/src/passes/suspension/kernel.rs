//! Native generator protocols over the shared synchronous state driver.
//!
//! Only suspension and completion events cross this seam; source instructions
//! remain in the bytecode callback. The engine owns request queues and brands.
use super::*;
use swc_core::common::{DUMMY_SP, SyntaxContext};

fn event(kind: u8, value: Option<Box<Expr>>) -> Box<Expr> {
    let mut elems = vec![Some(ExprOrSpread {
        spread: None,
        expr: Box::new(Expr::Lit(Lit::Num(Number {
            span: DUMMY_SP,
            value: kind as f64,
            raw: None,
        }))),
    })];
    elems.push(Some(ExprOrSpread {
        spread: None,
        expr: value.unwrap_or_else(|| {
            Box::new(Expr::Unary(UnaryExpr {
                span: DUMMY_SP,
                op: UnaryOp::Void,
                arg: Box::new(Expr::Lit(Lit::Num(Number {
                    span: DUMMY_SP,
                    value: 0.0,
                    raw: None,
                }))),
            }))
        }),
    }));
    Box::new(Expr::Array(ArrayLit {
        span: DUMMY_SP,
        elems,
    }))
}

/// Tag source yield/delegation/return before compatibility lowering. Await and
/// for-await still use the existing transform and are normalized afterwards.
#[derive(Default)]
pub(super) struct Sources(HashSet<(u32, SyntaxContext)>);
impl Sources {
    /// Native module declarations transplant their own kernel into the public
    /// declaration, so their state producer must remain private and unwrapped.
    pub(super) fn exclude(&mut self, span: u32) {
        self.0.retain(|(position, _)| *position != span);
    }
}

pub(super) fn prepare(program: &mut Program) -> Sources {
    struct Source(bool, Sources);
    impl VisitMut for Source {
        fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(e, self);
        }
        fn visit_mut_function(&mut self, function: &mut Function) {
            if function.is_generator && !function.is_async {
                self.1.0.insert((function.span.lo.0, function.ctxt));
            }
            let previous = std::mem::replace(&mut self.0, function.is_generator);
            function.visit_mut_children_with(self);
            self.0 = previous;
        }
        fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
            let previous = std::mem::replace(&mut self.0, false);
            arrow.visit_mut_children_with(self);
            self.0 = previous;
        }
        fn visit_mut_constructor(&mut self, constructor: &mut Constructor) {
            let previous = std::mem::replace(&mut self.0, false);
            constructor.visit_mut_children_with(self);
            self.0 = previous;
        }
        fn visit_mut_yield_expr(&mut self, yielded: &mut YieldExpr) {
            yielded.visit_mut_children_with(self);
            if self.0 {
                yielded.arg = Some(event(
                    if yielded.delegate { 2 } else { 1 },
                    yielded.arg.take(),
                ));
                yielded.delegate = false;
            }
        }
        fn visit_mut_stmt(&mut self, statement: &mut Stmt) {
            statement.visit_mut_children_with(self);
            if self.0
                && let Stmt::Return(returned) = statement
            {
                *statement = Stmt::Expr(ExprStmt {
                    span: returned.span,
                    expr: Box::new(Expr::Yield(YieldExpr {
                        span: returned.span,
                        delegate: false,
                        arg: Some(event(
                            if returned.arg.is_some() { 3 } else { 4 },
                            returned.arg.take(),
                        )),
                    })),
                });
            }
        }
    }
    let mut source = Source(false, Sources::default());
    program.visit_mut_with(&mut source);
    source.1
}

#[derive(Default)]
pub(super) struct Kernels {
    helpers: std::collections::BTreeMap<(bool, usize), Ident>,
    sources: Sources,
    sync: HashMap<(u32, SyntaxContext), Ident>,
}

#[derive(Default)]
struct Events {
    depth: usize,
    maximum: usize,
}
impl VisitMut for Events {
    fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(e, self);
    }
    fn visit_mut_function(&mut self, function: &mut Function) {
        if let Some(body) = &mut function.body
            && mangler_jsast::span::is_suspension_entry_span(body.span)
        {
            body.visit_mut_with(self);
        }
    }
    fn visit_mut_arrow_expr(&mut self, _: &mut ArrowExpr) {}
    fn visit_mut_try_stmt(&mut self, statement: &mut TryStmt) {
        let added = usize::from(statement.finalizer.is_some());
        self.depth += added;
        self.maximum = self.maximum.max(self.depth);
        statement.visit_mut_children_with(self);
        self.depth -= added;
    }
    fn visit_mut_yield_expr(&mut self, yielded: &mut YieldExpr) {
        yielded.visit_mut_children_with(self);
        if let Some(value) = &mut yielded.arg
            && let Expr::Call(call) = &mut **value
            && matches!(&call.callee, Callee::Expr(callee) if matches!(&**callee, Expr::Ident(id) if id.sym == *"_await_async_generator"))
            && call.args.len() == 1
        {
            *value = event(0, Some(call.args.remove(0).expr));
        }
    }
}
impl VisitMut for Kernels {
    fn visit_mut_function(&mut self, function: &mut Function) {
        function.visit_mut_children_with(self);
        let key = (function.span.lo.0, function.ctxt);
        if self.sources.0.contains(&key) {
            let body = function
                .body
                .as_mut()
                .expect("source generator body retained");
            let depth = prepare_events(body);
            let kernel = self.helper(false, depth);
            self.sync.insert(key, kernel);
        }
    }
    fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(e, self);
    }
    fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
        call.visit_mut_children_with(self);
        if !matches!(&call.callee, Callee::Expr(callee) if matches!(&**callee, Expr::Ident(id) if id.span.is_dummy() && id.sym == *"_wrap_async_generator"))
        {
            return;
        }
        let Some(argument) = call.args.first_mut() else {
            return;
        };
        let Expr::Fn(generator) = &mut *argument.expr else {
            return;
        };
        let Some(body) = &mut generator.function.body else {
            return;
        };
        let depth = prepare_events(body);
        let kernel = self.helper(true, depth);
        call.args.push(ExprOrSpread {
            spread: None,
            expr: Box::new(Expr::Ident(kernel.clone())),
        });
    }
}

/// Count finally regions after resource/for-await lowering, while all source
/// regions still belong to this function rather than loop helper generators.
pub(super) fn lower(program: &mut Program, sources: Sources) -> Kernels {
    let mut kernels = Kernels {
        sources,
        ..Kernels::default()
    };
    program.visit_mut_with(&mut kernels);
    kernels
}

/// Count source cleanup depth and normalize compatibility await events. Module
/// declarations use the same pass when their native kernel is transplanted.
pub(super) fn prepare_events(body: &mut FunctionBody) -> usize {
    let mut events = Events::default();
    body.visit_mut_with(&mut events);
    events.maximum
}

impl Kernels {
    fn helper(&mut self, is_async: bool, depth: usize) -> Ident {
        self.helpers
            .entry((is_async, depth))
            .or_insert_with(|| {
                Ident::new(
                    if is_async {
                        "_async_generator_kernel"
                    } else {
                        "_generator_kernel"
                    }
                    .into(),
                    DUMMY_SP,
                    SyntaxContext::empty().apply_mark(Mark::new()),
                )
            })
            .clone()
    }

    /// Source sync generators return the engine's iterator directly. Only the
    /// original function's driver is wrapped; generated loop/pattern callbacks
    /// and the synchronous drivers of ordinary async functions stay private.
    pub(super) fn wrap_sync(&self, program: &mut Program) {
        struct Wrap<'a>(&'a HashMap<(u32, SyntaxContext), Ident>);
        impl VisitMut for Wrap<'_> {
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(expression, self);
            }
            fn visit_mut_function(&mut self, function: &mut Function) {
                function.visit_mut_children_with(self);
                let Some(kernel) = self.0.get(&(function.span.lo.0, function.ctxt)) else {
                    return;
                };
                let Some(Stmt::Return(returned)) = function
                    .body
                    .as_mut()
                    .and_then(|body| body.stmts.last_mut())
                else {
                    panic!("lowered source generator returns its state driver");
                };
                let driver = returned.arg.take().expect("state driver expression");
                assert!(
                    matches!(&*driver, Expr::Call(call) if matches!(&call.callee, Callee::Expr(callee) if matches!(&**callee, Expr::Ident(id) if id.span.is_dummy() && id.sym == "_ts_generator"))),
                    "source generator owns a single state driver"
                );
                returned.arg = Some(Box::new(Expr::Call(CallExpr {
                    span: DUMMY_SP,
                    ctxt: SyntaxContext::empty(),
                    callee: Callee::Expr(Box::new(Expr::Ident(kernel.clone()))),
                    args: vec![ExprOrSpread {
                        spread: None,
                        expr: driver,
                    }],
                    type_args: None,
                })));
            }
        }
        program.visit_mut_with(&mut Wrap(&self.sync));
    }

    pub(super) fn install(self, program: &mut Program, unresolved: Mark) {
        use mangler_core::Language;
        use mangler_jsast::{Js, ParseOpts};
        let mut declarations = Vec::new();
        for ((is_async, depth), identity) in self.helpers {
            let source = source(depth, is_async);
            let mut helper = Js
                .parse(&source, &ParseOpts::default())
                .expect("native suspension kernel parses")
                .into_program();
            resolver(unresolved, Mark::new(), false).process(&mut helper);
            struct Clear;
            impl VisitMut for Clear {
                fn visit_mut_span(&mut self, span: &mut swc_core::common::Span) {
                    *span = DUMMY_SP;
                }
            }
            helper.visit_mut_with(&mut Clear);
            let Program::Script(mut script) = helper else {
                unreachable!()
            };
            let Stmt::Decl(Decl::Fn(function)) = &mut script.body[0] else {
                unreachable!()
            };
            function.ident = identity;
            generated::certify_helpers(&mut script.body);
            declarations.extend(script.body);
        }
        mangler_jsast::directives::insert_program_statements(program, declarations);
        // Compatibility helper flags were set before Await events were normalized.
        // Their implementations are now unreachable and must not survive as glue.
        fn obsolete(statement: &Stmt) -> bool {
            if matches!(statement, Stmt::Decl(Decl::Fn(f)) if f.function.span.is_dummy() && matches!(f.ident.sym.as_ref(), "_async_generator" | "_async_generator_delegate" | "_await_async_generator" | "_overload_yield"))
            {
                return true;
            }
            // The upstream helper includes prototype assignments beside its
            // declaration. Remove that entire generated implementation.
            let Stmt::Expr(expression) = statement else {
                return false;
            };
            let Expr::Assign(assignment) = &*expression.expr else {
                return false;
            };
            let AssignTarget::Simple(SimpleAssignTarget::Member(member)) = &assignment.left else {
                return false;
            };
            let mut base = &*member.obj;
            while let Expr::Member(member) = base {
                base = &member.obj;
            }
            matches!(base, Expr::Ident(id) if id.span.is_dummy() && id.sym == *"_async_generator")
        }
        match program {
            Program::Script(script) => script.body.retain(|s| !obsolete(s)),
            Program::Module(module) => module
                .body
                .retain(|s| !matches!(s, ModuleItem::Stmt(statement) if obsolete(statement))),
        }
    }
}

/// The same engine-owned protocol is used by callable envelopes and genuine
/// module declarations. Sync generators omit await; every source yield* remains
/// native, including its original IteratorResult identity and getter ordering.
pub(super) fn source(depth: usize, is_async: bool) -> String {
    let tokens = (0..=depth)
        .map(|level| format!("token{level}"))
        .collect::<Vec<_>>()
        .join(",");
    let prefix = if is_async { "async " } else { "" };
    format!(
        "{prefix}function* _kernel(driver){{'use strict';var {tokens},input,key='next',record=driver.next();{}}}",
        drive(0, depth, is_async)
    )
}

/// Each source finally region needs one native finalization loop. The final
/// completion-only layer cannot suspend; source control-flow depth proves that
/// bound, including finalies synthesized for resource and iterator cleanup.
fn drive(level: usize, depth: usize, is_async: bool) -> String {
    let mut completions = String::new();
    for index in 0..level {
        let jump = if index + 1 == level {
            format!("break drive{level}")
        } else {
            format!("continue drive{}", index + 1)
        };
        completions.push_str(&format!("if(record.value===token{index}){jump};"));
    }
    if level > depth {
        return format!(
            "drive{level}:{{if(!record.done)throw new TypeError('Invalid suspension completion');{completions}return;}}"
        );
    }
    let cancelled = if level == 0 {
        String::new()
    } else {
        format!(
            "if(!driver.hasReturn(token{}))continue drive{};",
            level - 1,
            level - 1
        )
    };
    let awaiting = if is_async {
        "case 0:input=await record.value[1];break;"
    } else {
        ""
    };
    format!(
        "drive{level}:while(true){{if(record.done){{{completions}return;}}{cancelled}let normal{level}=false;try{{switch(record.value[0]){{{awaiting}case 1:input=yield record.value[1];break;case 2:input=yield* record.value[1];break;case 3:return record.value[1];case 4:return;default:throw new TypeError('Invalid suspension event');}}key='next';normal{level}=true;}}catch(error){{input=error;key='throw';normal{level}=true;}}finally{{if(!normal{level}){{token{level}={{}};record=driver.internalReturn(token{level});{nested}}}}}record=driver[key](input);}}",
        nested = drive(level + 1, depth, is_async)
    )
}
