//! The typed artifact bus — a validated, type-keyed cross-pass side channel.
//!
//! This generalizes the original `PipelineArtifacts` struct (a fixed set of
//! `Option<…>` fields) into an open, [`TypeId`]-keyed map: a pass `put`s a value
//! of some artifact type `T`, and a later pass `get`s it back by that same type.
//! New artifacts no longer require editing a central struct — a pass just defines
//! its own `T` and any consumer reads `get::<T>()`.
//!
//! # The contract
//!
//! The bus is **validated against the declared reads/writes**. The runner scopes
//! the bus to the currently-running pass (via [`ArtifactBus::enter_pass`]) with
//! the resources that pass declared. Then:
//!
//! * [`ArtifactBus::put`] of an artifact type whose [`Artifact::RESOURCE`] is not
//!   in the running pass's `writes()` is a **contract violation**.
//! * [`ArtifactBus::get`] of an artifact type whose [`Artifact::RESOURCE`] is not
//!   in the running pass's `reads()` is a **contract violation**.
//!
//! This catches "a pass quietly depended on another pass's output without
//! declaring it" — exactly the implicit coupling the scheduler exists to make
//! explicit. A violation panics in debug builds (loud, during tests) and is
//! reported as a recoverable [`BusError`] in release builds — see
//! [`ArtifactBus::get`]/[`put`](ArtifactBus::put) for the exact contract.
//!
//! Each [`Artifact`] type is tied to exactly one [`Resource`] via its
//! `RESOURCE` const, so "the type you read" and "the resource you declared" are
//! the same fact checked two ways.

use crate::resource::Resource;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;

/// A value that can travel on the [`ArtifactBus`], tied to the [`Resource`] that
/// names it.
///
/// Implement this on any cross-pass artifact (the JS decoder handle, the VM
/// table names, …). The `RESOURCE` association is what lets the bus validate a
/// `get`/`put` against a pass's declared reads/writes: putting a `T` is allowed
/// iff `T::RESOURCE` is in the pass's `writes()`, and getting one iff it is in
/// the pass's `reads()`.
pub trait Artifact: Any + 'static {
    /// The resource this artifact *is*. A pass that writes this artifact must
    /// declare `RESOURCE` in its `writes()`; a pass that reads it must declare
    /// `RESOURCE` in its `reads()`.
    const RESOURCE: Resource;
}

/// A bus contract violation: a pass touched an artifact it did not declare.
///
/// Returned by [`ArtifactBus::get`]/[`ArtifactBus::put`] in release builds; the
/// same conditions panic in debug builds so tests fail loudly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    /// A pass called `get::<T>()` for `resource` it did not list in `reads()`.
    UndeclaredRead {
        /// The id of the offending pass.
        pass: &'static str,
        /// The resource it read without declaring.
        resource: Resource,
    },
    /// A pass called `put::<T>()` for `resource` it did not list in `writes()`.
    UndeclaredWrite {
        /// The id of the offending pass.
        pass: &'static str,
        /// The resource it wrote without declaring.
        resource: Resource,
    },
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusError::UndeclaredRead { pass, resource } => write!(
                f,
                "bus contract: pass `{pass}` read artifact for resource `{resource}` \
                 it did not declare in reads()"
            ),
            BusError::UndeclaredWrite { pass, resource } => write!(
                f,
                "bus contract: pass `{pass}` wrote artifact for resource `{resource}` \
                 it did not declare in writes()"
            ),
        }
    }
}

impl std::error::Error for BusError {}

/// The currently-running pass's declared resource scope, installed by the runner
/// via [`ArtifactBus::enter_pass`] so the bus can validate each access.
#[derive(Clone)]
struct PassScope {
    id: &'static str,
    reads: Vec<Resource>,
    writes: Vec<Resource>,
}

/// A type-keyed cross-pass artifact store with declaration-checked access.
///
/// Stores at most one value per artifact type (`TypeId`). The runner calls
/// [`ArtifactBus::enter_pass`] before invoking each pass, scoping subsequent
/// `get`/`put` calls to that pass's declared reads/writes.
#[derive(Default)]
pub struct ArtifactBus {
    store: HashMap<TypeId, Box<dyn Any>>,
    scope: Option<PassScope>,
}

impl ArtifactBus {
    /// An empty bus with no pass scope installed.
    pub fn new() -> Self {
        ArtifactBus::default()
    }

    /// Install the scope for the pass about to run: its id (for diagnostics) and
    /// its declared `reads`/`writes`. Every [`get`](Self::get)/[`put`](Self::put)
    /// until the next `enter_pass` is validated against these. The runner calls
    /// this immediately before [`crate::Pass::run`].
    pub fn enter_pass(&mut self, id: &'static str, reads: &[Resource], writes: &[Resource]) {
        self.scope = Some(PassScope {
            id,
            reads: reads.to_vec(),
            writes: writes.to_vec(),
        });
    }

    /// Read the artifact of type `T`, if one was produced.
    ///
    /// # Contract
    ///
    /// `T::RESOURCE` must be in the running pass's declared `reads()`. If it is
    /// not, this is a contract violation: **panics in debug builds**, and in
    /// release builds returns `Err(`[`BusError::UndeclaredRead`]`)`. A clean
    /// `Ok(None)` means the read is *declared* but no producer ran (the upstream
    /// pass was disabled) — the documented soft-degrade path.
    pub fn get<T: Artifact>(&self) -> Result<Option<&T>, BusError> {
        self.check_read(T::RESOURCE)?;
        Ok(self
            .store
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>()))
    }

    /// Store the artifact of type `T`, replacing any previous value of that type.
    ///
    /// # Contract
    ///
    /// `T::RESOURCE` must be in the running pass's declared `writes()`. If it is
    /// not, this is a contract violation: **panics in debug builds**, and in
    /// release builds returns `Err(`[`BusError::UndeclaredWrite`]`)` without
    /// storing anything.
    pub fn put<T: Artifact>(&mut self, value: T) -> Result<(), BusError> {
        self.check_write(T::RESOURCE)?;
        self.store.insert(TypeId::of::<T>(), Box::new(value));
        Ok(())
    }

    /// Whether an artifact of type `T` is currently present. Not scope-checked —
    /// this is a structural query used by the runner/tests, not a pass-facing
    /// read of the value.
    pub fn contains<T: Artifact>(&self) -> bool {
        self.store.contains_key(&TypeId::of::<T>())
    }

    fn check_read(&self, resource: Resource) -> Result<(), BusError> {
        let pass = self.scope.as_ref();
        let ok = pass.is_some_and(|s| s.reads.contains(&resource));
        if ok {
            return Ok(());
        }
        let id = pass.map(|s| s.id).unwrap_or("<no-scope>");
        let err = BusError::UndeclaredRead { pass: id, resource };
        debug_assert!(false, "{err}");
        Err(err)
    }

    fn check_write(&self, resource: Resource) -> Result<(), BusError> {
        let pass = self.scope.as_ref();
        let ok = pass.is_some_and(|s| s.writes.contains(&resource));
        if ok {
            return Ok(());
        }
        let id = pass.map(|s| s.id).unwrap_or("<no-scope>");
        let err = BusError::UndeclaredWrite { pass: id, resource };
        debug_assert!(false, "{err}");
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{Builtin, Resource};

    // A decoder-anchor artifact (ports `DecoderHandle`).
    #[derive(Debug, PartialEq, Eq)]
    struct DecoderHandle {
        core_name: String,
    }
    impl Artifact for DecoderHandle {
        const RESOURCE: Resource = Resource::Builtin(Builtin::DecoderAnchor);
    }

    // A VM-table artifact (ports `VmHandle`).
    #[derive(Debug, PartialEq, Eq)]
    struct VmHandle {
        interp: String,
    }
    impl Artifact for VmHandle {
        const RESOURCE: Resource = Resource::Builtin(Builtin::VmTable);
    }

    #[test]
    fn declared_write_then_declared_read_round_trips() {
        let mut bus = ArtifactBus::new();
        // Producer declares DecoderAnchor as a write.
        bus.enter_pass("strings", &[], &[Resource::decoder_anchor()]);
        bus.put(DecoderHandle { core_name: "_0x5".into() }).unwrap();

        // Consumer declares DecoderAnchor as a read.
        bus.enter_pass("expr", &[Resource::decoder_anchor()], &[]);
        let h = bus.get::<DecoderHandle>().unwrap().unwrap();
        assert_eq!(h.core_name, "_0x5");
    }

    #[test]
    fn declared_read_with_no_producer_is_ok_none() {
        let mut bus = ArtifactBus::new();
        bus.enter_pass("expr", &[Resource::decoder_anchor()], &[]);
        // Declared, but nobody put one → soft-degrade None, not an error.
        assert!(bus.get::<DecoderHandle>().unwrap().is_none());
    }

    // The two violation tests must observe the *release-build* `Err` path; the
    // debug_assert would otherwise panic. Gate them on debug_assertions being off
    // for the error-return assertion, and assert the panic when on.
    #[cfg(not(debug_assertions))]
    #[test]
    fn undeclared_read_is_rejected_release() {
        let mut bus = ArtifactBus::new();
        bus.enter_pass("expr", &[], &[]); // did NOT declare DecoderAnchor read
        let err = bus.get::<DecoderHandle>().unwrap_err();
        assert_eq!(
            err,
            BusError::UndeclaredRead { pass: "expr", resource: Resource::decoder_anchor() }
        );
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn undeclared_write_is_rejected_release() {
        let mut bus = ArtifactBus::new();
        bus.enter_pass("expr", &[], &[]); // did NOT declare DecoderAnchor write
        let err = bus.put(DecoderHandle { core_name: "x".into() }).unwrap_err();
        assert_eq!(
            err,
            BusError::UndeclaredWrite { pass: "expr", resource: Resource::decoder_anchor() }
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "did not declare in reads")]
    fn undeclared_read_panics_in_debug() {
        let mut bus = ArtifactBus::new();
        bus.enter_pass("expr", &[], &[]);
        let _ = bus.get::<DecoderHandle>();
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "did not declare in writes")]
    fn undeclared_write_panics_in_debug() {
        let mut bus = ArtifactBus::new();
        bus.enter_pass("expr", &[], &[]);
        let _ = bus.put(DecoderHandle { core_name: "x".into() });
    }

    #[test]
    fn artifacts_are_keyed_by_type_not_resource() {
        // Two distinct artifact types coexist; reading one never returns the other.
        let mut bus = ArtifactBus::new();
        bus.enter_pass(
            "p",
            &[Resource::decoder_anchor(), Resource::vm_table()],
            &[Resource::decoder_anchor(), Resource::vm_table()],
        );
        bus.put(DecoderHandle { core_name: "d".into() }).unwrap();
        bus.put(VmHandle { interp: "v".into() }).unwrap();
        assert_eq!(bus.get::<DecoderHandle>().unwrap().unwrap().core_name, "d");
        assert_eq!(bus.get::<VmHandle>().unwrap().unwrap().interp, "v");
        assert!(bus.contains::<DecoderHandle>());
    }
}
