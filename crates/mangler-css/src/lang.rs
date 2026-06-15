//! The [`Css`] [`Language`] implementation.
//!
//! # The lifetime problem and how this resolves it
//!
//! lightningcss models a parsed stylesheet as `StyleSheet<'i, 'o>`, which
//! *borrows* the source text (`'i`) and parser-owned data (`'o`). That fights the
//! [`Language`] contract, whose `type Ast` is an **owned** type with no lifetime
//! parameter: you cannot name `StyleSheet<'i, 'o>` as an associated type without
//! threading those borrows through the whole pass graph, which the generic seam
//! deliberately refuses to do.
//!
//! Several designs resolve this; the simplest *correct* one is chosen here:
//!
//! **[`parse`](Css::parse) does the parse + minify eagerly and stores the
//! resulting owned, minified CSS `String`; [`print`](Css::print) is then a cheap
//! emit (a clone of that string).** The borrowing `StyleSheet<'i, 'o>` lives only
//! for the duration of one `parse` call — it never escapes into the AST — so no
//! lifetime ever leaks across the `Language` boundary.
//!
//! ## Tradeoff
//!
//! The minify work happens in `parse`, not in the `"css-minify"` pass, which is
//! slightly counter-intuitive: the pass is, today, a structural no-op that exists
//! to make CSS a real schedule rather than a special-cased stub (and to be the
//! place a future, gated renaming pass slots in). The alternative — an owned CSS
//! IR we re-lower ourselves — would let passes mutate a real tree, but it means
//! re-implementing lightningcss's model and is far more code for zero present
//! benefit, since the only transform we are *allowed* to do is minify. If/when a
//! renaming pass lands (see [`crate::pass::rename`]) it can re-parse the stored
//! source with renaming enabled; the [`CssAst`] keeps that door open by also
//! retaining the original source.

use mangler_core::{Error, Language, Result};

use lightningcss::printer::PrinterOptions;
use lightningcss::stylesheet::{MinifyOptions, ParserOptions, StyleSheet};

/// The CSS [`Language`] front-end: parse + minify ⇄ print via [`lightningcss`].
///
/// Stateless; construct with `Css` and use through the [`Language`] trait (or the
/// [`crate::process`] / [`crate::process_inline`] convenience wrappers).
#[derive(Debug, Clone, Copy, Default)]
pub struct Css;

/// Owned, lifetime-free CSS AST.
///
/// Holds the *minified* output (produced eagerly in [`Css::parse`]) so
/// [`Css::print`] is a cheap clone, and retains the original `source` +
/// [`CssParseOpts`] so a future renaming pass (see [`crate::pass::rename`]) can
/// re-derive a fresh borrowing `StyleSheet` without the generic pipeline ever
/// seeing a lifetime. See the [module docs](self) for why the borrowing
/// `StyleSheet<'i, 'o>` is confined to `parse`.
#[derive(Debug, Clone)]
pub struct CssAst {
    /// The original source as handed to [`Css::parse`] (the inline-wrapped form
    /// for [`CssParseOpts::inline`]). Retained for a future re-parse.
    pub source: String,
    /// The minified CSS, ready to emit. For [`CssParseOpts::inline`] this is the
    /// *unwrapped* declaration block (wrapper rule stripped back off).
    pub minified: String,
    /// The parse mode this AST was produced under.
    pub opts: CssParseOpts,
}

/// Per-parse configuration selecting how the source is interpreted.
///
/// This folds the old free-standing `process_inline` entry point into a
/// *parse-opt variant* of the single [`Language`] path: an inline declaration
/// block is just a [`Css::parse`] with [`CssParseOpts::inline`] set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CssParseOpts {
    /// When `true`, the source is a **bare declaration block** (the body of an
    /// inline `style="…"` attribute), not a whole stylesheet. It is wrapped in a
    /// throwaway rule, minified, then unwrapped so only the minified declarations
    /// survive. When `false` (the default) the source is a whole stylesheet.
    pub inline: bool,
}

impl CssParseOpts {
    /// Parse the source as a whole stylesheet (the default).
    pub const fn stylesheet() -> Self {
        Self { inline: false }
    }

    /// Parse the source as a bare inline declaration block (an inline
    /// `style="…"` attribute body).
    pub const fn inline() -> Self {
        Self { inline: true }
    }
}

/// The throwaway selector wrapped around an inline declaration block. Chosen to
/// be collision-unlikely; lightningcss never renames it.
const INLINE_WRAPPER: &str = "__m_inline";

impl Language for Css {
    type Ast = CssAst;
    type ParseOpts = CssParseOpts;
    const ID: &'static str = "css";

    /// Parse + minify `src`, returning an owned [`CssAst`].
    ///
    /// For [`CssParseOpts::inline`] the declarations are wrapped in a throwaway
    /// rule before minifying and the wrapper is stripped back off afterwards.
    /// The borrowing `StyleSheet<'i, 'o>` is fully confined to this call.
    fn parse(&self, src: &str, opts: &CssParseOpts) -> Result<CssAst> {
        let (to_minify, owned_src) = if opts.inline {
            (format!("{INLINE_WRAPPER}{{{src}}}"), src.to_string())
        } else {
            (src.to_string(), src.to_string())
        };

        let whole = minify_stylesheet(&to_minify)?;

        let minified = if opts.inline {
            unwrap_inline(&whole)?
        } else {
            whole
        };

        Ok(CssAst {
            source: owned_src,
            minified,
            opts: *opts,
        })
    }

    /// Emit the minified CSS. Cheap: the work happened in [`Css::parse`].
    fn print(&self, ast: &CssAst) -> String {
        ast.minified.clone()
    }
}

/// Parse + minify a whole stylesheet, mapping every lightningcss failure onto
/// [`Error::Parse`] for the `"css"` language. The returned `String` is the
/// minified CSS.
fn minify_stylesheet(src: &str) -> Result<String> {
    let mut sheet = StyleSheet::parse(src, ParserOptions::default())
        .map_err(|e| Error::parse(Css::ID, format!("css parse error: {e}")))?;
    sheet
        .minify(MinifyOptions::default())
        .map_err(|e| Error::parse(Css::ID, format!("css minify error: {e}")))?;
    let res = sheet
        .to_css(PrinterOptions {
            minify: true,
            ..Default::default()
        })
        .map_err(|e| Error::parse(Css::ID, format!("css print error: {e}")))?;
    Ok(res.code)
}

/// Strip the [`INLINE_WRAPPER`] rule back off a minified `WRAPPER{<decls>}` form,
/// returning just the minified declarations.
///
/// Tolerates trailing whitespace/newline after the closing brace via `trim_end`
/// (guarding against any lightningcss version that emits one).
fn unwrap_inline(minified: &str) -> Result<String> {
    let prefix = format!("{INLINE_WRAPPER}{{");
    let trimmed = minified.trim_end();
    trimmed
        .strip_prefix(&prefix)
        .and_then(|s| s.strip_suffix('}'))
        .map(str::to_string)
        .ok_or_else(|| {
            Error::parse(
                Css::ID,
                format!("inline css: unexpected minified shape: {minified}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stylesheet_minifies_and_drops_comments() {
        let ast = Css
            .parse(
                "/* c */ .a {  color: #ffffff;  margin: 0px; }",
                &CssParseOpts::stylesheet(),
            )
            .unwrap();
        let out = Css.print(&ast);
        assert!(!out.contains("/*"));
        assert!(!out.contains(' ') || out.len() < 30);
        assert!(out.contains(".a"));
    }

    #[test]
    fn inline_minifies_declarations() {
        let ast = Css
            .parse("color:#ffffff;  margin:0px;", &CssParseOpts::inline())
            .unwrap();
        let out = Css.print(&ast);
        assert!(!out.contains("  "), "double space remained: {out}");
        assert!(out.contains("#fff"), "color not minified: {out}");
        assert!(out.contains("margin:0"), "0px not minified: {out}");
        assert!(!out.contains('{') && !out.contains('}'), "wrapper leaked: {out}");
    }

    #[test]
    fn inline_tolerates_trailing_whitespace() {
        let ast = Css.parse("color: red", &CssParseOpts::inline()).unwrap();
        let out = Css.print(&ast);
        assert!(!out.contains('{') && !out.contains('}'), "wrapper leaked: {out}");
        assert!(out.contains("red") || out.contains("color"), "content lost: {out}");
    }

    #[test]
    fn inline_round_trips_simple_decl() {
        let ast = Css.parse("color:red", &CssParseOpts::inline()).unwrap();
        assert_eq!(Css.print(&ast), "color:red");
    }

    #[test]
    fn unwrap_inline_trim_end_guard() {
        // If lightningcss were to emit "WRAPPER{decls}\n", the trim_end guard
        // must silently succeed rather than error.
        let fake = format!("{INLINE_WRAPPER}{{color:red}}\n");
        assert_eq!(unwrap_inline(&fake).unwrap(), "color:red");
    }

    #[test]
    fn parse_error_flows_through_result() {
        // lightningcss is lenient (top-level recovery), but genuinely broken
        // input must surface as a mapped css parse error, not a panic. Stray
        // closing braces are a hard parse failure under default options.
        let err = Css.parse("}}}", &CssParseOpts::stylesheet()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("css"), "err lacks lang tag: {msg}");
        assert!(msg.contains("parse error"), "err not a parse error: {msg}");
    }

    #[test]
    fn ast_retains_source_for_future_reparse() {
        let ast = Css
            .parse(".a { color: red }", &CssParseOpts::stylesheet())
            .unwrap();
        assert_eq!(ast.source, ".a { color: red }");
        assert_eq!(ast.opts, CssParseOpts::stylesheet());
    }

    #[test]
    fn id_is_css() {
        assert_eq!(Css::ID, "css");
    }
}
