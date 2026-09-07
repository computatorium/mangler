//! Injection of the TDZ / const-assignment guard helper functions.
//!
//! When the TDZ rewrite (see [`super::tdz`]) lowers any `let`/`const` binding in
//! a file, two helper functions are injected once at the top of the program.
//! Semantically they are always:
//!
//! ```js
//! function <tdz>(n){ throw new ReferenceError("Cannot access '" + n + "' before initialization"); }
//! function <cst>(n){ throw new TypeError("Assignment to constant variable."); }
//! ```
//!
//! …but the *surface form* is diversified per seed so the helpers do not present
//! a single grep-able structural / verbatim-string signature.
//!
//! Two axes of variation, both driven by the per-pass seeded RNG (so output stays
//! byte-identical per seed):
//!
//! * **Message assembly** (`MsgStyle`): the spec error text is never emitted as
//!   any readable string literal. Each segment is shipped as a numeric char-code
//!   array XOR'd with a seed-derived single-byte key and reconstructed at runtime
//!   by a tiny inline decoder. No contiguous OR fragmentary cleartext spec
//!   substring survives.
//! * **Helper ordering**: which of the two helpers is emitted first is also seeded.
//!
//! The error *type* is invariant (`ReferenceError` for TDZ, `TypeError` for const).
//!
//! The actual function names are seeded via `FileConfig::fresh_name` for
//! mangle-safety, so we build the source with the chosen names and parse it rather
//! than hand-rolling the AST.

use swc_core::common::sync::Lrc;
use swc_core::common::{FileName, SourceMap};
use swc_core::ecma::ast::*;
use swc_core::ecma::parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};

use crate::config::FileConfig;
use mangler_core::Rng;

/// Number of inline-decoder shapes the selector chooses among.
const MSG_STYLE_COUNT: usize = 3;

/// The seeded names of the two guard helpers for one file, plus the per-file
/// seeded surface-form choices that diversify their emitted skeleton.
#[derive(Clone)]
pub struct TdzHelpers {
    pub throw_tdz: String,
    pub throw_const: String,
    /// Inline-decoder shape index (`0..MSG_STYLE_COUNT`).
    msg_style: usize,
    /// Per-file XOR key byte (forced nonzero) woven into every encoded char-code.
    msg_key: u8,
    /// When true, the const helper is emitted before the TDZ helper.
    const_first: bool,
}

impl TdzHelpers {
    /// Allocates the two helper names (from `cfg`) and draws the seeded
    /// surface-form choices (from `rng`). Draw order is fixed (two names, then
    /// style, then key, then ordering) so the RNG sequence — and hence all
    /// downstream per-body name allocation — is identical regardless of which
    /// variant is later emitted.
    pub fn new(cfg: &FileConfig, rng: &mut Rng) -> Self {
        let throw_tdz = cfg.fresh_name();
        let throw_const = cfg.fresh_name();
        let msg_style = rng.pick(MSG_STYLE_COUNT);
        // Force the key nonzero (1..=255): a zero key would leave the char-code
        // array equal to the plaintext code points, reintroducing readable bytes.
        let msg_key = (rng.pick(255) + 1) as u8;
        let const_first = rng.pick(2) == 1;
        TdzHelpers {
            throw_tdz,
            throw_const,
            msg_style,
            msg_key,
            const_first,
        }
    }
}

/// Encodes `text` (ASCII spec text) as a JS array body of per-char codes XOR'd
/// with `key`, e.g. `120,33,9`.
fn xor_codes(text: &str, key: u8) -> String {
    text.bytes()
        .map(|b| (b ^ key).to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Builds a runtime string-expression that evaluates EXACTLY to `text`, with the
/// text shipped only as an XOR'd integer array — no cleartext byte remains.
fn assemble_message(text: &str, style: usize, key: u8, param: &str) -> String {
    let codes = xor_codes(text, key);
    // A non-foldable `0` derived from the (always-string) param.
    let zero = format!("({param}+\"\").slice(({param}+\"\").length).length");
    match style % MSG_STYLE_COUNT {
        0 => {
            format!("String.fromCharCode.apply(null,[{codes}].map(function(c){{return c^{key};}}))")
        }
        1 => format!(
            "(function(a){{var s=\"\",k=0;for(;k<a.length;k++)s+=String.fromCharCode(a[k]^{key});return s;}})([{codes}])"
        ),
        _ => format!(
            "String.fromCharCode.apply(null,[{codes}].map(function(c){{return c^({key}^{zero});}}))"
        ),
    }
}

/// Source text for the TDZ helper: throws a `ReferenceError` whose message is the
/// spec `Cannot access '<n>' before initialization`, with the constant prefix and
/// suffix each shipped only as XOR'd integer arrays.
fn tdz_helper_src(name: &str, style: usize, key: u8) -> String {
    let prefix = assemble_message("Cannot access '", style, key, "n");
    let suffix = assemble_message("' before initialization", style, key, "n");
    format!("function {name}(n){{throw new ReferenceError({prefix}+n+{suffix});}}")
}

/// Source text for the const helper: throws a `TypeError` whose message is the
/// spec `Assignment to constant variable.`, shipped only as an XOR'd integer array.
fn const_helper_src(name: &str, style: usize, key: u8) -> String {
    let msg = assemble_message("Assignment to constant variable.", style, key, "n");
    format!("function {name}(n){{throw new TypeError({msg});}}")
}

/// Parses the two helper function declarations as standalone statements, in the
/// seeded order, with the seeded message-assembly style.
fn parse_helper_decls(helpers: &TdzHelpers) -> Vec<Stmt> {
    let tdz = tdz_helper_src(&helpers.throw_tdz, helpers.msg_style, helpers.msg_key);
    let cst = const_helper_src(&helpers.throw_const, helpers.msg_style, helpers.msg_key);
    let src = if helpers.const_first {
        format!("{cst}\n{tdz}")
    } else {
        format!("{tdz}\n{cst}")
    };
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(Lrc::new(FileName::Custom("__cf_helpers.js".into())), src);
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::EsNext,
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let program = parser
        .parse_program()
        .expect("internal: TDZ helper template must parse");
    match program {
        Program::Module(m) => m
            .body
            .into_iter()
            .filter_map(|item| match item {
                ModuleItem::Stmt(s) => Some(s),
                _ => None,
            })
            .collect(),
        Program::Script(s) => s.body,
    }
}

/// Prepends the guard helpers to the top of `program`. Call once per file, only
/// when at least one `let`/`const` was actually rewritten.
pub fn inject(program: &mut Program, helpers: &TdzHelpers) {
    mangler_jsast::directives::insert_program_statements(program, parse_helper_decls(helpers));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::cfflatten::test_support::emit_block;
    use swc_core::common::{DUMMY_SP, SyntaxContext};

    /// Render the injected helper declarations as a single source string.
    fn render_helpers(helpers: &TdzHelpers) -> String {
        let stmts = parse_helper_decls(helpers);
        let b = BlockStmt {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            stmts,
        };
        emit_block(&b)
    }

    fn helpers_for(seed: u64) -> TdzHelpers {
        let cfg = crate::passes::cfflatten::test_support_cfg();
        let mut rng = Rng::for_pass(seed, "cfflatten");
        TdzHelpers::new(&cfg, &mut rng)
    }

    /// Every assembled message must evaluate (via the inline decoder) to the spec
    /// text exactly, for each style and a range of seeded keys — and must contain
    /// NO cleartext byte of the spec text.
    #[test]
    fn assembled_messages_are_encoded_but_correct() {
        use mangler_testkit::eval::assert_behaviorally_equal;
        const FRAGMENTS: &[&str] = &[
            "Cannot acce",
            "annot acce",
            "ss '",
            "before in",
            " before in",
            "nitializ",
            "tialization",
            "Assignment to co",
            "nstant var",
            "nstant varia",
            "ble.",
            "ariable",
        ];
        for style in 0..MSG_STYLE_COUNT {
            for &key in &[1u8, 7, 42, 200, 255] {
                let prefix = assemble_message("Cannot access '", style, key, "n");
                let suffix = assemble_message("' before initialization", style, key, "n");
                // The inline decoder must reconstruct the exact spec text.
                assert_behaviorally_equal(
                    "globalThis.__out=\"Cannot access '_0xv' before initialization\";",
                    &format!(
                        "globalThis.__out=(function(n){{return {prefix}+n+{suffix};}})(\"_0xv\");"
                    ),
                );

                let cst = assemble_message("Assignment to constant variable.", style, key, "n");
                assert_behaviorally_equal(
                    "globalThis.__out=\"Assignment to constant variable.\";",
                    &format!("globalThis.__out=(function(n){{return {cst};}})(\"_0xv\");"),
                );

                for s in [&prefix, &suffix, &cst] {
                    for frag in FRAGMENTS {
                        assert!(!s.contains(frag), "fragment {frag:?} leaked: {s}");
                    }
                }
            }
        }
    }

    /// Same seed ⇒ byte-identical helper output (determinism contract).
    #[test]
    fn helpers_deterministic_per_seed() {
        assert_eq!(
            render_helpers(&helpers_for(42)),
            render_helpers(&helpers_for(42))
        );
    }

    /// Across seeds the surface form must actually vary.
    #[test]
    fn helpers_diversify_across_seeds() {
        let mut shapes = std::collections::HashSet::new();
        for seed in 0..64u64 {
            shapes.insert(render_helpers(&helpers_for(seed)));
        }
        assert!(
            shapes.len() >= 2,
            "expected diverse helper skeletons, got {}",
            shapes.len()
        );
    }
}
