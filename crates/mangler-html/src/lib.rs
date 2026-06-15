//! `mangler-html` — the HTML [`Language`] implementation over a real DOM.
//!
//! Parse→DOM→serialize, preserving the old streaming rewriter's whitespace/`pre`/
//! comment behavior, and recursing into embedded JS/CSS via the *string boundary*
//! (we hand the JS/CSS source text to injected callbacks and splice their output
//! back into the DOM). Inline-transform failures record a [`Note`] rather than
//! emitting silent cleartext.
//!
//! # Layout
//!
//! * [`lang`] — the [`Html`] [`Language`]: parse to a kuchikiki DOM, print back.
//!   Its rustdoc carries the DOM-parser justification (kuchikiki vs. raw
//!   html5ever/`markup5ever_rcdom`).
//! * [`pass`] — the [`CompactEmbed`](pass::CompactEmbed) [`Pass`]: strip comments,
//!   collapse whitespace (respecting `<pre>`/`<textarea>`/`<script>`/`<style>`),
//!   and route embedded JS/CSS through the injected [`EmbedHandlers`](pass::EmbedHandlers).
//!
//! # The dependency-inversion seam
//!
//! WP8 must not depend on `mangler-js`/`mangler-css`. The embedding recursion
//! takes injected callbacks ([`EmbedHandlers`](pass::EmbedHandlers)); WP9 plugs in
//! the real processors, tests plug in fakes, and [`EmbedHandlers::noop`](pass::EmbedHandlers::noop)
//! lets HTML be compacted with no JS/CSS work.
//!
//! [`Note`]: mangler_core::Note
//! [`Language`]: mangler_core::Language
//! [`Pass`]: mangler_passgraph::Pass

pub mod lang;
pub mod pass;

pub use lang::{Dom, DomKind, Html};
pub use pass::{CompactEmbed, CssContext, EmbedHandlers, JsContext};

use mangler_core::{Language, Notes, PassConfig, Result, Rng};
use mangler_passgraph::{ArtifactBus, Pass};

/// The HTML pipeline entry point WP9 calls.
///
/// Mirrors the old `html::process`, but threaded through the real DOM
/// [`Language`] + the [`CompactEmbed`] pass, and taking:
///
/// * `src` — the HTML source.
/// * `cfg` — the pass config (supplies the deterministic seed; generic over
///   `C: PassConfig` so WP9's concrete config plugs in without coupling).
/// * `handlers` — the injected JS/CSS transform callbacks (the dependency-
///   inversion seam). Pass [`EmbedHandlers::noop`] to compact only.
/// * `notes` — the non-error channel. A handler failure records a note here and
///   keeps the original body; it is **never** a hard error and never silently
///   emits cleartext as if it had succeeded.
///
/// Returns the serialized, compacted-and-embedded HTML.
pub fn process<C: PassConfig>(
    src: &str,
    cfg: &C,
    handlers: EmbedHandlers<'_>,
    notes: &mut Notes,
) -> Result<String> {
    let html = Html;
    let mut dom = html.parse(src, &())?;
    let pass = CompactEmbed::<C>::new(handlers);
    let mut rng = Rng::for_pass(cfg.seed(), pass.id());
    let mut bus = ArtifactBus::new();
    pass.run(&mut dom, cfg, &mut rng, &mut bus, notes)?;
    Ok(html.print(&dom))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Cfg;
    impl PassConfig for Cfg {
        fn seed(&self) -> u64 {
            0
        }
    }

    #[test]
    fn process_compacts_with_noop_handlers() {
        let mut notes = Notes::new();
        let out = process(
            "<!-- x --><div>  a   b  </div>",
            &Cfg,
            EmbedHandlers::noop(),
            &mut notes,
        )
        .unwrap();
        assert!(!out.contains("x"), "comment not stripped: {out}");
        assert!(out.contains("<div> a b </div>"), "got: {out}");
        assert!(notes.is_empty());
    }

    #[test]
    fn process_is_deterministic() {
        let mut n1 = Notes::new();
        let mut n2 = Notes::new();
        let a = process("<div>  a  </div><span>x</span>", &Cfg, EmbedHandlers::noop(), &mut n1).unwrap();
        let b = process("<div>  a  </div><span>x</span>", &Cfg, EmbedHandlers::noop(), &mut n2).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn process_records_note_on_failure_and_keeps_body() {
        let handlers = EmbedHandlers {
            js: &|_, _| Err(()),
            css: &|s, _| Ok(s.to_owned()),
        };
        let mut notes = Notes::new();
        let out = process("<script>var s=1;</script>", &Cfg, handlers, &mut notes).unwrap();
        assert!(out.contains("var s=1;"), "got: {out}");
        assert_eq!(notes.len(), 1);
    }
}
