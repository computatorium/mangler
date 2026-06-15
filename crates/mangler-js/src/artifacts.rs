//! Bus artifact types — the typed cross-pass side channel.
//!
//! Each type here implements [`mangler_passgraph::Artifact`], tying it to exactly
//! one [`mangler_passgraph::Resource`] via its `RESOURCE` const. A producing pass
//! `put`s one (and must declare that resource in its `writes()`); a consuming pass
//! `get`s it (and must declare it in its `reads()`). The bus validates every
//! access against those declarations, so an undeclared cross-pass dependency is a
//! loud contract violation rather than silent coupling.
//!
//! These generalize the legacy `PipelineArtifacts` struct's fixed `Option<…>`
//! fields ([`DecoderHandle`], [`VmHandle`], the resolver marks) into open,
//! type-keyed bus entries.
//!
//! # How a pass uses these
//!
//! ```ignore
//! // Producer (declares `writes() = [Resource::decoder_anchor()]`):
//! bus.put(DecoderAnchorArtifact { core_name: name })?;
//!
//! // Consumer (declares `reads() = [Resource::decoder_anchor()]`):
//! if let Some(anchor) = bus.get::<DecoderAnchorArtifact>()? {
//!     // use anchor.core_name …
//! }
//! ```
//!
//! A clean `Ok(None)` from `get` means the read was *declared* but the producer
//! did not run (it was disabled) — the documented soft-degrade path.

use mangler_passgraph::{Artifact, Resource};
use swc_core::common::Mark;
use swc_core::ecma::ast::Pat;

/// Resolver-assigned scope information: the swc `(unresolved, top_level)` marks.
///
/// Written by the resolver pseudo-pass (`id() == "resolver"`); read by every pass
/// that needs resolved scopes — control-flow-flatten's TDZ rewrite and the
/// identifier renamer. This is what makes the legacy Pre/PostResolver phase split
/// fall out of the topological sort: a "post-resolver" pass simply declares
/// `reads() = [Resource::resolved_scopes()]`.
///
/// `Mark` is a cheap `Copy` swc handle valid only within the `GLOBALS` scope the
/// runner installs for the whole file; readers use it inside that same scope.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedScopesArtifact {
    /// The mark swc assigns to unresolved (free / global) references.
    pub unresolved_mark: Mark,
    /// The mark swc assigns to top-level bindings.
    pub top_level_mark: Mark,
}

impl Artifact for ResolvedScopesArtifact {
    const RESOURCE: Resource = Resource::resolved_scopes();
}

/// The strings-decoder anchor — the non-foldable opaque primitive other passes
/// couple their opaque values to (the legacy `DecoderHandle`).
///
/// Written by the strings pass when it injected a usable decoder (≥1 entry); read
/// by the expression, control-flow-flatten and dead-code passes. When absent, the
/// opaque library ([`crate::opaque`]) falls back to its OWN injected anchor (see
/// design decision 4), so reading this is OPTIONAL — a `None` is a soft-degrade,
/// not an error.
#[derive(Debug, Clone)]
pub struct DecoderAnchorArtifact {
    /// Name of the `var <core> = (function(){…})()` decoder declarator. A call
    /// `core(0)` is the impure-looking, swc-unfoldable anchor.
    pub core_name: String,
}

impl DecoderAnchorArtifact {
    /// Is `name` the binding of the decoder core's own declarator? The single
    /// source of truth for the "don't emit `core(0)` inside `var <core> = …`"
    /// guard that expr / cf-flatten / dead-code apply. Mirrors the legacy
    /// `DecoderHandle::is_core_declarator`.
    pub fn is_core_declarator(&self, name: &Pat) -> bool {
        matches!(name, Pat::Ident(bi) if bi.id.sym.as_ref() == self.core_name)
    }
}

impl Artifact for DecoderAnchorArtifact {
    const RESOURCE: Resource = Resource::decoder_anchor();
}

/// The VM interpreter + shared bytecode program table (the legacy `VmHandle`).
///
/// Written by the virtualize pass (and, for in-VM strings, the strings pass) when
/// any body was virtualized; read by passes that must avoid bloating the table's
/// hoisted bytecode-integer arrays (expr, cf-flatten, dead-code skip these names).
#[derive(Debug, Clone)]
pub struct VmTableArtifact {
    /// Name of the injected VM interpreter function.
    pub interp_name: String,
    /// Name of the hoisted bytecode program-table binding shared by all
    /// virtualized chunks. Downstream passes read this to skip (not bloat) it.
    pub program_table_name: String,
}

impl Artifact for VmTableArtifact {
    const RESOURCE: Resource = Resource::vm_table();
}

/// The self-coupled-key (`--self-coupled-key`) hand-off: the name of the strings
/// pass's emitted in-VM interpreter, communicated to the runner so its POST-codegen
/// finalizer can compute the build-time expected source hash over the EXACT emitted
/// interpreter + decode-wrapper and rewrite the `SCK<digits>` sentinel.
///
/// Written by the strings pass ONLY when `self_coupled_key` is active (flag on, a VM
/// decode chunk was produced, not `--verify`); read by the runner after
/// `selfdefend::wrap`. Reuses the `decoder_anchor` resource channel — the strings
/// pass already declares it in `writes()`, and putting two artifacts on one resource
/// is fine (the bus keys on the concrete type). Absent = the finalizer is skipped.
#[derive(Debug, Clone)]
pub struct SelfCoupledKeyArtifact {
    /// Name of the strings-VM interpreter function (a top-level declaration, NOT
    /// mangled, so its emitted name equals this generated name).
    pub interp_name: String,
}

impl Artifact for SelfCoupledKeyArtifact {
    const RESOURCE: Resource = Resource::decoder_anchor();
}

/// The local-identifier mangling decision (the legacy `suppress_builtin_mangle`
/// flag, plus the run-wide reserved set for swc's mangle).
///
/// Written by the identifier-renaming pass: when it applies its own confusing
/// scheme it sets `suppress_builtin_mangle = true` so swc's built-in mangle does
/// not double-rename. Read by the terminal codegen (`minify`), which the runner
/// performs by calling [`mangler_jsast::Js::print_optimized`] with `mangle =
/// !suppress_builtin_mangle`.
///
/// `reserved` carries the `--keep-names` matches so swc's mangle preserves them on
/// the fallback path. Defaults to "run swc mangle, reserve the keep-names".
#[derive(Debug, Clone, Default)]
pub struct MangleControlArtifact {
    /// When `true`, suppress swc's built-in name mangle (the renamer already
    /// renamed the exact locals swc would have).
    pub suppress_builtin_mangle: bool,
    /// Names to preserve from swc's mangle on the fallback path (`--keep-names`).
    pub reserved: Vec<String>,
}

impl Artifact for MangleControlArtifact {
    const RESOURCE: Resource = Resource::mangle_control();
}

/// Marker that property-name string literals now exist (`obj.prop` →
/// `obj["prop"]`). The member-access pass writes it; the strings pass reads it so
/// those literals exist before it encodes them. Carries no payload — its presence
/// (and the scheduler edge it implies) is the whole signal.
#[derive(Debug, Clone, Copy, Default)]
pub struct PropertyLiteralsArtifact;

impl Artifact for PropertyLiteralsArtifact {
    const RESOURCE: Resource = Resource::property_literals();
}

/// Marker that global-name string literals were injected (indirected global
/// references). The global-reference pass writes it; the strings pass reads it so
/// the injected names get encoded. Payload-free, like [`PropertyLiteralsArtifact`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GlobalNameLiteralsArtifact;

impl Artifact for GlobalNameLiteralsArtifact {
    const RESOURCE: Resource = Resource::global_name_literals();
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_passgraph::ArtifactBus;
    use swc_core::common::{SyntaxContext, DUMMY_SP};
    use swc_core::ecma::ast::{BindingIdent, Ident};

    fn ident_pat(sym: &str) -> Pat {
        Pat::Ident(BindingIdent {
            id: Ident::new(sym.into(), DUMMY_SP, SyntaxContext::empty()),
            type_ann: None,
        })
    }

    #[test]
    fn decoder_anchor_round_trips_through_bus() {
        let mut bus = ArtifactBus::new();
        bus.enter_pass("strings", &[], &[Resource::decoder_anchor()]);
        bus.put(DecoderAnchorArtifact {
            core_name: "_0x5".into(),
        })
        .unwrap();

        bus.enter_pass("expr", &[Resource::decoder_anchor()], &[]);
        let a = bus.get::<DecoderAnchorArtifact>().unwrap().unwrap();
        assert_eq!(a.core_name, "_0x5");
        assert!(a.is_core_declarator(&ident_pat("_0x5")));
        assert!(!a.is_core_declarator(&ident_pat("_0x6")));
    }

    #[test]
    fn distinct_artifact_types_coexist() {
        let mut bus = ArtifactBus::new();
        bus.enter_pass(
            "p",
            &[],
            &[Resource::decoder_anchor(), Resource::vm_table()],
        );
        bus.put(DecoderAnchorArtifact {
            core_name: "d".into(),
        })
        .unwrap();
        bus.put(VmTableArtifact {
            interp_name: "v".into(),
            program_table_name: "t".into(),
        })
        .unwrap();
        assert!(bus.contains::<DecoderAnchorArtifact>());
        assert!(bus.contains::<VmTableArtifact>());
    }

    #[test]
    fn mangle_control_default_runs_swc_mangle() {
        let mc = MangleControlArtifact::default();
        assert!(!mc.suppress_builtin_mangle);
        assert!(mc.reserved.is_empty());
    }
}
