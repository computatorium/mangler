//! The generic language seam.
//!
//! The pass graph (WP2) and the driver operate over *some* language without
//! knowing whether it is JavaScript, CSS, or HTML. The [`Language`] trait is the
//! minimal contract that makes a concrete front-end pluggable: it can parse
//! source text into an opaque AST, and print an AST back to source. Everything
//! between — the passes — works against `L::Ast` generically.
//!
//! This module is deliberately **swc-free** (and dependency-free generally).
//! The JavaScript implementation, with its swc AST, lands in WP3; it will simply
//! `impl Language for Js` with `type Ast = swc::Program`. Keeping the trait here,
//! in the foundation crate, lets WP2 be generic over `L: Language` without
//! pulling in any front-end.
//!
//! # Why this shape
//!
//! * [`Language::Ast`] is an associated type, not a boxed trait object, so
//!   passes get the concrete AST with no dynamic dispatch and no erasure.
//! * [`Language::ParseOpts`] lets each language carry its own parse
//!   configuration (script vs. module, dialect flags, …) without leaking those
//!   knobs into the generic seam.
//! * [`Language::parse`] returns [`crate::Result`] so parse failures flow
//!   through the one error path; [`Language::print`] is infallible because
//!   printing a well-formed in-memory AST cannot fail.
//! * [`Language::ID`] gives a stable short tag (used in diagnostics and as part
//!   of `pass_id` derivation) so a `Language` is self-describing.

use crate::error::Result;

/// A pluggable language front-end: parse source ⇄ print AST.
///
/// Implementors own their AST representation entirely; the rest of the pipeline
/// treats it as opaque and only touches it through passes written against that
/// concrete type. See the [module docs](self) for the design rationale.
pub trait Language {
    /// The in-memory syntax tree this language parses to and prints from.
    /// Opaque to the generic pipeline.
    type Ast;

    /// Per-language parse configuration (e.g. module vs. script, dialect).
    /// Use `()` if the language needs none.
    type ParseOpts;

    /// A short, stable identifier for this language (e.g. `"js"`, `"css"`,
    /// `"html"`). Used in diagnostics and as a component of derived `pass_id`s.
    const ID: &'static str;

    /// Parse `src` into an [`Ast`](Language::Ast). Returns an
    /// [`Error::Parse`](crate::error::Error::Parse) on malformed input.
    fn parse(&self, src: &str, opts: &Self::ParseOpts) -> Result<Self::Ast>;

    /// Print an [`Ast`](Language::Ast) back to source text. Infallible: a
    /// well-formed in-memory AST always prints.
    fn print(&self, ast: &Self::Ast) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial uppercase-only "language" used to exercise the trait shape: it
    /// parses by validating the text is ASCII and prints it back unchanged.
    struct Shout;

    impl Language for Shout {
        type Ast = String;
        type ParseOpts = ();
        const ID: &'static str = "shout";

        fn parse(&self, src: &str, _opts: &()) -> Result<String> {
            if src.is_ascii() {
                Ok(src.to_ascii_uppercase())
            } else {
                Err(crate::error::Error::parse(Self::ID, "non-ascii input"))
            }
        }

        fn print(&self, ast: &String) -> String {
            ast.clone()
        }
    }

    /// Generic helper: proves passes can be written against any `L: Language`.
    fn round_trip<L: Language>(lang: &L, src: &str, opts: &L::ParseOpts) -> Result<String> {
        let ast = lang.parse(src, opts)?;
        Ok(lang.print(&ast))
    }

    #[test]
    fn parse_print_round_trip_is_generic() {
        let out = round_trip(&Shout, "hello", &()).unwrap();
        assert_eq!(out, "HELLO");
        assert_eq!(Shout::ID, "shout");
    }

    #[test]
    fn parse_error_flows_through_result() {
        let err = Shout.parse("héllo", &()).unwrap_err();
        assert_eq!(err.to_string(), "parse error (shout): non-ascii input");
    }
}
