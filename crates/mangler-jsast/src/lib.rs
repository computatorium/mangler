//! `mangler-jsast` — the JS/TS Language implementation over swc, plus the ONE
//! reusable traversal/rewrite layer and the ONE AST-builder codegen discipline.
//!
//! Shared parsing, binding analysis, AST construction, traversal and final
//! code generation. Compiler passes use these mechanisms to preserve source
//! identity, resolver contexts and runtime provenance across transformations.
//!
//! ## Module map
//!
//! * [`lang`] — [`Js`](lang::Js): the [`mangler_core::Language`] impl. [`Ast`](lang::Ast)
//!   bundles the swc `Program` **and** its `SourceMap`. Explicit dialect via
//!   [`ParseOpts`](lang::ParseOpts) (no filename sniffing). Also the shared resolver
//!   ([`Js::resolve`](lang::Js::resolve)) and optimize+emit
//!   ([`Js::print_optimized`](lang::Js::print_optimized)) plumbing.
//! * [`rewrite`] — the single `VisitMut` wrapper: [`rewrite_exprs`](rewrite::rewrite_exprs),
//!   [`rewrite_stmts`](rewrite::rewrite_stmts), [`replace_if`](rewrite::replace_if),
//!   the [`Walk`](rewrite::Walk) builder with explicit [`Order`](rewrite::Order), and the
//!   first-class skip-protected-subtree mechanism
//!   ([`Walk::skip_subtree`](rewrite::Walk::skip_subtree) / [`SubtreeMark`](rewrite::SubtreeMark)).
//! * [`build`] — the consolidated node-builder helper set (`ident`, `str_lit`,
//!   `num`, `paren`, `bin`, `bit_not`, `call`, `member_computed`, `array`,
//!   `assign`, `ternary`, …).
//! * [`codegen`] — the typed runtime-code builder (function/loop builders) and the
//!   canonical [`djb2_fn`](codegen::djb2_fn) emitted from one place.
//! * [`deep`] — heap-backed cloning and traversal of generated expression chains.
//! * [`span`] — the single [`injected_span`](span::injected_span) seam.
//! * [`analysis`] — the shared scope/binding/eligibility predicates.

pub mod analysis;
mod annex_b;
pub mod assignment_target;
pub mod build;
mod callable_names;
mod class_scope;
pub mod codegen;
mod compression_guards;
pub mod deep;
pub mod directives;
pub mod lang;
mod pattern_elisions;
pub mod rewrite;
mod scope_bindings;
pub mod span;
mod switch_scope;
pub use switch_scope::SuspensionDeclarations as SwitchSuspensionDeclarations;

pub use lang::{Ast, Js, ParseGoal, ParseOpts};
