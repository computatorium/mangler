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

/// The closure construction paths emitted by bytecode producers.
#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum ClosureMode {
    Arrow = 1,
    Sloppy = 2,
    Strict = 4,
    ArgumentsFactory = 8,
    SelfBinding = 16,
    InitialLength = 32,
}

/// The instructions an interpreter must support, before opcode diversification.
/// A table accumulates one union for each strictness/exception-handling variant,
/// including all descendants that re-enter that same interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionUsage {
    opcodes: [bool; crate::isa::N_OPCODES],
    binary: [bool; crate::isa::N_BIN_OPS],
    unary: [bool; crate::isa::N_UN_OPS],
    suspension: bool,
    constants: u8,
    eval: bool,
    largest_body: usize,
    closure_modes: u8,
}

impl Default for InstructionUsage {
    fn default() -> Self {
        Self {
            opcodes: [false; crate::isa::N_OPCODES],
            binary: [false; crate::isa::N_BIN_OPS],
            unary: [false; crate::isa::N_UN_OPS],
            suspension: false,
            constants: 0,
            eval: false,
            largest_body: 0,
            closure_modes: 0,
        }
    }
}

impl InstructionUsage {
    /// Include this body and every nested bytecode closure.
    pub fn include(&mut self, compiled: &Compiled) {
        self.eval |= compiled.requires_source_compiler;
        self.largest_body = self.largest_body.max(compiled.code.len());
        for constant in &compiled.consts {
            self.constants |= match constant {
                Const::Str(_) | Const::Utf16(_) => 1,
                Const::BigInt(_) => 2,
                Const::RegExp { .. } | Const::RegExpUtf16 { .. } => 4,
                Const::TemplateObject { .. } | Const::TemplateObjectUtf16 { .. } => 8,
                _ => 0,
            };
        }
        for (position, instr) in compiled.code.iter().enumerate() {
            if let Instr::MakeClosure {
                child,
                is_arrow,
                up_slots,
                ..
            } = instr
            {
                self.closure_modes |= if *is_arrow {
                    ClosureMode::Arrow as u8
                } else if compiled
                    .children
                    .get(*child as usize)
                    .is_some_and(|c| c.is_strict)
                {
                    ClosureMode::Strict as u8
                } else {
                    ClosureMode::Sloppy as u8
                };
                if compiled.children.get(*child as usize).is_some_and(|c| {
                    c.compiled
                        .code
                        .iter()
                        .any(|op| matches!(op, Instr::MapArgument(_, _)))
                }) {
                    self.closure_modes |= ClosureMode::ArgumentsFactory as u8;
                }
                if up_slots.contains(&crate::isa::SELF_UPVALUE) {
                    self.closure_modes |= ClosureMode::SelfBinding as u8;
                }
                if !matches!(
                    compiled
                        .code
                        .iter()
                        .skip(position + 1)
                        .find(|op| !matches!(op, Instr::ClearRef(_))),
                    Some(Instr::SetFunctionLength(_))
                ) {
                    self.closure_modes |= ClosureMode::InitialLength as u8;
                }
            }
            self.eval |= matches!(instr, Instr::EvalCall(_));
            self.opcodes[instr.discriminant()] = true;
            match instr {
                Instr::Bin(op) => self.binary[*op as usize] = true,
                Instr::Un(op) => self.unary[*op as usize] = true,
                _ => {}
            }
        }
        for child in &compiled.children {
            self.suspension |= child.suspension.is_some();
            self.include(&child.compiled);
        }
    }

    /// Callable construction capabilities actually present in this bytecode tree.
    pub(crate) fn closure_mode(&self, mode: ClosureMode) -> bool {
        self.closure_modes & mode as u8 != 0
    }

    /// Large bodies use direct dispatch to keep operand state in optimized locals.
    pub fn prefers_direct_dispatch(&self) -> bool {
        self.largest_body > 256
    }

    pub fn has_eval(&self) -> bool {
        self.eval
    }

    pub fn constant_kind(&self, bit: u8) -> bool {
        self.constants & bit != 0
    }

    pub fn has_suspension(&self) -> bool {
        self.suspension
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
    /// Lossless JavaScript UTF-16, including lone surrogates.
    Utf16(Vec<u16>),
    /// Decimal BigInt payload, decoded once.
    BigInt(String),
    /// RegExp literal metadata; NewRegExp creates a fresh object each evaluation.
    RegExp { pattern: String, flags: String },
    /// Lossless regexp source supplied by the runtime compiler, including lone surrogates.
    RegExpUtf16 { pattern: Vec<u16>, flags: String },
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
    /// Static lexical/object environment layout for direct eval.
    Environment(crate::eval::EnvironmentMetadata),
    TemplateObjectUtf16 {
        cooked: Vec<Option<Vec<u16>>>,
        raw: Vec<Vec<u16>>,
    },
}

/// One function body lowered to a flat VM program.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    /// Source-resolved indirect eval or function construction requires the embedded compiler.
    pub requires_source_compiler: bool,
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

/// Source binding operations which a capture can observe, derived from bytecode
/// rather than spelling or a source-level approximation. All captures remain lazy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CaptureCapabilities(u8);
impl CaptureCapabilities {
    pub const ALL: Self = Self(15);
    pub fn writes(self) -> bool {
        self.0 & 1 != 0
    }
    pub fn uses_typeof(self) -> bool {
        self.0 & 2 != 0
    }
    pub fn deletes(self) -> bool {
        self.0 & 4 != 0
    }
    pub fn writes_strictly(self) -> bool {
        self.0 & 8 != 0
    }
}

impl Compiled {
    /// Propagate operations through exact closure upvalue slots. Dynamic/native
    /// references and eval environments conservatively retain every capability.
    pub fn capture_capabilities(&self, strict: bool) -> Vec<CaptureCapabilities> {
        let start = self.cap_start();
        let mut capabilities = vec![CaptureCapabilities::default(); self.captures.len()];
        let mut mark = |slot: u32, flags: CaptureCapabilities| {
            if let Some(index) = slot.checked_sub(start)
                && let Some(capability) = capabilities.get_mut(index as usize)
            {
                capability.0 |= flags.0;
            }
        };
        for instruction in &self.code {
            match instruction {
                Instr::StoreLocal(slot) => {
                    mark(*slot, CaptureCapabilities(if strict { 9 } else { 1 }))
                }
                Instr::TypeOfBinding(slot) => mark(*slot, CaptureCapabilities(2)),
                Instr::DeleteBinding(slot) => mark(*slot, CaptureCapabilities(4)),
                Instr::LocalRef(packed) => mark(packed >> 1, CaptureCapabilities::ALL),
                Instr::MakeNativeClosure { up_slots, .. } => {
                    for &slot in up_slots {
                        mark(slot, CaptureCapabilities::ALL);
                    }
                }
                Instr::MakeClosure {
                    child, up_slots, ..
                } => {
                    let child = self.children.get(*child as usize);
                    let child_flags = child.map(|child| {
                        child
                            .compiled
                            .capture_capabilities(strict || child.is_strict)
                    });
                    for (index, &slot) in up_slots.iter().enumerate() {
                        mark(
                            slot,
                            child_flags
                                .as_ref()
                                .and_then(|flags| flags.get(index))
                                .copied()
                                .unwrap_or(CaptureCapabilities::ALL),
                        );
                    }
                }
                _ => {}
            }
        }
        for constant in &self.consts {
            if let Const::Environment(metadata) = constant {
                for scope in &metadata.scopes {
                    match scope {
                        crate::eval::EnvironmentScope::Bindings(bindings) => {
                            for binding in bindings {
                                mark(binding.slot, CaptureCapabilities::ALL);
                            }
                        }
                        crate::eval::EnvironmentScope::WithObject(slot) => {
                            mark(*slot, CaptureCapabilities::ALL)
                        }
                        crate::eval::EnvironmentScope::Variables => {}
                    }
                }
            }
        }
        capabilities
    }

    /// The slot index where captured values begin (the thunk's `capStart` arg).
    pub fn cap_start(&self) -> u32 {
        self.slots - self.captures.len() as u32
    }
}

/// Original callable kind retained after suspension lowering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspensionKind {
    Async,
    Generator,
    AsyncGenerator,
}
impl SuspensionKind {
    /// Stable serialized callable kind used by program tables.
    pub fn code(self) -> u8 {
        match self {
            Self::Async => 1,
            Self::Generator => 2,
            Self::AsyncGenerator => 3,
        }
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
    /// Explicit child strictness, inherited strictness is resolved by the table.
    pub is_strict: bool,
    /// Original suspension callable shape after state-machine lowering.
    pub suspension: Option<SuspensionKind>,
}

/// The public handle returned to a client after it registers a compiled body with
/// the [`crate::TableBuilder`]. It names the root program-table index plus the
/// frame metadata the client needs to emit the calling thunk — exactly the fields
/// the legacy embed path returned, but as a clean first-class value rather than a
/// 13-field cross-pass struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Binding capabilities in the same order as `captures`.
    pub capture_capabilities: Vec<CaptureCapabilities>,
    /// Actual native parameter mapping pairs required by this bytecode body.
    pub argument_mappings: Vec<(u32, u32)>,
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
