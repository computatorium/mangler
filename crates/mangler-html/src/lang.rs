//! The [`Html`] [`Language`] implementation over a real DOM.
//!
//! # Why kuchikiki (the maintained kuchiki fork) over raw html5ever / `markup5ever_rcdom`
//!
//! The brief asked us to weigh `html5ever`/`markup5ever` against `kuchikiki` and
//! defend the pick. We chose **kuchikiki**:
//!
//! * **Parsing fidelity is identical.** kuchikiki *is* html5ever underneath — it
//!   wires html5ever's spec-compliant tree builder into its own `Sink`. We get
//!   the browser-grade tokenizer/tree-construction (including correct raw-text
//!   handling, implied `<html>/<head>/<body>`, foster-parenting, etc.) for free,
//!   with no fidelity loss versus driving html5ever directly.
//! * **Serialization round-trips through html5ever's `HtmlSerializer`.** Crucially
//!   for us, that serializer writes the bodies of *raw-text* elements
//!   (`<script>`, `<style>`) verbatim — it does **not** HTML-escape `<`, `&`, …
//!   inside them. That is exactly what an obfuscator needs: the JS/CSS we drop
//!   back into a `<script>`/`<style>` body must survive serialization byte-for-
//!   byte. Both kuchikiki and bare html5ever share this serializer, so the
//!   property holds either way; kuchikiki just hands it to us as `NodeRef::serialize`.
//! * **Ergonomics: an `Rc`-based node-ref DOM with parent/sibling/child links and
//!   interior-mutable text/attributes.** `markup5ever_rcdom::RcDom` gives only a
//!   bare `Handle` tree with no parent pointers and no convenient mutation API;
//!   walking + in-place editing it (which is the whole job of our compact+embed
//!   pass) would mean reimplementing traversal and a mutable attribute map by
//!   hand. kuchikiki ships `descendants()`/`children()` iterators, `as_element()`/
//!   `as_text()`/`as_comment()` accessors, `detach()`, and a `RefCell<Attributes>`
//!   (an ordered `IndexMap`, so attribute order — and thus our determinism — is
//!   preserved) out of the box.
//! * **Raw-text element handling.** Because the underlying tokenizer is
//!   html5ever, `<script>`/`<style>`/`<textarea>`/`<pre>` are tokenized with the
//!   correct content models; a `<script>` body is one `Text` child, not a
//!   mis-parsed element soup. We read it with `text_contents()` and replace it
//!   with a single new `Text` node.
//!
//! The only cost of kuchikiki over raw html5ever is one extra (thin) dependency
//! layer; the ergonomic and round-trip wins dominate for a tree-rewriting pass.
//!
//! # Fragment vs. full document
//!
//! The old streaming rewriter never wrapped its input, so a bare `<div>hi</div>`
//! came out as `<div>hi</div>`. A DOM parser, by contrast, will imply
//! `<html><head><body>` around a fragment. To preserve the old observable
//! behavior we parse with [`kuchikiki::parse_fragment`] in a `body` context and
//! serialize only the fragment's nodes (never the synthetic context element),
//! *unless* the source carries a full-document marker (a doctype or a literal
//! `<html`/`<head`/`<body` tag), in which case we parse and serialize the whole
//! document so the doctype and structure round-trip.

use kuchikiki::traits::*;
use kuchikiki::{parse_fragment, parse_html, NodeRef};
use mangler_core::{Language, Result};

/// Whether a parsed [`Dom`] came from full-document parsing or fragment parsing.
/// Determines how it serializes back (whole document vs. fragment children only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomKind {
    /// Parsed as a complete document (had a doctype or an explicit
    /// `<html>`/`<head>`/`<body>`). Serialize the whole tree.
    Document,
    /// Parsed as a fragment in a `body` context. Serialize only the fragment's
    /// own nodes, not the synthetic context element.
    Fragment,
}

/// The HTML AST: a kuchikiki DOM plus the parse mode it was produced in.
///
/// `root` is the kuchikiki document node. For [`DomKind::Fragment`] its single
/// child is the synthetic `<html>` context element whose children are the actual
/// fragment; for [`DomKind::Document`] it is the real document.
pub struct Dom {
    /// The kuchikiki document node returned by the parser.
    pub root: NodeRef,
    /// How it was parsed (controls serialization).
    pub kind: DomKind,
}

/// The HTML [`Language`]: parse source into a real DOM, print it back out.
pub struct Html;

/// Heuristic: does this source look like a complete HTML document (rather than a
/// fragment)? We treat a leading doctype or an explicit `<html`/`<head`/`<body`
/// tag as the marker. Matches case-insensitively and ignores leading whitespace.
fn looks_like_document(src: &str) -> bool {
    let lower = src.to_ascii_lowercase();
    let trimmed = lower.trim_start();
    trimmed.starts_with("<!doctype")
        || lower.contains("<html")
        || lower.contains("<head")
        || lower.contains("<body")
}

impl Language for Html {
    type Ast = Dom;
    type ParseOpts = ();
    const ID: &'static str = "html";

    fn parse(&self, src: &str, _opts: &()) -> Result<Dom> {
        if looks_like_document(src) {
            let root = parse_html().one(src).document_node;
            Ok(Dom {
                root,
                kind: DomKind::Document,
            })
        } else {
            // Parse as a fragment in a `body` context so a bare `<div>` does not
            // get an implied `<html><head><body>` wrapper baked into the output.
            // Build the context QualName from strings (the HTML namespace URL)
            // rather than the `ns!`/`local_name!` macros, which need extra macros
            // in scope from `#[macro_use] extern crate`.
            let ctx_name = html5ever::QualName::new(
                None,
                html5ever::Namespace::from("http://www.w3.org/1999/xhtml"),
                html5ever::LocalName::from("body"),
            );
            let root = parse_fragment(ctx_name, Vec::new()).one(src).document_node;
            Ok(Dom {
                root,
                kind: DomKind::Fragment,
            })
        }
    }

    fn print(&self, ast: &Dom) -> String {
        match ast.kind {
            DomKind::Document => ast.root.to_string(),
            DomKind::Fragment => {
                // The document node's first child is the synthetic `<html>`
                // context element; the real fragment is *its* children. Serialize
                // those, concatenated, so the context wrapper never leaks out.
                let mut out = String::new();
                if let Some(ctx_el) = ast.root.first_child() {
                    for child in ctx_el.children() {
                        out.push_str(&child.to_string());
                    }
                }
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_round_trips_without_wrapper() {
        let dom = Html.parse("<div>hi</div>", &()).unwrap();
        assert_eq!(dom.kind, DomKind::Fragment);
        assert_eq!(Html.print(&dom), "<div>hi</div>");
    }

    #[test]
    fn fragment_round_trips_unknown_markup() {
        // Arbitrary/unknown markup must pass through uncorrupted.
        let dom = Html.parse("<x-widget data-q=\"1\">a<b>c</b></x-widget>", &()).unwrap();
        assert_eq!(Html.print(&dom), "<x-widget data-q=\"1\">a<b>c</b></x-widget>");
    }

    #[test]
    fn document_round_trips_with_doctype() {
        let dom = Html.parse("<!doctype html><html><head></head><body><p>x</p></body></html>", &()).unwrap();
        assert_eq!(dom.kind, DomKind::Document);
        let out = Html.print(&dom);
        assert!(out.starts_with("<!DOCTYPE html>"), "got: {out}");
        assert!(out.contains("<p>x</p>"));
    }

    #[test]
    fn raw_text_script_body_not_escaped() {
        // A `<script>` body containing `<` and `&` must serialize verbatim.
        let dom = Html.parse("<script>if (a < b && c) f();</script>", &()).unwrap();
        let out = Html.print(&dom);
        assert!(out.contains("a < b && c"), "got: {out}");
    }
}
