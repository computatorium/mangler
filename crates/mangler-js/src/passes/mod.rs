//! The JS passes — one submodule per pass.
//!
//! Each pass is a [`Pass<Js, FileConfig>`](mangler_passgraph::Pass) that declares
//! its [`reads`](mangler_passgraph::Pass::reads) /
//! [`writes`](mangler_passgraph::Pass::writes) over the
//! [`Resource`](mangler_passgraph::Resource) vocabulary; the scheduler derives the
//! execution order, so the legacy PreResolver/PostResolver phase split falls out of
//! the topological sort (the resolver is a pseudo-pass that writes
//! [`Resource::resolved_scopes`](mangler_passgraph::Resource::resolved_scopes)).
//!
//! # The canonical pass shape (copy this for a new pass)
//!
//! ```ignore
//! use crate::config::FileConfig;
//! use mangler_core::{Language, Notes, Result, Rng};
//! use mangler_jsast::Js;
//! use mangler_passgraph::{ArtifactBus, Pass, Resource};
//!
//! pub struct MyPass;
//!
//! impl Pass<Js, FileConfig> for MyPass {
//!     fn id(&self) -> &'static str { "mypass" }            // unique; keys the RNG
//!     fn reads(&self)  -> &[Resource] { const R: &[Resource] = &[/* … */]; R }
//!     fn writes(&self) -> &[Resource] { const W: &[Resource] = &[/* … */]; W }
//!     fn enabled(&self, cfg: &FileConfig) -> bool {
//!         cfg.resolved().passes.<x>.<knob>                 // read your config knob
//!     }
//!     fn run(
//!         &self,
//!         ast: &mut <Js as Language>::Ast,
//!         cfg: &FileConfig,
//!         rng: &mut Rng,                                   // per-pass; from (seed, id)
//!         bus: &mut ArtifactBus,
//!         notes: &mut Notes,
//!     ) -> Result<()> {
//!         // names: cfg.fresh_name();   randomness: rng.*;   config: cfg.resolved()
//!         // read an artifact:  if let Some(a) = bus.get::<SomeArtifact>()? { … }
//!         // write an artifact: bus.put(SomeArtifact { … })?;
//!         Ok(())
//!     }
//! }
//! ```
//!
//! Then append a boxed instance in [`crate::runner::register_passes`].
//!
//! # Implemented here
//!
//! * [`memberaccess`] — `obj.prop` → `obj["prop"]` (exemplar; writes
//!   `PropertyLiterals`).
//! * [`expr`] — integer/boolean literal obfuscation via the decoupled
//!   [`opaque`](crate::opaque) library (exemplar; reads `DecoderAnchor`,
//!   optionally).
//!
//! # Pass resource dependencies
//!
//! | pass         | reads                                  | writes                  | enabled knob                                  |
//! |--------------|----------------------------------------|-------------------------|-----------------------------------------------|
//! | memberaccess | —                                      | PropertyLiterals        | `expr.member_access`                          |
//! | globalref    | PropertyLiterals                       | GlobalNameLiterals      | `global_indirect.mode != Off`                 |
//! | strings      | PropertyLiterals, GlobalNameLiterals   | DecoderAnchor (+VmTable)| `strings.mode != None`                        |
//! | resolver     | —                                      | ResolvedScopes          | always (pseudo-pass; in the runner)           |
//! | expr         | DecoderAnchor (optional)               | —                       | `expr.expr_obfuscation`                       |
//! | cfflatten    | DecoderAnchor (opt), ResolvedScopes    | —                       | `cf_flatten.enabled`                          |
//! | deadcode     | DecoderAnchor (optional)               | —                       | `cf_flatten.dead_code_rate > 0`               |
//! | virtualize   | —                                      | VmTable                 | `virtualize.target.is_some()`                 |
//! | idnames      | ResolvedScopes                         | MangleControl           | `mangle.enabled` (+ naming scheme)            |
//! | minify       | MangleControl                          | —                       | always (terminal codegen; in the runner)      |
//!
//! Scheduling invariants:
//! * **globalref** must run after memberaccess (so it sees `document["getElementById"]`)
//!   and before strings (so its injected global-name literals get encoded) — both
//!   edges come from the `PropertyLiterals` read and the `GlobalNameLiterals` write.
//! * **virtualize** runs before the resolver in the legacy flow (so its spliced
//!   interpreter gets marks). Declare `writes = [VmTable]` and NO `reads =
//!   [ResolvedScopes]`; the sort then places it pre-resolver. cf-flatten reads
//!   `ResolvedScopes`, so it lands post-resolver, after virtualize.
//! * **strings** may ALSO write `VmTable` (in-VM strings); declaring it is what lets
//!   downstream passes skip the shared table.
//! * **anti-tamper** and the self-coupled-key patch are NOT scheduler passes — they
//!   are post-codegen string finalizers applied by the runner (see
//!   [`crate::selfdefend`] and [`crate::runner`]).

pub mod expr;
pub mod memberaccess;

pub mod cfflatten;
pub mod deadcode;
pub mod globalref;
pub mod idnames;
pub mod strings;
pub(crate) mod intrinsics;
pub mod suspension;
mod resources;
pub mod virtualize;
