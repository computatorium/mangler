//! Resource scopes lower to ordinary lexical bindings and exception regions.
//! Initializers and the disposal loop remain in the protected body. Only resource
//! registration and intrinsic SuppressedError creation are native protocol glue.
//! https://tc39.es/ecma262/#sec-disposeresources
use crate::config::FileConfig;
use mangler_core::{Language, Result};
use mangler_jsast::{
    build as b,
    lang::{Js, ParseOpts},
};
use std::collections::{HashMap, HashSet};
use swc_core::common::{BytePos, DUMMY_SP, GLOBALS, Mark, Span};
use swc_core::ecma::ast::*;
use swc_core::ecma::transforms::{
    base::{
        fixer::fixer,
        helpers::{HELPERS, Helpers, inject_helpers},
        hygiene::{Config as HygieneConfig, hygiene_with_config},
        resolver,
    },
    proposal::explicit_resource_management::explicit_resource_management,
};
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

struct Names {
    add: String,
    suppress: String,
    apply: String,
    promise: String,
}

#[derive(Default)]
pub(super) struct Lowered {
    pub helpers: Vec<Stmt>,
    pub internals: HashSet<String>,
}

/// Lower selected functions and nested top-level scopes. Top-level resource
/// declarations retain their native module lifetime, like lexical/export bindings;
/// their initializer expressions and disposal methods receive VM chunks later.
/// Returned declarations belong to the isolated runtime prologue.
pub(super) fn lower(
    program: &mut Program,
    selected: &HashSet<u32>,
    whole: bool,
    cfg: &FileConfig,
) -> Result<Lowered> {
    struct Has(bool);
    impl Visit for Has {
        fn visit_using_decl(&mut self, _: &UsingDecl) {
            self.0 = true;
        }
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            mangler_jsast::deep::walk_binary(e, self);
        }
    }
    let mut has = Has(false);
    program.visit_with(&mut has);
    if !has.0 {
        return Ok(Lowered::default());
    }
    let names = Names {
        add: cfg.fresh_name(),
        suppress: cfg.fresh_name(),
        apply: cfg.fresh_name(),
        promise: cfg.fresh_name(),
    };
    let mut work = || lower_inner(program, selected, whole, cfg, &names);
    if GLOBALS.is_set() {
        work()
    } else {
        GLOBALS.set(&Default::default(), work)
    }
}

fn lower_inner(
    program: &mut Program,
    selected: &HashSet<u32>,
    whole: bool,
    cfg: &FileConfig,
    names: &Names,
) -> Result<Lowered> {
    let unresolved = Mark::new();
    let top = Mark::new();
    resolver(unresolved, top, false).process(program);
    Js::repair_resolver_scopes(program);
    let mut original_bindings = bound_ids(program);
    let result = HELPERS.set(&Helpers::new(false), || -> Result<()> {
        program.visit_mut_with(&mut Selected { selected });
        if whole {
            let mut mask = Mask::default();
            program.visit_mut_with(&mut mask);
            let top_resources = preserve_top_resources(program);
            explicit_resource_management().process(program);
            restore_top_resources(program, &top_resources);
            program.visit_mut_with(&mut Restore(&mask));
        }
        inject_helpers(unresolved).process(program);
        Ok(())
    });
    result?;
    let mut helpers = Vec::new();
    let mut helper_ids = HashMap::new();
    fn take_helper(stmt: &Stmt, ids: &mut HashMap<Id, String>, names: &Names) -> bool {
        let Stmt::Decl(Decl::Fn(f)) = stmt else {
            return false;
        };
        let replacement = match f.ident.sym.as_ref() {
            "_ts_add_disposable_resource" => &names.add,
            "_ts_dispose_resources" => &names.suppress,
            _ => return false,
        };
        if !f.function.span.is_dummy() {
            return false;
        }
        ids.insert(f.ident.to_id(), replacement.clone());
        true
    }
    match program {
        Program::Script(s) => s.body.retain(|s| !take_helper(s, &mut helper_ids, names)),
        Program::Module(m) => m.body.retain(
            |s| !matches!(s, ModuleItem::Stmt(s) if take_helper(s, &mut helper_ids, names)),
        ),
    }
    struct Finish<'a> {
        ids: &'a HashMap<Id, String>,
        cfg: &'a FileConfig,
        names: &'a Names,
        source_bindings: &'a mut HashSet<Id>,
        error: Option<mangler_core::Error>,
    }
    impl VisitMut for Finish<'_> {
        fn visit_mut_for_of_stmt(&mut self, statement: &mut ForOfStmt) {
            statement.visit_mut_children_with(self);
            repair_resource_head(statement, &self.names.add, self.cfg, self.source_bindings);
        }
        fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(e, self);
        }
        fn visit_mut_try_stmt(&mut self, t: &mut TryStmt) {
            t.visit_mut_children_with(self);
            let Some(finalizer) = &mut t.finalizer else {
                return;
            };
            let Some(first) = finalizer.stmts.first() else {
                return;
            };
            let (call, asynchronous) = match first {
                Stmt::Expr(e) => (e.expr.as_ref(), false),
                Stmt::Decl(Decl::Var(v)) => match v.decls.first().and_then(|d| d.init.as_deref()) {
                    Some(e) => (e, true),
                    None => return,
                },
                _ => return,
            };
            let Expr::Call(call) = call else {
                return;
            };
            let Callee::Expr(callee) = &call.callee else {
                return;
            };
            let Expr::Ident(id) = callee.as_ref() else {
                return;
            };
            if self.ids.get(&id.to_id()) != Some(&self.names.suppress) {
                return;
            }
            let Some(Expr::Ident(env)) = call.args.first().map(|a| a.expr.as_ref()) else {
                return;
            };
            match disposal_block(env, asynchronous, self.cfg, self.names) {
                Ok(block) => *finalizer = block,
                Err(e) => self.error = Some(e),
            }
        }
        fn visit_mut_var_declarator(&mut self, declaration: &mut VarDeclarator) {
            declaration.visit_mut_children_with(self);
            let Pat::Ident(binding) = &declaration.name else {
                return;
            };
            let Some(Expr::Call(call)) = declaration.init.as_deref_mut() else {
                return;
            };
            if !matches!(&call.callee, Callee::Expr(callee) if matches!(callee.as_ref(), Expr::Ident(id) if id.sym == self.names.add))
            {
                return;
            }
            if call.args.len() == 3 && anonymous_initializer(&call.args[1].expr) {
                let value = std::mem::replace(
                    &mut call.args[1].expr,
                    Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
                );
                let key = b::str_lit(binding.id.sym.as_ref());
                // Property NamedEvaluation runs before class static elements and
                // does not create an extra self-name binding. The computed key
                // also treats __proto__ as an ordinary data property.
                *call.args[1].expr = Expr::Member(MemberExpr {
                    span: DUMMY_SP,
                    obj: Box::new(Expr::Object(ObjectLit {
                        span: DUMMY_SP,
                        props: vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
                            key: PropName::Computed(ComputedPropName {
                                span: DUMMY_SP,
                                expr: Box::new(key.clone()),
                            }),
                            value,
                        })))],
                    })),
                    prop: MemberProp::Computed(ComputedPropName {
                        span: DUMMY_SP,
                        expr: Box::new(key),
                    }),
                });
            }
        }
        fn visit_mut_ident(&mut self, id: &mut Ident) {
            if let Some(name) = self.ids.get(&id.to_id())
                && name != &self.names.suppress
            {
                *id = b::ident(name);
            }
        }
    }
    let mut finish = Finish {
        ids: &helper_ids,
        cfg,
        names,
        source_bindings: &mut original_bindings,
        error: None,
    };
    program.visit_mut_with(&mut finish);
    if let Some(error) = finish.error {
        return Err(error);
    }
    let mut introduced: Vec<_> = bound_ids(program)
        .difference(&original_bindings)
        .cloned()
        .collect();
    introduced.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.as_u32().cmp(&b.1.as_u32())));
    let renames: HashMap<_, _> = introduced
        .into_iter()
        .map(|id| (id, cfg.fresh_name()))
        .collect();
    let mut internals: HashSet<_> = renames.values().cloned().collect();
    internals.extend([
        names.add.clone(),
        names.suppress.clone(),
        names.apply.clone(),
        names.promise.clone(),
    ]);
    struct Rename<'a>(&'a HashMap<Id, String>);
    impl VisitMut for Rename<'_> {
        fn visit_mut_ident(&mut self, id: &mut Ident) {
            if let Some(name) = self.0.get(&id.to_id()) {
                id.sym = name.clone().into();
            }
        }
        fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(e, self);
        }
    }
    program.visit_mut_with(&mut Rename(&renames));
    hygiene_with_config(HygieneConfig {
        keep_class_names: true,
        top_level_mark: top,
        ..Default::default()
    })
    .process(program);
    fixer(None).process(program);
    helpers.extend(protocol(names)?);
    Ok(Lowered { helpers, internals })
}

/// Restore the loop-head TDZ erased by the upstream resource transform. The
/// incoming iteration value is copied outside the resource binding's inner scope,
/// so both source scopes can retain the original spelling for direct eval.
fn repair_resource_head(
    statement: &mut ForOfStmt,
    helper: &str,
    cfg: &FileConfig,
    source_bindings: &mut HashSet<Id>,
) {
    let ForHead::VarDecl(head) = &mut statement.left else {
        return;
    };
    if head.kind != VarDeclKind::Const || head.decls.len() != 1 {
        return;
    }
    let Pat::Ident(head_binding) = &mut head.decls[0].name else {
        return;
    };
    let old_head = head_binding.id.to_id();
    let Stmt::Block(body) = statement.body.as_mut() else {
        return;
    };
    let Some((position, resource)) = body.stmts.iter_mut().enumerate().find_map(|(index, item)| {
        let Stmt::Try(region) = item else { return None };
        let Some(Stmt::Decl(Decl::Var(declaration))) = region.block.stmts.first_mut() else { return None };
        if declaration.kind != VarDeclKind::Const || declaration.decls.len() != 1 { return None; }
        let binding = &mut declaration.decls[0];
        let Some(Expr::Call(call)) = binding.init.as_deref() else { return None };
        if !matches!(&call.callee, Callee::Expr(callee) if matches!(callee.as_ref(), Expr::Ident(id) if id.sym == helper)) { return None; }
        if !matches!(call.args.get(1).map(|a| a.expr.as_ref()), Some(Expr::Ident(id)) if id.to_id() == old_head) { return None; }
        Some((index, binding))
    }) else { return };
    let Pat::Ident(resource_binding) = &resource.name else {
        return;
    };
    let original = resource_binding.id.to_id();
    let mut tdz_head = head_binding.id.clone();
    tdz_head.sym = resource_binding.id.sym.clone();
    source_bindings.insert(tdz_head.to_id());
    head_binding.id = tdz_head.clone();

    struct Redirect<'a> {
        old: &'a Id,
        new: &'a Ident,
    }
    impl VisitMut for Redirect<'_> {
        fn visit_mut_ident(&mut self, identifier: &mut Ident) {
            if identifier.to_id() == *self.old {
                identifier.sym = self.new.sym.clone();
                identifier.ctxt = self.new.ctxt;
            }
        }
        fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(expression, self);
        }
    }
    statement.right.visit_mut_with(&mut Redirect {
        old: &original,
        new: &tdz_head,
    });
    let incoming = cfg.fresh_name();
    let Some(Expr::Call(call)) = resource.init.as_deref_mut() else {
        unreachable!()
    };
    *call.args[1].expr = b::ident_expr(&incoming);
    body.stmts.insert(
        position,
        b::var_decl(VarDeclKind::Const, &incoming, Expr::Ident(tdz_head)),
    );
}

fn bound_ids(program: &Program) -> HashSet<Id> {
    #[derive(Default)]
    struct Bindings(HashSet<Id>);
    impl Visit for Bindings {
        fn visit_binding_ident(&mut self, binding: &BindingIdent) {
            self.0.insert(binding.id.to_id());
        }
        fn visit_fn_decl(&mut self, function: &FnDecl) {
            self.0.insert(function.ident.to_id());
            function.visit_children_with(self);
        }
        fn visit_fn_expr(&mut self, function: &FnExpr) {
            if let Some(id) = &function.ident {
                self.0.insert(id.to_id());
            }
            function.visit_children_with(self);
        }
        fn visit_class_decl(&mut self, class: &ClassDecl) {
            self.0.insert(class.ident.to_id());
            class.visit_children_with(self);
        }
        fn visit_class_expr(&mut self, class: &ClassExpr) {
            if let Some(id) = &class.ident {
                self.0.insert(id.to_id());
            }
            class.visit_children_with(self);
        }
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            mangler_jsast::deep::walk_binary(e, self);
        }
    }
    let mut bindings = Bindings::default();
    program.visit_with(&mut bindings);
    bindings.0
}

// A module's resource lifetime spans native lexical declarations and exports.
// Moving those declarations into a try block would destroy TDZ/live-binding
// semantics. Hide only direct resource declarations from the downlevel transform;
// the later native-envelope pass still protects their initializer expressions.
fn preserve_top_resources(program: &mut Program) -> HashMap<u32, bool> {
    fn hide(statement: &mut Stmt, kinds: &mut HashMap<u32, bool>) {
        let Stmt::Decl(Decl::Using(using)) = statement else {
            return;
        };
        kinds.insert(using.span.lo.0, using.is_await);
        *statement = Stmt::Decl(Decl::Var(Box::new(VarDecl {
            span: using.span,
            kind: VarDeclKind::Const,
            decls: std::mem::take(&mut using.decls),
            ..Default::default()
        })));
    }
    let mut kinds = HashMap::new();
    match program {
        Program::Script(script) => {
            for statement in &mut script.body {
                hide(statement, &mut kinds);
            }
        }
        Program::Module(module) => {
            for item in &mut module.body {
                if let ModuleItem::Stmt(statement) = item {
                    hide(statement, &mut kinds);
                }
            }
        }
    }
    kinds
}

fn restore_top_resources(program: &mut Program, kinds: &HashMap<u32, bool>) {
    fn restore(statement: &mut Stmt, kinds: &HashMap<u32, bool>) {
        let Stmt::Decl(Decl::Var(variable)) = statement else {
            return;
        };
        let Some(&is_await) = kinds.get(&variable.span.lo.0) else {
            return;
        };
        *statement = Stmt::Decl(Decl::Using(Box::new(UsingDecl {
            span: variable.span,
            is_await,
            decls: std::mem::take(&mut variable.decls),
        })));
    }
    match program {
        Program::Script(script) => {
            for statement in &mut script.body {
                restore(statement, kinds);
            }
        }
        Program::Module(module) => {
            for item in &mut module.body {
                if let ModuleItem::Stmt(statement) = item {
                    restore(statement, kinds);
                }
            }
        }
    }
}

struct Selected<'a> {
    selected: &'a HashSet<u32>,
}
impl VisitMut for Selected<'_> {
    fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(e, self);
    }
    fn visit_mut_function(&mut self, f: &mut Function) {
        f.visit_mut_children_with(self);
        if self.selected.contains(&f.span.lo.0) {
            lower_function(f);
        }
    }
    fn visit_mut_arrow_expr(&mut self, a: &mut ArrowExpr) {
        a.visit_mut_children_with(self);
        if !self.selected.contains(&a.span.lo.0) {
            return;
        }
        let ArrowFunctionBody::FunctionBody(body) = a.body.as_mut() else {
            return;
        };
        let mut f = Function {
            span: a.span,
            body: Some(body.clone()),
            is_async: a.is_async,
            ..Default::default()
        };
        lower_function(&mut f);
        *body = f.body.unwrap();
    }
}

#[derive(Default)]
struct Mask {
    functions: HashMap<u32, Function>,
    arrows: HashMap<u32, ArrowExpr>,
}
impl Mask {
    fn marker(&self) -> Span {
        // Multiple earlier generated functions can share DUMMY_SP. A private
        // temporary marker identifies each saved node until restoration; it is
        // never emitted or used for source coverage.
        let position = u32::MAX - (self.functions.len() + self.arrows.len()) as u32;
        Span::new(BytePos(position), BytePos(position))
    }
}
impl VisitMut for Mask {
    fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(e, self);
    }
    fn visit_mut_function(&mut self, f: &mut Function) {
        let span = self.marker();
        self.functions.insert(
            span.lo.0,
            std::mem::replace(
                f,
                Function {
                    span,
                    ..Default::default()
                },
            ),
        );
    }
    fn visit_mut_arrow_expr(&mut self, a: &mut ArrowExpr) {
        let span = self.marker();
        let old = a.clone();
        a.span = span;
        a.params.clear();
        a.is_async = false;
        *a.body = ArrowFunctionBody::FunctionBody(FunctionBody::default());
        self.arrows.insert(span.lo.0, old);
    }
}
struct Restore<'a>(&'a Mask);
impl VisitMut for Restore<'_> {
    fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(e, self);
    }
    fn visit_mut_function(&mut self, f: &mut Function) {
        if let Some(old) = self.0.functions.get(&f.span.lo.0) {
            *f = old.clone();
        }
    }
    fn visit_mut_arrow_expr(&mut self, a: &mut ArrowExpr) {
        if let Some(old) = self.0.arrows.get(&a.span.lo.0) {
            *a = old.clone();
        }
    }
}

fn lower_function(function: &mut Function) {
    let Some(body) = &mut function.body else {
        return;
    };
    struct OwnResources(bool);
    impl Visit for OwnResources {
        fn visit_using_decl(&mut self, _: &UsingDecl) {
            self.0 = true;
        }
        fn visit_function(&mut self, _: &Function) {}
        fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            mangler_jsast::deep::walk_binary(e, self);
        }
    }
    let mut resources = OwnResources(false);
    body.visit_with(&mut resources);
    if !resources.0 {
        return;
    }
    let directives: Vec<_> = body
        .stmts
        .iter()
        .take_while(
            |s| matches!(s, Stmt::Expr(e) if matches!(e.expr.as_ref(), Expr::Lit(Lit::Str(_)))),
        )
        .cloned()
        .collect();
    let declarations: HashSet<_> = body
        .stmts
        .iter()
        .filter_map(|s| {
            if let Stmt::Decl(Decl::Fn(f)) = s {
                Some(f.function.span.lo.0)
            } else {
                None
            }
        })
        .collect();
    let mut mask = Mask::default();
    function.visit_mut_children_with(&mut mask);
    let mut program = Program::Script(Script {
        body: vec![Stmt::Expr(ExprStmt {
            span: DUMMY_SP,
            expr: Box::new(Expr::Fn(FnExpr {
                ident: None,
                function: Box::new(function.clone()),
            })),
        })],
        ..Default::default()
    });
    explicit_resource_management().process(&mut program);
    let Program::Script(script) = program else {
        unreachable!()
    };
    let Stmt::Expr(e) = script.body.into_iter().next().unwrap() else {
        unreachable!()
    };
    let Expr::Fn(f) = *e.expr else { unreachable!() };
    *function = *f.function;
    function.visit_mut_children_with(&mut Restore(&mask));
    let body = function.body.as_mut().unwrap();
    if !directives.is_empty()
        && !matches!(body.stmts.first(), Some(Stmt::Expr(e)) if matches!(e.expr.as_ref(), Expr::Lit(Lit::Str(_))))
    {
        body.stmts.splice(0..0, directives);
    }
    for statement in &mut body.stmts {
        let Stmt::Try(t) = statement else {
            continue;
        };
        let mut hoisted = Vec::new();
        t.block.stmts.retain(|s| {
            let Stmt::Decl(Decl::Fn(f)) = s else {
                return true;
            };
            if !declarations.contains(&f.function.span.lo.0) {
                return true;
            }
            let mut declaration = b::var_decl(
                VarDeclKind::Var,
                f.ident.sym.as_ref(),
                Expr::Fn(FnExpr {
                    ident: None,
                    function: f.function.clone(),
                }),
            );
            if let Stmt::Decl(Decl::Var(variable)) = &mut declaration {
                variable.decls[0].name = Pat::Ident(f.ident.clone().into());
            }
            hoisted.push(declaration);
            false
        });
        t.block.stmts.splice(0..0, hoisted);
    }
}

fn anonymous_initializer(mut expr: &Expr) -> bool {
    while let Expr::Paren(paren) = expr {
        expr = &paren.expr;
    }
    matches!(
        expr,
        Expr::Fn(FnExpr { ident: None, .. })
            | Expr::Arrow(_)
            | Expr::Class(ClassExpr { ident: None, .. })
    )
}

fn parsed_body(source: &str) -> Result<BlockStmt> {
    let Program::Script(script) = Js.parse(source, &ParseOpts::default())?.into_program() else {
        unreachable!()
    };
    let Stmt::Decl(Decl::Fn(f)) = script.body.into_iter().next().unwrap() else {
        unreachable!()
    };
    let body = f.function.body.unwrap();
    Ok(BlockStmt {
        span: body.span,
        stmts: body.stmts,
        ..Default::default()
    })
}

fn disposal_block(
    env: &Ident,
    asynchronous: bool,
    cfg: &FileConfig,
    names: &Names,
) -> Result<BlockStmt> {
    let resource = cfg.fresh_name();
    let result = cfg.fresh_name();
    let error = cfg.fresh_name();
    let need = cfg.fresh_name();
    let done = cfg.fresh_name();
    let before = if asynchronous {
        format!("if(!{resource}.async&&{need}&&!{done}){{await void 0;{need}=false;}}")
    } else {
        String::new()
    };
    let after = if asynchronous {
        format!("if({resource}.async){{{done}=true;await {result};}}")
    } else {
        String::new()
    };
    let empty = if asynchronous {
        format!("else {need}=true;")
    } else {
        String::new()
    };
    let final_await = if asynchronous {
        format!("if({need}&&!{done})await void 0;")
    } else {
        String::new()
    };
    let source = format!(
        "async function template(){{let {need}=false,{done}=false;while(__RESOURCE_ENV.stack.length){{let {resource}=__RESOURCE_ENV.stack[__RESOURCE_ENV.stack.length-1];__RESOURCE_ENV.stack.length--;{before}if({resource}.dispose!==void 0){{try{{let {result}={apply}({resource}.dispose,{resource}.value,[]);{after}}}catch({error}){{__RESOURCE_ENV.error=__RESOURCE_ENV.hasError?{suppress}({error},__RESOURCE_ENV.error):{error};__RESOURCE_ENV.hasError=true;}}}}{empty}}}{final_await}if(__RESOURCE_ENV.hasError)throw __RESOURCE_ENV.error;}}",
        apply = names.apply,
        suppress = names.suppress
    );
    let mut body = parsed_body(&source)?;
    struct Env<'a>(&'a Ident);
    impl VisitMut for Env<'_> {
        fn visit_mut_ident(&mut self, id: &mut Ident) {
            if id.sym == "__RESOURCE_ENV" {
                *id = self.0.clone();
            }
        }
    }
    body.visit_mut_with(&mut Env(env));
    Ok(body)
}

fn protocol(names: &Names) -> Result<Vec<Stmt>> {
    let source = include_str!("resources/protocol.js")
        .replace("__RESOURCE_ADD", &names.add)
        .replace("__RESOURCE_SUPPRESS", &names.suppress)
        .replace("__RESOURCE_APPLY", &names.apply)
        .replace("__RESOURCE_PROMISE", &names.promise);
    let Program::Script(script) = Js.parse(&source, &ParseOpts::default())?.into_program() else {
        unreachable!()
    };
    Ok(script.body)
}

#[cfg(test)]
mod tests;
