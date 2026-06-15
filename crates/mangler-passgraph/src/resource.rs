//! The resource vocabulary — the nouns the scheduler reasons about.
//!
//! A [`Resource`] is a named thing a pass `reads` and/or `writes`. The scheduler
//! ([`crate::schedule`]) derives execution order purely from these declarations:
//! a pass that *reads* resource `R` is ordered after every enabled pass that
//! *writes* `R`. This replaces the original two-value [`Capability`] enum + the
//! hardcoded PreResolver/PostResolver phase split — *both* now fall out of the
//! same topological sort over a richer set of resources.
//!
//! # Why an open vocabulary
//!
//! The original model hardcoded JavaScript's two cross-pass artifacts
//! (`DecoderAnchor`, `VmArtifacts`) directly into a closed enum. That does not
//! generalize: CSS and HTML passes have their own ordering constraints and their
//! own artifacts. So [`Resource`] is **extensible** — it has a set of common,
//! language-agnostic built-ins (the [`Resource::Builtin`] variants) *and* an
//! escape hatch ([`Resource::Custom`]) carrying a stable string key, so a
//! language front-end can mint its own resources without modifying this crate.
//!
//! Two resources are equal iff they denote the same noun; equality drives the
//! "writer-before-reader" edges, so it must be cheap and total. Built-ins compare
//! by variant; custom resources compare by their string key.

use std::fmt;

/// The language-agnostic resources the scheduler understands out of the box.
///
/// These name the cross-pass dependencies that recur across front-ends. A
/// JavaScript pipeline uses most of them; other languages use the subset that
/// applies and mint the rest via [`Resource::Custom`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Builtin {
    /// Property names exposed as ordinary string literals (e.g. `obj.prop` →
    /// `obj["prop"]`). The member-access pass *writes* this; the strings pass
    /// *reads* it so those literals exist before it encodes them.
    PropertyLiterals,
    /// Global-name string literals (e.g. the `"document"` in an indirected global
    /// reference). The global-reference pass *writes* this; strings *reads* it so
    /// the injected names get encoded.
    GlobalNameLiterals,
    /// The strings-decoder anchor — a non-foldable opaque primitive other passes
    /// couple their opaque values to. The strings pass *writes* it; expression,
    /// control-flow-flatten and dead-code passes *read* it.
    DecoderAnchor,
    /// The VM interpreter + shared bytecode program table. The virtualize pass
    /// *writes* it; a later pass that wants to avoid bloating the table *reads*
    /// it.
    VmTable,
    /// Resolver-assigned scope information (swc `SyntaxContext`s). The resolver
    /// **pseudo-pass** *writes* this; any pass needing resolved scopes (e.g.
    /// control-flow flatten's TDZ rewrite, identifier renaming) *reads* it. This
    /// is what makes the old Pre/PostResolver split fall out of the sort.
    ResolvedScopes,
    /// The local-identifier mangling decision. The identifier-renaming pass
    /// *writes* it (it decides whether the built-in minifier mangle is
    /// suppressed); the minify pass *reads* it.
    MangleControl,
}

impl Builtin {
    /// The stable string key for this built-in, used in diagnostics and as the
    /// canonical identity shared with the [`Resource::Custom`] namespace. The key
    /// is prefixed so a custom resource can never collide with a built-in.
    pub const fn key(self) -> &'static str {
        match self {
            Builtin::PropertyLiterals => "builtin::property_literals",
            Builtin::GlobalNameLiterals => "builtin::global_name_literals",
            Builtin::DecoderAnchor => "builtin::decoder_anchor",
            Builtin::VmTable => "builtin::vm_table",
            Builtin::ResolvedScopes => "builtin::resolved_scopes",
            Builtin::MangleControl => "builtin::mangle_control",
        }
    }
}

/// A resource a pass declares as a read and/or a write.
///
/// Either one of the language-agnostic [`Builtin`]s or a language-specific
/// [`Custom`](Resource::Custom) resource keyed by a stable `&'static str`. Two
/// resources are equal iff their canonical [`key`](Resource::key)s match, so a
/// `Custom` resource that reuses a built-in's reserved prefix would alias — don't
/// do that (custom keys must not start with `builtin::`).
#[derive(Debug, Clone, Copy)]
pub enum Resource {
    /// One of the well-known, language-agnostic resources.
    Builtin(Builtin),
    /// A language-specific resource keyed by a stable string. The key is the
    /// identity: two `Custom` resources are the same resource iff their keys are
    /// equal.
    Custom(&'static str),
}

impl Resource {
    /// The canonical string identity of this resource. Equality, hashing and
    /// diagnostics all go through this, so it is the single source of truth for
    /// "are these the same noun".
    pub const fn key(self) -> &'static str {
        match self {
            Resource::Builtin(b) => b.key(),
            Resource::Custom(k) => k,
        }
    }

    // Convenience constructors for the built-ins so call sites read cleanly
    // (`Resource::decoder_anchor()` rather than the nested-enum spelling).

    /// The [`Builtin::PropertyLiterals`] resource.
    pub const fn property_literals() -> Self {
        Resource::Builtin(Builtin::PropertyLiterals)
    }
    /// The [`Builtin::GlobalNameLiterals`] resource.
    pub const fn global_name_literals() -> Self {
        Resource::Builtin(Builtin::GlobalNameLiterals)
    }
    /// The [`Builtin::DecoderAnchor`] resource.
    pub const fn decoder_anchor() -> Self {
        Resource::Builtin(Builtin::DecoderAnchor)
    }
    /// The [`Builtin::VmTable`] resource.
    pub const fn vm_table() -> Self {
        Resource::Builtin(Builtin::VmTable)
    }
    /// The [`Builtin::ResolvedScopes`] resource.
    pub const fn resolved_scopes() -> Self {
        Resource::Builtin(Builtin::ResolvedScopes)
    }
    /// The [`Builtin::MangleControl`] resource.
    pub const fn mangle_control() -> Self {
        Resource::Builtin(Builtin::MangleControl)
    }

    /// Mint a language-specific resource. The `key` is the identity and must be
    /// stable across runs; it must **not** begin with the reserved `builtin::`
    /// prefix (debug-asserted) or it could alias a built-in.
    pub fn custom(key: &'static str) -> Self {
        debug_assert!(
            !key.starts_with("builtin::"),
            "custom resource key {key:?} must not use the reserved `builtin::` prefix"
        );
        Resource::Custom(key)
    }
}

impl PartialEq for Resource {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for Resource {}

impl std::hash::Hash for Resource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state);
    }
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

impl From<Builtin> for Resource {
    fn from(b: Builtin) -> Self {
        Resource::Builtin(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_and_resource_keys_agree() {
        assert_eq!(
            Resource::decoder_anchor().key(),
            Builtin::DecoderAnchor.key()
        );
        assert_eq!(Resource::Builtin(Builtin::VmTable), Resource::vm_table());
    }

    #[test]
    fn custom_identity_is_the_key() {
        assert_eq!(Resource::custom("css::selectors"), Resource::custom("css::selectors"));
        assert_ne!(Resource::custom("css::selectors"), Resource::custom("css::at_rules"));
    }

    #[test]
    fn builtin_never_aliases_custom() {
        // Every built-in key carries the reserved prefix, so no well-formed
        // custom resource can ever collide with one.
        for b in [
            Builtin::PropertyLiterals,
            Builtin::GlobalNameLiterals,
            Builtin::DecoderAnchor,
            Builtin::VmTable,
            Builtin::ResolvedScopes,
            Builtin::MangleControl,
        ] {
            assert!(b.key().starts_with("builtin::"));
        }
    }

    #[test]
    fn equality_is_by_key_across_constructors() {
        let a: Resource = Builtin::DecoderAnchor.into();
        let b = Resource::decoder_anchor();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), "builtin::decoder_anchor");
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "reserved")]
    fn custom_rejects_builtin_prefix_in_debug() {
        let _ = Resource::custom("builtin::sneaky");
    }
}
