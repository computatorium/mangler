//! Bytecode → flat numeric code array (+ const pool), and the JS-array rendering.
//!
//! The encoding is driven entirely by the ONE ISA table: the opcode word is
//! `perm[instr.discriminant()]`, and the operand words follow the instruction's
//! [`crate::isa::Layout`]. There are NO hand-written opcode integer literals here —
//! the serializer asks each [`Instr`] for its discriminant, so it can never drift
//! from the enum or the interpreter.
//!
//! Two diversification couplings are applied here and mirrored in [`crate::emit`]:
//!   * the opcode-selector permutation `perm` (a bijection on the dispatch space);
//!   * the Bin/Un sub-code permutations `bin_perm`/`un_perm`.
//!
//! Jump targets are translated from instruction indices to flat offsets (so the
//! interpreter `pc` indexes the flat array directly). The `PushHandler` `u32::MAX`
//! "absent" sentinel passes through verbatim (the handler maps `>2e9` back to -1).
//!
//! [`code_array_js`] packs unsigned words into five-bit variable-length groups.
//! Each group carries a continuation bit and is XOR-masked with the seed's low
//! six bits before rendering as printable ASCII. [`code_decode_js`] expands the
//! payload once into the same shared word array used by the interpreter.

use crate::chunk::{Compiled, Const};
use crate::isa::{Instr, Layout};

/// Map a canonical discriminant through the per-file permutation, as a JS number.
#[inline]
fn op(perm: &[usize], canonical: usize) -> f64 {
    perm[canonical] as f64
}

/// Translate a `PushHandler` PC operand (an instruction index) to a flat offset,
/// passing the `u32::MAX` "absent" sentinel through unchanged.
#[inline]
fn handler_pc(pc: u32, offsets: &[u32]) -> f64 {
    if pc == u32::MAX {
        u32::MAX as f64
    } else {
        offsets[pc as usize] as f64
    }
}

/// Serialize a [`Compiled`] to the flat numeric code array the interpreter
/// consumes, translating instruction-index jump targets to flat offsets.
///
/// `perm` permutes the top-level opcode numbering; `bin_perm`/`un_perm` permute the
/// Bin/Un sub-codes. The interpreter built with the SAME permutations dispatches
/// identically. Returns `(code, consts)`; the const pool is returned unchanged
/// (rendered separately by [`consts_array_js`]).
pub fn serialize(
    c: &Compiled,
    perm: &[usize],
    bin_perm: &[usize],
    un_perm: &[usize],
) -> (Vec<f64>, Vec<Const>) {
    // Pass 1: flat offset of each instruction (plus one-past-end).
    let mut offsets: Vec<u32> = Vec::with_capacity(c.code.len() + 1);
    let mut acc = 0u32;
    for i in &c.code {
        offsets.push(acc);
        acc += i.size();
    }
    offsets.push(acc);

    // Pass 2: opcode word (permuted discriminant) + operand words per layout.
    let mut out: Vec<f64> = Vec::with_capacity(acc as usize);
    for i in &c.code {
        out.push(op(perm, i.discriminant()));
        match i {
            // Unary-layout operand-carrying ops. Most pass their operand verbatim;
            // a few translate it (Bin/Un sub-codes are permuted; Jump targets are
            // instruction indices rewritten to flat offsets).
            Instr::PushConst(ci) | Instr::NewRegExp(ci) => out.push(*ci as f64),
            Instr::LoadLocal(slot)
            | Instr::StoreLocal(slot)
            | Instr::MakeCell(slot)
            | Instr::LoadCell(slot)
            | Instr::StoreCell(slot)
            | Instr::BeginLexical(slot)
            | Instr::InitLocal(slot)
            | Instr::CloneLexical(slot)
            | Instr::TypeOfBinding(slot)
            | Instr::DeleteBinding(slot)
            | Instr::UpdateProp(slot)
            | Instr::LocalRef(slot)
            | Instr::CaptureRef(slot)
            | Instr::EnterWith(slot)
            | Instr::UpdateRef(slot)
            | Instr::SetFunctionLength(slot)
            | Instr::ClearRef(slot)
            | Instr::SuperAssign(slot)
            | Instr::SuperUpdate(slot)
            | Instr::BeginVarEnvironment(slot)
            | Instr::CaptureClosureEnvironment(slot)
            | Instr::EvalCall(slot)
            | Instr::EnvironmentRef(slot)
            | Instr::AccessorRef(slot)
            | Instr::MakeSuperProvider(slot) => out.push(*slot as f64),
            Instr::Bin(o) => out.push(bin_perm[*o as usize] as f64),
            Instr::Un(o) => out.push(un_perm[*o as usize] as f64),
            Instr::MakeArray(n) | Instr::MakeObject(n) => out.push(*n as f64),
            Instr::Call(argc) | Instr::CallResolved(argc) | Instr::New(argc) => {
                out.push(*argc as f64)
            }
            Instr::LoadRest(fixed) => out.push(*fixed as f64),
            Instr::Jump(target) | Instr::JumpIfFalse(target) => {
                out.push(offsets[*target as usize] as f64)
            }
            // Binary-layout (two-operand) ops.
            Instr::MapArgument(index, slot)
            | Instr::WithRef(index, slot)
            | Instr::WithRefCell(index, slot) => {
                out.push(*index as f64);
                out.push(*slot as f64);
            }
            Instr::PushHandler(catch_pc, fin_pc) => {
                out.push(handler_pc(*catch_pc, &offsets));
                out.push(handler_pc(*fin_pc, &offsets));
            }
            Instr::BreakUnwind(target, depth) => {
                // targetPC is an instruction index -> flat offset; targetDepth is a
                // literal handler-count emitted raw.
                out.push(offsets[*target as usize] as f64);
                out.push(*depth as f64);
            }
            // Closure-layout: child table index, arrow flag, child frame params,
            // upvalue count, then each upvalue slot (the SELF sentinel passes
            // through verbatim).
            Instr::MakeClosure {
                child,
                is_arrow,
                cap_start,
                pcount,
                up_slots,
            } => {
                out.push(*child as f64);
                out.push(if *is_arrow { 1.0 } else { 0.0 });
                out.push(*cap_start as f64);
                out.push(*pcount as f64);
                out.push(up_slots.len() as f64);
                for s in up_slots {
                    out.push(*s as f64);
                }
            }
            // NativeClosure-layout: const index, arrow flag, upvalue count, then
            // each upvalue slot (no SELF sentinel — a native fn keeps its own JS
            // self reference).
            Instr::MakeNativeClosure {
                const_idx,
                is_arrow,
                up_slots,
            } => {
                out.push(*const_idx as f64);
                out.push(if *is_arrow { 1.0 } else { 0.0 });
                out.push(up_slots.len() as f64);
                for s in up_slots {
                    out.push(*s as f64);
                }
            }
            // Nullary-layout ops have no operand words.
            _ => debug_assert_eq!(
                i.layout(),
                Layout::Nullary,
                "unhandled operand layout for {i:?}"
            ),
        }
    }

    (out, c.consts.clone())
}

/// Render compact bytecode as a mutable, one-element array containing its packed
/// string. Five payload bits plus one continuation bit encode each unsigned word
/// in one to seven ASCII characters; a seed-derived XOR masks each group. This
/// is obfuscation, not cryptographic encryption. Decoding replaces the array's
/// contents in place, so every thunk and recursive closure shares the cache.
pub fn code_array_js(code: &[f64], key: u32) -> String {
    let mut s = String::from("[\"");
    for &n in code {
        let mut word = n as u32;
        loop {
            let mut group = word & 31;
            word >>= 5;
            if word != 0 {
                group |= 32;
            }
            // The alphabet is ASCII 63..126, containing neither quote nor `<`.
            // Only backslash needs escaping in the JavaScript string literal.
            let ch = (63 + (group ^ (key & 63))) as u8 as char;
            if ch == '\\' {
                s.push('\\');
            }
            s.push(ch);
            if word == 0 {
                break;
            }
        }
    }
    s.push_str("\"]");
    s
}

/// The sole runtime decoder for [`code_array_js`]. Uses the interpreter's existing
/// scratch locals, and keeps the decoded unsigned words in `code` for later calls.
pub(crate) fn code_decode_js(key: u32) -> String {
    let mask = key & 63;
    format!(
        "if(!code.d){{t=code[0];code.length=0;n=0;k=0;\
for(i=0;i<t.length;i++){{v=(Reflect.apply(String.prototype.charCodeAt,t,[i])-63)^{mask};n|=(v&31)<<k;\
if(v&32)k+=5;else{{code.push(n>>>0);n=0;k=0;}}}}code.d=1;}}C=code;"
    )
}

/// Render one string as an array of XOR'd UTF-16 code units (the encrypted-Str form
/// the interpreter de-XORs + `String.fromCharCode`s on first use). `ck` is the
/// 16-bit non-zero key.
fn enc_str(s: &str, ck: u32) -> String {
    enc_units(s.encode_utf16(), ck)
}

fn enc_units(units: impl IntoIterator<Item = u16>, ck: u32) -> String {
    let mut out = String::from("[");
    for (i, u) in units.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&((u as u32) ^ ck).to_string());
    }
    out.push(']');
    out
}

/// Format a number const the way the interpreter expects to read it (preserving
/// `NaN`/`±Infinity`/`-0`).
fn num_js(n: f64) -> String {
    if n.is_nan() {
        "(0/0)".to_string()
    } else if n.is_infinite() {
        if n > 0.0 {
            "(1/0)".to_string()
        } else {
            "(-1/0)".to_string()
        }
    } else if n == 0.0 && n.is_sign_negative() {
        "-0".to_string()
    } else {
        n.to_string()
    }
}

/// Render the const pool as a JS array literal, encrypting `Str` consts as arrays
/// of XOR'd UTF-16 code units. `Num`/`Bool` render unchanged. The key is `code_key`
/// masked to 16 bits and forced non-zero.
pub fn consts_array_js(consts: &[Const], key: u32) -> String {
    let ck = (key & 0xFFFF) | 1;
    let mut s = String::from("[");
    for (i, c) in consts.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match c {
            Const::Num(n) => s.push_str(&num_js(*n)),
            Const::Bool(b) => s.push_str(if *b { "true" } else { "false" }),
            Const::Str(st) => s.push_str(&enc_str(st, ck)),
            Const::Utf16(units) => s.push_str(&enc_units(units.iter().copied(), ck)),
            Const::BigInt(value) => s.push_str(&format!("{{b:{}}}", enc_str(value, ck))),
            Const::RegExp { pattern, flags } => s.push_str(&format!(
                "{{r:{},f:{}}}",
                enc_str(pattern, ck),
                enc_str(flags, ck)
            )),
            Const::RegExpUtf16 { pattern, flags } => s.push_str(&format!(
                "{{r:{},f:{}}}",
                enc_units(pattern.iter().copied(), ck),
                enc_str(flags, ck)
            )),
            // D4: a tagged-template object renders as `{q:[cooked...],w:[raw...]}`.
            // Each raw element is an encrypted-Str array; each cooked element is an
            // encrypted-Str array OR the number `0` for an invalid-escape hole.
            Const::TemplateObject { cooked, raw } => {
                s.push_str("{q:[");
                for (j, ck_el) in cooked.iter().enumerate() {
                    if j > 0 {
                        s.push(',');
                    }
                    match ck_el {
                        Some(st) => s.push_str(&enc_str(st, ck)),
                        None => s.push('0'),
                    }
                }
                s.push_str("],w:[");
                for (j, st) in raw.iter().enumerate() {
                    if j > 0 {
                        s.push(',');
                    }
                    s.push_str(&enc_str(st, ck));
                }
                s.push_str("]}");
            }
            // Phase 3: a native-closure factory renders VERBATIM as its function
            // expression source, parenthesized so it is unambiguously an expression
            // element of the array literal. It is a function value (not an array /
            // `.q` object), so the interpreter's const-decode loop leaves it
            // untouched. The source is already deterministic (built from the AST), so
            // the bytes are identical for a given seed.
            Const::TemplateObjectUtf16 { cooked, raw } => {
                s.push_str("{q:[");
                for (j, el) in cooked.iter().enumerate() {
                    if j > 0 {
                        s.push(',');
                    }
                    match el {
                        Some(u) => s.push_str(&enc_units(u.iter().copied(), ck)),
                        None => s.push('0'),
                    }
                }
                s.push_str("],w:[");
                for (j, u) in raw.iter().enumerate() {
                    if j > 0 {
                        s.push(',');
                    }
                    s.push_str(&enc_units(u.iter().copied(), ck));
                }
                s.push_str("]}");
            }
            Const::Environment(metadata) => {
                use crate::eval::EnvironmentScope;
                s.push_str(&format!("{{c:{}", metadata.source_context as u8));
                if let Some(class) = &metadata.class_context {
                    s.push_str(&format!(
                        ",k:{{capsuleSlot:{},capsuleCell:{},privateNames:[",
                        class.capsule_slot, class.capsule_cell
                    ));
                    for (index, name) in class.private_names.iter().enumerate() {
                        if index > 0 {
                            s.push(',');
                        }
                        s.push('"');
                        for unit in name.encode_utf16() {
                            s.push_str(&format!("\\u{unit:04x}"));
                        }
                        s.push('"');
                    }
                    s.push_str(&format!(
                        "],allowSuperProperty:{},allowSuperCall:{},argumentsForbidden:{}}}",
                        class.allow_super_property,
                        class.allow_super_call,
                        class.arguments_forbidden
                    ));
                }
                s.push_str(",e:[");
                for (i, scope) in metadata.scopes.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    match scope {
                        EnvironmentScope::Variables => s.push('0'),
                        EnvironmentScope::WithObject(slot) => s.push_str(&format!("{{w:{slot}}}")),
                        EnvironmentScope::Bindings(bindings) => {
                            s.push('[');
                            for (j, binding) in bindings.iter().enumerate() {
                                if j > 0 {
                                    s.push(',');
                                }
                                s.push_str(&format!(
                                    "[{},{},{}",
                                    binding.name_const,
                                    binding.slot,
                                    u8::from(binding.cell)
                                        | (u8::from(binding.lexical) * 2)
                                        | (u8::from(binding.accessor_cell) * 4)
                                ));
                                if !binding.objects.is_empty() {
                                    s.push_str(",[");
                                    for (index, (slot, boxed)) in binding.objects.iter().enumerate()
                                    {
                                        if index > 0 {
                                            s.push(',');
                                        }
                                        s.push_str(&format!("[{slot},{boxed}]"));
                                    }
                                    s.push(']');
                                }
                                s.push(']');
                            }
                            s.push(']');
                        }
                    }
                }
                s.push_str("]}");
            }
            Const::NativeFactory(src) => {
                s.push('(');
                s.push_str(src);
                s.push(')');
            }
        }
    }
    s.push(']');
    s
}

/// Render the shared program table as a JS array-literal of `[code,consts]` pairs.
/// Each entry's `code`/`consts` are already-rendered JS array literals.
pub fn program_table_js(programs: &[(String, String)]) -> String {
    let mut s = String::from("[");
    for (i, (code_js, consts_js)) in programs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('[');
        s.push_str(code_js);
        s.push(',');
        s.push_str(consts_js);
        s.push(']');
    }
    s.push(']');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isa::{N_BIN_OPS, N_OPCODES, N_UN_OPS};

    fn id_perm() -> Vec<usize> {
        (0..N_OPCODES + 2).collect()
    }
    fn id_bin() -> Vec<usize> {
        (0..N_BIN_OPS).collect()
    }
    fn id_un() -> Vec<usize> {
        (0..N_UN_OPS).collect()
    }

    fn compiled(code: Vec<Instr>, consts: Vec<Const>) -> Compiled {
        Compiled {
            requires_source_compiler: false,
            code,
            consts,
            captures: vec![],
            slots: 4,
            pcount: 2,
            children: vec![],
        }
    }

    /// THE serialized-form round-trip: serialize a program containing EVERY opcode,
    /// then walk the flat code array back using ONLY the ISA (the inverse permutation
    /// recovers each discriminant; the [`crate::isa::Layout`] of that discriminant
    /// says how many operand words follow). The decoded discriminant sequence and the
    /// total consumed length must exactly match the source instructions' discriminants
    /// and sizes — proving discriminant ↔ size ↔ serialized form are internally
    /// consistent for every opcode, with a real encode→decode.
    #[test]
    fn every_opcode_encode_decode_round_trips() {
        use crate::isa::{Layout, OPCODE_TABLE};
        // A non-identity permutation, so the decode genuinely inverts it.
        let perm: Vec<usize> = {
            let mut p: Vec<usize> = (0..N_OPCODES + 3).collect();
            p.reverse();
            p
        };
        let inv = {
            let mut inv = vec![0usize; perm.len()];
            for (canon, &label) in perm.iter().enumerate() {
                inv[label] = canon;
            }
            inv
        };

        // One representative of every opcode (jump/handler targets kept in-range).
        let code: Vec<Instr> = vec![
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
            Instr::MakeClosure {
                child: 0,
                is_arrow: true,
                cap_start: 1,
                pcount: 0,
                up_slots: vec![0, 1, 2],
            },
            Instr::MakeNativeClosure {
                const_idx: 0,
                is_arrow: true,
                up_slots: vec![0, 1],
            },
            Instr::CopyProps,
            Instr::BeginLexical(0),
            Instr::InitLocal(0),
            Instr::CloneLexical(0),
            Instr::LoadArguments,
            Instr::CallArray,
            Instr::RestProps,
            Instr::RequireObject,
            Instr::BeginFinally,
            Instr::IterElide,
            Instr::NewRegExp(0),
            Instr::MapArgument(0, 0),
            Instr::ArrayAppend,
            Instr::ArraySpread,
            Instr::ArrayHole,
            Instr::NewArray,
            Instr::DefineData,
            Instr::DefineGetter,
            Instr::DefineSetter,
            Instr::SetPrototype,
            Instr::SetFunctionName,
            Instr::DefineMethod,
            Instr::TypeOfBinding(0),
            Instr::DeleteBinding(0),
            Instr::UpdateProp(0),
            Instr::LocalRef(0),
            Instr::WithRef(0, 0),
            Instr::ResolveRef,
            Instr::GetRef,
            Instr::PutRef,
            Instr::RefCall,
            Instr::CaptureRef(0),
            Instr::DeleteRef,
            Instr::TypeOfRef,
            Instr::EnterWith(0),
            Instr::UpdateRef(0),
            Instr::UnmapArguments,
            Instr::SetFunctionLength(0),
            Instr::ClearRef(0),
            Instr::SuperAssign(0),
            Instr::SuperUpdate(0),
            Instr::PushNewTarget,
            Instr::BeginVarEnvironment(0),
            Instr::CaptureClosureEnvironment(0),
            Instr::EvalCall(0),
            Instr::EnvironmentRef(0),
            Instr::AccessorRef(0),
            Instr::RefAdapter,
            Instr::WithRefCell(0, 0),
            Instr::MakeSuperProvider(0),
            Instr::ThrowReferenceError,
        ];
        assert_eq!(code.len(), N_OPCODES);
        let src_discs: Vec<usize> = code.iter().map(|i| i.discriminant()).collect();
        let src_sizes: Vec<u32> = code.iter().map(|i| i.size()).collect();

        let c = compiled(code, vec![Const::Num(1.0)]);
        let (words, _) = serialize(&c, &perm, &id_bin(), &id_un());

        // Decode: at each pc, the word is a permuted label -> canonical discriminant;
        // the Layout of that discriminant tells how many operand words to skip.
        let mut decoded_discs = Vec::new();
        let mut decoded_sizes = Vec::new();
        let mut pc = 0usize;
        while pc < words.len() {
            let label = words[pc] as usize;
            let canon = inv[label];
            decoded_discs.push(canon);
            let layout = OPCODE_TABLE[canon].1;
            let n_operands = match layout {
                Layout::Nullary => 0,
                Layout::Unary => 1,
                Layout::Binary => 2,
                // Closure: 5 fixed operands + nUp (the word at pc+5) upvalue slots.
                Layout::Closure => 5 + words[pc + 5] as usize,
                // NativeClosure: 3 fixed operands + nUp (the word at pc+3) slots.
                Layout::NativeClosure => 3 + words[pc + 3] as usize,
            };
            decoded_sizes.push(1 + n_operands as u32);
            pc += 1 + n_operands;
        }
        assert_eq!(pc, words.len(), "decode consumed exactly the whole stream");
        assert_eq!(
            decoded_discs, src_discs,
            "decoded discriminants match source"
        );
        assert_eq!(
            decoded_sizes, src_sizes,
            "decoded sizes match Instr::size()"
        );
    }

    #[test]
    fn nullary_and_unary_encode_with_discriminant() {
        let c = compiled(
            vec![Instr::PushUndef, Instr::LoadLocal(3), Instr::Ret],
            vec![],
        );
        let (code, _) = serialize(&c, &id_perm(), &id_bin(), &id_un());
        // PushUndef(disc 1), LoadLocal(disc 3) slot 3, Ret(disc 18).
        assert_eq!(code, vec![1.0, 3.0, 3.0, 18.0]);
    }

    #[test]
    fn jump_targets_translate_to_flat_offsets() {
        // [PushUndef, Jump->instr2(=PushNull), PushNull]. PushUndef size 1 @off0,
        // Jump size 2 @off1, PushNull size 1 @off3. Jump target instr index 2 -> off 3.
        let c = compiled(
            vec![Instr::PushUndef, Instr::Jump(2), Instr::PushNull],
            vec![],
        );
        let (code, _) = serialize(&c, &id_perm(), &id_bin(), &id_un());
        // PushUndef(1), Jump(14) target-offset 3, PushNull(2).
        assert_eq!(code, vec![1.0, 14.0, 3.0, 2.0]);
    }

    #[test]
    fn bin_un_subcodes_are_permuted() {
        let bin_perm: Vec<usize> = (0..N_BIN_OPS).rev().collect();
        let un_perm: Vec<usize> = (0..N_UN_OPS).rev().collect();
        let c = compiled(vec![Instr::Bin(0), Instr::Un(0)], vec![]);
        let (code, _) = serialize(&c, &id_perm(), &bin_perm, &un_perm);
        assert_eq!(
            code,
            vec![5.0, (N_BIN_OPS - 1) as f64, 6.0, (N_UN_OPS - 1) as f64]
        );
    }

    #[test]
    fn push_handler_sentinel_passes_through() {
        // PushHandler @off0 (size3), PopHandler @off3. catch -> instr1 offset 3,
        // fin -> u32::MAX sentinel passes through.
        let c = compiled(
            vec![Instr::PushHandler(1, u32::MAX), Instr::PopHandler],
            vec![],
        );
        let (code, _) = serialize(&c, &id_perm(), &id_bin(), &id_un());
        assert_eq!(code, vec![23.0, 3.0, u32::MAX as f64, 24.0]);
    }

    #[test]
    fn makeclosure_variable_length() {
        let c = compiled(
            vec![Instr::MakeClosure {
                child: 7,
                is_arrow: true,
                cap_start: 2,
                pcount: 1,
                up_slots: vec![0, 5],
            }],
            vec![],
        );
        let (code, _) = serialize(&c, &id_perm(), &id_bin(), &id_un());
        // disc 35, child 7, arrow 1, capStart 2, pcount 1, nUp 2, slots 0,5.
        assert_eq!(code, vec![35.0, 7.0, 1.0, 2.0, 1.0, 2.0, 0.0, 5.0]);
    }

    #[test]
    fn makenativeclosure_variable_length() {
        let c = compiled(
            vec![Instr::MakeNativeClosure {
                const_idx: 4,
                is_arrow: true,
                up_slots: vec![0, 5],
            }],
            vec![],
        );
        let (code, _) = serialize(&c, &id_perm(), &id_bin(), &id_un());
        // disc 36, constIdx 4, arrow 1, nUp 2, slots 0,5.
        assert_eq!(code, vec![36.0, 4.0, 1.0, 2.0, 0.0, 5.0]);
    }

    #[test]
    fn native_factory_const_renders_verbatim_and_deterministic() {
        // A NativeFactory const renders as its parenthesized function-expression
        // source, verbatim and untouched by the XOR/Str encoding. Same input ⇒
        // byte-identical output (determinism).
        let key = 0x9E37_79B9;
        let factory = "function(u0){return function render(){return u0[0]}}";
        let consts = vec![
            Const::Num(1.0),
            Const::NativeFactory(factory.to_string()),
            Const::Str("x".to_string()),
        ];
        let a = consts_array_js(&consts, key);
        let b = consts_array_js(&consts, key);
        assert_eq!(a, b, "same input ⇒ byte-identical");
        assert!(
            a.contains(&format!("({factory})")),
            "factory rendered verbatim parenthesized: {a}"
        );
        // The factory text is NOT XOR-mangled (it is a function value, decoded by
        // running it, not by the Str/template de-XOR path).
        assert!(a.contains("function render"), "function body intact: {a}");
    }

    #[test]
    fn packed_code_round_trips_all_masks_and_word_boundaries() {
        use mangler_testkit::assert_behaviorally_equal;
        let code = vec![
            0.0,
            5.0,
            31.0,
            32.0,
            255.0,
            1023.0,
            1024.0,
            2147483646.0,
            2147483647.0,
            2147483648.0,
            4294967295.0,
        ];
        for key in 0..64 {
            let js = code_array_js(&code, key);
            let decode = code_decode_js(key);
            let source = format!(
                "var code={js},i,t,n,k,v,C;{decode}\
var first=JSON.stringify(code);{decode}globalThis.__out=first+'|'+JSON.stringify(code);"
            );
            let expected = "[0,5,31,32,255,1023,1024,2147483646,2147483647,2147483648,4294967295]";
            assert_behaviorally_equal(
                &format!("globalThis.__out='{expected}|{expected}';"),
                &source,
            );
        }
    }

    #[test]
    fn packed_code_empty_and_large_payloads() {
        use mangler_testkit::assert_behaviorally_equal;
        for code in [vec![], vec![7.0; 100_000]] {
            let js = code_array_js(&code, 42);
            let source = format!(
                "var code={js},i,t,n,k,v,C;{}globalThis.__out=code.length;",
                code_decode_js(42)
            );
            assert_behaviorally_equal(&format!("globalThis.__out={};", code.len()), &source);
            assert_eq!(js, code_array_js(&code, 42));
        }
    }

    #[test]
    fn consts_render_num_bool_str_template() {
        let key = 0x12345;
        let ck = (key & 0xFFFF) | 1;
        let consts = vec![
            Const::Num(3.0),
            Const::Bool(true),
            Const::Num(f64::NAN),
            Const::Num(-0.0),
            Const::Str("hi".to_string()),
        ];
        let js = consts_array_js(&consts, key);
        assert!(js.starts_with("[3,true,(0/0),-0,["));
        // The string "hi" is rendered as XOR'd UTF-16 units.
        let h = ('h' as u32) ^ ck;
        let i = ('i' as u32) ^ ck;
        assert!(js.contains(&format!("[{h},{i}]")));
    }

    #[test]
    fn program_table_pairs() {
        let progs = vec![
            ("[1,2]".to_string(), "[3]".to_string()),
            ("[4]".to_string(), "[]".to_string()),
        ];
        assert_eq!(program_table_js(&progs), "[[[1,2],[3]],[[4],[]]]");
    }
}
