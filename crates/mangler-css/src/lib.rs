//! `mangler-css` — the CSS [`Language`](mangler_core::Language) implementation
//! over [`lightningcss`].
//!
//! CSS is the *conservative* front-end in the workspace: it is **minified but not
//! truly obfuscated**. Selectors, custom properties (`--var`) and keyframe names
//! are reachable from arbitrary external HTML and JS, so renaming them would be
//! unsound (see [`pass::rename`] for the documented seam and the config gate a
//! future renaming pass would require). The whole CSS schedule today is therefore
//! a single minify pass.
//!
//! # Layout
//!
//! * [`lang`] — the [`Css`] [`Language`](mangler_core::Language) impl. Its
//!   [`type Ast`](mangler_core::Language::Ast) is an **owned** [`lang::CssAst`]
//!   (the minified output string), and its
//!   [`ParseOpts`](mangler_core::Language::ParseOpts) is [`lang::CssParseOpts`],
//!   a variant that selects whole-stylesheet vs. inline-declaration-block mode.
//!   See [`lang`] for the lifetime-handling rationale.
//! * [`pass`] — the [`pass::MinifyPass`] (`"css-minify"`), the one-pass schedule,
//!   plus the [`pass::process`] / [`pass::process_inline`] convenience entry
//!   points the HTML crate (WP8) and CLI (WP9) call.
//! * [`pass::rename`] — the documented, currently-disabled extension seam for a
//!   future selector/var/keyframe renaming pass.
//!
//! # Public API at a glance
//!
//! ```no_run
//! use mangler_css::{Css, CssParseOpts, process, process_inline};
//! # fn demo() -> mangler_core::Result<()> {
//! let whole = process(".a { color: #ffffff }")?;        // full stylesheet
//! let inline = process_inline("color:#ffffff; margin:0px")?; // bare decl block
//! # let _ = (Css, CssParseOpts::stylesheet());
//! # let _ = (whole, inline);
//! # Ok(())
//! # }
//! ```

pub mod lang;
pub mod pass;

pub use lang::{Css, CssAst, CssParseOpts};
pub use pass::{process, process_inline, MinifyPass};
