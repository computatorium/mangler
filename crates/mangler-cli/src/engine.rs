//! The [`Engine`] façade — the library entry point.
//!
//! `Engine` is a thin, web-app-usable façade over the per-language processors. It
//! owns the **language-dispatch seam** (the single place that names a concrete
//! language module), wires HTML's [`EmbedHandlers`] so embedded `<script>`/`on*=`
//! go through `mangler_js` and `<style>`/`style=` through `mangler_css`, and owns
//! the batch driver ([`Engine::process_many`]) with its **rayon** parallelism,
//! **result ordering**, and aggregate **stats**.
//!
//! It is a library: it returns structured [`Output`]/[`Error`] and NEVER prints or
//! exits. Rendering (stderr notes, the size report) and exit codes live in the
//! CLI (`main.rs`).

use mangler_config::{Lang, ResolvedConfig};
use mangler_core::{Error, Notes, PassConfig, Result};
use mangler_html::{CssContext, EmbedHandlers, JsContext};
use mangler_jsast::ParseOpts;
use rayon::prelude::*;

/// One unit of work handed to the [`Engine`]: a source string plus everything
/// needed to process and place its output.
///
/// Built fluently (`Input::new(src).with_lang(..).with_path(..)`) so embedders
/// and the CLI alike construct it without poking at fields.
#[derive(Debug, Clone, Default)]
pub struct Input {
    /// Source file path, or `None` when the input came from stdin / memory.
    pub path: Option<std::path::PathBuf>,
    /// Explicit language. `None` means "detect from the path extension".
    pub lang: Option<Lang>,
    /// The full source text.
    pub source: String,
    /// Relative output path used when the output target is a DIRECTORY: the path
    /// RELATIVE to the input root (so a nested tree is mirrored, not collapsed
    /// onto basenames). `None` for stdin / in-memory inputs.
    pub rel: Option<std::path::PathBuf>,
}

impl Input {
    /// An input from in-memory `source` (no path, no forced language).
    pub fn new(source: impl Into<String>) -> Self {
        Input {
            source: source.into(),
            ..Default::default()
        }
    }

    /// An input read from stdin (no path; language must be set explicitly).
    pub fn stdin(source: impl Into<String>) -> Self {
        Input::new(source)
    }

    /// Attach the source path (used for language detection and in-place writes).
    pub fn with_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Force the language, bypassing extension detection.
    pub fn with_lang(mut self, lang: Lang) -> Self {
        self.lang = Some(lang);
        self
    }

    /// Set the directory-target-relative output path.
    pub fn with_rel(mut self, rel: impl Into<std::path::PathBuf>) -> Self {
        self.rel = Some(rel.into());
        self
    }

    /// A human-readable name for diagnostics (the path, or `<stdin>`).
    pub fn name(&self) -> String {
        self.path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<stdin>".into())
    }
}

/// Input/output byte sizes for one processed unit — the size report.
///
/// Obfuscation expands output, so the meaningful figure is the growth `ratio`
/// (out/in), not "% saved".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    /// Input length in bytes.
    pub input_bytes: usize,
    /// Output length in bytes.
    pub output_bytes: usize,
}

impl Stats {
    /// Output/input growth ratio (`1.0` = unchanged; `>1.0` = expansion).
    pub fn ratio(&self) -> f64 {
        self.output_bytes as f64 / self.input_bytes.max(1) as f64
    }
}

/// The result of processing one [`Input`]: the mangled code plus the non-error
/// notes channel and the size [`Stats`].
#[derive(Debug, Clone)]
pub struct Output {
    /// The obfuscated source, ready to be written to the output target.
    pub code: String,
    /// Non-error (skip/info) notes surfaced under `--verbose`.
    pub notes: Notes,
    /// Input/output byte sizes.
    pub stats: Stats,
}

/// Adapts a [`ResolvedConfig`] into the [`PassConfig`] the HTML pass needs
/// (it only requires the seed). `ResolvedConfig` lives in another crate and
/// cannot impl `PassConfig` itself, so this thin local wrapper bridges it.
struct SeedCfg(u64);

impl PassConfig for SeedCfg {
    fn seed(&self) -> u64 {
        self.0
    }
}

/// The library façade. Cheap to clone; holds the validated [`ResolvedConfig`]
/// and the fragment config derived once for embedded JS/CSS.
#[derive(Debug, Clone)]
pub struct Engine {
    config: ResolvedConfig,
    fragment: ResolvedConfig,
}

impl Engine {
    /// Build an engine from a validated configuration.
    pub fn new(config: ResolvedConfig) -> Self {
        let fragment = config.for_fragment();
        Engine { config, fragment }
    }

    /// The configuration this engine runs with.
    pub fn config(&self) -> &ResolvedConfig {
        &self.config
    }

    /// Resolve the language for `input`: explicit `Input::lang`, else the
    /// engine's `--lang`, else extension detection off the path.
    fn lang_of(&self, input: &Input) -> Result<Lang> {
        if let Some(l) = input.lang.or(self.config.engine.lang) {
            return Ok(l);
        }
        input
            .path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .and_then(Lang::from_ext)
            .ok_or_else(|| {
                Error::config(format!(
                    "cannot detect language for {} (pass --lang)",
                    input.name()
                ))
            })
    }

    /// Process a single [`Input`]: detect/use its [`Lang`], dispatch to the right
    /// processor, and (for HTML) wire the [`EmbedHandlers`]. This is the single
    /// language-dispatch seam.
    pub fn process(&self, input: &Input) -> Result<Output> {
        let lang = self.lang_of(input)?;
        if lang != Lang::Js && self.config.passes.virtualize.required.is_some() {
            return Err(Error::config(
                "--require-virtualized is supported only for JavaScript inputs",
            ));
        }
        let (code, notes) = match lang {
            Lang::Js => {
                let opts = match &input.path {
                    Some(p) => ParseOpts::from_filename(&p.to_string_lossy()),
                    None => ParseOpts::default(),
                };
                mangler_js::process(&input.source, &opts, &self.config)?
            }
            Lang::Css => (mangler_css::process(&input.source)?, Notes::new()),
            Lang::Html => self.process_html(&input.source)?,
        };
        let stats = Stats {
            input_bytes: input.source.len(),
            output_bytes: code.len(),
        };
        Ok(Output { code, notes, stats })
    }

    /// Process HTML, routing embedded JS/CSS through the real processors with the
    /// fragment config (anti-tamper / virtualize / in-VM strings stripped).
    fn process_html(&self, src: &str) -> Result<(String, Notes)> {
        // The fragment config is used for every embedded snippet, whether a full
        // `<script>`/`<style>` body or an inline `on*=`/`style=` value. JsContext
        // selects body-vs-inline parse opts; CSS uses whole-sheet vs inline-block.
        let fragment = &self.fragment;
        let js = move |code: &str, ctx: JsContext| -> std::result::Result<String, ()> {
            let opts = ParseOpts {
                // An inline handler is a bare statement list, never a module.
                module: matches!(ctx, JsContext::ScriptBody),
                ..ParseOpts::default()
            };
            mangler_js::process(code, &opts, fragment)
                .map(|(out, _notes)| out)
                .map_err(|_| ())
        };
        let css = move |decls: &str, ctx: CssContext| -> std::result::Result<String, ()> {
            match ctx {
                CssContext::StyleBody => mangler_css::process(decls).map_err(|_| ()),
                CssContext::InlineStyle => mangler_css::process_inline(decls).map_err(|_| ()),
            }
        };
        let handlers = EmbedHandlers { js: &js, css: &css };
        let mut notes = Notes::new();
        let out =
            mangler_html::process(src, &SeedCfg(self.config.engine.seed), handlers, &mut notes)?;
        Ok((out, notes))
    }

    /// Process many inputs in parallel (rayon), returning one result PER input in
    /// **input order** (a failing input does not drop or reorder the others).
    ///
    /// The engine owns parallelism here; the caller writes the results
    /// sequentially so stdout ordering stays deterministic. Each [`Output`]
    /// carries its own [`Stats`].
    pub fn process_many(&self, inputs: &[Input]) -> Vec<Result<Output>> {
        parallel_map(inputs, |input| self.process(input))
    }

    /// Load and process one bounded batch, dropping each source after its output
    /// is produced. Rayon preserves slice order, including read/transform errors.
    pub(crate) fn process_loaded<T: Sync>(
        &self,
        items: &[T],
        load: impl Fn(&T) -> anyhow::Result<Input> + Sync,
    ) -> Vec<anyhow::Result<Output>> {
        use anyhow::Context;
        parallel_map(items, |item| {
            let input = load(item)?;
            self.process(&input).with_context(|| input.name())
        })
    }
}

/// The single batch scheduling mechanism used by both library and CLI callers.
fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    operation: impl Fn(&T) -> R + Sync + Send,
) -> Vec<R> {
    items.par_iter().map(operation).collect()
}
