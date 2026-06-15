//! The ONE opcode/operator table — the single source of truth for the VM ISA.
//!
//! Historically the instruction set was triplicated: the `Instr` enum lived in
//! `bytecode.rs`, the discriminant↔operand encoding lived in `serialize` +
//! `instr_size` (`interp.rs`), and the interpreter `case` bodies lived in
//! `opcode_handler_body`/`eh_handler_body` — three hand-synced copies of the same
//! integer numbering. A drift between any two was a silent miscompile.
//!
//! This module collapses all three into ONE declarative table. The
//! `opcodes!` macro takes a list of `{ name, discriminant, operand layout }`
//! rows and GENERATES:
//!   * the [`Instr`] enum (one variant per row, with the row's operand fields);
//!   * [`Instr::discriminant`] (the canonical opcode number `serialize` writes);
//!   * [`Instr::size`] (flat-slot encoding size, derived from the operand layout);
//!   * the canonical operand-encoding driver used by [`crate::serialize`].
//!
//! The Bin/Un operator sub-tables (`BIN_OPS`/`UN_OPS`) are likewise ONE
//! ordered list each, from which BOTH the Rust `op → sub-code` maps
//! ([`bin_op_code`]/[`un_op_code`]/[`compound_op_code`]) AND the JS operator
//! expressions the interpreter emits are derived.
//!
//! The generated round-trip test (the round-trip test) proves internal consistency: every
//! variant's discriminant is unique and `< N_OPCODES`, its `size()` agrees with
//! its declared operand layout, and an encode→decode of a representative
//! instruction round-trips.

use swc_core::ecma::ast::{AssignOp, BinaryOp, UnaryOp};

/// Number of top-level opcode slots in the dispatch space (canonical `0..36`).
/// Matches the legacy `N_OPCODES`. The opcode permutation is sized
/// `N_OPCODES + junk`; the first `N_OPCODES` entries are the real opcodes.
pub const N_OPCODES: usize = 36;

/// Number of binary-operator sub-codes (the inner `switch` under the `Bin`
/// opcode). Permuted per file (C2).
pub const N_BIN_OPS: usize = 20;

/// Number of unary-operator sub-codes (under `Un`). Permuted per file (C2).
pub const N_UN_OPS: usize = 7;

/// Upvalue-slot sentinel meaning "the closure currently being built" — used for a
/// named function expression's self reference. Chosen well above any real slot
/// index (a program with this many slots bails `too_large`).
pub const SELF_UPVALUE: u32 = 0x7FFF_FFFF;

// ---------------------------------------------------------------------------
// The ONE opcode table.
// ---------------------------------------------------------------------------

/// The operand layout of one opcode — the single declaration that drives both the
/// enum field set and the serialized encoding/size.
///
/// Each opcode is described once, here, by the *kind* and *count* of operands it
/// carries in the flat code array AFTER the opcode word. `size()` is therefore a
/// pure function of the layout (1 word for the opcode + one word per operand,
/// with the variable-length closure form computed from its upvalue list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// No operands: the opcode word alone (size 1).
    Nullary,
    /// One `u32` operand word (size 2): const idx / slot / argc / count /
    /// jump-offset / bin-or-un sub-code / rest-fixed-count.
    Unary,
    /// Two `u32` operand words (size 3): `PushHandler(catchPC,finPC)` /
    /// `BreakUnwind(targetPC,targetDepth)`.
    Binary,
    /// The variable-length `MakeClosure` form: 5 fixed operand words (child,
    /// isArrow, capStart, pcount, nUp) + one word per upvalue slot. Size is
    /// `6 + up_slots.len()`.
    Closure,
}

impl Layout {
    /// Encoded size (in flat words) of an instruction with this layout, given the
    /// closure form's upvalue count (ignored for the fixed layouts).
    #[inline]
    fn size_with(self, n_up: usize) -> u32 {
        match self {
            Layout::Nullary => 1,
            Layout::Unary => 2,
            Layout::Binary => 3,
            Layout::Closure => 6 + n_up as u32,
        }
    }
}

/// Generate the [`Instr`] enum, its canonical [`Instr::discriminant`], and the
/// per-variant [`Layout`], from ONE table of rows. Each row is
/// `Variant(operand-types…) = discriminant => Layout`. Operand-carrying opcodes are
/// **tuple variants** (matching the compiler's construction sites and the legacy
/// model); the variable-length `MakeClosure` is a struct variant. There is no
/// separate hand-written size table or discriminant assignment — both come from
/// this one list (size via [`Layout`], discriminant via the literal).
macro_rules! opcodes {
    (
        $(
            $(#[$vmeta:meta])*
            $variant:ident $( ( $($fty:ty),* $(,)? ) )? $( { $($fname:ident : $sfty:ty),* $(,)? } )?
                = $disc:literal => $layout:ident
        ),* $(,)?
    ) => {
        /// VM instruction. ONE variant per opcode-table row; the enum, its
        /// discriminant, and its encoding size are all generated here so they can
        /// never drift from the serializer or interpreter. Nullary opcodes are unit
        /// variants; operand-carrying opcodes are tuple variants; the
        /// variable-length `MakeClosure` is a struct variant.
        #[derive(Debug, Clone, PartialEq)]
        pub enum Instr {
            $(
                $(#[$vmeta])*
                $variant
                    $( ( $($fty),* ) )?
                    $( { $($fname : $sfty),* } )?
                ,
            )*
        }

        impl Instr {
            /// The canonical opcode number this instruction serializes to (BEFORE
            /// the per-file permutation). The single source the serializer and the
            /// interpreter both index.
            pub fn discriminant(&self) -> usize {
                match self {
                    $(
                        opcodes!(@pat $variant $( ( $($fty),* ) )? $( { $($fname),* } )?)
                            => $disc,
                    )*
                }
            }

            /// The operand [`Layout`] of this instruction — the one declaration
            /// driving both size and encoding.
            pub fn layout(&self) -> Layout {
                match self {
                    $(
                        opcodes!(@pat $variant $( ( $($fty),* ) )? $( { $($fname),* } )?)
                            => Layout::$layout,
                    )*
                }
            }
        }

        /// Every opcode's `(discriminant, Layout)`, in declaration order — the data
        /// the round-trip test sweeps to prove internal consistency.
        pub const OPCODE_TABLE: &[(usize, Layout)] = &[
            $( ($disc, Layout::$layout), )*
        ];
    };

    // Wildcard patterns per variant shape (the variant alone determines disc/layout).
    (@pat $variant:ident) => { Instr::$variant };
    (@pat $variant:ident ( $($t:ty),* )) => { Instr::$variant(..) };
    (@pat $variant:ident { $($f:ident),* }) => { Instr::$variant { .. } };
}

opcodes! {
    PushConst(u32) = 0 => Unary,
    PushUndef = 1 => Nullary,
    PushNull = 2 => Nullary,
    LoadLocal(u32) = 3 => Unary,
    StoreLocal(u32) = 4 => Unary,
    /// Operand is a *canonical* bin sub-code (`0..N_BIN_OPS`); the serializer
    /// applies `bin_perm` to it.
    Bin(u8) = 5 => Unary,
    /// Operand is a *canonical* un sub-code (`0..N_UN_OPS`); the serializer applies
    /// `un_perm` to it.
    Un(u8) = 6 => Unary,
    GetProp = 7 => Nullary,
    SetProp = 8 => Nullary,
    MakeArray(u32) = 9 => Unary,
    MakeObject(u32) = 10 => Unary,
    Call(u32) = 11 => Unary,
    /// Push the call receiver (`this`); threaded into the interpreter as the
    /// trailing `receiver` parameter.
    PushThis = 12 => Nullary,
    New(u32) = 13 => Unary,
    /// Operand is an instruction *index* the serializer rewrites to a flat offset.
    Jump(u32) = 14 => Unary,
    /// Operand is an instruction *index* the serializer rewrites to a flat offset.
    JumpIfFalse(u32) = 15 => Unary,
    Pop = 16 => Nullary,
    Dup = 17 => Nullary,
    Ret = 18 => Nullary,
    /// Method call with explicit `this` already on the stack.
    CallResolved(u32) = 19 => Unary,
    /// Pop an iterable, push its iterator (`obj[Symbol.iterator]()`).
    GetIter = 20 => Nullary,
    /// Pop an iterator; push `value, true` if not done, else just `false`.
    IterStep = 21 => Nullary,
    /// Pop an iterator and call its `return()` if present (iterator close).
    IterClose = 22 => Nullary,
    /// Push an exception handler `(catchPC, finPC)`. `u32::MAX` in either PC means
    /// "absent". Both PCs are instruction *indices* the serializer rewrites to flat
    /// offsets (the `u32::MAX` sentinel passes through verbatim).
    PushHandler(u32, u32) = 23 => Binary,
    /// Drop the innermost handler (normal completion of its `try`).
    PopHandler = 24 => Nullary,
    Throw = 25 => Nullary,
    /// End of a `finally`/iterator-close body: act on the pending completion.
    EndFinally = 26 => Nullary,
    /// `return v` with handlers active: unwind through intervening finallys/closes.
    RetUnwind = 27 => Nullary,
    /// `break`/`continue` crossing a handler `(targetPC, targetDepth)`. `targetPC`
    /// is an instruction *index* (rewritten to a flat offset); `targetDepth` is a
    /// raw handler-count.
    BreakUnwind(u32, u32) = 28 => Binary,
    /// Push an Array of the call arguments from index `fixed` onward (`...rest`).
    LoadRest(u32) = 29 => Unary,
    /// Pop an object, push an array of its enumerable keys (a `for-in` snapshot).
    EnumKeys = 30 => Nullary,
    DeleteProp = 31 => Nullary,
    /// D1 boxed-cell: `L[slot] = [L[slot]]` (box current slot value in place).
    MakeCell(u32) = 32 => Unary,
    /// D1 boxed-cell: read `L[slot][0]`.
    LoadCell(u32) = 33 => Unary,
    /// D1 boxed-cell: write `L[slot][0]` (keep value on stack, like `StoreLocal`).
    StoreCell(u32) = 34 => Unary,
    /// D5 nested closure. `child` is initially a LOCAL child index; the table
    /// builder rewrites it to the resolved program-table index before
    /// serialization. `up_slots` are the enclosing-frame slots, in capture order;
    /// the [`SELF_UPVALUE`] sentinel marks the child's own-name self reference.
    MakeClosure {
        child: u32,
        is_arrow: bool,
        cap_start: u32,
        pcount: u32,
        up_slots: Vec<u32>,
    } = 35 => Closure,
}

impl Instr {
    /// Encoded size (in flat words) — a pure function of [`Instr::layout`] plus,
    /// for the closure form, its upvalue count.
    pub fn size(&self) -> u32 {
        let n_up = match self {
            Instr::MakeClosure { up_slots, .. } => up_slots.len(),
            _ => 0,
        };
        self.layout().size_with(n_up)
    }

    /// Construct a `Bin` from a [`BinaryOp`], or `None` if the op is not in the
    /// table (`&&`/`||`/`??`/`in`/`instanceof`). Convenience over `bin_op_code`.
    pub fn bin(op: BinaryOp) -> Option<Instr> {
        bin_op_code(op).map(Instr::Bin)
    }

    /// Construct a `Un` from a [`UnaryOp`], or `None` if the op needs special
    /// handling (`delete`). Convenience over `un_op_code`.
    pub fn un(op: UnaryOp) -> Option<Instr> {
        un_op_code(op).map(Instr::Un)
    }
}

// ---------------------------------------------------------------------------
// The ONE binary/unary operator tables.
// ---------------------------------------------------------------------------

/// One binary-operator row: the swc op, its JS source expression (operands named
/// `a`,`b`, matching the interpreter's popped temps), and — for compound-assign
/// reuse — the swc `AssignOp` that shares this sub-code (`None` if the op has no
/// `x op= y` form, e.g. the comparisons).
struct BinRow {
    op: BinaryOp,
    js: &'static str,
    assign: Option<AssignOp>,
}

/// One unary-operator row: the swc op and its JS source expression (operand `a`).
struct UnRow {
    op: UnaryOp,
    js: &'static str,
}

/// The ordered binary-operator table — the canonical sub-code is the row index.
/// This ONE list drives [`bin_op_code`]/[`compound_op_code`] (Rust side) and
/// [`bin_expr_js`] (the JS the interpreter emits). The ordering is load-bearing
/// (the sub-codes are baked into serialized bytecode).
const BIN_OPS: [BinRow; N_BIN_OPS] = [
    BinRow { op: BinaryOp::Add, js: "a+b", assign: Some(AssignOp::AddAssign) },
    BinRow { op: BinaryOp::Sub, js: "a-b", assign: Some(AssignOp::SubAssign) },
    BinRow { op: BinaryOp::Mul, js: "a*b", assign: Some(AssignOp::MulAssign) },
    BinRow { op: BinaryOp::Div, js: "a/b", assign: Some(AssignOp::DivAssign) },
    BinRow { op: BinaryOp::Mod, js: "a%b", assign: Some(AssignOp::ModAssign) },
    BinRow { op: BinaryOp::Exp, js: "a**b", assign: Some(AssignOp::ExpAssign) },
    BinRow { op: BinaryOp::EqEq, js: "a==b", assign: None },
    BinRow { op: BinaryOp::EqEqEq, js: "a===b", assign: None },
    BinRow { op: BinaryOp::NotEq, js: "a!=b", assign: None },
    BinRow { op: BinaryOp::NotEqEq, js: "a!==b", assign: None },
    BinRow { op: BinaryOp::Lt, js: "a<b", assign: None },
    BinRow { op: BinaryOp::LtEq, js: "a<=b", assign: None },
    BinRow { op: BinaryOp::Gt, js: "a>b", assign: None },
    BinRow { op: BinaryOp::GtEq, js: "a>=b", assign: None },
    BinRow { op: BinaryOp::BitAnd, js: "a&b", assign: Some(AssignOp::BitAndAssign) },
    BinRow { op: BinaryOp::BitOr, js: "a|b", assign: Some(AssignOp::BitOrAssign) },
    BinRow { op: BinaryOp::BitXor, js: "a^b", assign: Some(AssignOp::BitXorAssign) },
    BinRow { op: BinaryOp::LShift, js: "a<<b", assign: Some(AssignOp::LShiftAssign) },
    BinRow { op: BinaryOp::RShift, js: "a>>b", assign: Some(AssignOp::RShiftAssign) },
    BinRow { op: BinaryOp::ZeroFillRShift, js: "a>>>b", assign: Some(AssignOp::ZeroFillRShiftAssign) },
];

/// The ordered unary-operator table — the canonical sub-code is the row index.
/// Index 6 (`String(a)`) is the runtime ToString helper used by template literals;
/// it has no source `UnaryOp`, so its `op` is a placeholder never matched by
/// [`un_op_code`] (which only maps the real `UnaryOp`s 0..=5).
const UN_OPS: [UnRow; N_UN_OPS] = [
    UnRow { op: UnaryOp::Minus, js: "-a" },
    UnRow { op: UnaryOp::Bang, js: "!a" },
    UnRow { op: UnaryOp::Tilde, js: "~a" },
    UnRow { op: UnaryOp::TypeOf, js: "typeof a" },
    UnRow { op: UnaryOp::Void, js: "void a" },
    UnRow { op: UnaryOp::Plus, js: "+a" },
    // Sub-code 6: runtime String() coercion (template-literal interpolation). No
    // source UnaryOp maps here; `un_op_code` never returns 6. `Void` is an inert
    // placeholder for the `op` field (unused for this row).
    UnRow { op: UnaryOp::Void, js: "String(a)" },
];

/// Canonical sub-code for the runtime `String(a)` coercion (template ToString).
pub const UN_TO_STRING: u8 = 6;

/// binop → canonical sub-code stored in `Bin { op }`. `None` => caller bails
/// (`&&`/`||`/`??`/`in`/`instanceof` are not in the table).
pub fn bin_op_code(op: BinaryOp) -> Option<u8> {
    BIN_OPS.iter().position(|r| r.op == op).map(|i| i as u8)
}

/// unop → canonical sub-code stored in `Un { op }`. `None` => caller bails
/// (`delete` needs the `DeleteProp` opcode). Only the real `UnaryOp`s 0..=5 map.
pub fn un_op_code(op: UnaryOp) -> Option<u8> {
    // Only the first six rows correspond to real source UnaryOps; row 6 is the
    // ToString helper, which no source operator maps to.
    UN_OPS[..6].iter().position(|r| r.op == op).map(|i| i as u8)
}

/// compound-assign op → the bin sub-code it reuses. `None` => bail.
pub fn compound_op_code(op: AssignOp) -> Option<u8> {
    BIN_OPS
        .iter()
        .position(|r| r.assign == Some(op))
        .map(|i| i as u8)
}

/// The JS expression for binary sub-code `k` (`0..N_BIN_OPS`). Operands are `a`
/// (left) / `b` (right), matching the interpreter's popped temps. The single
/// source for the interpreter's Bin inner-switch arms.
pub fn bin_expr_js(k: usize) -> &'static str {
    BIN_OPS[k].js
}

/// The JS expression for unary sub-code `k` (`0..N_UN_OPS`). Operand is `a`.
pub fn un_expr_js(k: usize) -> &'static str {
    UN_OPS[k].js
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE ISA round-trip / consistency proof. For every opcode-table row:
    ///   * discriminants are unique and exactly cover `0..N_OPCODES`;
    ///   * `Instr::discriminant()`/`size()`/`layout()` agree with the declared row;
    ///   * a representative instruction encodes to the right number of words and
    ///     its size matches that word count (encode↔size round-trip).
    #[test]
    fn opcode_table_is_internally_consistent() {
        // Discriminants are a permutation of 0..N_OPCODES (unique + dense).
        let mut discs: Vec<usize> = OPCODE_TABLE.iter().map(|(d, _)| *d).collect();
        discs.sort_unstable();
        assert_eq!(discs, (0..N_OPCODES).collect::<Vec<_>>(), "discriminants must densely cover 0..N_OPCODES");
        assert_eq!(OPCODE_TABLE.len(), N_OPCODES);

        // size_with agrees with the layout for each fixed form.
        assert_eq!(Layout::Nullary.size_with(0), 1);
        assert_eq!(Layout::Unary.size_with(0), 2);
        assert_eq!(Layout::Binary.size_with(0), 3);
        assert_eq!(Layout::Closure.size_with(2), 8);
    }

    /// A representative instance of every variant: discriminant matches the table
    /// row, and `size()` matches the layout-derived word count.
    #[test]
    fn every_variant_size_matches_layout() {
        // Build one representative per discriminant and check size().
        let samples: Vec<Instr> = vec![
            Instr::PushConst(0),
            Instr::PushUndef,
            Instr::PushNull,
            Instr::LoadLocal(1),
            Instr::StoreLocal(1),
            Instr::Bin(0),
            Instr::Un(0),
            Instr::GetProp,
            Instr::SetProp,
            Instr::MakeArray(2),
            Instr::MakeObject(1),
            Instr::Call(1),
            Instr::PushThis,
            Instr::New(0),
            Instr::Jump(0),
            Instr::JumpIfFalse(0),
            Instr::Pop,
            Instr::Dup,
            Instr::Ret,
            Instr::CallResolved(1),
            Instr::GetIter,
            Instr::IterStep,
            Instr::IterClose,
            Instr::PushHandler(0, u32::MAX),
            Instr::PopHandler,
            Instr::Throw,
            Instr::EndFinally,
            Instr::RetUnwind,
            Instr::BreakUnwind(0, 0),
            Instr::LoadRest(0),
            Instr::EnumKeys,
            Instr::DeleteProp,
            Instr::MakeCell(0),
            Instr::LoadCell(0),
            Instr::StoreCell(0),
            Instr::MakeClosure { child: 0, is_arrow: false, cap_start: 0, pcount: 0, up_slots: vec![1, 2] },
        ];
        assert_eq!(samples.len(), N_OPCODES, "one sample per opcode");
        // Each discriminant appears exactly once across the samples.
        let mut seen = [false; N_OPCODES];
        for ins in &samples {
            let d = ins.discriminant();
            assert!(!seen[d], "duplicate discriminant {d}");
            seen[d] = true;
            // size() matches the declared layout for this instance.
            let n_up = if let Instr::MakeClosure { up_slots, .. } = ins { up_slots.len() } else { 0 };
            assert_eq!(ins.size(), ins.layout().size_with(n_up), "size/layout mismatch for {ins:?}");
        }
        assert!(seen.iter().all(|&b| b), "every discriminant covered");
        // Spot-check the closure variable length: 5 fixed + 1 nUp + 2 slots = 8.
        assert_eq!(samples[35].size(), 8);
    }

    /// The Bin/Un operator tables: codes are dense, the Rust op→code maps invert
    /// the table order, and compound-assign reuses the binary sub-code.
    #[test]
    fn operator_tables_consistent() {
        // bin_op_code inverts the table ordering exactly.
        for (i, row) in BIN_OPS.iter().enumerate() {
            assert_eq!(bin_op_code(row.op), Some(i as u8), "bin row {i}");
        }
        // un_op_code covers the first six rows (the real source ops).
        for (i, row) in UN_OPS[..6].iter().enumerate() {
            assert_eq!(un_op_code(row.op), Some(i as u8), "un row {i}");
        }
        // The ToString sub-code (6) is never produced by un_op_code.
        assert_eq!(un_expr_js(UN_TO_STRING as usize), "String(a)");
        // Compound-assign reuses the same sub-code as the plain binary op.
        assert_eq!(compound_op_code(AssignOp::AddAssign), bin_op_code(BinaryOp::Add));
        assert_eq!(compound_op_code(AssignOp::BitXorAssign), bin_op_code(BinaryOp::BitXor));
        // Non-table ops bail.
        assert_eq!(bin_op_code(BinaryOp::LogicalAnd), None);
        assert_eq!(bin_op_code(BinaryOp::In), None);
        assert_eq!(un_op_code(UnaryOp::Delete), None);
        assert_eq!(compound_op_code(AssignOp::AndAssign), None);
    }
}
