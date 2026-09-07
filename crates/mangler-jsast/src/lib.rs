//! `mangler-jsast` — the JS/TS Language implementation over swc, plus the ONE
//! reusable traversal/rewrite layer and the ONE AST-builder codegen discipline.
//!
//! This crate exists to kill two recurring sources of churn in the legacy code:
//! ~19 bespoke `VisitMut` visitors (each re-deriving post-order descent and the
//! `inside_core_init` protected-subtree guard), and the string-template /
//! Rust-mirror sync problem (runtime JS written as `format!` templates kept by
//! hand in lockstep with their Rust mirrors). Everything WP4 (vm) and WP6 (passes)
//! emit is built through this crate; no pass hand-rolls a visitor or a node-builder
//! again.
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
//! * [`span`] — the single [`injected_span`](span::injected_span) seam.
//! * [`analysis`] — the shared scope/binding/eligibility predicates.

pub mod analysis;
pub mod build;
mod class_scope;
pub mod codegen;
pub mod directives;
pub mod lang;
pub mod rewrite;
pub mod span;

pub use lang::{Ast, Js, ParseOpts};
