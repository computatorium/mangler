//! Suspension lowering for the bytecode VM.
//!
//! Async functions and generators become ordinary functions containing explicit
//! resumable state machines. The VM compiles those state callbacks recursively;
//! shared helpers implement synchronous iteration and native engine kernels own
//! asynchronous suspension, request queues, and iterator brands.
//! Keeping suspension outside the interpreter lets every resume use the same
//! exception-aware bytecode execution as an ordinary call.
use swc_core::common::{GLOBALS, Mark, comments::SingleThreadedComments};
use swc_core::ecma::ast::*;
use swc_core::ecma::transforms::{
    base::{
        fixer::fixer,
        helpers::{HELPERS, Helpers, inject_helpers},
        hygiene::Config as HygieneConfig,
        resolver,
    },
    compat::{es2015, es2017, es2018, es2020, es2021},
};
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

mod arguments;
mod arrow_lexicals;
mod block_functions;
mod class_frames;
mod class_names;
mod directives;
mod eval;
mod generated;
mod kernel;
mod lexical;
mod module_declarations;
mod objects;
mod patterns;
mod private_in;
mod super_delete;
mod templates;

use mangler_vm::SuspensionKind;
use std::collections::{HashMap, HashSet};

fn suspension_kind(is_async: bool, is_generator: bool) -> Option<SuspensionKind> {
    match (is_async, is_generator) {
        (true, true) => Some(SuspensionKind::AsyncGenerator),
        (true, false) => Some(SuspensionKind::Async),
        (false, true) => Some(SuspensionKind::Generator),
        _ => None,
    }
}

pub(crate) fn suspension_candidates(program: &Program) -> HashMap<u32, SuspensionKind> {
    #[derive(Default)]
    struct Scan(HashMap<u32, SuspensionKind>);
    impl Visit for Scan {
        fn visit_bin_expr(&mut self, expression: &BinExpr) {
            mangler_jsast::deep::walk_binary(expression, self);
        }
        fn visit_function(&mut self, f: &Function) {
            if let Some(kind) = suspension_kind(f.is_async, f.is_generator) {
                self.0.insert(f.span.lo.0, kind);
            }
            f.visit_children_with(self);
        }
        fn visit_arrow_expr(&mut self, a: &ArrowExpr) {
            if let Some(kind) = suspension_kind(a.is_async, a.is_generator) {
                self.0.insert(a.span.lo.0, kind);
            }
            a.visit_children_with(self);
        }
    }
    let mut scan = Scan::default();
    program.visit_with(&mut scan);
    scan.0
}

#[derive(Clone, Copy)]
pub(crate) struct NativeDeclaration {
    pub kind: SuspensionKind,
    pub finally_depth: usize,
}

pub(crate) fn generator_kernel(depth: usize, is_async: bool) -> String {
    kernel::source(depth, is_async)
}

pub(crate) struct Lowered {
    /// Module declarations restored around a compiled iterator-producing entry.
    pub native_declarations: HashMap<u32, NativeDeclaration>,
    pub helpers: Vec<Stmt>,
    pub internals: HashSet<String>,
    pub suspensions: HashMap<u32, SuspensionKind>,
    pub lexicals: mangler_vm::eval::SuspensionLexicalScopes,
    pub references: mangler_vm::eval::SuspensionLexicalReferences,
}

pub(crate) fn lower_with_lexicals(program: &mut Program, selected: &HashSet<u32>) -> Lowered {
    let mut kinds = suspension_candidates(program);
    let native: HashMap<_, _> = kinds
        .extract_if(|span, _| !selected.contains(span))
        .collect();
    if kinds.is_empty() {
        return Lowered {
            native_declarations: Default::default(),
            helpers: Vec::new(),
            internals: Default::default(),
            suspensions: kinds,
            lexicals: Default::default(),
            references: Default::default(),
        };
    }
    struct Mask<'a>(&'a HashMap<u32, SuspensionKind>);
    impl VisitMut for Mask<'_> {
        fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(expression, self);
        }
        fn visit_mut_function(&mut self, f: &mut Function) {
            if self.0.contains_key(&f.span.lo.0) {
                f.is_async = false;
                f.is_generator = false;
            }
            f.visit_mut_children_with(self);
        }
        fn visit_mut_arrow_expr(&mut self, a: &mut ArrowExpr) {
            if self.0.contains_key(&a.span.lo.0) {
                a.is_async = false;
                a.is_generator = false;
            }
            a.visit_mut_children_with(self);
        }
    }
    program.visit_mut_with(&mut Mask(&native));
    let environments = if GLOBALS.is_set() {
        lower_inner(program)
    } else {
        GLOBALS.set(&Default::default(), || lower_inner(program))
    };
    struct Restore<'a> {
        native: &'a HashMap<u32, SuspensionKind>,
        lowered: &'a HashMap<u32, SuspensionKind>,
    }
    impl VisitMut for Restore<'_> {
        fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(expression, self);
        }
        fn visit_mut_function(&mut self, f: &mut Function) {
            if let Some(kind) = self.native.get(&f.span.lo.0) {
                f.is_async = matches!(kind, SuspensionKind::Async | SuspensionKind::AsyncGenerator);
                f.is_generator = matches!(
                    kind,
                    SuspensionKind::Generator | SuspensionKind::AsyncGenerator
                );
            }
            if self.lowered.contains_key(&f.span.lo.0)
                && let Some(body) = &mut f.body
            {
                body.span = f.span;
            }
            f.visit_mut_children_with(self);
        }
        fn visit_mut_arrow_expr(&mut self, a: &mut ArrowExpr) {
            if let Some(kind) = self.native.get(&a.span.lo.0) {
                a.is_async = matches!(kind, SuspensionKind::Async | SuspensionKind::AsyncGenerator);
                a.is_generator = matches!(
                    kind,
                    SuspensionKind::Generator | SuspensionKind::AsyncGenerator
                );
            }
            if self.lowered.contains_key(&a.span.lo.0) {
                let span = a.span;
                arrow_block(a).span = span;
            }
            a.visit_mut_children_with(self);
        }
    }
    program.visit_mut_with(&mut Restore {
        native: &native,
        lowered: &kinds,
    });
    Lowered {
        native_declarations: environments.native_declarations,
        helpers: generated::take_helpers(program),
        internals: environments.internals,
        suspensions: kinds,
        lexicals: environments.lexicals,
        references: environments.references,
    }
}

struct Environments {
    native_declarations: HashMap<u32, NativeDeclaration>,
    internals: HashSet<String>,
    lexicals: mangler_vm::eval::SuspensionLexicalScopes,
    references: mangler_vm::eval::SuspensionLexicalReferences,
}
fn lower_inner(program: &mut Program) -> Environments {
    lower_inner_mode(program, false)
}

/// A top-level await run uses the module's own native await driver. Its VM
/// entry returns a synchronous iterator, avoiding an extra async-function
/// Promise adoption at module evaluation boundaries.
pub(crate) struct TopLevelLowered {
    pub internals: HashSet<String>,
    pub lexicals: mangler_vm::eval::SuspensionLexicalScopes,
    pub references: mangler_vm::eval::SuspensionLexicalReferences,
    pub function: Function,
    pub helpers: Vec<Stmt>,
    pub suspensions: HashMap<u32, SuspensionKind>,
}

pub(crate) fn lower_top_level(body: FunctionBody) -> TopLevelLowered {
    let mut program = Program::Script(Script {
        body: vec![Stmt::Expr(ExprStmt {
            span: swc_core::common::DUMMY_SP,
            expr: Box::new(Expr::Fn(FnExpr {
                ident: None,
                function: Box::new(Function {
                    body: Some(body),
                    is_async: true,
                    ..Default::default()
                }),
            })),
        })],
        ..Default::default()
    });
    let mut suspensions = suspension_candidates(&program);
    suspensions.remove(&0);
    let environments = if GLOBALS.is_set() {
        lower_inner_mode(&mut program, true)
    } else {
        GLOBALS.set(&Default::default(), || lower_inner_mode(&mut program, true))
    };
    struct Stamp<'a>(&'a HashMap<u32, SuspensionKind>);
    impl VisitMut for Stamp<'_> {
        fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(expression, self);
        }
        fn visit_mut_function(&mut self, function: &mut Function) {
            if self.0.contains_key(&function.span.lo.0)
                && let Some(body) = &mut function.body
            {
                body.span = function.span;
            }
            function.visit_mut_children_with(self);
        }
        fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
            if self.0.contains_key(&arrow.span.lo.0) {
                let span = arrow.span;
                arrow_block(arrow).span = span;
            }
            arrow.visit_mut_children_with(self);
        }
    }
    program.visit_mut_with(&mut Stamp(&suspensions));
    let Program::Script(script) = program else {
        unreachable!()
    };
    let mut function = None;
    let mut helpers = Vec::new();
    fn entry_expression(mut expression: &Expr) -> bool {
        while let Expr::Paren(paren) = expression {
            expression = &paren.expr;
        }
        matches!(expression, Expr::Fn(_))
    }
    for statement in script.body {
        if let Stmt::Expr(ExprStmt { expr, .. }) = &statement
            && entry_expression(expr)
        {
            let Stmt::Expr(ExprStmt { mut expr, .. }) = statement else {
                unreachable!()
            };
            while let Expr::Paren(paren) = *expr {
                expr = paren.expr;
            }
            let Expr::Fn(value) = *expr else {
                unreachable!()
            };
            function = Some(*value.function);
        } else {
            helpers.push(statement);
        }
    }
    TopLevelLowered {
        internals: environments.internals,
        lexicals: environments.lexicals,
        references: environments.references,
        function: function.expect("top-level generator entry"),
        helpers,
        suspensions,
    }
}

/// Module-level arguments has no synthetic function activation. Hide its
/// spelling from compatibility hoisters, then restore its original reference.
struct ModuleArguments(HashMap<Id, Ident>);
impl ModuleArguments {
    fn shield(program: &mut Program, unresolved: Mark) -> Self {
        struct Shield {
            unresolved: Mark,
            originals: HashMap<Id, Ident>,
            replacements: HashMap<Id, Ident>,
        }
        impl VisitMut for Shield {
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(expression, self);
            }
            fn visit_mut_function(&mut self, _: &mut Function) {}
            fn visit_mut_constructor(&mut self, _: &mut Constructor) {}
            fn visit_mut_getter_prop(&mut self, getter: &mut GetterProp) {
                getter.key.visit_mut_with(self);
            }
            fn visit_mut_setter_prop(&mut self, setter: &mut SetterProp) {
                setter.key.visit_mut_with(self);
            }
            fn visit_mut_prop(&mut self, property: &mut Prop) {
                if let Prop::Shorthand(name) = property
                    && name.sym == *"arguments"
                    && name.ctxt.has_mark(self.unresolved)
                {
                    let key = PropName::Ident(IdentName::new(name.sym.clone(), name.span));
                    let mut value = name.clone();
                    self.visit_mut_ident(&mut value);
                    *property = Prop::KeyValue(KeyValueProp {
                        key,
                        value: Box::new(Expr::Ident(value)),
                    });
                } else {
                    property.visit_mut_children_with(self);
                }
            }
            fn visit_mut_ident(&mut self, name: &mut Ident) {
                if name.sym != *"arguments" || !name.ctxt.has_mark(self.unresolved) {
                    return;
                }
                let replacement = self.replacements.entry(name.to_id()).or_insert_with(|| {
                    let replacement = Ident::new(
                        "_module_arguments".into(),
                        name.span,
                        swc_core::common::SyntaxContext::empty().apply_mark(Mark::new()),
                    );
                    self.originals.insert(replacement.to_id(), name.clone());
                    replacement
                });
                name.sym = replacement.sym.clone();
                name.ctxt = replacement.ctxt;
            }
        }
        let mut shield = Shield {
            unresolved,
            originals: HashMap::new(),
            replacements: HashMap::new(),
        };
        let Program::Script(script) = program else {
            unreachable!("top-level suspension carrier is a script");
        };
        let Stmt::Expr(expression) = &mut script.body[0] else {
            unreachable!("top-level suspension carrier expression");
        };
        let Expr::Fn(function) = &mut *expression.expr else {
            unreachable!("top-level suspension carrier function");
        };
        function.function.body.visit_mut_with(&mut shield);
        Self(shield.originals)
    }
    fn restore(self, program: &mut Program) {
        struct Restore(HashMap<Id, Ident>);
        impl VisitMut for Restore {
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(expression, self);
            }
            fn visit_mut_ident(&mut self, name: &mut Ident) {
                if let Some(original) = self.0.get(&name.to_id()) {
                    name.sym = original.sym.clone();
                    name.ctxt = original.ctxt;
                }
            }
        }
        program.visit_mut_with(&mut Restore(self.0));
    }
}

fn lower_inner_mode(program: &mut Program, top_level: bool) -> Environments {
    let unresolved = Mark::new();
    let top_mark = Mark::new();
    resolver(unresolved, top_mark, false).process(program);
    mangler_jsast::Js::repair_resolver_scopes(program);
    let switch_declarations = mangler_jsast::SwitchSuspensionDeclarations::capture(program);
    let existing_bindings = generated::existing_bindings(program);
    block_functions::lower(program);
    let mut declarations = module_declarations::Declarations::collect(program);
    let module_arguments = top_level.then(|| ModuleArguments::shield(program, unresolved));
    let mut lexical_scopes = Vec::new();
    let mut reference_plans = HashMap::<u32, String>::new();
    HELPERS.set(&Helpers::new(false), || {
        let mut arrow_lexicals = ArrowLexicals { non_constructible: false, unresolved, restore: false, bindings: HashMap::new() };
        program.visit_mut_with(&mut arrow_lexicals);
        let super_deletions = super_delete::prepare(program);
        let mut arguments_plan=arguments::prepare(program,unresolved);
        arguments_plan.native_generators(declarations.0.iter().filter_map(|(span, declaration)| (declaration.kind != SuspensionKind::Async).then_some(*span)));
        let mut kernel_sources = kernel::prepare(program);
        for span in declarations.0.keys() { kernel_sources.exclude(*span); }
        let async_strictness = directives::AsyncStrictness::capture(program);
        es2017::async_to_generator(Default::default(), unresolved).process(program);
        async_strictness.restore(program);
        declarations.lower(program);
        let (argument_aliases,argument_references,mut generator_arguments)=arguments_plan.restore_with_references(program);
        if top_level {
            extract_top_level_generator(program);
        }
        arrow_lexicals.restore = true;
        program.visit_mut_with(&mut arrow_lexicals);
        lexical_scopes = lexical::lower_with_references(program, unresolved, argument_aliases, &mut reference_plans, argument_references, &switch_declarations);
        eval::split(program, &lexical_scopes);
        let class_names = patterns::lower(program, unresolved);
        program.visit_mut_with(&mut IteratorResults);
        let kernels = kernel::lower(program, kernel_sources);
        declarations.prepare_generators(program);
        let mut prerequisites = GeneratorPrerequisites {
            unresolved,
            hoisted: Vec::new(),
            templates: templates::Sites::default(),
        };
        program.visit_mut_with(&mut prerequisites);
        class_names.restore_prototypes(program);
        match program {
            Program::Script(script) => {
                let at = script.body.iter().take_while(|s| s.is_expr() && matches!(s.as_expr().unwrap().expr.as_ref(), Expr::Lit(Lit::Str(_)))).count();
                script.body.splice(at..at, prerequisites.hoisted);
            }
            Program::Module(module) => {
                let at = module.body.iter().take_while(|s| matches!(s, ModuleItem::Stmt(Stmt::Expr(e)) if matches!(e.expr.as_ref(), Expr::Lit(Lit::Str(_))))).count();
                module.body.splice(at..at, prerequisites.hoisted.into_iter().map(ModuleItem::Stmt));
            }
        }
        let mut super_bindings = GeneratorSuper { unresolved, restore: false, bindings: HashMap::new() };
        program.visit_mut_with(&mut super_bindings);
        generator_arguments.shield(program);
        let directives = directives::GeneratorDirectives::take(program);
        let class_frames = class_frames::ClassFrames::take(program);
        let private_in = private_in::PrivateIn::take(program);
        es2015::generator::generator(unresolved, SingleThreadedComments::default()).process(program);
        private_in.restore(program);
        class_frames.restore(program);
        directives.restore(program);
        class_names.restore(program);
        generator_arguments.restore(program);
        prerequisites.templates.restore(program);
        kernels.wrap_sync(program);
        struct StateEntries;
        impl VisitMut for StateEntries {
            fn visit_mut_function(&mut self, function: &mut Function) {
                function.visit_mut_children_with(self);
                if mangler_jsast::span::is_suspension_entry_span(function.span)
                    && let Some(body) = &mut function.body
                {
                    body.span = mangler_jsast::span::suspension_entry_span();
                }
            }
            fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(expression, self);
            }
            fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
                call.visit_mut_children_with(self);
                if call.span.is_dummy()
                    && matches!(&call.callee, Callee::Expr(callee) if matches!(&**callee, Expr::Ident(id) if id.sym == *"_ts_generator"))
                    && let Some(argument) = call.args.get_mut(1)
                    && let Expr::Fn(callback) = &mut *argument.expr
                    && let Some(body) = &mut callback.function.body
                {
                    body.span = mangler_jsast::span::suspension_entry_span();
                }
            }
        }
        program.visit_mut_with(&mut StateEntries);
        super_bindings.restore = true;
        program.visit_mut_with(&mut super_bindings);
        super_deletions.restore(program);
        preserve_call_identity(program, unresolved, &lexical_scopes);
        let before_injection = generated::existing_bindings(program);
        inject_helpers(unresolved).process(program);
        let injected: HashSet<Id> = generated::existing_bindings(program).difference(&before_injection).cloned().collect();
        kernels.install(program, unresolved);
        install_iterator_protocol(program, unresolved);
        fn certify_injected(statement: &mut Stmt, injected: &HashSet<Id>) {
            if matches!(statement, Stmt::Decl(Decl::Fn(function)) if injected.contains(&function.ident.to_id())) {
                generated::certify_helpers(std::slice::from_mut(statement));
            }
        }
        match program {
            Program::Script(script) => for statement in &mut script.body { certify_injected(statement, &injected); },
            Program::Module(module) => for item in &mut module.body { if let ModuleItem::Stmt(statement) = item { certify_injected(statement, &injected); } },
        }
    });
    if let Some(module_arguments) = module_arguments {
        module_arguments.restore(program);
    }
    switch_declarations.protect(program);
    let internals = generated::isolate(program, &existing_bindings);
    switch_declarations.hygiene(
        program,
        HygieneConfig {
            keep_class_names: true,
            top_level_mark: top_mark,
            ..Default::default()
        },
    );
    switch_declarations.restore(program);
    let lexicals = lexical::take_eval_references(program, &lexical_scopes);
    let references = lexical::take_lexical_references(program, &reference_plans);
    fixer(None).process(program);
    Environments {
        native_declarations: declarations.0,
        internals,
        lexicals,
        references,
    }
}

fn extract_top_level_generator(program: &mut Program) {
    let Program::Script(script) = program else {
        unreachable!()
    };
    let function = script
        .body
        .iter_mut()
        .find_map(|statement| match statement {
            Stmt::Expr(ExprStmt { expr, .. }) => match expr.as_mut() {
                Expr::Fn(function) => Some(&mut function.function),
                _ => None,
            },
            _ => None,
        })
        .expect("synthetic top-level function");
    struct Extract(Option<Function>);
    impl VisitMut for Extract {
        fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
            if self.0.is_some() {
                return;
            }
            if matches!(&call.callee, Callee::Expr(callee) if matches!(callee.as_ref(), Expr::Ident(id) if id.sym == "_async_to_generator"))
                && let Some(argument) = call.args.first_mut()
                && let Expr::Fn(generator) = argument.expr.as_mut()
                && generator.function.is_generator
            {
                self.0 = Some(std::mem::take(&mut *generator.function));
                return;
            }
            call.visit_mut_children_with(self);
        }
    }
    let mut extract = Extract(None);
    function.visit_mut_children_with(&mut extract);
    **function = extract.0.expect("lowered async entry contains generator");
}

fn arrow_block(arrow: &mut ArrowExpr) -> &mut FunctionBody {
    if matches!(&*arrow.body, ArrowFunctionBody::Expr(_)) {
        let previous = std::mem::replace(
            &mut *arrow.body,
            ArrowFunctionBody::FunctionBody(FunctionBody::default()),
        );
        let ArrowFunctionBody::Expr(expression) = previous else {
            unreachable!()
        };
        *arrow.body = ArrowFunctionBody::FunctionBody(FunctionBody {
            span: arrow.span,
            stmts: vec![Stmt::Return(ReturnStmt {
                span: swc_core::common::DUMMY_SP,
                arg: Some(expression),
            })],
            ..Default::default()
        });
    }
    let ArrowFunctionBody::FunctionBody(body) = &mut *arrow.body else {
        unreachable!()
    };
    body
}

/// Async arrows retain their lexical this, new.target and super constructor. Capture
/// lexical capabilities in the original arrow, outside its generated state body.
struct ArrowLexicals {
    non_constructible: bool,
    unresolved: Mark,
    restore: bool,
    bindings: HashMap<u32, Vec<Stmt>>,
}
impl VisitMut for ArrowLexicals {
    fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(expression, self);
    }
    fn visit_mut_function(&mut self, function: &mut Function) {
        let previous = self.non_constructible;
        // Ordinary async/generator functions cannot be constructed; arrows
        // inherit their always-undefined new.target, even after lowering creates
        // ordinary generator functions around their bodies.
        self.non_constructible = function.is_async || function.is_generator;
        function.visit_mut_children_with(self);
        self.non_constructible = previous;
    }
    fn visit_mut_constructor(&mut self, constructor: &mut Constructor) {
        let previous = std::mem::replace(&mut self.non_constructible, false);
        constructor.visit_mut_children_with(self);
        self.non_constructible = previous;
    }
    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        if !self.restore
            && self.non_constructible
            && matches!(expression, Expr::MetaProp(meta) if meta.kind == MetaPropKind::NewTarget)
        {
            *expression =
                mangler_jsast::build::unary(UnaryOp::Void, mangler_jsast::build::num(0.0));
        } else {
            expression.visit_mut_children_with(self);
        }
    }
    fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
        if self.restore {
            if let Some(bindings) = self.bindings.remove(&arrow.span.lo.0) {
                arrow_lexicals::defer_generated_receivers(&mut arrow.body);
                let body = arrow_block(arrow);
                let at = mangler_jsast::directives::leading_directive_count(&body.stmts);
                body.stmts.splice(at..at, bindings);
            }
        } else if arrow.is_async && !self.non_constructible {
            // Ordinary async/generator owners already have initialized `this`.
            // Their generated function boundary must not receive a class-only
            // lazy receiver marker; the normal async transform owns that capture.
            // The outermost async arrow captures for its nested lexical arrows
            // too. Postorder capture would restore a fresh new.target reference
            // inside a generated generator's environment.
            if !self.non_constructible {
                let mut hoister = swc_core::ecma::utils::function::FnEnvHoister::new(
                    swc_core::common::SyntaxContext::empty().apply_mark(self.unresolved),
                );
                hoister.disable_arguments();
                hoister.disable_this();
                hoister.disable_super();
                arrow.body.visit_mut_with(&mut hoister);
                if let Some(binding) = hoister.to_stmt() {
                    self.bindings
                        .entry(arrow.span.lo.0)
                        .or_default()
                        .push(binding);
                }
            }
            self.bindings
                .entry(arrow.span.lo.0)
                .or_default()
                .extend(arrow_lexicals::capture(&mut arrow.body));
        }
        arrow.visit_mut_children_with(self);
    }
}

/// Validate iterator results in the existing awaited continuation. Next checks
/// precede reading done; close checks only require an object and retain the
/// surrounding finally region's precedence for a pending body throw.
struct IteratorResults;
impl IteratorResults {
    fn awaited_iterator<'a>(expression: &'a Expr, method: &str) -> Option<&'a Ident> {
        let Expr::Yield(yielded) = expression else {
            return None;
        };
        let mut value = yielded.arg.as_deref()?;
        if let Expr::Call(call) = value
            && let Callee::Expr(callee) = &call.callee
            && matches!(callee.as_ref(), Expr::Ident(id) if id.sym == "_await_async_generator")
            && let Some(argument) = call.args.first()
        {
            value = &argument.expr;
        }
        let Expr::Call(call) = value else { return None };
        let Callee::Expr(callee) = &call.callee else {
            return None;
        };
        let Expr::Member(member) = callee.as_ref() else {
            return None;
        };
        if !matches!(&member.prop, MemberProp::Ident(id) if id.sym == method) {
            return None;
        }
        let Expr::Ident(iterator) = member.obj.as_ref() else {
            return None;
        };
        (iterator.sym == "_iterator" && iterator.span.is_dummy()).then_some(iterator)
    }

    fn validate(expression: &mut Expr, iterator: Ident) {
        let callee = Expr::Member(MemberExpr {
            span: swc_core::common::DUMMY_SP,
            obj: Box::new(Expr::Ident(iterator)),
            prop: MemberProp::Ident(IdentName::new(
                "validate".into(),
                swc_core::common::DUMMY_SP,
            )),
        });
        let value = std::mem::replace(
            expression,
            Expr::Invalid(Invalid {
                span: swc_core::common::DUMMY_SP,
            }),
        );
        *expression = Expr::Call(CallExpr {
            callee: Callee::Expr(Box::new(callee)),
            args: vec![ExprOrSpread {
                spread: None,
                expr: Box::new(value),
            }],
            ..Default::default()
        });
    }
}
impl VisitMut for IteratorResults {
    fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(expression, self);
    }
    fn visit_mut_assign_expr(&mut self, assignment: &mut AssignExpr) {
        assignment.visit_mut_children_with(self);
        if assignment
            .left
            .as_ident()
            .is_some_and(|step| step.id.sym == "_step" && step.id.span.is_dummy())
            && let Some(iterator) = Self::awaited_iterator(&assignment.right, "next").cloned()
        {
            Self::validate(&mut assignment.right, iterator);
        }
    }
    fn visit_mut_expr(&mut self, expression: &mut Expr) {
        expression.visit_mut_children_with(self);
        if let Some(iterator) = Self::awaited_iterator(expression, "return").cloned() {
            Self::validate(expression, iterator);
        }
    }
}

/// Keep lexical super references in the original method environment; the
/// generated state callback is an ordinary function and has no home object.
struct GeneratorSuper {
    unresolved: Mark,
    restore: bool,
    bindings: HashMap<swc_core::common::SyntaxContext, Stmt>,
}
impl VisitMut for GeneratorSuper {
    fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(expression, self);
    }
    fn visit_mut_function(&mut self, function: &mut Function) {
        function.visit_mut_children_with(self);
        if self.restore {
            if let Some(binding) = self.bindings.remove(&function.ctxt)
                && let Some(body) = &mut function.body
            {
                let at = body.stmts.iter().take_while(|s| matches!(s, Stmt::Expr(e) if matches!(e.expr.as_ref(), Expr::Lit(Lit::Str(_))))).count();
                body.stmts.insert(at, binding);
            }
        } else if function.is_generator
            && let Some(body) = &mut function.body
        {
            let mut hoister = swc_core::ecma::utils::function::FnEnvHoister::new(
                swc_core::common::SyntaxContext::empty().apply_mark(self.unresolved),
            );
            hoister.disable_arguments();
            hoister.disable_this();
            body.visit_mut_with(&mut hoister);
            if let Some(binding) = hoister.to_stmt() {
                self.bindings.insert(function.ctxt, binding);
            }
        }
    }
}

/// The state builder caches a suspended call and emits `target.apply(...)`.
/// That must remain a Call of the original target, even when application code
/// gives the callable its own `apply` property. Source-written member calls keep
/// their original spans and continue to observe those properties.
fn preserve_call_identity(program: &mut Program, unresolved: Mark, evals: &lexical::RawScopes) {
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};
    let mut helper=Js.parse("function _suspension_apply(target,receiver,args){return Reflect.apply(target,receiver,args)}",&ParseOpts::default()).expect("suspension call protocol parses").into_program();
    resolver(unresolved, Mark::new(), false).process(&mut helper);
    let Program::Script(script) = &helper else {
        unreachable!()
    };
    let Stmt::Decl(Decl::Fn(decl)) = &script.body[0] else {
        unreachable!()
    };
    struct Calls {
        helper: Ident,
        evals: HashSet<u32>,
        used: bool,
    }
    impl VisitMut for Calls {
        fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(e, self);
        }
        fn visit_mut_call_expr(&mut self, c: &mut CallExpr) {
            c.visit_mut_children_with(self);
            if self.evals.contains(&c.span.lo.0)
                || c.args.len() != 2
                || c.args.iter().any(|a| a.spread.is_some())
            {
                return;
            }
            let Callee::Expr(callee) = &mut c.callee else {
                return;
            };
            let Expr::Member(member) = callee.as_mut() else {
                return;
            };
            if !member.span.is_dummy()
                || !matches!(&member.prop,MemberProp::Ident(id) if id.sym=="apply")
            {
                return;
            }
            let target = std::mem::replace(
                &mut member.obj,
                Box::new(Expr::Invalid(Invalid {
                    span: swc_core::common::DUMMY_SP,
                })),
            );
            c.callee = Callee::Expr(Box::new(Expr::Ident(self.helper.clone())));
            c.args.insert(
                0,
                ExprOrSpread {
                    spread: None,
                    expr: target,
                },
            );
            self.used = true;
        }
    }
    let mut calls = Calls {
        helper: decl.ident.clone(),
        evals: evals.iter().map(|(span, _)| *span).collect(),
        used: false,
    };
    program.visit_mut_with(&mut calls);
    if calls.used {
        struct Clear;
        impl VisitMut for Clear {
            fn visit_mut_span(&mut self, s: &mut swc_core::common::Span) {
                *s = swc_core::common::DUMMY_SP;
            }
        }
        helper.visit_mut_with(&mut Clear);
        let Program::Script(mut script) = helper else {
            unreachable!()
        };
        generated::certify_helpers(&mut script.body);
        mangler_jsast::directives::insert_program_statements(program, script.body);
    }
}

/// The upstream state-machine builder is reusable independently of its runtime.
/// Its TypeScript helper skips iterator-result validation and re-reads `next`
/// during `yield*`; use a checked protocol adapter for the same state format.
fn install_iterator_protocol(program: &mut Program, unresolved: Mark) {
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};
    let source = concat!(
        include_str!("suspension/generator.js"),
        "\n",
        include_str!("suspension/iterators.js"),
        "\nfunction _wrap_async_generator(fn,kernel){return function(){return kernel(Reflect.apply(fn,this,arguments));};}"
    );
    let mut helper = Js
        .parse(source, &ParseOpts::default())
        .expect("checked generator runtime source")
        .into_program();
    resolver(unresolved, Mark::new(), false).process(&mut helper);
    // Resolve any helper-to-helper reference to its injected identity before
    // hygiene, preserving protocol linkage under source name shadowing.
    #[derive(Default)]
    struct HelperIds(HashMap<String, swc_core::common::SyntaxContext>);
    impl Visit for HelperIds {
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            mangler_jsast::deep::walk_binary(e, self);
        }
        fn visit_fn_decl(&mut self, declaration: &FnDecl) {
            if declaration.function.span.is_dummy() {
                self.0
                    .insert(declaration.ident.sym.to_string(), declaration.ident.ctxt);
            }
        }
    }
    let mut helper_ids = HelperIds::default();
    program.visit_with(&mut helper_ids);
    struct LinkHelpers {
        ids: HashMap<String, swc_core::common::SyntaxContext>,
        unresolved: swc_core::common::SyntaxContext,
    }
    impl VisitMut for LinkHelpers {
        fn visit_mut_ident(&mut self, ident: &mut Ident) {
            if ident.ctxt == self.unresolved
                && let Some(context) = self.ids.get(ident.sym.as_ref())
            {
                ident.ctxt = *context;
            }
        }
    }
    helper.visit_mut_with(&mut LinkHelpers {
        ids: helper_ids.0,
        unresolved: swc_core::common::SyntaxContext::empty().apply_mark(unresolved),
    });
    struct GeneratedSpans;
    impl VisitMut for GeneratedSpans {
        fn visit_mut_span(&mut self, span: &mut swc_core::common::Span) {
            *span = swc_core::common::DUMMY_SP;
        }
    }
    helper.visit_mut_with(&mut GeneratedSpans);
    let Program::Script(script) = helper else {
        unreachable!()
    };
    let replacements = script
        .body
        .into_iter()
        .filter_map(|statement| match statement {
            Stmt::Decl(Decl::Fn(function)) => {
                Some((function.ident.sym.to_string(), function.function))
            }
            _ => None,
        })
        .collect();
    struct Install(std::collections::HashMap<String, Box<Function>>);
    impl VisitMut for Install {
        fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(e, self);
        }
        fn visit_mut_fn_decl(&mut self, declaration: &mut FnDecl) {
            if declaration.function.span.is_dummy()
                && let Some(replacement) = self.0.get(declaration.ident.sym.as_ref())
            {
                declaration.function = replacement.clone();
            }
            if declaration.function.span.is_dummy()
                && matches!(
                    declaration.ident.sym.as_ref(),
                    "_async_to_generator" | "_wrap_async_generator"
                )
                && let Some(body) = &mut declaration.function.body
            {
                // Source entries already apply their own strict/sloppy receiver
                // semantics. Protocol forwarding must preserve that exact value.
                for statement in &mut body.stmts {
                    if let Stmt::Return(ReturnStmt {
                        arg: Some(value), ..
                    }) = statement
                    {
                        let mut value = &mut **value;
                        while let Expr::Paren(paren) = value {
                            value = &mut paren.expr;
                        }
                        if let Expr::Fn(function) = value
                            && let Some(body) = &mut function.function.body
                        {
                            body.stmts.insert(
                                0,
                                Stmt::Expr(ExprStmt {
                                    span: swc_core::common::DUMMY_SP,
                                    expr: Box::new(mangler_jsast::build::str_lit("use strict")),
                                }),
                            );
                        }
                    }
                }
            }
        }
    }
    program.visit_mut_with(&mut Install(replacements));
    install_promise_intrinsics(program, unresolved);
}

/// Await uses realm intrinsics, independent of application replacements of
/// global Promise, Promise.resolve, or Promise.prototype.then. Keep a tiny native
/// await reaction bridge; only protocol callbacks enter it, never source bodies.
fn install_promise_intrinsics(program: &mut Program, unresolved: Mark) {
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};
    fn promise_helper(name: &str) -> bool {
        matches!(
            name,
            "asyncGeneratorStep" | "_async_to_generator" | "_async_iterator"
        )
    }
    #[derive(Default)]
    struct Needed(bool);
    impl Visit for Needed {
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            mangler_jsast::deep::walk_binary(e, self);
        }
        fn visit_fn_decl(&mut self, f: &FnDecl) {
            self.0 |= f.function.span.is_dummy() && promise_helper(f.ident.sym.as_ref());
        }
    }
    let mut needed = Needed::default();
    program.visit_with(&mut needed);
    if !needed.0 {
        return;
    }
    let source = "var _vm_promise=(async function(){})().constructor,_vm_resolve=_vm_promise.resolve.bind(_vm_promise),_vm_reject=_vm_promise.reject.bind(_vm_promise);async function _vm_then(p,ok,no){let value;try{value=await p}catch(error){if(no)return no(error);throw error}return ok?ok(value):value}";
    let mut support = Js
        .parse(source, &ParseOpts::default())
        .expect("native Promise protocol parses")
        .into_program();
    resolver(unresolved, Mark::new(), false).process(&mut support);
    #[derive(Default)]
    struct Ids(HashMap<String, Ident>);
    impl Visit for Ids {
        fn visit_binding_ident(&mut self, id: &BindingIdent) {
            self.0.insert(id.id.sym.to_string(), id.id.clone());
        }
        fn visit_fn_decl(&mut self, f: &FnDecl) {
            self.0.insert(f.ident.sym.to_string(), f.ident.clone());
        }
    }
    let mut ids = Ids::default();
    support.visit_with(&mut ids);
    struct Rewrite<'a> {
        ids: &'a HashMap<String, Ident>,
        inside: bool,
    }
    impl VisitMut for Rewrite<'_> {
        fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(e, self);
        }
        fn visit_mut_fn_decl(&mut self, f: &mut FnDecl) {
            let previous = self.inside;
            self.inside =
                previous || (f.function.span.is_dummy() && promise_helper(f.ident.sym.as_ref()));
            f.visit_mut_children_with(self);
            self.inside = previous;
        }
        fn visit_mut_expr(&mut self, e: &mut Expr) {
            if !self.inside {
                e.visit_mut_children_with(self);
                return;
            }
            if let Expr::Call(call) = e
                && let Callee::Expr(callee) = &mut call.callee
                && let Expr::Member(member) = callee.as_mut()
                && matches!(&member.prop, MemberProp::Ident(id) if id.sym == "then")
            {
                let mut receiver = std::mem::replace(
                    &mut member.obj,
                    Box::new(Expr::Invalid(Invalid {
                        span: swc_core::common::DUMMY_SP,
                    })),
                );
                // Native await performs PromiseResolve itself. Keeping the
                // helper's explicit resolve observes constructor getters twice.
                if let Expr::Call(resolve) = receiver.as_mut()
                    && matches!(&resolve.callee,Callee::Expr(callee) if matches!(callee.as_ref(),Expr::Member(m) if matches!(&*m.obj,Expr::Ident(id) if id.sym=="Promise") && matches!(&m.prop,MemberProp::Ident(id) if id.sym=="resolve")))
                    && resolve.args.len() == 1
                    && resolve.args[0].spread.is_none()
                {
                    receiver = resolve.args.remove(0).expr;
                }
                call.callee = Callee::Expr(Box::new(Expr::Ident(self.ids["_vm_then"].clone())));
                call.args.insert(
                    0,
                    ExprOrSpread {
                        spread: None,
                        expr: receiver,
                    },
                );
            }
            if let Expr::Member(member) = e
                && matches!(&*member.obj, Expr::Ident(id) if id.sym == "Promise")
                && let MemberProp::Ident(property) = &member.prop
            {
                let replacement = match property.sym.as_ref() {
                    "resolve" => Some("_vm_resolve"),
                    "reject" => Some("_vm_reject"),
                    _ => None,
                };
                if let Some(name) = replacement {
                    *e = Expr::Ident(self.ids[name].clone());
                    return;
                }
            }
            if let Expr::Ident(id) = e
                && id.sym == "Promise"
            {
                *id = self.ids["_vm_promise"].clone();
                return;
            }
            e.visit_mut_children_with(self);
        }
    }
    program.visit_mut_with(&mut Rewrite {
        ids: &ids.0,
        inside: false,
    });
    struct Clear;
    impl VisitMut for Clear {
        fn visit_mut_span(&mut self, s: &mut swc_core::common::Span) {
            *s = swc_core::common::DUMMY_SP;
        }
    }
    support.visit_mut_with(&mut Clear);
    struct IntrinsicDiscovery;
    impl VisitMut for IntrinsicDiscovery {
        fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
            call.visit_mut_children_with(self);
            if matches!(&call.callee, Callee::Expr(callee) if matches!(&**callee, Expr::Paren(paren) if matches!(&*paren.expr, Expr::Fn(function) if function.function.is_async)))
            {
                call.span = mangler_jsast::span::runtime_span();
            }
        }
    }
    support.visit_mut_with(&mut IntrinsicDiscovery);
    let Program::Script(mut script) = support else {
        unreachable!()
    };
    generated::certify_helpers(&mut script.body);
    mangler_jsast::directives::insert_program_statements(program, script.body);
}

/// Generator lowering expects iterator loops, spread, and binding patterns to
/// have been reduced first. Apply those prerequisites only inside generators,
/// retaining the rest of the application's modern lexical and class semantics.
struct GeneratorPrerequisites {
    unresolved: Mark,
    hoisted: Vec<Stmt>,
    templates: templates::Sites,
}

impl VisitMut for GeneratorPrerequisites {
    fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(expression, self);
    }
    fn visit_mut_function(&mut self, function: &mut Function) {
        if !function.is_generator {
            function.visit_mut_children_with(self);
            return;
        }
        let mut temporary = Program::Script(Script {
            body: vec![Stmt::Expr(ExprStmt {
                span: function.span,
                expr: Box::new(Expr::Fn(FnExpr {
                    ident: None,
                    function: Box::new(mangler_jsast::deep::clone_function(function)),
                })),
            })],
            ..Default::default()
        });
        // Destructuring's function hook moves defaults into the body, which
        // would defer generator parameter initialization until the first next.
        // The VM already handles parameters; keep them outside prerequisite
        // lowering (including nested functions and arrows).
        let mut parameters = ParameterShield::default();
        temporary.visit_mut_with(&mut parameters);
        es2021::logical_assignments().process(&mut temporary);
        es2020::es2020(Default::default(), self.unresolved).process(&mut temporary);
        es2018::object_rest_spread(Default::default()).process(&mut temporary);
        self.templates.lower(&mut temporary);
        mangler_jsast::deep::with_flattened_spines(&mut temporary, |program| {
            es2015::template_literal::template_literal(Default::default()).process(program);
        });
        es2015::for_of::for_of(Default::default()).process(&mut temporary);
        // Array/call spread is supported by the state builder and bytecode VM.
        // Lowering it to `.apply` would erase direct eval reference semantics.
        es2015::computed_props::computed_properties(Default::default()).process(&mut temporary);
        es2015::destructuring::destructuring(Default::default()).process(&mut temporary);
        es2015::block_scoping(self.unresolved).process(&mut temporary);
        parameters.restore = true;
        temporary.visit_mut_with(&mut parameters);
        let Program::Script(script) = temporary else {
            unreachable!()
        };
        for statement in script.body {
            match statement {
                Stmt::Expr(ExprStmt { expr, .. }) if matches!(expr.as_ref(), Expr::Fn(_)) => {
                    let Expr::Fn(transformed) = *expr else {
                        unreachable!()
                    };
                    *function = *transformed.function;
                }
                helper => self.hoisted.push(helper),
            }
        }
        function.params.visit_mut_with(self);
    }
}

struct ShieldedParameters<P> {
    context: swc_core::common::SyntaxContext,
    parameters: Vec<P>,
    captures: Vec<Id>,
}

#[derive(Default)]
struct ParameterShield {
    restore: bool,
    functions: HashMap<swc_core::common::SyntaxContext, ShieldedParameters<Param>>,
    arrows: HashMap<swc_core::common::SyntaxContext, ShieldedParameters<Pat>>,
    remappings: Vec<HashMap<Id, Id>>,
}
impl ParameterShield {
    // Parameters are detached during compatibility lowering, but their free
    // references must remain visible to block_scoping's loop capture analysis.
    fn capture_hint<N: VisitWith<ParameterReferences>>(parameters: &N) -> Option<(Stmt, Vec<Id>)> {
        let mut references = ParameterReferences::default();
        parameters.visit_with(&mut references);
        let mut captures = Vec::new();
        let mut elems = vec![Some(ExprOrSpread {
            spread: None,
            expr: Box::new(Expr::Lit(Lit::Str(Str {
                span: swc_core::common::DUMMY_SP,
                value: "\0mangler_parameter_captures".into(),
                raw: None,
            }))),
        })];
        for ident in references.uses {
            if !references.bindings.contains(&ident.to_id()) {
                captures.push(ident.to_id());
                elems.push(Some(ExprOrSpread {
                    spread: None,
                    expr: Box::new(Expr::Ident(ident)),
                }));
            }
        }
        (!captures.is_empty()).then(|| {
            (
                Stmt::Expr(ExprStmt {
                    span: swc_core::common::DUMMY_SP,
                    expr: Box::new(Expr::Array(ArrayLit {
                        span: swc_core::common::DUMMY_SP,
                        elems,
                    })),
                }),
                captures,
            )
        })
    }

    fn insert_hint(body: &mut FunctionBody, hint: Stmt) {
        let first_statement = body
            .stmts
            .iter()
            .take_while(|statement| {
                matches!(statement, Stmt::Expr(ExprStmt { expr, .. })
                if matches!(expr.as_ref(), Expr::Lit(Lit::Str(_))))
            })
            .count();
        body.stmts.insert(first_statement, hint);
    }

    fn take_hint(body: &mut FunctionBody, captures: &[Id]) -> HashMap<Id, Id> {
        let mut remapping = HashMap::new();
        body.stmts.retain(|statement| {
            let Stmt::Expr(ExprStmt { span, expr }) = statement else {
                return true;
            };
            let Expr::Array(array) = expr.as_ref() else {
                return true;
            };
            if !span.is_dummy()
                || !matches!(array.elems.first(), Some(Some(ExprOrSpread { expr, spread: None }))
                if matches!(expr.as_ref(), Expr::Lit(Lit::Str(marker))
                    if marker.value == "\0mangler_parameter_captures"))
            {
                return true;
            }
            // The loop transform can rename captured references to its fresh
            // iteration parameters. Transfer those identities into the detached
            // defaults before removing the analysis-only references.
            for (source, element) in captures.iter().zip(array.elems.iter().skip(1)) {
                if let Some(ExprOrSpread { expr, spread: None }) = element
                    && let Expr::Ident(target) = expr.as_ref()
                    && *source != target.to_id()
                {
                    remapping.insert(source.clone(), target.to_id());
                }
            }
            false
        });
        remapping
    }
}

#[derive(Default)]
struct ParameterReferences {
    uses: Vec<Ident>,
    bindings: HashSet<Id>,
    seen: HashSet<Id>,
}
impl Visit for ParameterReferences {
    fn visit_bin_expr(&mut self, expression: &BinExpr) {
        mangler_jsast::deep::walk_binary(expression, self);
    }
    fn visit_ident(&mut self, ident: &Ident) {
        if self.seen.insert(ident.to_id()) {
            self.uses.push(ident.clone());
        }
    }
    fn visit_binding_ident(&mut self, ident: &BindingIdent) {
        self.bindings.insert(ident.id.to_id());
    }
    fn visit_fn_expr(&mut self, expression: &FnExpr) {
        if let Some(ident) = &expression.ident {
            self.bindings.insert(ident.to_id());
        }
        expression.function.visit_with(self);
    }
    fn visit_class_expr(&mut self, expression: &ClassExpr) {
        if let Some(ident) = &expression.ident {
            self.bindings.insert(ident.to_id());
        }
        expression.class.visit_with(self);
    }
    fn visit_fn_decl(&mut self, declaration: &FnDecl) {
        self.bindings.insert(declaration.ident.to_id());
        declaration.function.visit_with(self);
    }
    fn visit_class_decl(&mut self, declaration: &ClassDecl) {
        self.bindings.insert(declaration.ident.to_id());
        declaration.class.visit_with(self);
    }
}

impl VisitMut for ParameterShield {
    fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(expression, self);
    }
    fn visit_mut_ident(&mut self, ident: &mut Ident) {
        let source = ident.to_id();
        if let Some(target) = self
            .remappings
            .iter()
            .rev()
            .find_map(|map| map.get(&source))
        {
            ident.sym = target.0.clone();
            ident.ctxt = target.1;
        }
    }
    fn visit_mut_function(&mut self, function: &mut Function) {
        let depth = self.remappings.len();
        if self.restore {
            if let Some(saved) = self.functions.remove(&function.ctxt) {
                function.ctxt = saved.context;
                function.params = saved.parameters;
                if let Some(body) = &mut function.body {
                    self.remappings.push(Self::take_hint(body, &saved.captures));
                }
            }
        } else if !function.params.is_empty() {
            let mut captures = Vec::new();
            if let Some((hint, ids)) = Self::capture_hint(&function.params)
                && let Some(body) = &mut function.body
            {
                Self::insert_hint(body, hint);
                captures = ids;
            }
            function.params.visit_mut_with(self);
            let tag = swc_core::common::SyntaxContext::empty().apply_mark(Mark::new());
            self.functions.insert(
                tag,
                ShieldedParameters {
                    context: function.ctxt,
                    parameters: std::mem::take(&mut function.params),
                    captures,
                },
            );
            function.ctxt = tag;
        }
        function.visit_mut_children_with(self);
        self.remappings.truncate(depth);
    }
    fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
        let depth = self.remappings.len();
        if self.restore {
            if let Some(saved) = self.arrows.remove(&arrow.ctxt) {
                arrow.ctxt = saved.context;
                arrow.params = saved.parameters;
                if let ArrowFunctionBody::FunctionBody(body) = arrow.body.as_mut() {
                    self.remappings.push(Self::take_hint(body, &saved.captures));
                }
            }
        } else if !arrow.params.is_empty() {
            let mut captures = Vec::new();
            if let Some((hint, ids)) = Self::capture_hint(&arrow.params) {
                if let ArrowFunctionBody::Expr(expression) = arrow.body.as_mut() {
                    *arrow.body = ArrowFunctionBody::FunctionBody(FunctionBody {
                        span: swc_core::common::DUMMY_SP,
                        stmts: vec![Stmt::Return(ReturnStmt {
                            span: swc_core::common::DUMMY_SP,
                            arg: Some(std::mem::replace(
                                expression,
                                Box::new(Expr::Invalid(Invalid {
                                    span: swc_core::common::DUMMY_SP,
                                })),
                            )),
                        })],
                    });
                }
                if let ArrowFunctionBody::FunctionBody(body) = arrow.body.as_mut() {
                    Self::insert_hint(body, hint);
                    captures = ids;
                }
            }
            arrow.params.visit_mut_with(self);
            let tag = swc_core::common::SyntaxContext::empty().apply_mark(Mark::new());
            self.arrows.insert(
                tag,
                ShieldedParameters {
                    context: arrow.ctxt,
                    parameters: std::mem::take(&mut arrow.params),
                    captures,
                },
            );
            arrow.ctxt = tag;
        }
        arrow.visit_mut_children_with(self);
        self.remappings.truncate(depth);
    }
}

#[cfg(test)]
mod tests;
