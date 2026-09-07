//! Final-source hardening without wrapping the application's lexical scope.
//!
//! The integrity check detects changes to its probe and to unambiguous top-level
//! function declarations. It does not attest arbitrary top-level statements or
//! provide a security boundary against an attacker who can replace the guard.
//! Checks fail with an exception rather than an unbounded CPU loop. Debug timers
//! are unreferenced in Node so enabling protection does not keep a CLI alive.

use mangler_config::AntiTamperConfig;
use mangler_core::hash::djb2_utf16;
use mangler_core::{Language, NameAllocator, Result, Rng};
use mangler_jsast::directives::is_directive;
use mangler_jsast::{Js, ParseOpts};
use std::collections::HashMap;
use swc_core::common::{SourceMapper, Spanned};
use swc_core::ecma::ast::{Decl, ModuleDecl, ModuleItem, Program, Stmt};

fn debug_protection_snippet(rng: &mut Rng, names: &mut NameAllocator) -> String {
    let f = names.fresh();
    let timer = names.fresh();
    let period = 2000 + (rng.random_u32() % 4000);
    format!(
        "(function(){{try{{var {f}=function(){{try{{(function(){{}}).constructor('debugger')();}}catch(e){{}}}};{f}();var {timer}=setInterval({f},{period});if({timer}&&typeof {timer}.unref==='function'){timer}.unref();}}catch(e){{}}}})();"
    )
}

fn self_defending_snippet(
    names: &mut NameAllocator,
    functions: &[(String, String)],
    rng: &mut Rng,
) -> String {
    let check = names.fresh();
    let probe = names.fresh();
    let nonce = rng.random_u32();
    let probe_src = format!("function {probe}(){{return {nonce};}}");
    let mut tests = format!("{check}({probe})!=={}", djb2_utf16(&probe_src));
    for (name, source) in functions {
        tests.push_str(&format!("||{check}({name})!=={}", djb2_utf16(source)));
    }
    format!(
        "(function(){{function {check}(f){{var s=(function(){{}}).toString.call(f),h=5381,k=0;for(;k<s.length;k++)h=(h*33+s.charCodeAt(k))>>>0;return h;}}{probe_src}if({tests})throw 'Mangler integrity check failed';}})();"
    )
}

/// Finalize the exact emitted source, inserting guards after its directives and
/// shebang. No re-emission happens after checksums are calculated.
pub fn wrap(
    output: String,
    cfg: &AntiTamperConfig,
    eff_seed: u64,
    opts: &ParseOpts,
) -> Result<String> {
    if !cfg.self_defending && !cfg.debug_protection {
        return Ok(output);
    }
    let ast = Js.parse(&output, opts)?;
    let cm = ast.source_map();
    let mut names = NameAllocator::new(eff_seed);
    names.reserve(crate::seed::effective_seed_and_idents(ast.program(), eff_seed).1);
    let mut rng = Rng::for_pass(eff_seed, "anti-tamper");
    let mut declarations = Vec::new();
    let mut directive_end = None;
    let mut in_directives = true;
    let mut inspect = |stmt: &Stmt| {
        if in_directives && is_directive(stmt) {
            directive_end = Some(stmt.span().hi);
        } else {
            in_directives = false;
        }
        if let Stmt::Decl(Decl::Fn(decl)) = stmt
            && let Ok(source) = cm.span_to_snippet(decl.function.span)
        {
            declarations.push((decl.ident.sym.to_string(), source));
        }
    };
    match ast.program() {
        Program::Script(script) => {
            for stmt in &script.body {
                inspect(stmt);
            }
        }
        Program::Module(module) => {
            for item in &module.body {
                match item {
                    ModuleItem::Stmt(stmt) => inspect(stmt),
                    // Exported declarations are hoisted like ordinary functions.
                    ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => {
                        inspect(&Stmt::Decl(export.decl.clone()));
                    }
                    _ => {
                        inspect(&Stmt::Empty(swc_core::ecma::ast::EmptyStmt {
                            span: swc_core::common::DUMMY_SP,
                        }));
                    }
                }
            }
        }
    }
    // Duplicate declarations resolve to the last definition, not each source
    // occurrence. Omit ambiguous names instead of rejecting valid scripts.
    let mut counts = HashMap::new();
    for (name, _) in &declarations {
        *counts.entry(name.clone()).or_insert(0) += 1;
    }
    declarations.retain(|(name, _)| counts[name] == 1);
    let mut prefix = String::new();
    if cfg.debug_protection {
        prefix.push_str(&debug_protection_snippet(&mut rng, &mut names));
    }
    if cfg.self_defending {
        prefix.push_str(&self_defending_snippet(&mut names, &declarations, &mut rng));
    }
    let at = directive_end
        .map(|pos| cm.lookup_byte_offset(pos).pos.0 as usize)
        .unwrap_or_else(|| {
            if output.starts_with("#!") {
                output.find('\n').map_or(output.len(), |at| at + 1)
            } else {
                0
            }
        });
    let mut result = String::with_capacity(output.len() + prefix.len() + 1);
    result.push_str(&output[..at]);
    // Source may have an ASI-terminated directive or an unterminated shebang.
    if directive_end.is_some() && !output[..at].ends_with(';') {
        result.push(';');
    } else if at > 0 && !output[..at].ends_with([';', '\n']) {
        result.push('\n');
    }
    result.push_str(&prefix);
    result.push_str(&output[at..]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(self_defending: bool, debug_protection: bool) -> AntiTamperConfig {
        AntiTamperConfig {
            self_defending,
            debug_protection,
        }
    }

    fn harden(src: &str, cfg: &AntiTamperConfig) -> String {
        wrap(src.to_string(), cfg, 7, &ParseOpts::default()).unwrap()
    }

    #[test]
    fn disabled_is_a_no_op() {
        assert_eq!(harden("CODE;", &at(false, false)), "CODE;");
    }

    #[test]
    fn wrap_preserves_shebang_and_directives() {
        let src = "#!/usr/bin/env node\n'use strict';'custom';console.log(42);";
        let output = harden(src, &at(true, true));
        assert!(output.starts_with("#!/usr/bin/env node\n'use strict';'custom';"));
        assert!(
            Js::reparse(&output, &ParseOpts::default()).is_ok(),
            "{output}"
        );
        assert_eq!(output, harden(src, &at(true, true)));
    }

    #[test]
    fn guards_execute_without_changing_strict_this() {
        let src = "'use strict'; function pay(){return this===undefined;} globalThis.__out=pay();";
        mangler_testkit::assert_behaviorally_equal(src, &harden(src, &at(true, true)));
    }

    #[test]
    fn detects_application_function_mutation() {
        let output = harden(
            "function pay(){return 42;}globalThis.__out=pay();",
            &at(true, false),
        );
        let changed = output.replace("return 42", "return 43");
        assert_ne!(output, changed);
        mangler_testkit::assert_behaviorally_equal(
            "throw 'Mangler integrity check failed';",
            &changed,
        );
    }

    #[test]
    fn preserves_duplicate_declaration_semantics() {
        let src = "function pay(){return 1;} function pay(){return 2;}globalThis.__out=pay();";
        mangler_testkit::assert_behaviorally_equal(src, &harden(src, &at(true, false)));
    }
    #[test]
    fn asi_directives_and_unicode_function_sources_remain_valid() {
        let src = "'use strict' // directive comment\nfunction pay(){return 'paid:😀';}globalThis.__out=pay();";
        mangler_testkit::assert_behaviorally_equal(src, &harden(src, &at(true, false)));
    }
}
