//! `mangler-vm` — the self-contained virtualization engine.
//!
//! One opcode/operator table as data ([`isa`]) generates the [`isa::Instr`] enum,
//! its discriminant, its encoding size, and the operand-encoding the serializer and
//! interpreter share. A decomposed AST→bytecode compiler, a
//! serializer, and an interpreter emitter driven by an [`InterpreterSpec`] +
//! [`VmDiversity`]. String-decode and function-virtualize are two clients of ONE
//! shared table via [`TableBuilder`].

pub mod cells;
pub mod chunk;
pub mod compile;
pub mod descriptors;
pub mod diversity;
pub mod eligibility;
pub mod emit;
pub mod eval_class;
pub mod isa;
mod runtime_env;
mod runtime_ref;
pub mod serialize;
pub mod source_text;
pub mod table;

#[cfg(test)]
pub mod test_support;

pub use cells::{BoxPlan, plan_and_rewrite};
pub use chunk::{ChildChunk, Chunk, Compiled, Const, SuspensionKind};
pub use compile::{
    CompileOptions, compile_body, compile_body_boxed, compile_body_with_opts,
    compile_body_with_plan,
};
pub use diversity::VmDiversity;
pub use eligibility::{Eligibility, classify_body};
pub use emit::{InterpreterSpec, emit_interpreter, emit_interpreters};
pub use isa::{Instr, N_BIN_OPS, N_OPCODES, N_UN_OPS, SELF_UPVALUE};
pub use table::{TableBuilder, VmNames, VmTable};

pub mod eval;

include!(concat!(env!("OUT_DIR"), "/compiler_fingerprint.rs"));
