//! The CSS pass schedule.
//!
//! Today the whole schedule is **one pass**, [`MinifyPass`] (`"css-minify"`).
//! Because [`Css::parse`](crate::lang::Css::parse) already produces minified
//! output (see the [`crate::lang`] lifetime rationale), the pass is a structural
//! no-op that exists to make CSS a *real* [`Pass`]-graph node rather than a
//! special-cased stub, and to mark the seam where a future, gated renaming pass
//! ([`rename`]) would slot in. It is always enabled and fragment-safe (an inline
//! declaration block is a fragment).
//!
//! [`process`] / [`process_inline`] are the convenience entry points that run the
//! [`Css`](crate::lang::Css) language + this one-pass schedule end to end; they
//! mirror the old `css::process` / `css::process_inline` functions the HTML crate
//! (WP8) and CLI (WP9) call.

use mangler_core::{Language, Notes, PassConfig, Result, Rng};
use mangler_passgraph::{ArtifactBus, Pass, Resource};

use crate::lang::{Css, CssAst, CssParseOpts};

pub mod rename;

/// The custom resource the CSS minify pass writes: "this stylesheet has been
/// minified". A future renaming pass would *read* it so renaming runs after
/// minification settles the structure.
pub const CSS_MINIFIED: Resource = Resource::Custom("css::minified");

/// The one-pass CSS schedule: minify.
///
/// Reads nothing, writes [`CSS_MINIFIED`], always enabled, fragment-safe. The
/// actual minify is done eagerly in [`Css::parse`](crate::lang::Css::parse), so
/// `run` is a structural no-op — it exists so CSS participates in the pass graph
/// like every other language and so the dependency edge for a future renaming
/// pass has somewhere to attach. See the [module docs](self).
#[derive(Debug, Clone, Copy, Default)]
pub struct MinifyPass;

impl<C: PassConfig> Pass<Css, C> for MinifyPass {
    fn id(&self) -> &'static str {
        "css-minify"
    }

    fn writes(&self) -> &[Resource] {
        const W: &[Resource] = &[CSS_MINIFIED];
        W
    }

    fn enabled(&self, _cfg: &C) -> bool {
        true
    }

    /// An inline declaration block is a fragment, and minify is safe on it.
    fn fragment_safe(&self) -> bool {
        true
    }

    fn run(
        &self,
        _ast: &mut CssAst,
        _cfg: &C,
        _rng: &mut Rng,
        _bus: &mut ArtifactBus,
        _notes: &mut Notes,
    ) -> Result<()> {
        // Minification already happened in `Css::parse`. Nothing to do here yet;
        // this is the attach point for the gated renaming pass (see `rename`).
        Ok(())
    }
}

/// A minimal `PassConfig` used by the convenience entry points below. The CSS
/// schedule has no preset-dependent knobs today, so a fixed seed suffices for the
/// deterministic per-pass RNG (the pass draws no randomness anyway).
#[derive(Debug, Clone, Copy)]
struct CssRunConfig {
    seed: u64,
}

impl PassConfig for CssRunConfig {
    fn seed(&self) -> u64 {
        self.seed
    }
}

/// Run the [`Css`] language + the one-pass CSS schedule over `src` with the given
/// [`CssParseOpts`], returning the minified output.
///
/// This is the generic core both [`process`] and [`process_inline`] delegate to;
/// it threads the parse-opt through and runs [`MinifyPass`] exactly as the WP6
/// runner loop would (per-pass RNG keyed on `(seed, id)`, bus scoped to the
/// pass's declared reads/writes).
fn run_schedule(src: &str, opts: CssParseOpts) -> Result<String> {
    let mut ast = Css.parse(src, &opts)?;

    let cfg = CssRunConfig { seed: 0 };
    let pass = MinifyPass;
    let mut rng = Rng::for_pass(cfg.seed(), Pass::<Css, CssRunConfig>::id(&pass));
    let mut bus = ArtifactBus::new();
    let mut notes = Notes::new();
    bus.enter_pass(
        Pass::<Css, CssRunConfig>::id(&pass),
        Pass::<Css, CssRunConfig>::reads(&pass),
        Pass::<Css, CssRunConfig>::writes(&pass),
    );
    pass.run(&mut ast, &cfg, &mut rng, &mut bus, &mut notes)?;

    Ok(Css.print(&ast))
}

/// Minify a whole CSS stylesheet.
///
/// Selectors / custom properties (`--var`) / keyframe names are **not** renamed:
/// they are externally referenceable from arbitrary HTML/JS, so renaming is
/// unsound (see [`rename`]). Mirrors the old `css::process` entry point that the
/// HTML crate (WP8) and CLI (WP9) call.
pub fn process(src: &str) -> Result<String> {
    run_schedule(src, CssParseOpts::stylesheet())
}

/// Minify a bare declaration block — the contents of an inline `style="…"`
/// attribute.
///
/// The declarations are wrapped in a throwaway rule, minified, then unwrapped so
/// only the minified declarations are returned. Mirrors the old
/// `css::process_inline` entry point.
pub fn process_inline(decls: &str) -> Result<String> {
    run_schedule(decls, CssParseOpts::inline())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_minifies_and_drops_comments() {
        let out = process("/* c */ .a {  color: #ffffff;  margin: 0px; }").unwrap();
        assert!(!out.contains("/*"));
        assert!(!out.contains(' ') || out.len() < 30);
        assert!(out.contains(".a"));
    }

    #[test]
    fn process_inline_minifies_declarations() {
        let out = process_inline("color:#ffffff;  margin:0px;").unwrap();
        assert!(!out.contains("  "), "double space remained: {out}");
        assert!(out.contains("#fff"), "color not minified: {out}");
        assert!(out.contains("margin:0"), "0px not minified: {out}");
        assert!(!out.contains('{') && !out.contains('}'), "wrapper leaked: {out}");
    }

    #[test]
    fn process_inline_tolerates_trailing_whitespace() {
        let out = process_inline("color: red").unwrap();
        assert!(!out.contains('{') && !out.contains('}'), "wrapper leaked: {out}");
        assert!(out.contains("red") || out.contains("color"), "content lost: {out}");
    }

    #[test]
    fn process_inline_round_trips_simple_decl() {
        let out = process_inline("color:red").unwrap();
        assert_eq!(out, "color:red");
    }

    #[test]
    fn process_is_deterministic() {
        let a = process(".a { color: #ffffff }").unwrap();
        let b = process(".a { color: #ffffff }").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn minify_pass_declares_its_resource() {
        let p = MinifyPass;
        assert_eq!(Pass::<Css, CssRunConfig>::id(&p), "css-minify");
        assert!(Pass::<Css, CssRunConfig>::writes(&p).contains(&CSS_MINIFIED));
        assert!(Pass::<Css, CssRunConfig>::reads(&p).is_empty());
        assert!(Pass::<Css, CssRunConfig>::fragment_safe(&p));
        assert!(Pass::<Css, CssRunConfig>::enabled(&p, &CssRunConfig { seed: 0 }));
    }
}
