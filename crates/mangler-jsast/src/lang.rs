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
use swc_core::ecma::transforms::base::{fixer::fixer, resolver};
use swc_core::ecma::visit::{Visit, VisitMutWith, VisitWith};

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
}

impl std::fmt::Debug for Ast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ast")
            .field("typescript", &self.typescript)
            .finish_non_exhaustive()
    }
}

impl Ast {
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
        let program = if opts.module {
            parser.parse_module().map(Program::Module)
        } else {
            parser.parse_program()
        }
        .map_err(|e| Error::parse(Self::ID, format!("{e:?}")))?;
        if let Some(error) = parser.take_errors().into_iter().next() {
            return Err(Error::parse(Self::ID, format!("{error:?}")));
        }
        Ok(Ast {
            program,
            source_map: cm,
            typescript: opts.typescript,
        })
    }

    /// Minified, ascii-only emit — the production codegen path (no minifier pass;
    /// see [`Js::print_optimized`] for the full optimize+emit).
    fn print(&self, ast: &Ast) -> String {
        emit(&ast.program, &ast.source_map, true)
    }
}

impl Js {
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
        ast.program
            .visit_mut_with(&mut crate::class_scope::RepairClassHeritage);
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
        let mut safety = CompressionSafety::default();
        ast.program.visit_with(&mut safety);
        let mut program = optimize(
            ast.program,
            cm.clone(),
            None,
            None,
            &MinifyOptions {
                compress: Some(CompressOptions {
                    drop_debugger: false,
                    // SWC's return merging drops directives independently of
                    // its directives option. DCE also overlooks key coercion and
                    // class-heritage exceptions. Limit those passes to programs
                    // without the affected constructs; other compression stays on.
                    directives: false,
                    if_return: !safety.directives,
                    unused: !safety.observable_initializers,
                    dead_code: !safety.observable_initializers,
                    side_effects: !safety.observable_initializers,
                    ..Default::default()
                }),
                mangle: if mangle {
                    Some(MangleOptions {
                        top_level: Some(false),
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
#[derive(Default)]
struct CompressionSafety {
    directives: bool,
    observable_initializers: bool,
}

impl Visit for CompressionSafety {
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
