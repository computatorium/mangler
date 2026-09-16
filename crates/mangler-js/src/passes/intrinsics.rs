//! Keep VM intrinsic references independent of source lexical shadows and later
//! replacements of global methods. Native source factories retain source scope.

use crate::config::FileConfig;
use mangler_jsast::build;
use std::collections::{HashMap, HashSet};
#[path = "intrinsic_recovery.rs"]
mod recovery;
#[path = "intrinsic_scopes.rs"]
mod scopes;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

const INTRINSIC_ROOTS: &[&str] = &[
    "Object",
    "Array",
    "Iterator",
    "Reflect",
    "String",
    "Symbol",
    "TypeError",
    "ReferenceError",
    "Proxy",
    "Function",
    "RegExp",
    "BigInt",
    "WeakMap",
    "Map",
    "Set",
    "Error",
    "SyntaxError",
    "SuppressedError",
    "Number",
    "JSON",
    "Uint8Array",
    "TextEncoder",
    "TextDecoder",
    "WebAssembly",
    "atob",
    "eval",
];

fn intrinsic(name: &str) -> bool {
    INTRINSIC_ROOTS.contains(&name)
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};

    #[test]
    fn isolated_value_reads_preserve_identity_alongside_bound_calls() {
        let source = "var captured=Function.prototype.bind,call=Function.prototype.call,apply=Function.prototype.apply;globalThis.__out=captured===Function.prototype.bind&&call===Function.prototype.call&&apply===Function.prototype.apply&&Object.prototype.hasOwnProperty.call({x:1},'x')&&Array.prototype.join.bind([1,2],'-')()==='1-2';";
        let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
        let config = FileConfig::new(
            mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
                preset: Some(mangler_config::Intensity::Minify),
                ..Default::default()
            })
            .unwrap(),
            42,
            HashSet::new(),
        );
        let isolation = Isolation::prepare(ast.program());
        let Program::Script(script) = ast.program_mut() else {
            unreachable!()
        };
        isolation
            .protect(&mut script.body, "table", &config)
            .unwrap();
        let output = Js.print(&ast);
        // Compare with original identities through an unrewritten observer too.
        mangler_testkit::assert_behaviorally_equal(
            "globalThis.__out=true",
            &format!(
                "{output}globalThis.__out=globalThis.__out&&captured===Function.prototype.bind&&call===Function.prototype.call&&apply===Function.prototype.apply;"
            ),
        );
    }
}

pub(crate) fn snapshot_factory() -> String {
    let roots = INTRINSIC_ROOTS
        .iter()
        .map(|name| format!("[{name:?},typeof {name}===\"undefined\"?void 0:{name}]"))
        .collect::<Vec<_>>()
        .join(",");
    include_str!("intrinsic_snapshot.js")
        .replace("__INTRINSIC_ROOTS__", &roots)
        .replace(
            "__DESCRIPTOR_FACTORY__",
            mangler_vm::descriptors::factory_source(),
        )
}

#[derive(Default)]
struct SourceBindings {
    intrinsic: HashSet<String>,
    global_this: bool,
    depth: usize,
}
impl SourceBindings {
    fn binding(&mut self, name: &str) {
        self.global_this |= name == "globalThis";
        if intrinsic(name) {
            self.intrinsic.insert(name.into());
        }
    }
}
impl Visit for SourceBindings {
    fn visit_bin_expr(&mut self, binary: &BinExpr) {
        mangler_jsast::deep::walk_binary(binary, self);
    }
    fn visit_binding_ident(&mut self, _: &BindingIdent) {}
    fn visit_var_decl(&mut self, v: &VarDecl) {
        if self.depth == 0 || v.kind == VarDeclKind::Var {
            for d in &v.decls {
                mangler_jsast::analysis::binding_names(&d.name, &mut |id| {
                    self.binding(id.sym.as_ref());
                });
            }
        }
        v.visit_children_with(self);
    }
    fn visit_block_stmt(&mut self, b: &BlockStmt) {
        self.depth += 1;
        b.visit_children_with(self);
        self.depth -= 1;
    }
    fn visit_for_stmt(&mut self, s: &ForStmt) {
        self.depth += 1;
        s.visit_children_with(self);
        self.depth -= 1;
    }
    fn visit_for_in_stmt(&mut self, s: &ForInStmt) {
        self.depth += 1;
        s.visit_children_with(self);
        self.depth -= 1;
    }
    fn visit_for_of_stmt(&mut self, s: &ForOfStmt) {
        self.depth += 1;
        s.visit_children_with(self);
        self.depth -= 1;
    }
    fn visit_switch_stmt(&mut self, s: &SwitchStmt) {
        self.depth += 1;
        s.visit_children_with(self);
        self.depth -= 1;
    }
    fn visit_import_specifier(&mut self, s: &ImportSpecifier) {
        self.binding(s.local().sym.as_ref());
    }
    fn visit_fn_decl(&mut self, f: &FnDecl) {
        if self.depth == 0 {
            self.binding(f.ident.sym.as_ref());
        }
    }
    fn visit_class_decl(&mut self, c: &ClassDecl) {
        if self.depth == 0 {
            self.binding(c.ident.sym.as_ref());
        }
    }
    fn visit_class(&mut self, _: &Class) {}
    fn visit_export_default_decl(&mut self, d: &ExportDefaultDecl) {
        match &d.decl {
            DefaultDecl::Fn(f) => {
                if let Some(id) = &f.ident {
                    self.binding(id.sym.as_ref());
                }
            }
            DefaultDecl::Class(c) => {
                if let Some(id) = &c.ident {
                    self.binding(id.sym.as_ref());
                }
            }
            DefaultDecl::TsInterfaceDecl(_) => {}
        }
    }
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
}

pub(crate) struct Isolation {
    global: bool,
    script_this: bool,
    recovery: HashSet<String>,
}
impl Isolation {
    pub(crate) fn prepare(program: &Program) -> Self {
        let mut source = SourceBindings::default();
        program.visit_with(&mut source);
        let script_this = source.global_this && matches!(program, Program::Script(_));
        let mut recovery = if source.global_this && !script_this {
            source.intrinsic
        } else {
            HashSet::new()
        };
        if let Program::Script(script) = program {
            // Only direct script function declarations replace the global during
            // declaration instantiation. Annex B block/if functions assign later,
            // after the primitive capture prologue has already executed.
            for statement in &script.body {
                if let Stmt::Decl(Decl::Fn(function)) = statement
                    && intrinsic(function.ident.sym.as_ref())
                {
                    recovery.insert(function.ident.sym.to_string());
                }
            }
        }
        Self {
            global: !source.global_this,
            script_this,
            recovery,
        }
    }

    pub(crate) fn protect(
        &self,
        statements: &mut Vec<Stmt>,
        table: &str,
        cfg: &FileConfig,
    ) -> Result<(), String> {
        // Capture the externally referenced declarations before private intrinsic
        // aliases and shared helpers are introduced.
        let exports = declared_names(statements);
        struct Rewrite<'a> {
            isolation: &'a Isolation,
            cfg: &'a FileConfig,
            aliases: HashMap<scopes::Intrinsic, String>,
            descriptor_factory: Option<String>,
            declarations: Vec<Stmt>,
            missing: Option<String>,
            plan: scopes::Plan<scopes::Intrinsic>,
        }
        impl Rewrite<'_> {
            fn root(&self, name: &str) -> Expr {
                if self.isolation.script_this {
                    // Script top-level this is the realm's global object even
                    // under a use-strict directive; source lexical names cannot
                    // shadow it. Modules deliberately do not take this route.
                    build::member_ident(
                        Expr::This(ThisExpr {
                            span: swc_core::common::DUMMY_SP,
                        }),
                        name,
                    )
                } else if self.isolation.global {
                    build::member_ident(build::ident_expr("globalThis"), name)
                } else {
                    build::ident_expr(name)
                }
            }
            fn descriptor_factory(&mut self) -> String {
                if let Some(name) = &self.descriptor_factory {
                    return name.clone();
                }
                let name = self.cfg.fresh_name();
                let mut expression = mangler_vm::descriptors::factory_expression()
                    .expect("descriptor factory parses");
                let Expr::Call(call) = &mut expression else {
                    unreachable!()
                };
                for (argument, path) in call.args.iter_mut().zip([
                    vec!["Object", "getPrototypeOf"],
                    vec!["Object", "getOwnPropertyNames"],
                    vec!["Object", "getOwnPropertySymbols"],
                    vec!["Object", "prototype", "hasOwnProperty"],
                    vec!["Reflect", "apply"],
                    vec!["Object", "prototype", "propertyIsEnumerable"],
                ]) {
                    argument.expr = Box::new(self.alias(scopes::Intrinsic {
                        path: path.into_iter().map(str::to_string).collect(),
                        bind_receiver: false,
                    }));
                }
                self.declarations
                    .push(build::var_decl(VarDeclKind::Var, &name, expression));
                self.descriptor_factory = Some(name.clone());
                name
            }
            fn alias(&mut self, intrinsic: scopes::Intrinsic) -> Expr {
                if let Some(name) = self.aliases.get(&intrinsic) {
                    return build::ident_expr(name);
                }
                let path = &intrinsic.path;
                let name = self.cfg.fresh_name();
                let mut value = if let Some(resolver) = self.cfg.runtime_intrinsics() {
                    build::call(
                        build::ident_expr(resolver),
                        vec![
                            build::array(path.iter().map(|part| build::str_lit(part)).collect()),
                            build::bool_lit(intrinsic.bind_receiver),
                        ],
                    )
                } else if self.isolation.recovery.contains(&path[0]) {
                    match recovery::recover(path) {
                        Some(value) => value,
                        None => {
                            self.missing.get_or_insert_with(|| path.join("."));
                            return build::ident_expr(&path[0]);
                        }
                    }
                } else {
                    let mut value = self.root(&path[0]);
                    for key in &path[1..] {
                        value = build::member_ident(value, key);
                    }
                    value
                };
                // Inherited function call/apply/bind methods depend on their
                // receiver. Reflect.apply ignores its receiver and a recovered
                // apply is already an intrinsically bound callable.
                if self.cfg.runtime_intrinsics().is_none() && intrinsic.bind_receiver {
                    let receiver_path = &path[..path.len() - 1];
                    let receiver = if self.isolation.recovery.contains(&path[0]) {
                        recovery::recover(receiver_path).expect("intrinsic receiver recovery")
                    } else {
                        let mut receiver = self.root(&path[0]);
                        for key in &path[1..path.len() - 1] {
                            receiver = build::member_ident(receiver, key);
                        }
                        receiver
                    };
                    value = build::call(build::member_ident(value, "bind"), vec![receiver]);
                }
                if self.cfg.runtime_intrinsics().is_none()
                    && let Some(kind) = mangler_vm::descriptors::adapter_kind(path)
                {
                    let factory = self.descriptor_factory();
                    value = build::call(
                        build::ident_expr(&factory),
                        vec![build::num(kind as f64), value],
                    );
                }
                self.declarations
                    .push(build::var_decl(VarDeclKind::Var, &name, value));
                self.aliases.insert(intrinsic, name.clone());
                build::ident_expr(&name)
            }
        }
        impl VisitMut for Rewrite<'_> {
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                if let Some(path) = self
                    .plan
                    .pop_front()
                    .expect("intrinsic scope plan matches AST")
                {
                    *expression = self.alias(path);
                } else {
                    expression.visit_mut_children_with(self);
                }
            }
        }
        let mut rewrite = Rewrite {
            isolation: self,
            cfg,
            aliases: HashMap::new(),
            descriptor_factory: None,
            declarations: Vec::new(),
            missing: None,
            plan: scopes::intrinsic_plan(statements, table),
        };
        scopes::visit_runtime(statements, table, &mut rewrite);
        assert!(rewrite.plan.is_empty(), "intrinsic scope plan consumed");
        if let Some(path) = rewrite.missing {
            return Err(path);
        }
        statements.splice(0..0, rewrite.declarations);
        privatize_runtime(statements, exports, table, cfg);
        Ok(())
    }
}

fn declared_names(statements: &[Stmt]) -> Vec<String> {
    let mut names = Vec::new();
    for statement in statements {
        match statement {
            Stmt::Decl(Decl::Fn(function)) => names.push(function.ident.sym.to_string()),
            Stmt::Decl(Decl::Class(class)) => names.push(class.ident.sym.to_string()),
            Stmt::Decl(Decl::Var(declaration)) => {
                if mangler_jsast::span::is_private_runtime_declaration_span(declaration.span) {
                    continue;
                }
                for variable in &declaration.decls {
                    mangler_jsast::analysis::binding_names(&variable.name, &mut |id| {
                        let name = id.sym.to_string();
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    });
                }
            }
            _ => {}
        }
    }
    names
}

/// Cache intrinsics once in private lexical bindings. Exported VM entry points
/// close over these bindings, which can then use ordinary local-name minification.
/// The arrow retains the source's lexical this/arguments for native factories.
fn privatize_runtime(
    statements: &mut Vec<Stmt>,
    exports: Vec<String>,
    table: &str,
    cfg: &FileConfig,
) {
    let Some(first) = exports.first() else {
        return;
    };
    pool_stateless_helpers(statements, table, cfg);
    let mut body = std::mem::take(statements);
    body.push(build::return_stmt(build::array(
        exports.iter().map(|name| build::ident_expr(name)).collect(),
    )));
    let span = mangler_jsast::span::runtime_span();
    let factory = Expr::Arrow(ArrowExpr {
        span,
        ctxt: Default::default(),
        params: Vec::new(),
        body: Box::new(ArrowFunctionBody::FunctionBody(FunctionBody {
            span,
            stmts: body,
        })),
        is_async: false,
        is_generator: false,
        type_params: None,
        return_type: None,
    });
    let mut call = build::call(build::paren(factory), Vec::new());
    if let Expr::Call(call) = &mut call {
        call.span = span;
    }
    // Reuse the first export as the temporary tuple, avoiding another public
    // runtime binding. All reads occur before that binding receives its function.
    statements.push(build::var_decl(VarDeclKind::Var, first, call));
    for (index, name) in exports.iter().enumerate().skip(1) {
        let mut value = build::member_computed(build::ident_expr(first), build::num(index as f64));
        if let Expr::Member(member) = &mut value {
            member.span = span;
        }
        statements.push(build::var_decl(VarDeclKind::Var, name, value));
    }
    let mut assign = build::assign(
        first,
        build::member_computed(build::ident_expr(first), build::num(0.0)),
    );
    if let Expr::Assign(assign) = &mut assign {
        assign.span = span;
    }
    statements.push(build::expr_stmt(assign));
}

fn pool_stateless_helpers(statements: &mut Vec<Stmt>, table: &str, cfg: &FileConfig) {
    const PURE: &[&str] = &[
        "List",
        "Push",
        "Pop",
        "Append",
        "Slice",
        "Map",
        "MapItems",
        "Contains",
        "Sd",
        "Check",
        "DecodeCode",
        "DecodeConstants1",
        "DecodeConstants2",
        "DecodeConstants3",
        "DecodeConstants4",
        "DecodeConstants5",
        "DecodeConstants6",
        "DecodeConstants7",
        "DecodeConstants8",
        "DecodeConstants9",
        "DecodeConstants10",
        "DecodeConstants11",
        "DecodeConstants12",
        "DecodeConstants13",
        "DecodeConstants14",
        "DecodeConstants15",
    ];
    let owner = cfg.fresh_name();
    struct Rename<'a> {
        plan: scopes::Plan<String>,
        owner: &'a str,
    }
    impl VisitMut for Rename<'_> {
        fn visit_mut_expr(&mut self, expression: &mut Expr) {
            if let Some(key) = self
                .plan
                .pop_front()
                .expect("helper scope plan matches AST")
            {
                *expression = build::member_ident(build::ident_expr(self.owner), &key);
            } else {
                expression.visit_mut_children_with(self);
            }
        }
    }
    let (plan, reachable, mut declarations) = scopes::helper_plan(statements, table, PURE);
    let mut rename = Rename {
        plan,
        owner: &owner,
    };
    scopes::visit_runtime(statements, table, &mut rename);
    assert!(rename.plan.is_empty(), "helper scope plan consumed");
    let mut names = HashSet::new();
    let mut shared = Vec::new();
    for statement in statements.iter_mut() {
        let Stmt::Decl(Decl::Fn(interpreter)) = statement else {
            continue;
        };
        let Some(body) = &mut interpreter.function.body else {
            continue;
        };
        let mut pool = |name: &Ident, function: Expr| {
            let original = name.sym.as_ref();
            if !PURE.contains(&original) {
                return false;
            }
            let Some(key) = declarations
                .pop_front()
                .expect("helper declaration plan matches AST")
            else {
                return false;
            };
            if reachable.contains(&key) && names.insert(original.to_string()) {
                shared.push((key, function));
            }
            true
        };
        body.stmts.retain_mut(|statement| match statement {
            Stmt::Decl(Decl::Fn(helper)) => !pool(
                &helper.ident,
                Expr::Fn(FnExpr {
                    ident: None,
                    function: helper.function.clone(),
                }),
            ),
            Stmt::Decl(Decl::Var(declaration)) => {
                declaration.decls.retain(|variable| {
                    if let Pat::Ident(binding) = &variable.name
                        && let Some(Expr::Fn(function)) = variable.init.as_deref()
                    {
                        return !pool(&binding.id, Expr::Fn(function.clone()));
                    }
                    true
                });
                !declaration.decls.is_empty()
            }
            _ => true,
        });
    }
    assert!(declarations.is_empty(), "helper declaration plan consumed");
    if shared.is_empty() {
        return;
    }
    let helpers = build::object_computed(
        shared
            .iter()
            .map(|(key, function)| (key.as_str(), function.clone()))
            .collect(),
    );
    statements.insert(0, build::var_decl(VarDeclKind::Var, &owner, helpers));
}
