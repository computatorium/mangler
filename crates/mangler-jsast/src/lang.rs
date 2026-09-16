//! The JS/TS [`Language`] implementation over swc.
//!
//! [`Js`] is the front-end the generic pipeline (WP2) plugs in. Its [`Ast`](Ast)
//! wraps the swc [`Program`] **together with the [`SourceMap`]** it was parsed
//! against, because every later stage — passes that re-parse a generated snippet,
//! the resolver, the minifier, and codegen — needs that same `SourceMap`. Bundling
//! them means a pass receives one owned object it can mutate
//! ([`Ast::program_mut`]) and hand to codegen without re-threading the map.
//!
//! ## Dialect is explicit
//!
//! The legacy driver sniffed TS/JSX off the **filename** (`.ts`/`.tsx`). That is a
//! policy decision that does not belong in the parser, so it moves out into
//! [`ParseOpts`]: the caller (config / CLI) decides `{ typescript, jsx, module }`
//! and the parser obeys. [`ParseOpts::from_filename`] is offered as a convenience
//! for callers that *want* the old behavior, but the seam no longer assumes it.
//!
//! ## What lives here vs. in passes
//!
//! `parse` / `print` are the [`Language`] contract. Beyond it, this module also
//! exposes the shared swc plumbing every pass pipeline needs but should not
//! re-implement: [`Js::resolve`] (the single resolver run that assigns
//! `SyntaxContext` marks) and [`Js::print_optimized`] (minify + `fixer` + emit).
//! Codegen of *generated runtime code* is in [`crate::codegen`].

use crate::span::injected_span;
use mangler_core::{Error, Language, Result};
use swc_core::common::sync::Lrc;
use swc_core::common::{FileName, GLOBALS, Mark, SourceMap};
use swc_core::ecma::ast::{Class, EsVersion, Expr, Lit, Module, Program, PropName, Stmt};
use swc_core::ecma::codegen::{Config as CodegenConfig, Emitter, text_writer::JsWriter};
use swc_core::ecma::minifier::optimize;
use swc_core::ecma::minifier::option::{
    CompressOptions, ExtraOptions, MangleOptions, MinifyOptions,
};
use swc_core::ecma::parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};
use swc_core::ecma::transforms::base::{
    fixer::fixer,
    hygiene::{Config as HygieneConfig, hygiene_with_config},
    resolver,
};
use swc_core::ecma::visit::{Visit, VisitMutWith, VisitWith};

/// An explicit ECMAScript grammar goal. Unlike automatic program parsing,
/// Script never accepts module declarations or module-only top-level await.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseGoal {
    Script,
    Module,
}

/// Explicit parse configuration. Dialect is a **decision the caller makes**, not
/// a filename guess (the legacy `.ts`/`.tsx` sniffing is gone from the seam).
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseOpts {
    /// Parse TypeScript syntax (type annotations, `as`, enums, …).
    pub typescript: bool,
    /// Allow JSX (`tsx` when combined with `typescript`).
    pub jsx: bool,
    /// Parse as an ES module (allows `import`/`export`/top-level await). When
    /// `false`, swc still auto-detects modules from `import`/`export`, but this
    /// flag forces module mode for ambiguous scripts.
    pub module: bool,
}

impl ParseOpts {
    /// Convenience mirroring the legacy filename-sniffing behavior, for callers
    /// that still want it. New callers should set the flags explicitly.
    pub fn from_filename(filename: &str) -> Self {
        let typescript = filename.ends_with(".ts")
            || filename.ends_with(".tsx")
            || filename.ends_with(".mts")
            || filename.ends_with(".cts");
        let jsx = filename.ends_with(".jsx") || filename.ends_with(".tsx");
        ParseOpts {
            typescript,
            jsx,
            module: filename.ends_with(".mjs") || filename.ends_with(".mts"),
        }
    }

    /// The swc [`Syntax`] this dialect selects.
    fn syntax(&self) -> Syntax {
        if self.typescript {
            Syntax::Typescript(TsSyntax {
                tsx: self.jsx,
                ..Default::default()
            })
        } else {
            Syntax::Es(EsSyntax {
                jsx: self.jsx,
                explicit_resource_management: true,
                ..Default::default()
            })
        }
    }
}

/// A parsed JS/TS program plus the [`SourceMap`] it was parsed against.
///
/// Passes mutate the [`Program`] via [`program_mut`](Ast::program_mut); codegen and
/// the resolver read the [`SourceMap`] via [`source_map`](Ast::source_map). Keeping
/// them together is what lets a pass re-parse a generated snippet against the same
/// map and splice it in (see WP4/WP6).
pub struct Ast {
    program: Program,
    source_map: Lrc<SourceMap>,
    /// Whether the source was parsed as TypeScript — the resolver needs this to
    /// handle TS-specific scoping (e.g. type-only references).
    typescript: bool,
    runtime_bindings: std::collections::HashSet<String>,
    native_safety: CompressionSafety<'static>,
}

impl std::fmt::Debug for Ast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ast")
            .field("typescript", &self.typescript)
            .finish_non_exhaustive()
    }
}

impl Ast {
    /// Register compiler-owned runtime declarations for compression analysis.
    /// Their computed indices/keys are controlled by the runtime. Constant pools
    /// remain source-bearing and are inspected independently of their container.
    pub fn register_runtime(&mut self, statements: &[Stmt], table: Option<&str>) {
        use swc_core::ecma::ast::{Decl, Pat};
        for statement in statements {
            match statement {
                Stmt::Decl(Decl::Fn(function)) => {
                    self.runtime_bindings.insert(function.ident.sym.to_string());
                }
                Stmt::Decl(Decl::Var(declaration)) => {
                    for variable in &declaration.decls {
                        if let Pat::Ident(binding) = &variable.name {
                            self.runtime_bindings.insert(binding.id.sym.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(table) = table {
            struct NativeConstants<'a, 'b> {
                table: &'a str,
                safety: &'b mut CompressionSafety<'static>,
            }
            impl Visit for NativeConstants<'_, '_> {
                fn visit_var_declarator(
                    &mut self,
                    declaration: &swc_core::ecma::ast::VarDeclarator,
                ) {
                    if matches!(&declaration.name, Pat::Ident(binding) if binding.id.sym == self.table)
                    {
                        if let Some(Expr::Array(chunks)) = declaration.init.as_deref() {
                            for chunk in chunks.elems.iter().flatten() {
                                if let Expr::Array(fields) = chunk.expr.as_ref()
                                    && let Some(Some(constants)) = fields.elems.get(1)
                                {
                                    constants.expr.visit_with(self.safety);
                                    self.safety.inferred_names |=
                                        crate::callable_names::native_inferred(&constants.expr);
                                }
                            }
                        }
                        return;
                    }
                    declaration.visit_children_with(self);
                }
                fn visit_bin_expr(&mut self, binary: &swc_core::ecma::ast::BinExpr) {
                    crate::deep::walk_binary(binary, self);
                }
            }
            // Runtime isolation can nest the table inside a private bootstrap.
            // Inspect source constants before excluding that bootstrap from the
            // generated-code analysis; retain only the bounded safety summary.
            statements.visit_with(&mut NativeConstants {
                table,
                safety: &mut self.native_safety,
            });
        }
    }
    /// Shared read access to the [`Program`].
    pub fn program(&self) -> &Program {
        &self.program
    }

    /// Mutable access to the [`Program`] — the handle a pass mutates in place.
    pub fn program_mut(&mut self) -> &mut Program {
        &mut self.program
    }

    /// The [`SourceMap`] this program was parsed against. Codegen, the resolver,
    /// and any re-parse of a generated snippet must share this map.
    pub fn source_map(&self) -> Lrc<SourceMap> {
        self.source_map.clone()
    }

    /// Whether this AST was parsed as TypeScript (drives the resolver flag).
    pub fn is_typescript(&self) -> bool {
        self.typescript
    }

    /// Consume the wrapper, yielding the owned [`Program`].
    pub fn into_program(self) -> Program {
        self.program
    }
}

/// The JS/TS language front-end.
pub struct Js;

impl Language for Js {
    type Ast = Ast;
    type ParseOpts = ParseOpts;
    const ID: &'static str = "js";

    /// Parse `src` into an [`Ast`]. swc parse errors map to [`Error::parse`].
    fn parse(&self, src: &str, opts: &ParseOpts) -> Result<Ast> {
        self.parse_selected_goal(src, opts, opts.module.then_some(ParseGoal::Module))
    }

    /// Minified, ascii-only emit — the production codegen path (no minifier pass;
    /// see [`Js::print_optimized`] for the full optimize+emit).
    fn print(&self, ast: &Ast) -> String {
        emit(&ast.program, &ast.source_map, true)
    }
}

impl Js {
    /// Parse with the exact Script or Module grammar goal, overriding
    /// `ParseOpts::module`. This performs parsing and shared early-error
    /// validation only; it never resolves imports, transforms, or executes code.
    /// Parser rejection is returned as the typed [`Error::Parse`] variant.
    pub fn parse_with_goal(&self, src: &str, opts: &ParseOpts, goal: ParseGoal) -> Result<Ast> {
        self.parse_selected_goal(src, opts, Some(goal))
    }

    fn parse_selected_goal(
        &self,
        src: &str,
        opts: &ParseOpts,
        goal: Option<ParseGoal>,
    ) -> Result<Ast> {
        let cm: Lrc<SourceMap> = Default::default();
        let fm = cm.new_source_file(
            Lrc::new(FileName::Custom(format!("{}.js", Self::ID))),
            src.to_string(),
        );
        let lexer = Lexer::new(
            opts.syntax(),
            EsVersion::EsNext,
            StringInput::from(&*fm),
            None,
        );
        let mut parser = Parser::new_from(lexer);
        let mut program = match goal {
            Some(ParseGoal::Script) => parser.parse_script().map(Program::Script),
            Some(ParseGoal::Module) => parser.parse_module().map(Program::Module),
            None => parser.parse_program(),
        }
        .map_err(|e| Error::parse(Self::ID, format!("{e:?}")))?;
        let mut errors = parser.take_errors();
        if !opts.typescript {
            Self::repair_annex_b(&mut program, false, &mut errors);
        }
        if let Some(error) = errors.into_iter().next() {
            return Err(Error::parse(Self::ID, format!("{error:?}")));
        }
        Self::repair_pattern_elisions(&mut program, src, fm.start_pos);
        Self::validate_resource_scopes(&program)?;
        Ok(Ast {
            program,
            source_map: cm,
            typescript: opts.typescript,
            runtime_bindings: Default::default(),
            native_safety: Default::default(),
        })
    }
}

// SWC accepts direct case-clause resource declarations, but ContainsUsing makes
// them early errors. A nested block owns its disposal scope and remains valid.
// https://tc39.es/ecma262/#sec-switch-statement-static-semantics-early-errors
#[derive(Default)]
struct ResourceCaseValidation {
    invalid: bool,
}
impl Visit for ResourceCaseValidation {
    fn visit_bin_expr(&mut self, expression: &swc_core::ecma::ast::BinExpr) {
        crate::deep::walk_binary(expression, self);
    }

    fn visit_switch_case(&mut self, case: &swc_core::ecma::ast::SwitchCase) {
        self.invalid |= case
            .cons
            .iter()
            .any(|statement| matches!(statement, Stmt::Decl(swc_core::ecma::ast::Decl::Using(_))));
        if !self.invalid {
            case.visit_children_with(self);
        }
    }
}

impl Js {
    /// Recover assignment-pattern elisions omitted by SWC's expression reparse.
    /// Custom parser contexts, including direct eval, use the same source repair.
    pub fn repair_pattern_elisions(
        program: &mut Program,
        source: &str,
        start: swc_core::common::BytePos,
    ) {
        crate::pattern_elisions::repair(program, source, start);
    }

    /// Supplement SWC's resource declaration checks for callers using a custom
    /// parser context, including direct eval. Nested blocks own disposal scopes.
    pub fn validate_resource_scopes(program: &Program) -> Result<()> {
        let mut resource_cases = ResourceCaseValidation::default();
        program.visit_with(&mut resource_cases);
        if resource_cases.invalid {
            return Err(Error::parse(
                Self::ID,
                "using declarations in switch cases require an enclosing block",
            ));
        }
        Ok(())
    }

    /// Repair named-class heritage and shared switch lexical scopes after a
    /// resolver run. Native capture factories use the same scope rules.
    pub fn repair_resolver_scopes(program: &mut Program) {
        program.visit_mut_with(&mut crate::class_scope::RepairClassHeritage);
        program.visit_mut_with(&mut crate::switch_scope::RepairSwitchBindings);
    }

    /// Parse-only verification: confirm `src` is syntactically valid JS/TS under
    /// `opts`. Used by `--verify` to catch a pass that emitted malformed code.
    /// No `GLOBALS`/resolver needed — it is a pure parse.
    pub fn reparse(src: &str, opts: &ParseOpts) -> Result<()> {
        Js.parse(src, opts).map(|_| ())
    }

    /// Run the single swc resolver pass that assigns `SyntaxContext` marks,
    /// returning the `(unresolved, top_level)` marks the post-resolver passes and
    /// the minifier need. Must run inside a [`GLOBALS`] scope (the caller owns the
    /// `GLOBALS.set(...)` so all marks share one interner).
    pub fn resolve(ast: &mut Ast) -> (Mark, Mark) {
        let unresolved = Mark::new();
        let top_level = Mark::new();
        ast.program
            .visit_mut_with(&mut resolver(unresolved, top_level, ast.typescript));
        Self::repair_resolver_scopes(&mut ast.program);
        (unresolved, top_level)
    }

    /// The production tail: swc minify (`optimize`) → `fixer` → minified ascii
    /// emit. `marks` are the resolver's `(unresolved, top_level)`. `mangle`
    /// toggles swc's built-in name mangle (`top_level=false`, so globals are never
    /// renamed); `reserved` names survive it. Must run inside [`GLOBALS`].
    pub fn print_optimized(
        ast: Ast,
        marks: (Mark, Mark),
        mangle: bool,
        reserved: &[String],
    ) -> String {
        let (unresolved_mark, top_level_mark) = marks;
        let cm = ast.source_map.clone();
        let mut safety = CompressionSafety {
            runtime_bindings: Some(&ast.runtime_bindings),
            ..ast.native_safety
        };
        ast.program.visit_with(&mut safety);
        let keep_callable_names = crate::callable_names::observable(&ast.program, top_level_mark);
        let keep_inferred_names = safety.inferred_names
            || crate::callable_names::inferred_observable(&ast.program, top_level_mark);
        let operators = crate::compression_guards::Operators::new(unresolved_mark);
        let mut source_program = ast.program;
        safety.immutable_writes |= operators.protect(&mut source_program);
        let mut program = optimize(
            source_program,
            cm.clone(),
            None,
            None,
            &MinifyOptions {
                compress: Some(CompressOptions {
                    drop_debugger: false,
                    keep_classnames: true,
                    keep_fnames: keep_callable_names,
                    // SWC's return merging drops directives independently of
                    // its directives option. DCE also overlooks key coercion and
                    // class-heritage exceptions. Limit those passes to programs
                    // without the affected constructs; other compression stays on.
                    directives: false,
                    if_return: !safety.directives,
                    // The unused pass drops writes to const and class inner
                    // bindings; those writes must retain their required errors.
                    unused: !safety.observable_initializers && !safety.immutable_writes,
                    dead_code: !safety.observable_initializers,
                    side_effects: !safety.observable_initializers,
                    evaluate: !safety.intrinsic_calls,
                    bools: !safety.binding_delete,
                    inline: if safety.observable_initializers || keep_inferred_names {
                        0
                    } else {
                        3
                    },
                    switches: !safety.lexical_switch,
                    reduce_vars: !safety.lexical_switch
                        && !safety.observable_initializers
                        && !keep_inferred_names,
                    collapse_vars: !safety.lexical_switch
                        && !safety.observable_initializers
                        && !keep_inferred_names,
                    typeofs: !safety.lexical_switch,
                    ..Default::default()
                }),
                mangle: if mangle {
                    Some(MangleOptions {
                        top_level: Some(false),
                        keep_class_names: true,
                        keep_fn_names: keep_callable_names,
                        reserved: reserved.iter().map(|s| s.as_str().into()).collect(),
                        ..Default::default()
                    })
                } else {
                    None
                },
                ..Default::default()
            },
            &ExtraOptions {
                unresolved_mark,
                top_level_mark,
                mangle_name_cache: None,
            },
        );
        operators.restore(&mut program);
        // Class declaration compression introduces an immutable named-expression
        // scope. Repair references before hygiene materializes its distinct name.
        program.visit_mut_with(&mut crate::class_scope::RepairClassHeritage);
        // Inlining can bring bindings with distinct resolver contexts into the
        // same textual scope, even when mangling is disabled or names reserved.
        // Materialize those identities before emitting JavaScript identifiers.
        program.visit_mut_with(&mut hygiene_with_config(HygieneConfig {
            keep_class_names: true,
            top_level_mark,
            ..Default::default()
        }));
        program.visit_mut_with(&mut fixer(None));
        emit(&program, &cm, true)
    }

    /// Run `body` inside a fresh swc [`GLOBALS`] scope. Every resolver run, mark
    /// allocation, and `optimize` call for one file must share ONE `GLOBALS`, so
    /// the pipeline driver wraps its whole per-file flow in this.
    pub fn with_globals<R>(body: impl FnOnce() -> R) -> R {
        GLOBALS.set(&Default::default(), body)
    }
}

/// Narrow guard around upstream optimizer assumptions that fail on observable
/// initialization. Keep this at the terminal compression boundary so every pass
/// and caller receives the same semantics.
#[derive(Clone, Copy, Default)]
struct CompressionSafety<'a> {
    runtime_bindings: Option<&'a std::collections::HashSet<String>>,
    directives: bool,
    observable_initializers: bool,
    immutable_writes: bool,
    intrinsic_calls: bool,
    lexical_switch: bool,
    binding_delete: bool,
    inferred_names: bool,
}

impl Visit for CompressionSafety<'_> {
    fn visit_bin_expr(&mut self, binary: &swc_core::ecma::ast::BinExpr) {
        crate::deep::walk_binary(binary, self);
    }

    fn visit_expr(&mut self, expression: &Expr) {
        if crate::span::is_runtime_span(swc_core::common::Spanned::span(expression)) {
            self.directives = true;
            return;
        }
        expression.visit_children_with(self);
    }

    fn visit_fn_decl(&mut self, function: &swc_core::ecma::ast::FnDecl) {
        if self
            .runtime_bindings
            .is_some_and(|names| names.contains(function.ident.sym.as_ref()))
        {
            self.directives = true;
        } else {
            function.visit_children_with(self);
        }
    }

    fn visit_var_declarator(&mut self, declaration: &swc_core::ecma::ast::VarDeclarator) {
        if let swc_core::ecma::ast::Pat::Ident(binding) = &declaration.name
            && self
                .runtime_bindings
                .is_some_and(|names| names.contains(binding.id.sym.as_ref()))
        {
            self.directives = true;
            return;
        }
        declaration.visit_children_with(self);
    }

    fn visit_unary_expr(&mut self, unary: &swc_core::ecma::ast::UnaryExpr) {
        self.binding_delete |= unary.op == swc_core::ecma::ast::UnaryOp::Delete
            && matches!(unary.arg.as_ref(), Expr::Ident(_));
        unary.visit_children_with(self);
    }

    fn visit_ident(&mut self, ident: &swc_core::ecma::ast::Ident) {
        // The upstream evaluator folds calls to these mutable bindings without
        // proving their identity. Aliases may become direct calls during inlining.
        self.intrinsic_calls |= matches!(
            ident.sym.as_ref(),
            "String" | "RegExp" | "Math" | "Number" | "Boolean" | "Object" | "Array"
        );
    }

    fn visit_member_expr(&mut self, member: &swc_core::ecma::ast::MemberExpr) {
        if let swc_core::ecma::ast::MemberProp::Computed(key) = &member.prop {
            self.observable_initializers |=
                !matches!(key.expr.as_ref(), Expr::Lit(lit) if !matches!(lit, Lit::Regex(_)));
        }
        member.visit_children_with(self);
    }

    fn visit_switch_stmt(&mut self, switch: &swc_core::ecma::ast::SwitchStmt) {
        use swc_core::ecma::ast::{Decl, VarDeclKind};
        let lexical = switch.cases.iter().flat_map(|case| &case.cons).any(|stmt| {
            matches!(stmt, Stmt::Decl(Decl::Var(v)) if v.kind != VarDeclKind::Var)
                || matches!(stmt, Stmt::Decl(Decl::Class(_) | Decl::Fn(_)))
        });
        self.lexical_switch |= lexical;
        self.observable_initializers |= lexical;
        switch.visit_children_with(self);
    }

    fn visit_stmts(&mut self, statements: &[Stmt]) {
        self.directives |= statements
            .first()
            .is_some_and(crate::directives::is_directive);
        statements.visit_children_with(self);
    }

    fn visit_module(&mut self, module: &Module) {
        self.directives |= module.body.first().is_some_and(|item| {
            matches!(item, swc_core::ecma::ast::ModuleItem::Stmt(stmt) if crate::directives::is_directive(stmt))
        });
        module.visit_children_with(self);
    }

    fn visit_prop_name(&mut self, name: &PropName) {
        // Primitive literal keys do not invoke user coercion. All other keys can
        // execute Symbol.toPrimitive/toString before the property's value.
        if let PropName::Computed(key) = name {
            self.observable_initializers |=
                !matches!(key.expr.as_ref(), Expr::Lit(lit) if !matches!(lit, Lit::Regex(_)));
        }
        name.visit_children_with(self);
    }

    fn visit_class_expr(&mut self, class: &swc_core::ecma::ast::ClassExpr) {
        if let Some(name) = &class.ident {
            self.immutable_writes |= crate::class_scope::inner_name_is_written(name, &class.class);
        }
        class.visit_children_with(self);
    }

    fn visit_class_decl(&mut self, class: &swc_core::ecma::ast::ClassDecl) {
        self.immutable_writes |=
            crate::class_scope::inner_name_is_written(&class.ident, &class.class);
        class.visit_children_with(self);
    }

    fn visit_class(&mut self, class: &Class) {
        // Even an unused heritage expression can throw (including a reference
        // to the class's own still-uninitialized name).
        self.observable_initializers |= class.super_class.is_some();
        class.visit_children_with(self);
    }
}

/// Emit `program` to source. `minify` selects the minified, ascii-only production
/// config (the only config used today); the `injected_span` seam means a future
/// source-map emit is localized.
fn emit(program: &Program, cm: &Lrc<SourceMap>, minify: bool) -> String {
    // Touch the span seam so the source-map TODO is anchored to one import site.
    let _ = injected_span();
    let mut buf = Vec::new();
    {
        let wr = JsWriter::new(cm.clone(), "\n", &mut buf, None);
        let mut emitter = Emitter {
            cfg: CodegenConfig::default()
                .with_minify(minify)
                .with_ascii_only(minify),
            cm: cm.clone(),
            comments: None,
            wr,
        };
        // emit_program is infallible for a well-formed in-memory program; a write
        // error would only come from the in-memory Vec, which cannot fail.
        emitter
            .emit_program(program)
            .expect("emitting a well-formed program to an in-memory buffer cannot fail");
    }
    String::from_utf8(buf).expect("swc ascii-only codegen emits valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_print_round_trips_and_minifies() {
        let ast = Js
            .parse("// hi\nconst x = 1 + 2;\n", &ParseOpts::default())
            .unwrap();
        let out = Js.print(&ast);
        assert!(!out.contains("hi"), "comment stripped: {out}");
        assert!(!out.contains('\n'), "minified single line: {out}");
        assert!(
            out.contains("1+2") || out.contains('3'),
            "expr present: {out}"
        );
    }

    #[test]
    fn parse_error_maps_to_error_parse() {
        let err = Js.parse("function (", &ParseOpts::default()).unwrap_err();
        assert!(matches!(err, Error::Parse { ref lang, .. } if lang == "js"));
    }

    #[test]
    fn typescript_dialect_is_explicit_not_filename() {
        // TS-only syntax parses iff typescript=true — independent of any filename.
        let ts = ParseOpts {
            typescript: true,
            ..Default::default()
        };
        // TS-only syntax (`interface`) parses under the TS dialect...
        assert!(Js.parse("interface I { x: number }", &ts).is_ok());
        // ...and must FAIL under the default (ES) dialect, proving the dialect
        // comes from opts, not a filename.
        assert!(
            Js.parse("interface I { x: number }", &ParseOpts::default())
                .is_err()
        );
    }

    #[test]
    fn jsx_dialect_is_explicit() {
        let jsx = ParseOpts {
            jsx: true,
            ..Default::default()
        };
        assert!(Js.parse("const e = <div/>;", &jsx).is_ok());
        assert!(
            Js.parse("const e = <div/>;", &ParseOpts::default())
                .is_err()
        );
    }

    #[test]
    fn from_filename_convenience_matches_legacy() {
        assert!(ParseOpts::from_filename("a.ts").typescript);
        assert!(ParseOpts::from_filename("a.tsx").jsx);
        assert!(ParseOpts::from_filename("a.jsx").jsx);
        assert!(!ParseOpts::from_filename("a.js").typescript);
    }

    #[test]
    fn shebang_keeps_its_required_line_break() {
        let ast = Js
            .parse(
                "#!/usr/bin/env node\nconsole.log(42);",
                &ParseOpts::default(),
            )
            .unwrap();
        let out = Js.print(&ast);
        assert!(out.starts_with("#!/usr/bin/env node\n"), "{out}");
        let parsed = Js.parse(&out, &ParseOpts::default()).unwrap();
        let Program::Script(script) = parsed.program() else {
            panic!("script expected")
        };
        assert_eq!(
            script.body.len(),
            1,
            "shebang must not swallow executable statements"
        );
    }

    #[test]
    fn module_option_enforces_implicit_strictness() {
        let opts = ParseOpts {
            module: true,
            ..Default::default()
        };
        let ast = Js.parse("const value = 1;", &opts).unwrap();
        assert!(matches!(ast.program(), Program::Module(_)));
        assert!(Js.parse("with ({}) {}", &opts).is_err());
        assert!(Js.parse("with ({}) {}", &ParseOpts::default()).is_ok());
        assert!(ParseOpts::from_filename("entry.mjs").module);
        assert!(ParseOpts::from_filename("entry.mts").typescript);
        assert!(ParseOpts::from_filename("entry.mts").module);
        assert!(ParseOpts::from_filename("entry.cts").typescript);
    }

    #[test]
    fn recoverable_parser_errors_are_rejected() {
        assert!(Js.parse("return 1;", &ParseOpts::default()).is_err());
    }

    #[test]
    fn switch_resource_declarations_require_a_block() {
        for source in [
            "function f(){switch(0){case 0:using resource=null;}}",
            "function f(){switch(0){default:using resource=null;}}",
            "async function f(){switch(0){case 0:await using resource=null;}}",
            "function f(){return ()=>{switch(0){default:using resource=null;}}}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
        }
        for source in [
            "function f(){switch(0){case 0:{using resource=null;}}}",
            "async function f(){switch(0){default:{await using resource=null;}}}",
            "function f(){switch(0){case 0:function g(){using resource=null;}}}",
        ] {
            assert!(Js.parse(source, &ParseOpts::default()).is_ok(), "{source}");
        }
    }

    #[test]
    fn reparse_accepts_valid_rejects_malformed() {
        assert!(Js::reparse("const x = 1;", &ParseOpts::default()).is_ok());
        assert!(Js::reparse("function (", &ParseOpts::default()).is_err());
    }

    #[test]
    fn print_optimized_mangles_locals_not_globals() {
        let src =
            "function f(){ var localVariable = 5; return localVariable + window.GLOBAL_THING; }";
        let out = Js::with_globals(|| {
            let mut ast = Js.parse(src, &ParseOpts::default()).unwrap();
            let marks = Js::resolve(&mut ast);
            Js::print_optimized(ast, marks, true, &[])
        });
        assert!(!out.contains("localVariable"), "local renamed: {out}");
        assert!(out.contains("GLOBAL_THING"), "global preserved: {out}");
    }

    #[test]
    fn print_optimized_respects_reserved() {
        // The local must be observably used in a way swc cannot inline away, so it
        // survives to be (otherwise) mangled — proving `reserved` is what keeps it.
        let src = "function f(x){ var keepThisName = x + 1; sink(keepThisName); return keepThisName; } f(window.q);";
        let out = Js::with_globals(|| {
            let mut ast = Js.parse(src, &ParseOpts::default()).unwrap();
            let marks = Js::resolve(&mut ast);
            Js::print_optimized(ast, marks, true, &["keepThisName".to_string()])
        });
        assert!(
            out.contains("keepThisName"),
            "reserved name preserved: {out}"
        );
    }
    #[test]
    fn explicit_function_strictness_survives_optimization() {
        let src = "function f(){'use strict';return this===undefined};console.log(f())";
        let out = Js::with_globals(|| {
            let mut ast = Js.parse(src, &ParseOpts::default()).unwrap();
            let marks = Js::resolve(&mut ast);
            Js::print_optimized(ast, marks, true, &[])
        });
        assert!(out.contains("use strict"), "{out}");
    }
}
