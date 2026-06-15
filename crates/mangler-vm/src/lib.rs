//! `mangler-vm` — the self-contained virtualization engine.
//!
//! One opcode/operator table as data ([`isa`]) generates the [`isa::Instr`] enum,
//! its discriminant, its encoding size, and the operand-encoding the serializer and
//! interpreter share. A decomposed AST→bytecode compiler (bail-to-safe), a
//! serializer, and an interpreter emitter driven by an [`InterpreterSpec`] +
//! [`VmDiversity`]. String-decode and function-virtualize are two clients of ONE
//! shared table via [`TableBuilder`].

pub mod cells;
pub mod chunk;
pub mod compile;
pub mod diversity;
pub mod eligibility;
pub mod emit;
pub mod isa;
pub mod serialize;
pub mod table;

#[cfg(test)]
pub mod test_support;

pub use cells::{plan_and_rewrite, BoxPlan};
pub use chunk::{Chunk, ChildChunk, Compiled, Const};
pub use compile::{compile_body, compile_body_boxed, compile_body_with_plan};
pub use diversity::VmDiversity;
pub use eligibility::{classify_body, Eligibility};
pub use emit::{emit_interpreter, InterpreterSpec};
pub use isa::{Instr, N_BIN_OPS, N_OPCODES, N_UN_OPS, SELF_UPVALUE};
pub use table::{TableBuilder, VmNames, VmTable};
