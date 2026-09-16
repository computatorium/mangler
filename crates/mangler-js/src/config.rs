//! [`FileConfig`] — the per-file configuration type every JS pass is run against.
//!
//! This is the `C: PassConfig` the runner threads through [`mangler_passgraph::Pass::run`].
//! It bundles three things a pass needs:
//!
//! 1. the resolved obfuscation config ([`mangler_config::ResolvedConfig`]) — a
//!    pass reads its own tuning via `cfg.resolved().passes.<x>`;
//! 2. the per-file **effective seed** (`eff_seed`, fingerprint-mixed) — returned
//!    by `seed()`, so every pass's RNG is `Rng::for_pass(eff_seed, pass.id())`;
//! 3. a **shared** [`NameAllocator`] behind a [`RefCell`], seeded from `eff_seed`
//!    with all source idents reserved — `cfg.fresh_name()` vends collision-free
//!    names.
//!
//! # Why ONE shared `RefCell<NameAllocator>` (the key soundness decision)
//!
//! `Pass::run` takes `cfg: &C` (a shared ref), and per-pass RNGs are independent
//! (`Rng::for_pass(seed, id)`). But fresh injected names must be unique **across
//! all passes file-wide** — two passes drawing from independent per-pass
//! allocators could mint the SAME `_0x…` name, capturing a binding and producing a
//! miscompile. A single shared allocator guarantees collision-freedom.
//!
//! This stays deterministic: pass order is the scheduler's (a pure function of the
//! declared reads/writes + id tie-break), and within a pass the draw order is
//! deterministic — so the sequence of `fresh_name()` calls, and thus the names
//! handed out, is identical for a given (source, seed). The `RefCell` is borrowed
//! only for the duration of one `fresh()` call, so there is never an aliasing
//! hazard (passes run sequentially, never concurrently).
//!
//! Note the allocator is seeded from `eff_seed` and consumes NO `Rng` draws (the
//! affine name scramble is derived from the seed alone), so it never interferes
//! with any pass's per-pass RNG stream.

use mangler_config::ResolvedConfig;
use mangler_core::{NameAllocator, PassConfig};
use std::cell::RefCell;
use std::collections::HashSet;

/// Per-file configuration handed to every pass as the `C: PassConfig`.
///
/// Construct with [`FileConfig::new`], passing the validated config, the per-file
/// effective seed, and the source identifier set (so injected names never collide
/// with user bindings). See the [module docs](self) for the shared-allocator
/// rationale.
pub struct FileConfig {
    resolved: ResolvedConfig,
    eff_seed: u64,
    allocator: RefCell<NameAllocator>,
    source_functions: Option<HashSet<(u32, String)>>,
    source_anonymous: Option<HashSet<u32>>,
    source_compiler_sites: Option<crate::passes::virtualize::SourceDependencies>,
    eval_class_contexts: Option<crate::passes::virtualize::eval_contexts::SourceClassContexts>,
    runtime_frontend: bool,
    runtime_intrinsics: Option<String>,
    runtime_eval_frontend: bool,
    runtime_eval_context: Option<mangler_vm::eval::EvalClassContext>,
    runtime_support: RefCell<Vec<(Vec<swc_core::ecma::ast::Stmt>, String)>>,
    runtime_internals: RefCell<HashSet<String>>,
    source_utf16: Option<mangler_vm::source_text::SourceTextMap>,
}

impl FileConfig {
    /// Build a per-file config.
    ///
    /// * `resolved` — the validated obfuscation configuration.
    /// * `eff_seed` — the per-file effective seed (fingerprint-mixed; see
    ///   [`crate::seed`]). This is what [`PassConfig::seed`] returns, so it drives
    ///   every pass's RNG.
    /// * `reserved_idents` — every identifier symbol in the source; reserved on
    ///   the allocator so `fresh_name` never collides with a user binding.
    pub fn new(resolved: ResolvedConfig, eff_seed: u64, reserved_idents: HashSet<String>) -> Self {
        let mut allocator = NameAllocator::new(eff_seed);
        allocator.reserve(reserved_idents);
        FileConfig {
            resolved,
            eff_seed,
            allocator: RefCell::new(allocator),
            source_functions: None,
            source_anonymous: None,
            source_compiler_sites: None,
            eval_class_contexts: None,
            runtime_frontend: false,
            runtime_intrinsics: None,
            runtime_eval_frontend: false,
            runtime_eval_context: None,
            runtime_support: RefCell::new(Vec::new()),
            runtime_internals: RefCell::new(HashSet::new()),
            source_utf16: None,
        }
    }

    pub(crate) fn with_source_utf16(mut self, map: mangler_vm::source_text::SourceTextMap) -> Self {
        if !map.is_empty() {
            self.source_utf16 = Some(map);
        }
        self
    }

    pub(crate) fn source_utf16(&self) -> Option<&mangler_vm::source_text::SourceTextMap> {
        self.source_utf16.as_ref()
    }

    pub(crate) fn with_eval_class_contexts(
        mut self,
        contexts: crate::passes::virtualize::eval_contexts::SourceClassContexts,
    ) -> Self {
        self.eval_class_contexts = Some(contexts);
        self
    }
    pub(crate) fn eval_class_contexts(
        &self,
    ) -> Option<&crate::passes::virtualize::eval_contexts::SourceClassContexts> {
        self.eval_class_contexts.as_ref()
    }

    pub(crate) fn with_runtime_eval_context(
        mut self,
        context: Option<mangler_vm::eval::EvalClassContext>,
    ) -> Self {
        self.runtime_eval_context = context;
        self
    }

    pub(crate) fn runtime_eval_context(&self) -> Option<&mangler_vm::eval::EvalClassContext> {
        self.runtime_eval_context.as_ref()
    }

    pub(crate) fn with_source_compiler_sites(
        mut self,
        sites: crate::passes::virtualize::SourceDependencies,
    ) -> Self {
        self.source_compiler_sites = Some(sites);
        self
    }

    pub(crate) fn source_compiler_dependencies(
        &self,
    ) -> Option<&crate::passes::virtualize::SourceDependencies> {
        self.source_compiler_sites.as_ref()
    }

    /// Pin protection targets to the input AST before generated helpers are inserted.
    pub(crate) fn with_source_functions(mut self, functions: Vec<(u32, String, bool)>) -> Self {
        self.source_anonymous = Some(
            functions
                .iter()
                .filter_map(|(span, _, anonymous)| anonymous.then_some(*span))
                .collect(),
        );
        self.source_functions = Some(
            functions
                .into_iter()
                .map(|(span, name, _)| (span, name))
                .collect(),
        );
        self
    }

    pub(crate) fn source_functions(&self) -> Option<&HashSet<(u32, String)>> {
        self.source_functions.as_ref()
    }

    pub(crate) fn source_anonymous(&self) -> Option<&HashSet<u32>> {
        self.source_anonymous.as_ref()
    }

    /// Bind generated helper reads to a host-provided startup snapshot.
    pub(crate) fn with_runtime_intrinsics(mut self, binding: String) -> Self {
        self.runtime_intrinsics = Some(binding);
        self
    }

    pub(crate) fn runtime_intrinsics(&self) -> Option<&str> {
        self.runtime_intrinsics.as_deref()
    }

    /// Runtime compilation shares the source pass but supplies its own compiler
    /// instance to generated tables instead of embedding another Wasm compiler.
    pub(crate) fn with_runtime_frontend(mut self) -> Self {
        self.runtime_frontend = true;
        self
    }

    pub(crate) fn runtime_frontend(&self) -> bool {
        self.runtime_frontend
    }

    pub(crate) fn with_runtime_eval_frontend(mut self) -> Self {
        self.runtime_frontend = true;
        self.runtime_eval_frontend = true;
        self
    }

    pub(crate) fn runtime_eval_frontend(&self) -> bool {
        self.runtime_eval_frontend
    }

    pub(crate) fn collect_runtime_internals(&self, names: &HashSet<String>) {
        self.runtime_internals
            .borrow_mut()
            .extend(names.iter().cloned());
    }

    pub(crate) fn take_runtime_internals(&self) -> HashSet<String> {
        self.runtime_internals.take()
    }

    /// Called only after the ordinary source-coverage gate accepts the program.
    pub(crate) fn collect_runtime_support(
        &self,
        statements: Vec<swc_core::ecma::ast::Stmt>,
        table: String,
    ) {
        self.runtime_support.borrow_mut().push((statements, table));
    }

    pub(crate) fn take_runtime_support(&self) -> Vec<(Vec<swc_core::ecma::ast::Stmt>, String)> {
        self.runtime_support.take()
    }

    /// The validated obfuscation config. A pass reads its own tuning here, e.g.
    /// `cfg.resolved().passes.expr.expr_obfuscation` or
    /// `cfg.resolved().engine.level`.
    pub fn resolved(&self) -> &ResolvedConfig {
        &self.resolved
    }

    /// Vend the next file-wide-unique, deterministic, collision-free `_0x…` name.
    ///
    /// Borrows the shared allocator for the duration of the call only. Every pass
    /// draws injected names through this single source, so no two passes can mint
    /// the same name (the soundness guarantee — see the [module docs](self)).
    pub fn fresh_name(&self) -> String {
        self.allocator.borrow_mut().fresh()
    }
}

impl PassConfig for FileConfig {
    /// The per-file **effective** seed (fingerprint-mixed), NOT the raw user seed.
    /// Combined with each pass's id to derive that pass's independent RNG stream.
    fn seed(&self) -> u64 {
        self.eff_seed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_config::{ConfigFlags, Intensity};

    fn resolved(level: Intensity) -> ResolvedConfig {
        let flags = ConfigFlags {
            preset: Some(level),
            seed: Some(1),
            ..Default::default()
        };
        ResolvedConfig::try_from(flags).expect("valid config")
    }

    #[test]
    fn seed_is_the_effective_seed() {
        let cfg = FileConfig::new(resolved(Intensity::Medium), 0xABCD, HashSet::new());
        assert_eq!(cfg.seed(), 0xABCD);
    }

    #[test]
    fn fresh_names_are_unique_and_deterministic() {
        let cfg = FileConfig::new(resolved(Intensity::Medium), 42, HashSet::new());
        let a = cfg.fresh_name();
        let b = cfg.fresh_name();
        assert_ne!(a, b, "successive fresh names must differ");
        assert!(a.starts_with("_0x"));

        // Same seed + same draw order → identical sequence (determinism).
        let cfg2 = FileConfig::new(resolved(Intensity::Medium), 42, HashSet::new());
        assert_eq!(a, cfg2.fresh_name());
    }

    #[test]
    fn fresh_name_skips_reserved_source_idents() {
        // Reserve the first name the allocator would produce, then prove it's skipped.
        let first = FileConfig::new(resolved(Intensity::Medium), 7, HashSet::new()).fresh_name();
        let mut reserved = HashSet::new();
        reserved.insert(first.clone());
        let cfg = FileConfig::new(resolved(Intensity::Medium), 7, reserved);
        assert_ne!(cfg.fresh_name(), first);
    }

    #[test]
    fn resolved_config_is_readable() {
        let cfg = FileConfig::new(resolved(Intensity::Minify), 1, HashSet::new());
        assert_eq!(cfg.resolved().engine.level, Intensity::Minify);
    }
}
