//! The compact + embed pass: a [`Pass<Html, C>`] that walks the real DOM,
//! strips comments, collapses insignificant whitespace (respecting preserve
//! elements), and routes embedded JS/CSS back through injected transform
//! callbacks.
//!
//! # The dependency-inversion seam
//!
//! This crate must NOT depend on `mangler-js`/`mangler-css` (it would couple WP8
//! to crates that may not be ready and is awkward besides). Instead the embedding
//! recursion takes injected callbacks via [`EmbedHandlers`]: the CLI (WP9) plugs
//! in the real JS/CSS processors; tests plug in fakes. A no-op default
//! ([`EmbedHandlers::noop`]) lets HTML be compacted without any JS/CSS work.
//!
//! # Note-on-failure, not silent cleartext
//!
//! The old streaming rewriter did `js::process(..).unwrap_or(leftover)` and
//! `Err(_) => set_str(full)` — on a handler failure it silently re-emitted the
//! *original* source, so the operator never learned obfuscation had been skipped.
//! Here, a handler returning `Err(())` causes us to (1) keep the original body
//! (the documented fallback — never corrupt the page) AND (2) push a
//! [`Note`](mangler_core::Note) naming the element/attribute that was skipped, so
//! the operator can see what was left untransformed.

use crate::lang::{Dom, Html};
use kuchikiki::NodeRef;
use mangler_core::{Note, Notes, PassConfig, Result, Rng};
use mangler_passgraph::{ArtifactBus, Pass};
use std::marker::PhantomData;

/// Where an injected JS transform is being applied. Lets the handler pick a
/// fragment-safe config and report sensible diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsContext {
    /// A full `<script>` element body (raw-text content).
    ScriptBody,
    /// An inline `on*=` event-handler attribute value (e.g. `onclick="…"`).
    InlineHandler,
}

/// Where an injected CSS transform is being applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CssContext {
    /// A full `<style>` element body (raw-text content).
    StyleBody,
    /// An inline `style="…"` declaration block on any element.
    InlineStyle,
}

/// Injected transform callbacks for embedded JS/CSS.
///
/// Each callback takes the source fragment plus the context describing where it
/// came from, and returns the transformed text on success or `Err(())` on
/// failure. A failure is **not** fatal: the pass keeps the original body and
/// records a [`Note`](mangler_core::Note). The callbacks are responsible for
/// using a fragment-safe config (we never want anti-tamper traps injected into an
/// inline handler).
pub struct EmbedHandlers<'a> {
    /// Transform a JavaScript fragment (`<script>` body or `on*=` handler).
    pub js: &'a dyn Fn(&str, JsContext) -> std::result::Result<String, ()>,
    /// Transform a CSS fragment (`<style>` body or inline `style=` block).
    pub css: &'a dyn Fn(&str, CssContext) -> std::result::Result<String, ()>,
}

impl<'a> EmbedHandlers<'a> {
    /// Identity handlers: return the input unchanged for both JS and CSS. Lets
    /// HTML be compacted (comments stripped, whitespace collapsed) without doing
    /// any JS/CSS processing.
    pub fn noop() -> EmbedHandlers<'static> {
        EmbedHandlers {
            js: &|s, _| Ok(s.to_owned()),
            css: &|s, _| Ok(s.to_owned()),
        }
    }
}

/// Collapse runs of internal ASCII whitespace in an HTML text node to a single
/// space, WITHOUT trimming the node's edges.
///
/// Ported verbatim from the original streaming rewriter. Edge whitespace is
/// preserved (collapsed to a single space if it is a run) so that significant
/// whitespace between inline elements — e.g. the space in `<a>x</a> <b>y</b>` —
/// is never deleted.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_ascii_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

/// Decide whether a `<script>` element's body should be treated as JavaScript.
///
/// Ported from the original: JS iff it has no `src` and its `type` is empty or
/// one of the JS MIME types / `module` (compared case-insensitively, trimmed).
fn script_is_js(el: &kuchikiki::ElementData) -> bool {
    let attrs = el.attributes.borrow();
    let has_src = attrs.get("src").is_some();
    let type_lower = attrs
        .get("type")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    !has_src
        && (type_lower.is_empty()
            || type_lower == "text/javascript"
            || type_lower == "application/javascript"
            || type_lower == "module")
}

/// Match `on` followed by an ASCII letter (onclick, onload, …) so we don't treat
/// `on`, `on-foo`, or `on2` as a handler. Ported from the original.
fn is_event_handler(lname: &str) -> bool {
    lname.starts_with("on") && lname.as_bytes().get(2).is_some_and(u8::is_ascii_alphabetic)
}

/// Replace the children of `el_node` with a single text node holding `body`.
fn set_raw_text_body(el_node: &NodeRef, body: String) {
    // Detach existing children.
    let children: Vec<NodeRef> = el_node.children().collect();
    for c in children {
        c.detach();
    }
    if !body.is_empty() {
        el_node.append(NodeRef::new_text(body));
    }
}

/// The compact + embed pass over the HTML DOM.
///
/// Carries the injected [`EmbedHandlers`]. Because [`Pass::run`] cannot take
/// extra arguments, the handlers live on the pass value; [`process`] constructs
/// the pass with the caller's handlers for a single run.
///
/// `'h` is the lifetime of the borrowed handler callbacks.
pub struct CompactEmbed<'h, C> {
    handlers: EmbedHandlers<'h>,
    _cfg: PhantomData<C>,
}

impl<'h, C> CompactEmbed<'h, C> {
    /// Build the pass with the given embedding handlers.
    pub fn new(handlers: EmbedHandlers<'h>) -> Self {
        CompactEmbed {
            handlers,
            _cfg: PhantomData,
        }
    }

    /// Walk the document, applying every transform. Pushes notes for any handler
    /// failure. Separated from [`Pass::run`] so it can be unit-tested directly.
    fn walk(&self, dom: &Dom, notes: &mut Notes) {
        // Preserve set: text inside these elements (or their descendants) must
        // not be whitespace-collapsed. `<script>`/`<style>` bodies are handled by
        // their own embedders, so they are preserve-set too.
        // We compute "inside a preserve element" by walking and tracking depth via
        // the parent chain rather than a running counter, because kuchikiki's
        // descendant iterator is pre-order and we need per-node context.
        let nodes: Vec<NodeRef> = dom.root.inclusive_descendants().collect();

        // First pass: remove comments and process raw-text + attributes.
        for node in &nodes {
            if node.as_comment().is_some() {
                node.detach();
                continue;
            }
            if let Some(el) = node.as_element() {
                let tag: String = el.name.local.as_ref().to_ascii_lowercase();
                if tag == "script" {
                    if script_is_js(el) {
                        let body = node.text_contents();
                        if !body.is_empty() {
                            self.embed_js(node, body, JsContext::ScriptBody, notes);
                        }
                    }
                } else if tag == "style" {
                    let body = node.text_contents();
                    if !body.is_empty() {
                        self.embed_css_body(node, body, notes);
                    }
                }
                // Inline attributes (`on*=` and `style=`) on every element.
                self.embed_attributes(el, notes);
            }
        }

        // Second pass: collapse whitespace in text nodes that are not inside a
        // preserve element (<pre>, <textarea>, <script>, <style>).
        for node in &nodes {
            if let Some(text_cell) = node.as_text() {
                if inside_preserve(node) {
                    continue;
                }
                let collapsed = collapse_ws(&text_cell.borrow());
                *text_cell.borrow_mut() = collapsed;
            }
        }
    }

    /// Run the JS embedder on a `<script>` body, replacing the body or noting a
    /// skip on failure.
    fn embed_js(&self, el_node: &NodeRef, body: String, ctx: JsContext, notes: &mut Notes) {
        match (self.handlers.js)(&body, ctx) {
            Ok(out) => set_raw_text_body(el_node, out),
            Err(()) => {
                // Documented fallback: keep the original body untouched, but
                // surface a note so the skip is visible.
                notes.push(Note::from(
                    "html-embed",
                    "skipped <script> body: JS transform failed; left original",
                ));
            }
        }
    }

    /// Run the CSS embedder on a `<style>` body.
    fn embed_css_body(&self, el_node: &NodeRef, body: String, notes: &mut Notes) {
        match (self.handlers.css)(&body, CssContext::StyleBody) {
            Ok(out) => set_raw_text_body(el_node, out),
            Err(()) => {
                notes.push(Note::from(
                    "html-embed",
                    "skipped <style> body: CSS transform failed; left original",
                ));
            }
        }
    }

    /// Process inline `on*=` (JS) and `style=` (CSS) attributes on one element.
    fn embed_attributes(&self, el: &kuchikiki::ElementData, notes: &mut Notes) {
        // Collect (local-name, value) first to avoid holding the borrow while we
        // mutate. Order is the IndexMap order — deterministic.
        let pending: Vec<(String, String)> = {
            let attrs = el.attributes.borrow();
            attrs
                .map
                .iter()
                .map(|(name, attr)| (name.local.to_string(), attr.value.clone()))
                .collect()
        };

        let tag: String = el.name.local.as_ref().to_ascii_lowercase();
        for (name, val) in pending {
            if val.trim().is_empty() {
                continue;
            }
            let lname = name.to_ascii_lowercase();
            let result = if lname == "style" {
                Some(((self.handlers.css)(&val, CssContext::InlineStyle), "style="))
            } else if is_event_handler(&lname) {
                Some(((self.handlers.js)(&val, JsContext::InlineHandler), "on*="))
            } else {
                None
            };
            let Some((res, what)) = result else { continue };
            match res {
                Ok(out) => {
                    let mut attrs = el.attributes.borrow_mut();
                    if let Some(slot) = attrs.get_mut(&*name) {
                        *slot = out;
                    }
                }
                Err(()) => {
                    notes.push(Note::from(
                        "html-embed",
                        format!("skipped {what}\"{name}\" on <{tag}>: transform failed; left original"),
                    ));
                }
            }
        }
    }
}

/// Is `node` inside an element whose text must be preserved verbatim
/// (`<pre>`, `<textarea>`, `<script>`, `<style>`)? Walks the parent chain.
fn inside_preserve(node: &NodeRef) -> bool {
    let mut cur = node.parent();
    while let Some(p) = cur {
        if let Some(el) = p.as_element() {
            let tag: String = el.name.local.as_ref().to_ascii_lowercase();
            if matches!(tag.as_str(), "pre" | "textarea" | "script" | "style") {
                return true;
            }
        }
        cur = p.parent();
    }
    false
}

impl<C: PassConfig> Pass<Html, C> for CompactEmbed<'_, C> {
    fn id(&self) -> &'static str {
        "html-compact-embed"
    }

    fn fragment_safe(&self) -> bool {
        true
    }

    fn run(
        &self,
        ast: &mut Dom,
        _cfg: &C,
        _rng: &mut Rng,
        _bus: &mut ArtifactBus,
        notes: &mut Notes,
    ) -> Result<()> {
        self.walk(ast, notes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Html;
    use mangler_core::Language;

    // A trivial config for driving the pass.
    struct Cfg {
        seed: u64,
    }
    impl PassConfig for Cfg {
        fn seed(&self) -> u64 {
            self.seed
        }
    }

    fn run_with<'h>(src: &str, handlers: EmbedHandlers<'h>) -> (String, Notes) {
        let mut dom = Html.parse(src, &()).unwrap();
        let pass = CompactEmbed::<Cfg>::new(handlers);
        let cfg = Cfg { seed: 0 };
        let mut rng = Rng::for_pass(cfg.seed(), pass.id());
        let mut bus = ArtifactBus::new();
        let mut notes = Notes::new();
        pass.run(&mut dom, &cfg, &mut rng, &mut bus, &mut notes).unwrap();
        (Html.print(&dom), notes)
    }

    // ---- Ports of the original 6 tests. ----

    #[test]
    fn strips_html_comments() {
        let (out, _) = run_with("<!-- secret --><div>hi</div>", EmbedHandlers::noop());
        assert!(!out.contains("secret"), "got: {out}");
        assert!(out.contains("<div>hi</div>"), "got: {out}");
    }

    #[test]
    fn minifies_inline_script_body() {
        // Fake JS handler: strip `// c` line comment and rename longLocalName.
        let js = |s: &str, _c: JsContext| -> std::result::Result<String, ()> {
            let stripped: String = s
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("");
            Ok(stripped.replace("longLocalName", "a"))
        };
        let handlers = EmbedHandlers {
            js: &js,
            css: &|s, _| Ok(s.to_owned()),
        };
        let html = "<script>// c\nfunction f(){ var longLocalName = 1; return longLocalName; }</script>";
        let (out, _) = run_with(html, handlers);
        assert!(!out.contains("// c"), "got: {out}");
        assert!(!out.contains("longLocalName"), "got: {out}");
    }

    #[test]
    fn minifies_inline_style_body() {
        let css = |s: &str, _c: CssContext| -> std::result::Result<String, ()> {
            // Strip `/* x */` comments crudely.
            Ok(s.replace("/* x */", "").trim().to_owned())
        };
        let handlers = EmbedHandlers {
            js: &|s, _| Ok(s.to_owned()),
            css: &css,
        };
        let html = "<style>/* x */ .a {  color: #ffffff; }</style>";
        let (out, _) = run_with(html, handlers);
        assert!(!out.contains("/* x */"), "got: {out}");
        assert!(out.contains(".a"), "got: {out}");
    }

    #[test]
    fn collapse_ws_basic() {
        assert_eq!(collapse_ws("  a   b  "), " a b ");
        assert_eq!(collapse_ws("a\n\n  b"), "a b");
        assert_eq!(collapse_ws(""), "");
        assert_eq!(collapse_ws("nospace"), "nospace");
    }

    #[test]
    fn collapses_text_whitespace() {
        let (out, _) = run_with("<div>  a   b  </div>", EmbedHandlers::noop());
        assert!(out.contains("<div> a b </div>"), "got: {out}");
    }

    #[test]
    fn preserves_pre_whitespace() {
        let inner = "  a   b\n  c";
        let (out, _) = run_with(&format!("<pre>{inner}</pre>"), EmbedHandlers::noop());
        assert!(out.contains(inner), "got: {out}");
    }

    // ---- New behavior: the seam, the contexts, and Note-on-failure. ----

    #[test]
    fn note_on_js_handler_failure_keeps_original() {
        let handlers = EmbedHandlers {
            js: &|_, _| Err(()),
            css: &|s, _| Ok(s.to_owned()),
        };
        let (out, notes) = run_with("<script>var keepMe=1;</script>", handlers);
        // Original body preserved (documented fallback) ...
        assert!(out.contains("var keepMe=1;"), "got: {out}");
        // ... AND a note was recorded — NOT silent cleartext.
        assert_eq!(notes.len(), 1);
        assert!(notes.iter().next().unwrap().message.contains("<script>"));
    }

    #[test]
    fn note_on_css_handler_failure_keeps_original() {
        let handlers = EmbedHandlers {
            js: &|s, _| Ok(s.to_owned()),
            css: &|_, _| Err(()),
        };
        let (out, notes) = run_with("<style>.a{color:red}</style>", handlers);
        assert!(out.contains(".a{color:red}"), "got: {out}");
        assert_eq!(notes.len(), 1);
        assert!(notes.iter().next().unwrap().message.contains("<style>"));
    }

    #[test]
    fn inline_handler_and_style_use_correct_contexts() {
        // Record the contexts the handlers were called with.
        let js_ctx = std::cell::RefCell::new(Vec::new());
        let css_ctx = std::cell::RefCell::new(Vec::new());
        let js = |s: &str, c: JsContext| -> std::result::Result<String, ()> {
            js_ctx.borrow_mut().push(c);
            Ok(s.to_owned())
        };
        let css = |s: &str, c: CssContext| -> std::result::Result<String, ()> {
            css_ctx.borrow_mut().push(c);
            Ok(s.to_owned())
        };
        let handlers = EmbedHandlers { js: &js, css: &css };
        let _ = run_with(
            "<div onclick=\"f()\" style=\"color:red\">x</div><script>g()</script><style>.a{}</style>",
            handlers,
        );
        assert!(js_ctx.borrow().contains(&JsContext::InlineHandler));
        assert!(js_ctx.borrow().contains(&JsContext::ScriptBody));
        assert!(css_ctx.borrow().contains(&CssContext::InlineStyle));
        assert!(css_ctx.borrow().contains(&CssContext::StyleBody));
    }

    #[test]
    fn script_with_src_or_nonjs_type_is_not_processed() {
        let called = std::cell::RefCell::new(false);
        let js = |s: &str, _c: JsContext| -> std::result::Result<String, ()> {
            *called.borrow_mut() = true;
            Ok(s.to_owned())
        };
        let handlers = EmbedHandlers {
            js: &js,
            css: &|s, _| Ok(s.to_owned()),
        };
        let _ = run_with("<script type=\"application/json\">{\"a\":1}</script>", handlers);
        assert!(!*called.borrow(), "non-JS script type must not be sent to the JS handler");
    }

    #[test]
    fn on_non_handler_attrs_are_left_alone() {
        // `on`, `on-foo`, `on2` must NOT be treated as handlers.
        let called = std::cell::RefCell::new(0u32);
        let js = |s: &str, _c: JsContext| -> std::result::Result<String, ()> {
            *called.borrow_mut() += 1;
            Ok(s.to_owned())
        };
        let handlers = EmbedHandlers {
            js: &js,
            css: &|s, _| Ok(s.to_owned()),
        };
        let _ = run_with("<div on=\"a\" on-foo=\"b\" on2=\"c\" onclick=\"d\">x</div>", handlers);
        assert_eq!(*called.borrow(), 1, "only onclick should reach the JS handler");
    }
}
