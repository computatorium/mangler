//! The interpreter emitter — driven by ONE [`InterpreterSpec`] + ONE
//! [`VmDiversity`], NOT 13 positional params.
//!
//! ## Why this is a validated-fragment emitter, not pure node-building
//!
//! WP4's mandate is to kill the "string-template / Rust-mirror sync" anti-pattern:
//! a JS string that must be kept byte-identical to a Rust mirror by hand. The VM
//! interpreter is ~40 hand-tuned JS handler bodies plus a dispatch skeleton with
//! several proven-equivalent structural variants. Those handler bodies have NO Rust
//! mirror — they are the single source of the opcode semantics, keyed by the ONE
//! ISA table's discriminants ([`crate::isa::Instr::discriminant`]) and the ONE
//! operator table's expressions ([`crate::isa::bin_expr_js`]/[`crate::isa::un_expr_js`]).
//! So there is nothing to drift *against*: the serializer and this emitter both
//! index the same `discriminant()`/`perm`, which is the consistency the round-trip
//! test in [`crate::isa`] proves.
//!
//! Per WP4's allowance ("where a handler body is irreducibly a chunk of hand-tuned
//! JS, you may build it from a small parsed-and-validated fragment IF you validate
//! it parses"), the interpreter is assembled as a JS source string from these
//! fragments and the whole function is then **validated by `Js::reparse`** (a unit
//! test) AND executed end-to-end against rquickjs (the behavioral suite). The
//! fragments themselves come from the ISA table, so they cannot silently desync from
//! the bytecode encoding. [`emit_interpreter`] returns the validated function as an
//! AST [`Stmt`] (parsed via the jsast layer) so callers splice a node, not a string.

use mangler_core::Language;
use mangler_jsast::lang::{Js, ParseOpts};
use swc_core::ecma::ast::Stmt;

use crate::chunk::ClosureMode;
use crate::diversity::{
    DECOY_FORMS, HANDLER_VARIANTS, SKELETON_VARIANTS, VmDiversity, bin_mba_expr,
};
use crate::isa::{N_BIN_OPS, N_OPCODES, N_UN_OPS, bin_expr_js, un_expr_js};

/// The lean, non-positional description of an interpreter to emit. Replaces the
/// 13-positional-param `interpreter_src(...)` signature: the diversification seeds
/// all live in [`VmDiversity`]; only the genuinely per-interpreter knobs (names,
/// shape) are spelled out here.
#[derive(Debug, Clone)]
pub struct InterpreterSpec<'a> {
    /// The interpreter function name (e.g. `V` / `D`).
    pub name: &'a str,
    /// The shared program-table variable name.
    pub table: &'a str,
    /// The hoisted `Reflect.construct` alias (used by the `new` opcode).
    pub rc: &'a str,
    /// The hoisted `Symbol.iterator` alias (used by the iterator opcodes when EH).
    pub sy: &'a str,
    /// Whether to emit the exception-handling / iterator / completion shape.
    pub needs_eh: bool,
    /// Whether to emit the interpreter body under a leading `"use strict"` directive
    /// (§5a case 2): a strict interpreter's `Store*` opcodes (`o[k]=v`) throw a
    /// `TypeError` on a non-writable / getter-only / frozen target, where a sloppy
    /// interpreter silently no-ops. `false` (the default) is byte-for-byte today's
    /// output — the directive is the ONLY structural difference between the two.
    pub is_strict: bool,
    /// The per-file diversification (perms, key, seeds, skeleton variant).
    pub diversity: &'a VmDiversity,
    pub usage: Option<&'a crate::chunk::InstructionUsage>,
}

// ---------------------------------------------------------------------------
// Skeleton helpers (FU3).
// ---------------------------------------------------------------------------

/// Wrap an infinite dispatch-loop BODY in the seed-chosen loop frame. All three are
/// infinite loops broken ONLY by `return`/`throw` from inside `body`, so the loop
/// keyword is semantically irrelevant.
fn loop_frame(variant: usize, body: &str) -> String {
    match variant % SKELETON_VARIANTS {
        0 => format!("for(;;){{{body}}}"),
        1 => format!("while(1){{{body}}}"),
        _ => format!("do{{{body}}}while(1)"),
    }
}

// ---------------------------------------------------------------------------
// Handler bodies — keyed by canonical opcode (the ISA discriminant).
// ---------------------------------------------------------------------------

/// The variant-0 (historical) body of each top-level opcode handler, keyed by the
/// ISA discriminant. Empty string = "no body at this discriminant in the lean
/// table" (the EH/iterator ops, emitted by [`eh_handler_body`] only when needed; and
/// the dynamic ops 5/6/13/20/35 emitted specially by [`build_handlers`]).
///
/// Handlers fetch from the DECODED local code array `C`; the decode loop de-XORs the
/// encrypted `code` param into `C` once at init. Reads use the scratch temps
/// declared in the interpreter frame (`a,b,o,k,v,f,n,t,obj,base,i,...`).
fn opcode_handler_body(canonical: usize) -> &'static str {
    match canonical {
        0 => "Push(S,consts[C[pc++]]);break;",
        1 => "Push(S,undefined);break;",
        2 => "Push(S,null);break;",
        3 => "Push(S,L[C[pc++]]);break;",
        4 => "L[C[pc++]]=S[S.length-1];break;",
        // 5 (Bin) / 6 (Un) / 13 (New) / 20 (GetIter) / 35 (MakeClosure) are emitted
        // specially by build_handlers (they interpolate perms / aliases / helpers).
        7 => "k=Pop(S);o=Pop(S);Push(S,o[k]);break;",
        8 => "v=Pop(S);k=Pop(S);o=Pop(S);o[k]=v;Push(S,v);break;",
        9 => "n=C[pc++];a=Tail(S,n);Push(S,a);break;",
        10 => {
            "n=C[pc++];obj={};base=S.length-2*n;\
for(i=0;i<n;i++){Object.defineProperty(obj,S[base+2*i],{value:S[base+2*i+1],writable:true,enumerable:true,configurable:true});}\
S.length=base;Push(S,obj);break;"
        }
        11 => "n=C[pc++];a=Tail(S,n);f=Pop(S);Push(S,Reflect.apply(f,undefined,a));break;",
        12 => "Push(S,receiver);break;",
        14 => "pc=C[pc];break;",
        15 => "t=C[pc++];if(!Pop(S))pc=t;break;",
        16 => "Pop(S);break;",
        17 => "Push(S,S[S.length-1]);break;",
        18 => "return Pop(S);",
        19 => "n=C[pc++];a=Tail(S,n);f=Pop(S);o=Pop(S);Push(S,Reflect.apply(f,o,a));break;",
        25 => "throw Pop(S);",
        29 => "n=C[pc++];Push(S,Slice(args,n));break;",
        30 => {
            "o=Pop(S);v=(function*(o){for(var k in o)yield k;})(o);Push(S,{i:v,n:v.next,d:false});break;"
        }
        31 => "k=Pop(S);o=Pop(S);Push(S,delete o[k]);break;",
        32 => "n=C[pc++];L[n]=[L[n]];break;",
        33 => "Push(S,L[C[pc++]][0]);break;",
        34 => "n=C[pc++];L[n][0]=S[S.length-1];break;",
        37 => "o=Pop(S);Copy(S[S.length-1],o,[]);break;",
        38 => "n=C[pc++];Lex(n>>>1,n&1);break;",
        39 => "n=C[pc++];B[n](S[S.length-1]);break;",
        40 => "n=C[pc++];v=L[n];Lex(n,B[n].c);B[n](v);break;",
        41 => "Push(S,AG);break;",
        42 => "a=Pop(S);f=Pop(S);o=Pop(S);Push(S,Reflect.apply(f,o,a));break;",
        43 => "a=Pop(S);o=Pop(S);Push(S,Copy({},o,a));break;",
        44 => {
            "if(S[S.length-1]===null||S[S.length-1]===undefined)throw TypeError('Cannot destructure null or undefined');break;"
        }
        47 => "t=consts[C[pc++]];Push(S,new RegExp(t.r,t.f));break;",
        48 => "pc+=2;if(!paramRefs)throw TypeError('Missing native parameter references');break;",
        49 => {
            "v=Pop(S);o=S[S.length-1];Object.defineProperty(o,o.length,{value:v,writable:true,enumerable:true,configurable:true});break;"
        }
        50 => {
            "v=Pop(S);o=S[S.length-1];for(var av of v)Object.defineProperty(o,o.length,{value:av,writable:true,enumerable:true,configurable:true});break;"
        }
        51 => "S[S.length-1].length++;break;",
        52 => "a=Pop(S);f=Pop(S);Push(S,Reflect.construct(f,a));break;",
        53 => {
            "v=Pop(S);k=Pop(S);o=S[S.length-1];Object.defineProperty(o,k,{value:v,writable:true,enumerable:true,configurable:true});break;"
        }
        54 => {
            "v=Pop(S);k=Pop(S);o=S[S.length-1];Object.defineProperty(o,k,{get:Method(v,k,1),enumerable:true,configurable:true});break;"
        }
        55 => {
            "v=Pop(S);k=Pop(S);o=S[S.length-1];Object.defineProperty(o,k,{set:Method(v,k,2),enumerable:true,configurable:true});break;"
        }
        56 => "v=Pop(S);if(v===null||Object(v)===v)Object.setPrototypeOf(S[S.length-1],v);break;",
        57 => {
            "k=Pop(S);f=S[S.length-1];if(typeof k==='symbol')k=({[k](){}})[k].name;Object.defineProperty(f,'name',{value:k,configurable:true});break;"
        }
        58 => {
            "v=Pop(S);k=Pop(S);o=S[S.length-1];Object.defineProperty(o,k,{value:Method(v,k,0),writable:true,enumerable:true,configurable:true});break;"
        }
        59 => {
            "n=C[pc++];o=Object.getOwnPropertyDescriptor(L,n);f=o&&o.get;Push(S,f&&f.vmType?f.vmType():typeof L[n]);break;"
        }
        60 => {
            "n=C[pc++];o=Object.getOwnPropertyDescriptor(L,n);f=o&&o.get;Push(S,f&&f.vmDelete?f.vmDelete():false);break;"
        }
        61 => {
            "n=C[pc++];k=Pop(S);o=Pop(S);switch(n){case 0:v=o[k]++;break;case 1:v=++o[k];break;case 2:v=o[k]--;break;case 3:v=--o[k];break;}Push(S,v);break;"
        }
        73 => {
            "args=AG=Reflect.apply(function(){'use strict';return arguments;},undefined,args);break;"
        }
        74 => {
            "n=C[pc++];Object.defineProperty(S[S.length-1],'length',{value:n,configurable:true});break;"
        }
        76 => "n=C[pc++];v=Pop(S);o=Pop(S);k=Pop(S);a=Pop(S);Push(S,SuperOp(a,k,o,v,n));break;",
        77 => "n=C[pc++];o=Pop(S);k=Pop(S);a=Pop(S);Push(S,SuperOp(a,k,o,null,n|64));break;",
        78 => "Push(S,newTarget);break;",
        79..=82 => crate::runtime_env::handler(canonical),
        62..=72 | 75 | 83..=85 => crate::runtime_ref::handler(canonical),
        86 => "n=C[pc++];o=Pop(S);a=Pop(S);Push(S,MakeSuperProvider(a,o,n));break;",
        87 => "throw new ReferenceError('Invalid left-hand side in assignment');",
        _ => "",
    }
}

/// A semantically-identical alternative body for `(canonical, variant)` (Stage-1b),
/// or `None` to fall through to [`opcode_handler_body`]. Every alternative computes
/// byte-for-byte the same stack/local/pc effects as variant 0.
fn opcode_handler_variant(canonical: usize, variant: usize) -> Option<&'static str> {
    let v = variant % HANDLER_VARIANTS;
    if v == 0 {
        return None;
    }
    Some(match (canonical, v) {
        (3, 1) => "n=C[pc++];a=L[n];Push(S,a);break;",
        (3, 2) => "Push(S,L[C[pc++]]);break;",
        (4, 1) => "n=C[pc++];L[n]=S[S.length-1];break;",
        (4, 2) => "n=C[pc++];a=S[S.length-1];L[n]=a;break;",
        (7, 1) => "k=Pop(S);o=Pop(S);a=o[k];Push(S,a);break;",
        (7, 2) => "k=Pop(S);o=Pop(S);Push(S,o[k]);break;",
        (8, 1) => "v=Pop(S);k=Pop(S);o=Pop(S);Push(S,o[k]=v);break;",
        (8, 2) => "v=Pop(S);k=Pop(S);o=Pop(S);o[k]=v;Push(S,v);break;",
        (16, 1) => "S.length=S.length-1;break;",
        (16, 2) => "Pop(S);break;",
        (17, 1) => "a=S[S.length-1];Push(S,a);break;",
        (17, 2) => "Push(S,S[S.length-1]);break;",
        (18, 1) => "a=Pop(S);return a;",
        (18, 2) => "return Pop(S);",
        (15, 1) => "t=C[pc++];a=Pop(S);if(!a)pc=t;break;",
        (15, 2) => "t=C[pc++];if(!Pop(S))pc=t;break;",
        (11, 1) => "n=C[pc++];a=Tail(S,n);f=Pop(S);Push(S,Reflect.apply(f,void 0,a));break;",
        (11, 2) => "n=C[pc++];a=Tail(S,n);f=Pop(S);Push(S,Reflect.apply(f,undefined,a));break;",
        (9, 1) => "n=C[pc++];a=Tail(S,n);Push(S,a);break;",
        (9, 2) => "n=C[pc++];o=Tail(S,n);Push(S,o);break;",
        (1, 1) => "Push(S,void 0);break;",
        (1, 2) => "Push(S,undefined);break;",
        (33, 1) => "o=L[C[pc++]];Push(S,o[0]);break;",
        (33, 2) => "Push(S,L[C[pc++]][0]);break;",
        _ => return None,
    })
}

/// Exception/completion + iterator handler bodies (emitted only when `needs_eh`).
/// `GetIter` (20) is emitted by [`build_handlers`] (it interpolates the `sy` alias).
fn eh_handler_body(canonical: usize) -> &'static str {
    match canonical {
        21 => {
            "it=Pop(S);it.d=true;r=Reflect.apply(it.n,it.i,[]);Check(r);if(r.done){Push(S,false);}else{v=r.value;it.d=false;Push(S,v);Push(S,true);}break;"
        }
        22 => {
            "it=Pop(S);if(!it.d){it.d=true;try{m=it.i.return;if(m!==null&&m!==undefined)Check(Reflect.apply(m,it.i,[]));}catch(closeError){if(!(comp.t===1||(P.length&&P[P.length-1].t===1)))throw closeError;}}break;"
        }
        23 => "a=C[pc++];b=C[pc++];Push(H,[a>2e9?-1:a,b>2e9?-1:b,S.length,P.length]);break;",
        24 => "Pop(H);break;",
        26 => {
            "comp=Pop(P);if(comp.t===1){throw comp.v;}\
else if(comp.t===2){if(!unwind(0)){v=comp.v;comp=NORMAL;return v;}}\
else if(comp.t===3){if(!unwind(comp.f)){pc=comp.v;comp=NORMAL;}}break;"
        }
        27 => "comp={t:2,v:Pop(S),f:0};if(!unwind(0)){v=comp.v;comp=NORMAL;return v;}break;",
        28 => "a=C[pc++];b=C[pc++];comp={t:3,v:a,f:b};if(!unwind(b)){pc=a;comp=NORMAL;}break;",
        45 => "Push(P,comp);comp=NORMAL;break;",
        46 => {
            "it=Pop(S);it.d=true;r=Reflect.apply(it.n,it.i,[]);Check(r);if(r.done){Push(S,false);}else{it.d=false;Push(S,undefined);Push(S,true);}break;"
        }
        _ => "",
    }
}

/// Body for a JUNK (dead) opcode case (C3). Unreachable decoys that read like real
/// handlers; `serialize` never emits these labels.
fn junk_case_body(form: usize) -> &'static str {
    match form % DECOY_FORMS {
        0 => "a=C[pc++];Push(S,a^pc);break;",
        1 => "o=Pop(S);k=Pop(S);Push(S,o);break;",
        2 => "n=C[pc++];L[n]=S.length;break;",
        3 => "b=Pop(S);a=Pop(S);Push(S,a-b);break;",
        4 => "pc=C[pc];break;",
        5 => "n=C[pc++];a=Pop(S);Push(S,function(){return n^a;});break;",
        6 => "a=Pop(S);Push(S,Sd([a&65535]));break;",
        _ => "a=Pop(S);b=Pop(S);o=Pop(S);Push(S,b);Push(S,a);Push(S,o);break;",
    }
}

/// Build the `Bin` opcode body with per-file-permuted inner case labels, applying
/// the Stage-3b MBA tangle on the proven-exact integer-domain ops. Operator
/// expressions come from the ONE ISA table.
fn bin_switch_body(div: &VmDiversity, usage: Option<&crate::chunk::InstructionUsage>) -> String {
    let mut s = String::from("op=C[pc++];b=Pop(S);a=Pop(S);switch(op){");
    for k in 0..N_BIN_OPS {
        if usage.is_some_and(|u| !u.binary(k)) {
            continue;
        }
        let rendered = match div.bin_mba(k) {
            Some(form) => format!(
                "typeof a==='number'&&typeof b==='number'?({}):({})",
                bin_mba_expr(k, form),
                bin_expr_js(k)
            ),
            None => bin_expr_js(k).to_string(),
        };
        s.push_str(&format!(
            "case {}:Push(S,{rendered});break;",
            div.bin_perm[k]
        ));
    }
    s.push_str("}break;");
    s
}

/// Build the `Un` opcode body with per-file-permuted inner case labels. Operator
/// expressions come from the ONE ISA table.
fn un_switch_body(div: &VmDiversity, usage: Option<&crate::chunk::InstructionUsage>) -> String {
    let mut s = String::from("uop=C[pc++];a=Pop(S);switch(uop){");
    for (k, opcode) in div.un_perm.iter().enumerate().take(N_UN_OPS) {
        if usage.is_some_and(|u| !u.unary(k)) {
            continue;
        }
        s.push_str(&format!("case {opcode}:Push(S,{});break;", un_expr_js(k)));
    }
    s.push_str("}break;");
    s
}

/// Rewrite a `switch`-style handler body into a closure-dispatch entry body. The
/// only difference is dispatch-exit control flow: strip the trailing `break;`, route
/// `Ret`'s trailing `return EXPR;` through the shared done-flag/result-slot, and keep
/// `throw` verbatim (it propagates out of the closure/loop/function).
fn to_closure_body(body: &str, done: &str, ret: &str) -> String {
    if let Some(rest) = body.strip_suffix("break;") {
        return rest.to_string();
    }
    if body.starts_with("throw ") {
        return body.to_string();
    }
    if let Some(pos) = body.rfind("return ") {
        let prefix = &body[..pos];
        let after = &body[pos + "return ".len()..];
        if let Some(expr) = after.strip_suffix(';') {
            return format!("{prefix}{done}=1;{ret}={expr};return;");
        }
    }
    match body.strip_suffix("break;") {
        Some(rest) => rest.to_string(),
        None => body.to_string(),
    }
}

/// Collect `(permuted-label, body)` pairs for every real opcode + every decoy slot,
/// in canonical order then decoy order. The dynamic ops (5/6/13/20/35) and the
/// Stage-1b handler-variant selection are resolved here.
fn build_handlers(spec: &InterpreterSpec) -> Vec<(usize, String)> {
    let div = spec.diversity;
    let mut handlers: Vec<(usize, String)> = Vec::new();
    let runtime_source = spec.usage.is_none_or(|usage| usage.opcode(81));
    for (k, &label) in div.perm.iter().enumerate().take(N_OPCODES) {
        if spec.usage.is_some_and(|u| !u.opcode(k)) {
            continue;
        }
        if k == 13 {
            handlers.push((
                label,
                format!(
                    "n=C[pc++];a=Tail(S,n);f=Pop(S);Push(S,{});break;",
                    if runtime_source {
                        "SourceConstruct(f,a,f)".to_string()
                    } else {
                        format!("{}(f,a)", spec.rc)
                    }
                ),
            ));
            continue;
        }
        if k == 5 {
            handlers.push((label, bin_switch_body(div, spec.usage)));
            continue;
        }
        if k == 6 {
            handlers.push((label, un_switch_body(div, spec.usage)));
            continue;
        }
        if spec.needs_eh && k == 20 {
            handlers.push((label, format!("o=Pop(S);v=Reflect.apply(o[{}],o,[]);Check(v);Push(S,{{i:v,n:v.next,d:false}});break;", spec.sy)));
            continue;
        }
        if k == 35 {
            handlers.push((
                label,
                "n=C[pc++];cl_a=C[pc++];cl_s=C[pc++];cl_p=C[pc++];cl_n=C[pc++];\
cl_u=List();for(j=0;j<cl_n;j++)Push(cl_u,C[pc++]);\
Push(S,Mk(n,cl_a,cl_s,cl_p,cl_u,L,receiver,newTarget,nextEnvironment||VE));nextEnvironment=undefined;break;"
                    .to_string(),
            ));
            continue;
        }
        if k == 36 {
            // Phase 3 native-closure escape hatch: build the closure by CALLING the
            // factory const with the threaded upvalue slot values. `n` is the const
            // index, `cl_a` the (informational) arrow flag, `cl_n` the upvalue count;
            // each upvalue is the enclosing-frame slot value `L[C[pc++]]` (a local,
            // or a cell array for a mutable capture, or the receiver for an arrow's
            // lexical `this`). The factory returns the native fn, run in its own mode.
            handlers.push((
                label,
                "n=C[pc++];cl_a=C[pc++];cl_n=C[pc++];\
cl_u=List();for(j=0;j<cl_n;j++){t=C[pc++];Push(cl_u,t===2147483646?receiver:NativeReference(t));}\
Push(S,Reflect.apply(consts[n],null,cl_u));break;"
                    .to_string(),
            ));
            continue;
        }
        // Stage-1b: pick this opcode's seed-derived body variant (variant 0 = the
        // historical body).
        let body = match opcode_handler_variant(k, div.handler_variant(k)) {
            Some(alt) => alt,
            None => opcode_handler_body(k),
        };
        if !body.is_empty() {
            handlers.push((label, body.to_string()));
            continue;
        }
        if spec.needs_eh {
            let eh = eh_handler_body(k);
            if !eh.is_empty() {
                handlers.push((label, eh.to_string()));
            }
        }
    }
    // Decoy handlers at the extra permutation slots.
    for &label in &div.perm[N_OPCODES..] {
        handlers.push((label, junk_case_body(div.decoy_form(label)).to_string()));
    }
    for (_, body) in &mut handlers {
        if runtime_source {
            *body = body
                .replace("Reflect.apply(f,", "SourceInvoke(f,")
                .replace("Reflect.construct(f,a)", "SourceConstruct(f,a,f)");
        }
        *body = body
            .replace(
                "(function(){return this===undefined;})()",
                if spec.is_strict { "true" } else { "false" },
            )
            .replace("S.length=S.length-1;", "SPop();")
            .replace("S.length=base;", "Trim(base);")
            .replace("Push(S,", "SPush(")
            .replace("Pop(S)", "SPop()")
            .replace("Tail(S,n)", "STail(n)")
            .replace("S.length", "sp");
    }
    handlers
}

/// Build the hoisted `Sd`/`Mk` helper declarations + the shared decode-init prologue
/// for `spec`, in the FU3 skeleton-variant form.
fn decode_init(spec: &InterpreterSpec) -> String {
    let div = spec.diversity;
    let variant = div.skeleton();
    let key = div.code_key;
    let ck = (key & 0xFFFF) | 1;
    let name = spec.name;
    let table = spec.table;
    let used = |opcode| spec.usage.is_none_or(|usage| usage.opcode(opcode));
    let constant = |bit| spec.usage.is_none_or(|usage| usage.constant_kind(bit));
    let needs_sd = [1, 2, 4, 8].into_iter().any(constant)
        || div.perm[N_OPCODES..]
            .iter()
            .any(|&label| div.decoy_form(label) == 6);
    let sd_body = format!(
        "var s='',part=List();for(var j=0;j<a.length;j++){{Push(part,a[j]^{ck});if(part.length===8192||j+1===a.length){{s+=Reflect.apply(String.fromCharCode,null,part);part.length=0;}}}}return s;"
    );
    let sd_decl = if needs_sd {
        format!("function Sd(a){{{sd_body}}}")
    } else {
        String::new()
    };
    let environment = (79..=82).any(used);
    let suspension = spec.usage.is_none_or(|usage| usage.has_suspension());
    let needs_mk = used(35);
    let mut mk_decl = String::from(
        "function List(){return Object.setPrototypeOf([],null);}function Push(a,v){a[a.length]=v;return a.length;}function Pop(a){if(!a.length)return undefined;var v=a[a.length-1];a.length--;return v;}function SPush(v){S[sp++]=v;}function SPop(){if(!sp)return undefined;var v=S[--sp];S[sp]=undefined;return v;}function Trim(n){while(sp>n)S[--sp]=undefined;}",
    );
    let needs_tail = [9, 11, 13, 19].into_iter().any(used);
    if used(86) {
        let provider = crate::eval_class::provider(None, "Reflect.apply", spec.sy, false);
        mk_decl.push_str(&format!(
            "function MakeSuperProvider(home,receiver,strict){{return function(operation){{return function(){{var holder=strict?{{__proto__:Object.getPrototypeOf(home),run(){{'use strict';return {provider};}}}}:{{__proto__:Object.getPrototypeOf(home),run(){{return {provider};}}}},select=Reflect.apply(holder.run,receiver,[]),fn=select(operation);return Reflect.apply(fn,undefined,arguments);}};}};}}"
        ));
    }
    let needs_slice = needs_tail || used(29);
    if needs_slice || constant(8) {
        mk_decl.push_str("function Append(a,v){Object.defineProperty(a,a.length,{value:v,writable:true,enumerable:true,configurable:true});}");
    }
    if needs_slice {
        mk_decl.push_str("function Slice(a,start,end){var out=[];end=end===undefined?a.length:end;for(var z=start;z<end&&z<a.length;z++)Append(out,a[z]);return out;}");
    }
    if needs_tail {
        mk_decl.push_str("function STail(n){var out=Slice(S,sp-n,sp);Trim(sp-n);return out;}");
    }
    if constant(8) {
        mk_decl.push_str("function MapItems(a,fn){var out=[];for(var z=0;z<a.length;z++)Append(out,fn(a[z],z,a));return out;}");
    }
    if used(37) || used(43) {
        mk_decl.push_str("function Contains(a,k){for(var z=0;z<a.length;z++)if(a[z]===k)return true;return false;}");
    }

    if (62..=72).any(used)
        || used(75)
        || used(36)
        || used(83)
        || used(84)
        || used(85)
        || environment
    {
        mk_decl.push_str(&crate::runtime_ref::helpers(|op| {
            used(op) || environment && matches!(op, 62 | 63 | 66 | 68)
        }));
    }
    if environment {
        mk_decl.push_str(&format!("var Programs={table};"));
        if used(81) {
            mk_decl.push_str("var SourceInvoke=Object.prototype.hasOwnProperty.call(Programs,'invoke')?(function(invoke){return function(f,o,a){return invoke(f,o,a,IndirectEval);};})(Programs.invoke):Reflect.apply,SourceConstruct=Object.prototype.hasOwnProperty.call(Programs,'construct')?Programs.construct:Reflect.construct;");
        }
        mk_decl.push_str(&crate::runtime_env::helpers(used));
    }
    if spec.needs_eh {
        mk_decl.push_str("function Check(v){if(Object(v)!==v)throw TypeError('Iterator result is not an object');return v;}");
    }
    if used(54) || used(55) || used(58) {
        mk_decl.push_str("var Functions=new WeakMap();function Method(){var fn=arguments[0],key=arguments[1],kind=arguments[2],invoke=Reflect.apply(WeakMap.prototype.get,Functions,[fn])||function(recv,args){return Reflect.apply(fn,recv,args);},m;if(invoke.factory&&!invoke.kind)return invoke.factory(invoke,key,kind);if(kind===1)m=Object.getOwnPropertyDescriptor(invoke.strict?{get [key](){'use strict';return invoke(this,arguments);}}:{get [key](){return invoke(this,arguments);}},key).get;else if(kind===2)m=Object.getOwnPropertyDescriptor(invoke.strict?{set [key](v){'use strict';return invoke(this,arguments);}}:{set [key](v){return invoke(this,arguments);}},key).set;else{m=(invoke.strict?{[key](){'use strict';return invoke(this,arguments);}}:{[key](){return invoke(this,arguments);}})[key];}Object.defineProperty(m,'length',{value:fn.length,configurable:true});if(kind===0&&invoke.kind){var name=m.name,length=m.length;m=Suspended(invoke,invoke.kind,invoke.strict,false,undefined,invoke.factory);Object.defineProperty(m,'name',{value:name,configurable:true});Object.defineProperty(m,'length',{value:length,configurable:true});}return m;}");
    }
    if used(76) || used(77) {
        let operators = [
            "=", "+=", "-=", "*=", "/=", "%=", "**=", "<<=", ">>=", ">>>=", "|=", "^=", "&=",
            "&&=", "||=", "??=",
        ];
        let mut body = String::from("switch(mode&95){");
        for (mode, op) in operators.iter().enumerate() {
            body.push_str(&format!("case {mode}:return super[key]{op}rhs();"));
        }
        for (mode, expr) in [
            "super[key]++",
            "++super[key]",
            "super[key]--",
            "--super[key]",
        ]
        .iter()
        .enumerate()
        {
            body.push_str(&format!("case {}:return {expr};", mode + 64));
        }
        body.push('}');
        mk_decl.push_str(&format!("function SuperOp(home,key,recv,rhs,mode){{var obj={{__proto__:Object.getPrototypeOf(home),a(key,rhs,mode){{{body}}},s(key,rhs,mode){{'use strict';{body}}}}};return Reflect.apply(mode&32?obj.s:obj.a,recv,[key,rhs,mode]);}}"));
    }
    if used(81) {
        mk_decl = mk_decl.replace("Reflect.apply(fn,recv,args)", "SourceInvoke(fn,recv,args)");
    }
    if !suspension {
        mk_decl = mk_decl.replace("if(kind===0&&invoke.kind){var name=m.name,length=m.length;m=Suspended(invoke,invoke.kind,invoke.strict,false,undefined,invoke.factory);Object.defineProperty(m,'name',{value:name,configurable:true});Object.defineProperty(m,'length',{value:length,configurable:true});}", "");
    }

    if used(37) || used(43) {
        mk_decl.push_str("function Copy(target,source,skip){if(source!==null&&source!==undefined){var keys=Reflect.ownKeys(Object(source));for(var ix=0;ix<keys.length;ix++){var key=keys[ix];if(Contains(skip,key))continue;var d=Object.getOwnPropertyDescriptor(source,key);if(d&&d.enumerable)Object.defineProperty(target,key,{value:source[key],writable:true,enumerable:true,configurable:true});}}return target;}");
    }
    if needs_mk || used(36) {
        mk_decl.push_str("function Ref(P,n){var d=Object.getOwnPropertyDescriptor(P,n);return d&&d.get?d:{get:function(){return P[n];},set:function(v){P[n]=v;}};}");
    }
    if used(38) || used(40) {
        mk_decl.push_str("function Lex(n,c){var x,ready=false;B[n]=function(v){x=v;ready=true;};B[n].c=c;Object.defineProperty(L,n,{configurable:true,get:function(){if(!ready)throw ReferenceError('Uninitialized lexical binding');return x;},set:function(v){if(!ready)throw ReferenceError('Uninitialized lexical binding');if(c)throw TypeError('Assignment to constant variable');x=v;}});}");
    }
    if needs_mk {
        if suspension {
            mk_decl.push_str("function Suspended(invoke,kind,strict,ar,receiver,factory){var clo,template=kind===1?async function(){}:kind===2?function*(){}:async function*(){};function call(recv,args,refs,target){var value;if(kind===1){try{return invoke(recv,args,refs,target);}catch(error){return (async()=>{throw error;})();}}value=invoke(recv,args,refs,target);var iterator=value;var proto=clo.prototype;Object.setPrototypeOf(iterator,Object(proto)===proto?proto:Object.getPrototypeOf(template.prototype));return iterator;}clo=factory?factory(call,'',0):ar?((...args)=>call(receiver,args)):strict?({call(){'use strict';return call(this,arguments);}}).call:({call(){return call(this,arguments);}}).call;Object.setPrototypeOf(clo,Object.getPrototypeOf(template));if(kind!==1)Object.defineProperty(clo,'prototype',{value:template.prototype,writable:true});return clo;}");
        }
        let suspended = if suspension {
            "row[4]?Suspended(invoke,row[4],row[3],ar,prcv,row[5]):"
        } else {
            ""
        };
        let remember = if used(54) || used(55) || used(58) {
            "Object.defineProperty(invoke,'strict',{value:row[3]});Object.defineProperty(invoke,'kind',{value:row[4]});Object.defineProperty(invoke,'factory',{value:row[5]});Reflect.apply(WeakMap.prototype.set,Functions,[clo,invoke]);"
        } else {
            ""
        };
        let mode = |bit| spec.usage.is_none_or(|usage| usage.closure_mode(bit));
        let arrow = mode(ClosureMode::Arrow);
        let strict_child = (mode(ClosureMode::Sloppy) || mode(ClosureMode::Strict))
            && (spec.is_strict || mode(ClosureMode::Strict));
        let sloppy_child = !spec.is_strict && mode(ClosureMode::Sloppy);
        let factories = mode(ClosureMode::ArgumentsFactory);
        let target = if arrow {
            "ar?prtarget:target"
        } else {
            "target"
        };
        let needs_target = used(78) || environment;
        let call_suffix = |refs: &str, target: &str| {
            if environment {
                format!(",{refs},{target},penv")
            } else if needs_target {
                format!(",{refs},{target}")
            } else if factories {
                format!(",{refs}")
            } else {
                String::new()
            }
        };
        let invoke_suffix = call_suffix("refs", target);
        let ordinary_suffix = call_suffix("undefined", "new.target");
        let arrow_suffix = call_suffix("undefined", "prtarget");
        let invoke = if factories || suspension || !remember.is_empty() {
            format!(
                "var invoke=function(recv,args,refs,target){{return run(row[0],row[1],args,up,cs,pcnt,recv,true{invoke_suffix});}};"
            )
        } else {
            String::new()
        };
        let ordinary = |strict: bool| {
            format!(
                "function(){{{}return run(row[0],row[1],arguments,up,cs,pcnt,this,true{ordinary_suffix});}}",
                if strict { "\"use strict\";" } else { "" }
            )
        };
        let mut creation = match (strict_child, sloppy_child) {
            (true, true) => format!("row[3]?{}:{}", ordinary(true), ordinary(false)),
            (true, false) => ordinary(true),
            _ => ordinary(false),
        };
        if arrow {
            let arrow_source =
                format!("((...a)=>run(row[0],row[1],a,up,cs,pcnt,prcv,true{arrow_suffix}))");
            creation = if strict_child || sloppy_child {
                format!("ar?{arrow_source}:{creation}")
            } else {
                arrow_source
            };
        }
        creation = format!("{suspended}{creation}");
        if factories {
            creation = format!("row[5]&&!row[4]?row[5](invoke):{creation}");
        }
        let upvalue = if mode(ClosureMode::SelfBinding) {
            "sl[q]===2147483647?{get:function(){return clo;}}:Ref(PL,sl[q])"
        } else {
            "Ref(PL,sl[q])"
        };
        let length = if mode(ClosureMode::InitialLength) {
            "Object.defineProperty(clo,\"length\",{value:pcnt,configurable:true});"
        } else {
            ""
        };
        mk_decl.push_str(&format!(
            "function Mk(idx,ar,cs,pcnt,sl,PL,prcv,prtarget,penv){{var up=List(),q,clo;for(q=0;q<sl.length;q++)Push(up,{upvalue});var row={table}[idx],run=row[2]||{name};{invoke}clo={creation};{length}{remember}return clo;}}"
        ));
    }
    let sd_expr = if needs_sd {
        format!("var Sd=function(a){{{sd_body}}};")
    } else {
        String::new()
    };
    let helpers = match variant {
        0 => format!("{sd_decl}{mk_decl}"),
        1 => format!("{mk_decl}{sd_decl}"),
        _ => format!("{sd_expr}{mk_decl}"),
    };
    let code_decode = crate::serialize::code_decode_js(key)
        .replace(
            "t=code[0];code.length=0;",
            "t=code[0];Object.setPrototypeOf(code,null);code.length=0;",
        )
        .replace("code.push(", "Push(code,")
        .replace(
            "if(!code.d)",
            "if(!Object.prototype.hasOwnProperty.call(code,\"d\"))",
        )
        .replace("code.d=1;", "Object.defineProperty(code,\"d\",{value:1});");
    let mut constant_branches = Vec::new();
    if constant(1) {
        constant_branches.push("if(Array.isArray(t))consts[i]=Sd(t);".to_string());
    }
    if constant(2) {
        constant_branches.push("if(t&&typeof t==='object'&&Object.prototype.hasOwnProperty.call(t,'b'))consts[i]=BigInt(Sd(t.b));".to_string());
    }
    if constant(4) {
        constant_branches.push("if(t&&typeof t==='object'&&Object.prototype.hasOwnProperty.call(t,'r')){t.r=Sd(t.r);t.f=Sd(t.f);}".to_string());
    }
    if constant(8) {
        constant_branches.push("if(t&&typeof t==='object'&&Object.prototype.hasOwnProperty.call(t,'q')){a=MapItems(t.q,function(e){return Array.isArray(e)?Sd(e):undefined;});Object.defineProperty(a,'raw',{value:Object.freeze(MapItems(t.w,Sd))});consts[i]=Object.freeze(a);}".to_string());
    }
    let constant_decode = if constant_branches.is_empty() {
        String::new()
    } else {
        format!(
            "if(!Object.prototype.hasOwnProperty.call(consts,'d')){{for(i=0;i<consts.length;i++){{t=consts[i];{}}}Object.defineProperty(consts,'d',{{value:1}});}}",
            constant_branches.join("else ")
        )
    };
    // The decoder mutates only its arguments and has private scratch state. Its
    // identical declaration can be shared between this table's interpreter modes.
    let code_body = code_decode
        .strip_suffix("C=code;")
        .expect("code decode terminator");
    let decode_code = format!("function DecodeCode(code){{var t,n,k,i,v;{code_body}}}");
    let constant_mask = [1u8, 2, 4, 8]
        .into_iter()
        .filter(|bit| constant(*bit))
        .sum::<u8>();
    let decode_constants = if constant_mask == 0 {
        String::new()
    } else {
        format!("function DecodeConstants{constant_mask}(consts,Sd){{var t,i,a;{constant_decode}}}")
    };
    let decode_calls = if constant_mask == 0 {
        "DecodeCode(code);C=code;".to_string()
    } else {
        format!("DecodeCode(code);C=code;DecodeConstants{constant_mask}(consts,Sd);")
    };
    let helpers = format!("{helpers}{decode_code}{decode_constants}").replace(
        "(function(){return this===undefined;})()",
        if spec.is_strict { "true" } else { "false" },
    );
    // Only carry secondary reference operations when an instruction can observe
    // them. Ordinary live reads still install the original lazy accessor.
    let dynamic_reference = environment || used(36) || used(62) || used(68) || used(83) || used(84);
    let mut capture_metadata = String::new();
    if dynamic_reference || used(59) || used(70) {
        capture_metadata.push_str("if(t.type)t.get.vmType=t.type;");
    }
    if dynamic_reference || used(60) || used(69) {
        capture_metadata.push_str("if(t.del)t.get.vmDelete=t.del;");
    }
    if dynamic_reference
        || spec.is_strict
        || spec
            .usage
            .is_none_or(|usage| usage.closure_mode(ClosureMode::Strict))
    {
        capture_metadata.push_str("if(t.strictSet)t.get.vmStrictSet=t.strictSet;");
    }
    if !capture_metadata.is_empty() {
        capture_metadata = format!("if(t.get){{{capture_metadata}}}");
    }
    let capture_descriptor = if spec.is_strict {
        "t.get&&Object.prototype.hasOwnProperty.call(t.get,'vmCapture')?t.get.vmCapture(true):t.get?{__proto__:null,get:t.get,set:t.get.vmStrictSet||t.set}:t"
    } else {
        "t.get&&Object.prototype.hasOwnProperty.call(t.get,'vmCapture')?t.get.vmCapture(false):t"
    };
    format!(
        "{helpers}\
{decode_calls}\
for(i=0;i<args.length&&i<pcount;i++)L[i]=args[i];\
for(i=0;i<caps.length;i++){{if(live){{t=caps[i];{capture_metadata}Object.defineProperty(L,capStart+i,{capture_descriptor});}}else L[capStart+i]=caps[i];}}\
if(paramRefs)for(i=0;i<paramRefs.length;i++)Object.defineProperty(L,paramRefs[i][0],paramRefs[i][1]);"
    )
}

/// Emit the interpreter as a JS source string. Internal; [`emit_interpreter`] wraps
/// this and parses it to AST (validating it).
pub(crate) fn interpreter_src(spec: &InterpreterSpec) -> String {
    let div = spec.diversity;
    let variant = div.skeleton();
    let name = spec.name;
    let decode_init = decode_init(spec);
    let handlers = build_handlers(spec);
    let bindings = if spec.usage.is_none_or(|u| u.opcode(38) || u.opcode(40)) {
        "List()"
    } else {
        "[]"
    };
    // §5a case 2: a strict interpreter carries a leading `"use strict"` so its
    // `Store*` opcodes throw on non-writable/getter-only/frozen targets. For a sloppy
    // interpreter this is the empty string, leaving the body byte-for-byte unchanged.
    let strict = if spec.is_strict {
        "\"use strict\";"
    } else {
        ""
    };

    // The switch-case block (used by the switch shape AND whenever needs_eh).
    let mut cases = String::new();
    for (label, body) in &handlers {
        cases.push_str(&format!("case {label}:{body}"));
    }
    cases.push_str("default:throw Error('Invalid virtual machine instruction');");

    if spec.needs_eh {
        let inner = loop_frame(variant, &format!("switch(C[pc++]){{{cases}}}"));
        let outer_body =
            format!("try{{{inner}}}catch(e){{comp={{t:1,v:e,f:0}};if(!unwind(0))throw e;}}");
        let outer = loop_frame(variant, &outer_body);
        format!(
            "function {name}(code,consts,args,caps,capStart,pcount,receiver,live,paramRefs,newTarget,environment){{{strict}\
var L=List(),B={bindings},S=List(),sp=0,sv,C=[],VE=environment,nextEnvironment,AG=args,pc=0,i,a,b,o,k,v,f,n,t,obj,op,uop,base,r,it,m,j,cl_a,cl_s,cl_p,cl_n,cl_u,H=List(),P=List(),comp={{t:0,v:0,f:0}},NORMAL={{t:0,v:0,f:0}},h;\
{decode_init}\
function unwind(floor){{while(H.length>floor){{h=Pop(H);Trim(h[2]);P.length=h[3];\
if(comp.t===1&&h[0]>=0){{SPush(comp.v);pc=h[0];comp=NORMAL;return true;}}\
if(h[1]>=0){{pc=h[1];return true;}}}}return false;}}\
{outer}\
}}"
        )
    } else if div.dispatch_shape(spec.needs_eh) == 1
        && !spec.usage.is_some_and(|u| u.prefers_direct_dispatch())
    {
        // Stage-1a: array-of-closures dispatch (lean-only).
        let mut build = String::from("var F=List(),dn=0,rv;");
        for (label, body) in &handlers {
            let cb = to_closure_body(body, "dn", "rv");
            build.push_str(&format!("F[{label}]=function(){{{cb}}};"));
        }
        let lean = loop_frame(variant, "F[C[pc++]]();if(dn)return rv;");
        format!(
            "function {name}(code,consts,args,caps,capStart,pcount,receiver,live,paramRefs,newTarget,environment){{{strict}\
var L=List(),B={bindings},S=List(),sp=0,sv,C=[],VE=environment,nextEnvironment,AG=args,pc=0,i,a,b,o,k,v,f,n,t,obj,op,uop,base,j,cl_a,cl_s,cl_p,cl_n,cl_u;\
{decode_init}\
{build}\
{lean}\
}}"
        )
    } else {
        let lean = loop_frame(variant, &format!("switch(C[pc++]){{{cases}}}"));
        format!(
            "function {name}(code,consts,args,caps,capStart,pcount,receiver,live,paramRefs,newTarget,environment){{{strict}\
var L=List(),B={bindings},S=List(),sp=0,sv,C=[],VE=environment,nextEnvironment,AG=args,pc=0,i,a,b,o,k,v,f,n,t,obj,op,uop,base,j,cl_a,cl_s,cl_p,cl_n,cl_u;\
{decode_init}\
{lean}\
}}"
        )
    }
}

/// Emit the per-file stack-machine interpreter as a validated AST [`Stmt`].
///
/// The interpreter is assembled from the ISA-table-derived handler fragments and
/// then **parsed** through the jsast layer, which both validates it is syntactically
/// well-formed and yields a node to splice (no string handed downstream). A parse
/// failure is a hard bug in the emitter — the returned `Result` surfaces it rather
/// than emitting malformed JS.
pub fn emit_interpreter(spec: &InterpreterSpec) -> mangler_core::Result<Stmt> {
    let mut stmt = emit_unprotected_interpreter(spec)?;
    if let Stmt::Decl(swc_core::ecma::ast::Decl::Fn(function)) = &mut stmt
        && let Some(body) = &mut function.function.body
    {
        crate::descriptors::protect(&mut body.stmts, spec.table)?;
    }
    Ok(stmt)
}

/// Emit interpreter variants with one shared descriptor-support prologue.
/// Use this boundary when downstream code pools helpers across interpreters:
/// pooled helpers and their captured descriptor adapters then share one scope.
/// No source factories are present in this interpreter-only batch.
pub fn emit_interpreters(specs: &[InterpreterSpec<'_>]) -> mangler_core::Result<Vec<Stmt>> {
    let mut statements = specs
        .iter()
        .map(emit_unprotected_interpreter)
        .collect::<mangler_core::Result<Vec<_>>>()?;
    crate::descriptors::protect(&mut statements, "")?;
    Ok(statements)
}

pub(crate) fn emit_unprotected_interpreter(spec: &InterpreterSpec) -> mangler_core::Result<Stmt> {
    let src = interpreter_src(spec);
    let ast = Js.parse(&src, &ParseOpts::default())?;
    // The source is a single function declaration; pull it out as one Stmt.
    let program = ast.into_program();
    let stmt = match program {
        swc_core::ecma::ast::Program::Script(s) => s.body.into_iter().next(),
        swc_core::ecma::ast::Program::Module(m) => m.body.into_iter().find_map(|it| match it {
            swc_core::ecma::ast::ModuleItem::Stmt(s) => Some(s),
            _ => None,
        }),
    };
    let mut stmt = stmt.ok_or_else(|| {
        mangler_core::Error::transform("vm-emit", "interpreter produced no statement")
    })?;
    // A helper closure forces the operand pointer into a heap context even when
    // the engine inlines its calls. Expand only these private stack operations;
    // the saved value keeps argument evaluation before the pointer increment.
    use swc_core::ecma::visit::VisitMutWith;
    stmt.visit_mut_with(&mut mangler_jsast::span::GeneratedSpans);
    stmt.visit_mut_with(&mut InlineOperandStack);
    Ok(stmt)
}

struct InlineOperandStack;

impl swc_core::ecma::visit::VisitMut for InlineOperandStack {
    fn visit_mut_stmts(&mut self, stmts: &mut Vec<Stmt>) {
        use swc_core::ecma::visit::VisitMutWith;
        stmts.retain(|stmt| !matches!(stmt, Stmt::Decl(swc_core::ecma::ast::Decl::Fn(f)) if matches!(f.ident.sym.as_ref(), "SPush" | "SPop")));
        stmts.visit_mut_children_with(self);
    }

    fn visit_mut_expr(&mut self, expr: &mut swc_core::ecma::ast::Expr) {
        use mangler_jsast::build as b;
        use swc_core::ecma::ast::{
            AssignExpr, AssignOp, AssignTarget, Callee, Expr, SimpleAssignTarget,
        };
        use swc_core::ecma::visit::VisitMutWith;
        expr.visit_mut_children_with(self);
        let Expr::Call(call) = expr else {
            return;
        };
        let Callee::Expr(callee) = &call.callee else {
            return;
        };
        let Expr::Ident(id) = &**callee else {
            return;
        };
        let member = |index| b::member_computed(b::ident_expr("S"), index);
        let store = |index, value| {
            let Expr::Member(target) = member(index) else {
                unreachable!()
            };
            Expr::Assign(AssignExpr {
                span: id.span,
                op: AssignOp::Assign,
                left: AssignTarget::Simple(SimpleAssignTarget::Member(target)),
                right: Box::new(value),
            })
        };
        *expr = match id.sym.as_ref() {
            "SPush" if call.args.len() == 1 => b::paren(b::seq(vec![
                b::assign("sv", (*call.args[0].expr).clone()),
                store(b::ident_expr("sp"), b::ident_expr("sv")),
                b::assign_op(AssignOp::AddAssign, "sp", b::num(1.0)),
            ])),
            "SPop" if call.args.is_empty() => b::paren(b::seq(vec![
                b::assign_op(AssignOp::SubAssign, "sp", b::num(1.0)),
                b::assign("sv", member(b::ident_expr("sp"))),
                store(b::ident_expr("sp"), b::ident_expr("undefined")),
                b::ident_expr("sv"),
            ])),
            _ => return,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diversity::VmDiversity;

    fn spec_for<'a>(div: &'a VmDiversity, needs_eh: bool) -> InterpreterSpec<'a> {
        InterpreterSpec {
            name: "V",
            table: "T",
            rc: "rc",
            sy: "sy",
            needs_eh,
            is_strict: false,
            diversity: div,
            usage: None,
        }
    }

    fn spec_for_strict<'a>(div: &'a VmDiversity, needs_eh: bool) -> InterpreterSpec<'a> {
        InterpreterSpec {
            name: "V",
            table: "T",
            rc: "rc",
            sy: "sy",
            needs_eh,
            is_strict: true,
            diversity: div,
            usage: None,
        }
    }

    #[test]
    fn lean_interpreter_reparses_and_has_no_eh() {
        let div = VmDiversity::baseline(2);
        let src = interpreter_src(&spec_for(&div, false));
        assert!(
            !src.contains("H=List()") && !src.contains("function unwind("),
            "lean has no completion machinery:\n{src}"
        );
        use swc_core::ecma::visit::{Visit, VisitWith};
        struct DispatchTry(bool);
        impl Visit for DispatchTry {
            fn visit_function(&mut self, _: &swc_core::ecma::ast::Function) {}
            fn visit_arrow_expr(&mut self, _: &swc_core::ecma::ast::ArrowExpr) {}
            fn visit_try_stmt(&mut self, _: &swc_core::ecma::ast::TryStmt) {
                self.0 = true;
            }
        }
        let Stmt::Decl(swc_core::ecma::ast::Decl::Fn(interpreter)) =
            emit_interpreter(&spec_for(&div, false)).unwrap()
        else {
            panic!("interpreter declaration");
        };
        let mut dispatch_try = DispatchTry(false);
        interpreter
            .function
            .body
            .as_ref()
            .unwrap()
            .visit_with(&mut dispatch_try);
        assert!(!dispatch_try.0, "lean dispatch has no exception frame");
        assert!(
            Js::reparse(&src, &ParseOpts::default()).is_ok(),
            "lean reparses:\n{src}"
        );
    }

    #[test]
    fn eh_interpreter_reparses_and_has_eh() {
        let div = VmDiversity::baseline(2);
        let src = interpreter_src(&spec_for(&div, true));
        assert!(
            src.contains("catch") && src.contains("H=List()"),
            "EH carries machinery"
        );
        assert!(
            Js::reparse(&src, &ParseOpts::default()).is_ok(),
            "EH reparses:\n{src}"
        );
    }

    /// Every skeleton/dispatch variant the diversity space can select must reparse.
    #[test]
    fn all_diversity_variants_reparse() {
        for seed in [1u64, 2, 7, 42, 100, 999, 12345] {
            let div = VmDiversity::draw(&mut mangler_core::Rng::for_pass(seed, "vm"));
            for needs_eh in [false, true] {
                let src = interpreter_src(&spec_for(&div, needs_eh));
                assert!(
                    Js::reparse(&src, &ParseOpts::default()).is_ok(),
                    "seed {seed} needs_eh {needs_eh} must reparse:\n{src}"
                );
            }
        }
    }

    /// Strictness is explicit in both native directives and dynamic reference
    /// metadata; nested strict closures must select the strict capture setter.
    #[test]
    fn strict_spec_carries_directive_and_reference_mode() {
        for seed in [1u64, 7, 42, 999] {
            let div = VmDiversity::draw(&mut mangler_core::Rng::for_pass(seed, "vm"));
            for needs_eh in [false, true] {
                let sloppy = interpreter_src(&spec_for(&div, needs_eh));
                let strict = interpreter_src(&spec_for_strict(&div, needs_eh));
                let brace = strict.find('{').expect("fn has a body brace");
                assert!(strict[brace + 1..].starts_with("\"use strict\";"));
                let brace = sloppy.find('{').expect("fn has a body brace");
                assert!(!sloppy[brace + 1..].starts_with("\"use strict\";"));
                assert!(
                    strict.contains(
                        "t.get&&Object.prototype.hasOwnProperty.call(t.get,'vmCapture')?t.get.vmCapture(true):t.get?{__proto__:null,get:t.get,set:t.get.vmStrictSet||t.set}:t"
                    )
                );
                assert!(strict.contains("r.set(v,true)"));
                assert!(sloppy.contains("r.set(v,false)"));
                assert!(
                    Js::reparse(&strict, &ParseOpts::default()).is_ok(),
                    "strict reparses:\n{strict}"
                );
            }
        }
    }

    #[test]
    fn emit_interpreter_returns_a_stmt() {
        let div = VmDiversity::baseline(2);
        let stmt = emit_interpreter(&spec_for(&div, false)).expect("emit ok");
        assert!(matches!(stmt, Stmt::Decl(_)), "interpreter is a fn decl");
    }
    #[test]
    fn every_instruction_has_an_eh_runtime_handler() {
        let diversity = VmDiversity::baseline(0);
        let spec = spec_for(&diversity, true);
        let handlers = build_handlers(&spec);
        for opcode in 0..N_OPCODES {
            assert!(
                handlers
                    .iter()
                    .any(|(label, body)| *label == opcode && !body.is_empty()),
                "missing runtime implementation for opcode {opcode}"
            );
        }
    }
}
