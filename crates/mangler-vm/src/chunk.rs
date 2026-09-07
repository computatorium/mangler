//! The compiled-chunk data model: [`Const`], [`Compiled`], [`ChildChunk`], and the
//! public [`Chunk`] handle the table builder accepts from both clients.
//!
//! A [`Compiled`] is one function body lowered to a flat stack-machine program
//! ([`Instr`] stream + [`Const`] pool) plus the frame metadata (`slots`, `pcount`,
//! `captures`) and any nested-function [`ChildChunk`]s. The two VM clients — the
//! strings-decode pass and the function-virtualize pass — both produce
//! `Compiled`s, hand them to the [`crate::TableBuilder`], and receive table indices
//! back; this is the single shared currency that lets one table + one interpreter
//! serve both, eliminating the legacy 13-field `DeferredStringsVm` hand-off.

use crate::isa::Instr;

/// The instructions an interpreter must support, before opcode diversification.
/// A table accumulates one union for each strictness/exception-handling variant,
/// including all descendants that re-enter that same interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionUsage {
    opcodes: [bool; crate::isa::N_OPCODES],
    binary: [bool; crate::isa::N_BIN_OPS],
    unary: [bool; crate::isa::N_UN_OPS],
}

impl Default for InstructionUsage {
    fn default() -> Self {
        Self {
            opcodes: [false; crate::isa::N_OPCODES],
            binary: [false; crate::isa::N_BIN_OPS],
            unary: [false; crate::isa::N_UN_OPS],
        }
    }
}

impl InstructionUsage {
    /// Include this body and every nested bytecode closure.
    pub fn include(&mut self, compiled: &Compiled) {
        for instr in &compiled.code {
            self.opcodes[instr.discriminant()] = true;
            match instr {
                Instr::Bin(op) => self.binary[*op as usize] = true,
                Instr::Un(op) => self.unary[*op as usize] = true,
                _ => {}
            }
        }
        for child in &compiled.children {
            self.include(&child.compiled);
        }
    }

    /// Whether the canonical instruction is used.
    pub fn opcode(&self, opcode: usize) -> bool {
        self.opcodes[opcode]
    }

    /// Whether the canonical binary operator is used.
    pub fn binary(&self, operator: usize) -> bool {
        self.binary[operator]
    }

    /// Whether the canonical unary operator is used.
    pub fn unary(&self, operator: usize) -> bool {
        self.unary[operator]
    }
}

/// A VM constant-pool entry.
#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    /// A numeric literal.
    Num(f64),
    /// A string literal (interpreter de-XOR + `String.fromCharCode`s it on first
    /// use).
    Str(String),
    /// A boolean literal.
    Bool(bool),
    /// D4: a tagged-template's frozen template object. `cooked[i] == None` is an
    /// invalid-escape hole (the cooked value is JS `undefined`, legal only in a
    /// tagged template); `raw[i]` always has a value. The interpreter builds the
    /// frozen, `.raw`-bearing array ONCE on first use and caches it in the const
    /// slot, so the same call site reuses the SAME object across evaluations.
    TemplateObject {
        /// Cooked quasis; `None` is an invalid-escape hole (`undefined`).
        cooked: Vec<Option<String>>,
        /// Raw quasis; always present.
        raw: Vec<String>,
    },
    /// Phase 3 native-closure escape hatch (§4.2): a **factory function
    /// expression** stored as already-rendered JS source, e.g.
    /// `function(u0,u1){return function render(){…u0…}}`. The factory's params are
    /// the threaded upvalues (enclosing VM-frame locals / cells, plus the enclosing
    /// `this` for an arrow); its body returns the original excluded/ineligible
    /// function or arrow with free VM-frame locals rewritten to `u0..` and free
    /// module globals left untouched (it lives at module scope in the shared
    /// program-table, so globals resolve to the real ones — no threading, no
    /// obfuscation lost). The factory carries the original function's own
    /// strictness directive (§5a.2). Rendered VERBATIM into the consts array (it is
    /// a function value, so the interpreter's const-decode loop — which only
    /// de-XORs `Array.isArray` string consts and `.q` template objects — leaves it
    /// untouched); `MakeNativeClosure` then calls it with the up-slot values. The
    /// source is built deterministically (same diversity seed ⇒ byte-identical),
    /// since it is a pure function of the original AST + the upvalue rename map.
    NativeFactory(String),
}

/// One function body lowered to a flat VM program.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    /// The instruction stream.
    pub code: Vec<Instr>,
    /// The constant pool.
    pub consts: Vec<Const>,
    /// Free-global capture names, in slot order (the thunk threads them in).
    pub captures: Vec<String>,
    /// Total flat-frame slot count.
    pub slots: u32,
    /// Number of positional param slots the interpreter copies from `arguments`
    /// (params declared *before* any trailing `...rest`). Without a rest param this
    /// equals `params.len()`.
    pub pcount: u32,
    /// D5: nested functions compiled to their own chunks, in `MakeClosure` `child`
    /// index order. Empty for a leaf body.
    pub children: Vec<ChildChunk>,
}

impl Compiled {
    /// The slot index where captured values begin (the thunk's `capStart` arg).
    pub fn cap_start(&self) -> u32 {
        self.slots - self.captures.len() as u32
    }
}

/// A nested function compiled to its own VM chunk (D5).
#[derive(Debug, Clone, PartialEq)]
pub struct ChildChunk {
    /// The child's own compiled program (may itself carry grandchildren).
    pub compiled: Compiled,
    /// True if the source was an arrow (`=>`): the closure threads the enclosing
    /// `this` lexically instead of taking the call-time receiver.
    pub is_arrow: bool,
}

/// The public handle returned to a client after it registers a compiled body with
/// the [`crate::TableBuilder`]. It names the root program-table index plus the
/// frame metadata the client needs to emit the calling thunk — exactly the fields
/// the legacy embed path returned, but as a clean first-class value rather than a
/// 13-field cross-pass struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The root chunk's index in the shared program table.
    pub index: usize,
    /// Free-global capture names, in the order the thunk must thread them.
    pub captures: Vec<String>,
    /// Slot index where captures begin (thunk arg `capStart`).
    pub cap_start: u32,
    /// Positional param count (thunk arg `pcount`).
    pub pcount: u32,
    /// Whether this body uses exception-handling / iterator / completion opcodes,
    /// so the assembled interpreter must be the EH shape.
    pub needs_eh: bool,
    /// Whether this body must execute under strict mode (§5a): the chunk routes to a
    /// strict interpreter variant (its `Store*` opcodes throw on non-writable /
    /// getter-only / frozen targets) and its calling thunk is emitted strict (so the
    /// forwarded `this` is the un-coerced strict receiver). Sloppy chunks (the default)
    /// keep today's behavior byte-for-byte.
    pub is_strict: bool,
}
