//! Runtime source uses the same coverage-checked virtualization pass as files.
//! Only compiler-generated support is moved into a native namespace. The returned
//! initializer is an expression: async/class envelopes need not be function nodes.
use crate::config::FileConfig;
use crate::passes::virtualize::{VirtualizePass, source_functions};
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_core::{Error, Language, Notes, PassConfig, Result, Rng};
use mangler_jsast::{Js, ParseOpts};
use mangler_passgraph::{ArtifactBus, Pass};
use std::collections::BTreeSet;
use swc_core::common::{DUMMY_SP, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::transforms::base::{
    fixer::fixer,
    hygiene::{Config as HygieneConfig, hygiene_with_config},
};
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

/// Create the helper-intrinsic resolver once, before executing source code.
/// Pass its result as the sole argument to every runtime support factory.
pub fn intrinsic_snapshot_factory() -> String {
    crate::passes::intrinsics::snapshot_factory()
}

/// Isolate an ahead-of-time generated interpreter or host fragment through the
/// same capture planner used for runtime-compiled support. This accepts only
/// compiler machinery; source code must retain its observable global lookups.
pub fn isolate_generated_runtime(
    program: &mut Program,
    table: &str,
    intrinsics: &str,
) -> Result<()> {
    let (_, identifiers) = crate::seed::fingerprint_and_idents(program);
    let resolved = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(0),
        ..Default::default()
    })
    .map_err(|error| self::error(error.to_string()))?;
    let config =
        FileConfig::new(resolved, 0, identifiers).with_runtime_intrinsics(intrinsics.into());
    let isolation = crate::passes::intrinsics::Isolation::prepare(program);
    let Program::Script(script) = program else {
        return Err(error("generated runtime must be a script"));
    };
    isolation
        .protect(&mut script.body, table, &config)
        .map_err(|path| error(format!("unavailable runtime intrinsic {path}")))
}

/// A native factory containing only coverage-validated compiler support.
#[derive(Debug, Clone)]
pub struct RuntimeSupport {
    /// Accepts the resolver produced by [`intrinsic_snapshot_factory`].
    pub factory: String,
    pub names: Vec<String>,
    pub tables: Vec<String>,
}

/// Compile `return initializer` as the entry and resolve generated captures from
/// the dictionary returned by `support.factory`. Other captures remain live in
/// the caller-provided source environment.
#[derive(Debug, Clone)]
pub struct PreparedFunction {
    pub initializer: Box<Expr>,
    pub support: RuntimeSupport,
    pub name: Option<String>,
    pub length: usize,
}

/// Prepare a function/arrow expression, preserving an explicit lexical name.
pub fn prepare_function(source: &str) -> Result<PreparedFunction> {
    prepare(source, false, None)
}

/// Prepare separately parsed Function-constructor text. Its displayed
/// `anonymous` name is not a lexical self binding; the host sets display metadata.
pub fn prepare_constructor(source: &str) -> Result<PreparedFunction> {
    prepare(source, true, None)
}

/// Prepare parser-escaped runtime text while retaining its original UTF-16 units.
pub fn prepare_function_with_source_utf16(
    source: &str,
    map: &mangler_vm::source_text::SourceTextMap,
) -> Result<PreparedFunction> {
    prepare(source, false, Some(map))
}

pub fn prepare_constructor_with_source_utf16(
    source: &str,
    map: &mangler_vm::source_text::SourceTextMap,
) -> Result<PreparedFunction> {
    prepare(source, true, Some(map))
}

fn error(message: impl Into<String>) -> Error {
    Error::transform("runtime_frontend", message)
}

fn unparen(expression: &mut Expr) -> &mut Expr {
    match expression {
        Expr::Paren(paren) => unparen(&mut paren.expr),
        expression => expression,
    }
}

fn entry(ast: &mut mangler_jsast::lang::Ast) -> Result<&mut VarDeclarator> {
    let Program::Script(script) = ast.program_mut() else {
        return Err(error("runtime callable must be an expression"));
    };
    let [Stmt::Decl(Decl::Var(declaration))] = script.body.as_mut_slice() else {
        return Err(error(
            "runtime callable must contain exactly one expression",
        ));
    };
    let [declaration] = declaration.decls.as_mut_slice() else {
        return Err(error("runtime callable has an invalid entry declaration"));
    };
    Ok(declaration)
}

fn prepare(
    source: &str,
    constructor: bool,
    source_utf16: Option<&mangler_vm::source_text::SourceTextMap>,
) -> Result<PreparedFunction> {
    // Newlines prevent trailing source comments from consuming the envelope.
    let mut ast = Js.parse(
        &format!("var __mangler_input=(\n{source}\n);"),
        &ParseOpts::default(),
    )?;
    if let Some(map) = source_utf16 {
        ast.program_mut()
            .visit_mut_with(&mut mangler_vm::source_text::RestoreStringLiterals(map));
    }
    let expression = unparen(
        entry(&mut ast)?
            .init
            .as_deref_mut()
            .ok_or_else(|| error("missing callable"))?,
    );
    let (name, length) = match expression {
        Expr::Fn(function) => {
            if constructor {
                function.ident = None;
            }
            let name = function.ident.as_ref().map(|id| id.sym.to_string());
            let length = function
                .function
                .params
                .iter()
                .take_while(|p| !matches!(p.pat, Pat::Assign(_) | Pat::Rest(_)))
                .count();
            (name, length)
        }
        Expr::Arrow(function) if !constructor => {
            let length = function
                .params
                .iter()
                .take_while(|p| !matches!(p, Pat::Assign(_) | Pat::Rest(_)))
                .count();
            (None, length)
        }
        _ => {
            return Err(error(
                "runtime callable must be a function or arrow expression",
            ));
        }
    };
    let (_, mut identifiers) = crate::seed::fingerprint_and_idents(ast.program());
    let mut input_name = "__mangler_runtime_input".to_string();
    while identifiers.contains(&input_name) {
        input_name.push('_');
    }
    identifiers.insert(input_name.clone());
    let Pat::Ident(binding) = &mut entry(&mut ast)?.name else {
        return Err(error("runtime callable entry must be an identifier"));
    };
    binding.id.sym = input_name.clone().into();
    let resolved = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(0),
        virtualize: Some("*".into()),
        require_virtualized: Some("*".into()),
        ..Default::default()
    })
    .map_err(|e| error(e.to_string()))?;
    let (seed, _) = crate::seed::effective_seed_and_idents(ast.program(), 0);
    let config = FileConfig::new(resolved, seed, identifiers)
        .with_source_utf16(source_utf16.cloned().unwrap_or_default())
        .with_source_functions(source_functions(ast.program()))
        .with_runtime_frontend();
    let intrinsics = config.fresh_name();
    let config = config.with_runtime_intrinsics(intrinsics.clone());
    Js::with_globals(|| {
        let (statements, tables) = lower_program(&mut ast, &config)?;
        let mut initializer = None;
        let mut support = Vec::new();
        for statement in statements {
            match statement {
                Stmt::Decl(Decl::Var(mut declaration)) => {
                    declaration.decls.retain_mut(|variable| {
                        if matches!(&variable.name, Pat::Ident(id) if id.id.sym == input_name) {
                            initializer = variable.init.take();
                            false
                        } else {
                            true
                        }
                    });
                    if !declaration.decls.is_empty() {
                        support.push(Stmt::Decl(Decl::Var(declaration)));
                    }
                }
                statement => support.push(statement),
            }
        }
        let mut initializer = initializer
            .ok_or_else(|| error("runtime transformation lost its callable initializer"))?;
        let support = make_support(support, tables, &intrinsics)?;
        initializer.visit_mut_with(&mut ClearContexts);
        Ok(PreparedFunction {
            initializer,
            support,
            name,
            length,
        })
    })
}

/// Eval completion and declaration metadata stay authoritative while the shared
/// file frontend lowers nested functions, classes, suspension and resources.
pub struct PreparedEval {
    pub body: mangler_vm::eval::PreparedEvalBody,
    pub support: RuntimeSupport,
    pub class_contexts: mangler_vm::eval::EvalClassContexts,
}

pub fn prepare_eval(prepared: mangler_vm::eval::PreparedEvalBody) -> Result<PreparedEval> {
    prepare_eval_impl(prepared, None, None)
}

pub fn prepare_eval_with_source_utf16(
    prepared: mangler_vm::eval::PreparedEvalBody,
    map: &mangler_vm::source_text::SourceTextMap,
) -> Result<PreparedEval> {
    prepare_eval_impl(prepared, Some(map), None)
}

/// Lower eval with caller-owned class primitives. The named capsule is a hidden
/// bytecode capture; source text carries only its grammar and never its brands.
pub fn prepare_eval_with_context(
    prepared: mangler_vm::eval::PreparedEvalBody,
    map: &mangler_vm::source_text::SourceTextMap,
    context: Option<&mangler_vm::eval::EvalClassContext>,
) -> Result<PreparedEval> {
    prepare_eval_impl(prepared, Some(map), context)
}

fn prepare_eval_impl(
    mut prepared: mangler_vm::eval::PreparedEvalBody,
    source_utf16: Option<&mangler_vm::source_text::SourceTextMap>,
    class_context: Option<&mangler_vm::eval::EvalClassContext>,
) -> Result<PreparedEval> {
    if let Some(map) = source_utf16 {
        prepared
            .body
            .visit_mut_with(&mut mangler_vm::source_text::RestoreStringLiterals(map));
    }
    let mut ast = Js.parse("function __mangler_eval_entry(){}", &ParseOpts::default())?;
    let Program::Script(script) = ast.program_mut() else {
        unreachable!()
    };
    let Stmt::Decl(Decl::Fn(function)) = &mut script.body[0] else {
        unreachable!()
    };
    function.function.span = mangler_jsast::span::eval_entry_span();
    function.function.body = Some(std::mem::replace(
        &mut prepared.body,
        FunctionBody {
            span: DUMMY_SP,
            stmts: Vec::new(),
        },
    ));
    // The carrier only supplies frontend traversal. Final compilation retains
    // the caller's SourceContext and eval environment from PreparedEvalBody.
    if prepared.strict {
        script.body.insert(
            0,
            Stmt::Expr(ExprStmt {
                span: DUMMY_SP,
                expr: Box::new(Expr::Lit(Lit::Str(Str {
                    span: DUMMY_SP,
                    value: "use strict".into(),
                    raw: None,
                }))),
            }),
        );
    }
    let (_, mut identifiers) = crate::seed::fingerprint_and_idents(ast.program());
    if let Some(context) = class_context {
        identifiers.insert(context.capsule_binding.clone());
        prepared
            .internal_bindings
            .insert(context.capsule_binding.clone());
    }
    let mut input_name = "__mangler_runtime_eval".to_string();
    while identifiers.contains(&input_name) {
        input_name.push('_');
    }
    identifiers.insert(input_name.clone());
    let Program::Script(script) = ast.program_mut() else {
        unreachable!()
    };
    for statement in &mut script.body {
        if let Stmt::Decl(Decl::Fn(function)) = statement {
            function.ident.sym = input_name.clone().into();
        }
    }
    let functions = source_functions(ast.program());
    let resolved = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(0),
        virtualize: Some("*".into()),
        require_virtualized: (!functions.is_empty()).then(|| "*".into()),
        ..Default::default()
    })
    .map_err(|e| error(e.to_string()))?;
    let (seed, _) = crate::seed::effective_seed_and_idents(ast.program(), 0);
    let config = FileConfig::new(resolved, seed, identifiers)
        .with_source_utf16(source_utf16.cloned().unwrap_or_default())
        .with_source_functions(functions)
        .with_runtime_eval_frontend()
        .with_runtime_eval_context(class_context.cloned());
    let source_class_contexts =
        crate::passes::virtualize::eval_class_contexts(ast.program(), &config, class_context);
    let class_contexts = source_class_contexts.calls.clone();
    let config = config.with_eval_class_contexts(source_class_contexts);
    let intrinsics = config.fresh_name();
    let config = config.with_runtime_intrinsics(intrinsics.clone());
    Js::with_globals(|| {
        let (statements, tables) = lower_program(&mut ast, &config)?;
        let mut body = None;
        let mut support = Vec::new();
        for statement in statements {
            match statement {
                Stmt::Decl(Decl::Fn(mut function)) if function.ident.sym == input_name => {
                    body = function.function.body.take();
                }
                statement => support.push(statement),
            }
        }
        prepared.body = body.ok_or_else(|| error("runtime transformation lost its eval body"))?;
        prepared
            .internal_bindings
            .extend(config.take_runtime_internals());
        prepared.body.visit_mut_with(&mut ClearContexts);
        Ok(PreparedEval {
            body: prepared,
            support: make_support(support, tables, &intrinsics)?,
            class_contexts,
        })
    })
}

// Returned nodes must not retain contexts owned by this GLOBALS scope.
struct ClearContexts;
impl VisitMut for ClearContexts {
    fn visit_mut_syntax_context(&mut self, context: &mut SyntaxContext) {
        *context = SyntaxContext::empty();
    }
}

fn lower_program(
    ast: &mut mangler_jsast::lang::Ast,
    config: &FileConfig,
) -> Result<(Vec<Stmt>, Vec<String>)> {
    let pass = VirtualizePass;
    let mut bus = ArtifactBus::new();
    bus.enter_pass(pass.id(), pass.reads(), pass.writes());
    pass.run(
        ast,
        config,
        &mut Rng::for_pass(config.seed(), pass.id()),
        &mut bus,
        &mut Notes::new(),
    )?;
    let fragments = config.take_runtime_support();
    if fragments.is_empty() {
        return Err(error("required source produced no protected runtime"));
    }
    let mut statements = Vec::new();
    let mut tables = Vec::new();
    for (support, table) in fragments {
        statements.extend(support);
        tables.push(table);
    }
    let Program::Script(script) = ast.program_mut() else {
        return Err(error("runtime transformation introduced a module"));
    };
    statements.append(&mut script.body);
    script.body = statements;
    // Resolve support and source together before separating execution scopes.
    let (_, top_level_mark) = Js::resolve(ast);
    ast.program_mut()
        .visit_mut_with(&mut hygiene_with_config(HygieneConfig {
            keep_class_names: true,
            top_level_mark,
            ..Default::default()
        }));
    ast.program_mut().visit_mut_with(&mut fixer(None));
    let Program::Script(script) = ast.program_mut() else {
        unreachable!()
    };
    Ok((std::mem::take(&mut script.body), tables))
}

fn make_support(
    mut support: Vec<Stmt>,
    tables: Vec<String>,
    intrinsics: &str,
) -> Result<RuntimeSupport> {
    let mut names = BTreeSet::new();
    for statement in &support {
        match statement {
            Stmt::Decl(Decl::Fn(function)) => {
                names.insert(function.ident.sym.to_string());
            }
            Stmt::Decl(Decl::Class(class)) => {
                names.insert(class.ident.sym.to_string());
            }
            Stmt::Decl(Decl::Var(declaration)) => {
                for variable in &declaration.decls {
                    binding_names(&variable.name, &mut names);
                }
            }
            _ => {}
        }
    }
    if tables.iter().any(|table| !names.contains(table)) {
        return Err(error(
            "runtime table was not retained in its support namespace",
        ));
    }
    let names: Vec<_> = names.into_iter().collect();
    support.push(Stmt::Return(ReturnStmt {
        span: DUMMY_SP,
        arg: Some(Box::new(Expr::Object(ObjectLit {
            span: DUMMY_SP,
            props: names
                .iter()
                .map(|name| {
                    PropOrSpread::Prop(Box::new(Prop::Shorthand(Ident::new_no_ctxt(
                        name.clone().into(),
                        DUMMY_SP,
                    ))))
                })
                .collect(),
        }))),
    }));
    Ok(RuntimeSupport {
        factory: emit_expression(mangler_jsast::codegen::fn_expr(
            None,
            &[intrinsics],
            support,
        ))?,
        names,
        tables,
    })
}

fn binding_names(pattern: &Pat, names: &mut BTreeSet<String>) {
    match pattern {
        Pat::Ident(binding) => {
            names.insert(binding.id.sym.to_string());
        }
        Pat::Array(array) => {
            for item in array.elems.iter().flatten() {
                binding_names(item, names);
            }
        }
        Pat::Object(object) => {
            for property in &object.props {
                match property {
                    ObjectPatProp::KeyValue(property) => binding_names(&property.value, names),
                    ObjectPatProp::Assign(property) => {
                        names.insert(property.key.id.sym.to_string());
                    }
                    ObjectPatProp::Rest(rest) => binding_names(&rest.arg, names),
                }
            }
        }
        Pat::Assign(assign) => binding_names(&assign.left, names),
        Pat::Rest(rest) => binding_names(&rest.arg, names),
        _ => {}
    }
}

fn emit_expression(expression: Expr) -> Result<String> {
    use swc_core::ecma::codegen::{Config, Emitter, text_writer::JsWriter};
    let cm = Default::default();
    let mut bytes = Vec::new();
    let mut program = Program::Script(Script {
        span: DUMMY_SP,
        body: vec![Stmt::Expr(ExprStmt {
            span: DUMMY_SP,
            expr: Box::new(expression),
        })],
        shebang: None,
    });
    program.visit_mut_with(&mut fixer(None));
    Emitter {
        cfg: Config::default().with_minify(true),
        cm: std::clone::Clone::clone(&cm),
        comments: None,
        wr: JsWriter::new(cm, "", &mut bytes, None),
    }
    .emit_program(&program)
    .map_err(|e| error(e.to_string()))?;
    Ok(String::from_utf8(bytes)
        .map_err(|e| error(e.to_string()))?
        .trim_end_matches(';')
        .to_string())
}
