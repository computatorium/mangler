//! The (currently disabled) selector / custom-property / keyframe **renaming**
//! seam.
//!
//! # Why renaming CSS identifiers is UNSAFE today
//!
//! Minification is always safe: it changes the *bytes* of a stylesheet without
//! changing which elements match or what any selector/name *refers to*. Renaming
//! is fundamentally different — it rewrites identities that other documents may
//! depend on, and CSS identities are **open**: they are referenceable from
//! arbitrary, possibly-unseen HTML and JavaScript.
//!
//! Concretely, the three classes of CSS identifier are all externally reachable:
//!
//! * **Selectors** — class names (`.btn`), ids (`#nav`) and the attributes they
//!   match are chosen by *HTML* (`class="btn"`) and queried by *JS*
//!   (`document.querySelector(".btn")`, `el.classList.add("btn")`,
//!   `getElementsByClassName`). Renaming `.btn` → `.a` silently breaks every such
//!   reference we cannot see.
//! * **Custom properties** (`--brand-color`) — read/written from JS via
//!   `getComputedStyle(el).getPropertyValue("--brand-color")` and
//!   `el.style.setProperty("--brand-color", …)`, and inheritable across the whole
//!   subtree. Renaming one requires rewriting every consumer everywhere.
//! * **`@keyframes` names** — the name binds a `@keyframes foo` definition to its
//!   `animation-name: foo` use sites, which can live in *other* stylesheets, in
//!   inline styles, or be set from JS. Renaming the definition without every use
//!   site (across files we may never be handed) breaks the animation.
//!
//! Because the obfuscator is, in general, handed a *single* CSS file with **no
//! whole-program closure** over the HTML/JS that references it, renaming any of
//! these is unsound by default. This is why the CSS schedule
//! ([`crate::pass::MinifyPass`]) is minify-only.
//!
//! # What would make it safe — the config gate
//!
//! Renaming becomes sound only under an explicit **closed-world assumption**: the
//! caller asserts that *every* referencer of the CSS identities is present and
//! will be rewritten consistently in the same run. That assumption must be an
//! opt-in config knob, not a default. When WP5's [`PassConfig`] grows the needed
//! accessor, a renaming pass would gate on something like:
//!
//! ```ignore
//! /// Extension trait WP5's config would implement (or fold into PassConfig).
//! trait CssRenameConfig {
//!     /// The caller asserts a CLOSED WORLD: every HTML/JS referencer of CSS
//!     /// selectors / `--vars` / `@keyframes` names is in this run and will be
//!     /// rewritten with the same name map. Default: false (unsound otherwise).
//!     fn css_rename_external_refs_owned(&self) -> bool { false }
//! }
//! ```
//!
//! and only rename the classes the caller additionally enabled (selectors vs.
//! vars vs. keyframes can be gated independently, since their referencer sets
//! differ). Such a pass would **read** [`crate::pass::CSS_MINIFIED`] (run after
//! minify settles the structure) and a shared name map produced by the HTML/JS
//! side, re-parsing [`CssAst::source`](crate::lang::CssAst::source) with the
//! lightningcss `css_modules` / visitor machinery to rewrite identifiers
//! deterministically from the per-pass RNG.
//!
//! # Status
//!
//! Intentionally **not implemented**. This module is the documented extension
//! point only; there is no enabled renaming pass. When the closed-world gate and
//! the cross-language name map exist, the pass lands here as a `Pass<Css, C>`
//! that reads [`crate::pass::CSS_MINIFIED`].

// No public items yet — see the module docs for the seam this reserves.
