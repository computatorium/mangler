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
        0 => "S.push(consts[C[pc++]]);break;",
        1 => "S.push(undefined);break;",
        2 => "S.push(null);break;",
        3 => "S.push(L[C[pc++]]);break;",
        4 => "L[C[pc++]]=S[S.length-1];break;",
        // 5 (Bin) / 6 (Un) / 13 (New) / 20 (GetIter) / 35 (MakeClosure) are emitted
        // specially by build_handlers (they interpolate perms / aliases / helpers).
        7 => "k=S.pop();o=S.pop();S.push(o[k]);break;",
        8 => "v=S.pop();k=S.pop();o=S.pop();o[k]=v;S.push(v);break;",
        9 => "n=C[pc++];a=S.splice(S.length-n,n);S.push(a);break;",
        10 => {
            "n=C[pc++];obj={};base=S.length-2*n;\
for(i=0;i<n;i++){Object.defineProperty(obj,S[base+2*i],{value:S[base+2*i+1],writable:true,enumerable:true,configurable:true});}\
S.length=base;S.push(obj);break;"
        }
        11 => {
            "n=C[pc++];a=S.splice(S.length-n,n);f=S.pop();S.push(Reflect.apply(f,undefined,a));break;"
        }
        12 => "S.push(receiver);break;",
        14 => "pc=C[pc];break;",
        15 => "t=C[pc++];if(!S.pop())pc=t;break;",
        16 => "S.pop();break;",
        17 => "S.push(S[S.length-1]);break;",
        18 => "return S.pop();",
        19 => {
            "n=C[pc++];a=S.splice(S.length-n,n);f=S.pop();o=S.pop();S.push(Reflect.apply(f,o,a));break;"
        }
        25 => "throw S.pop();",
        29 => "n=C[pc++];S.push(Array.prototype.slice.call(args,n));break;",
        30 => "o=S.pop();S.push((function*(o){for(var k in o)yield k;})(o));break;",
        31 => "k=S.pop();o=S.pop();S.push(delete o[k]);break;",
        32 => "n=C[pc++];L[n]=[L[n]];break;",
        33 => "S.push(L[C[pc++]][0]);break;",
        34 => "n=C[pc++];L[n][0]=S[S.length-1];break;",
        37 => "o=S.pop();Copy(S[S.length-1],o,[]);break;",
        38 => "n=C[pc++];Lex(n>>>1,n&1);break;",
        39 => "n=C[pc++];B[n](S[S.length-1]);break;",
        40 => "n=C[pc++];v=L[n];Lex(n,B[n].c);B[n](v);break;",
        41 => "S.push(args);break;",
        42 => "a=S.pop();f=S.pop();o=S.pop();S.push(Reflect.apply(f,o,a));break;",
        43 => "a=S.pop();o=S.pop();S.push(Copy({},o,a));break;",
        44 => {
            "if(S[S.length-1]==null)throw TypeError('Cannot destructure null or undefined');break;"
        }
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
        (3, 1) => "n=C[pc++];a=L[n];S.push(a);break;",
        (3, 2) => "S.push(L[C[pc++]]);break;",
        (4, 1) => "n=C[pc++];L[n]=S[S.length-1];break;",
        (4, 2) => "n=C[pc++];a=S[S.length-1];L[n]=a;break;",
        (7, 1) => "k=S.pop();o=S.pop();a=o[k];S.push(a);break;",
        (7, 2) => "k=S.pop();o=S.pop();S.push(o[k]);break;",
        (8, 1) => "v=S.pop();k=S.pop();o=S.pop();S.push(o[k]=v);break;",
        (8, 2) => "v=S.pop();k=S.pop();o=S.pop();o[k]=v;S.push(v);break;",
        (16, 1) => "S.length=S.length-1;break;",
        (16, 2) => "S.pop();break;",
        (17, 1) => "a=S[S.length-1];S.push(a);break;",
        (17, 2) => "S.push(S[S.length-1]);break;",
        (18, 1) => "a=S.pop();return a;",
        (18, 2) => "return S.pop();",
        (15, 1) => "t=C[pc++];a=S.pop();if(!a)pc=t;break;",
        (15, 2) => "t=C[pc++];if(!S.pop())pc=t;break;",
        (11, 1) => {
            "n=C[pc++];a=S.splice(S.length-n,n);f=S.pop();S.push(Reflect.apply(f,void 0,a));break;"
        }
        (11, 2) => {
            "n=C[pc++];a=S.splice(S.length-n,n);f=S.pop();S.push(Reflect.apply(f,undefined,a));break;"
        }
        (9, 1) => "n=C[pc++];a=S.splice(S.length-n,n);S.push(a);break;",
        (9, 2) => "n=C[pc++];o=S.splice(S.length-n,n);S.push(o);break;",
        (1, 1) => "S.push(void 0);break;",
        (1, 2) => "S.push(undefined);break;",
        (33, 1) => "o=L[C[pc++]];S.push(o[0]);break;",
        (33, 2) => "S.push(L[C[pc++]][0]);break;",
        _ => return None,
    })
}

/// Exception/completion + iterator handler bodies (emitted only when `needs_eh`).
/// `GetIter` (20) is emitted by [`build_handlers`] (it interpolates the `sy` alias).
fn eh_handler_body(canonical: usize) -> &'static str {
    match canonical {
        21 => {
            "it=S.pop();r=it.next();if(r.done){S.push(false);}else{S.push(r.value);S.push(true);}break;"
        }
        22 => "it=S.pop();m=it.return;if(m!=null)m.call(it);break;",
        23 => "a=C[pc++];b=C[pc++];H.push([a>2e9?-1:a,b>2e9?-1:b,S.length,P.length]);break;",
        24 => "H.pop();break;",
        26 => {
            "comp=P.pop();if(comp.t===1){throw comp.v;}\
else if(comp.t===2){if(!unwind(0)){v=comp.v;comp=NORMAL;return v;}}\
else if(comp.t===3){if(!unwind(comp.f)){pc=comp.v;comp=NORMAL;}}break;"
        }
        27 => "comp={t:2,v:S.pop(),f:0};if(!unwind(0)){v=comp.v;comp=NORMAL;return v;}break;",
        28 => "a=C[pc++];b=C[pc++];comp={t:3,v:a,f:b};if(!unwind(b)){pc=a;comp=NORMAL;}break;",
        45 => "P.push(comp);comp=NORMAL;break;",
        46 => {
            "it=S.pop();r=it.next();if(r.done){S.push(false);}else{S.push(undefined);S.push(true);}break;"
        }
        _ => "",
    }
}

/// Body for a JUNK (dead) opcode case (C3). Unreachable decoys that read like real
/// handlers; `serialize` never emits these labels.
fn junk_case_body(form: usize) -> &'static str {
    match form % DECOY_FORMS {
        0 => "a=C[pc++];S.push(a^pc);break;",
        1 => "o=S.pop();k=S.pop();S.push(o);break;",
        2 => "n=C[pc++];L[n]=S.length;break;",
        3 => "b=S.pop();a=S.pop();S.push(a-b);break;",
        4 => "pc=C[pc];break;",
        5 => {
            "n=C[pc++];cl_n=C[pc++];cl_u=[];for(j=0;j<cl_n;j++)cl_u.push(C[pc++]);\
S.push(Mk(n,0,cl_n,cl_n,cl_u,L,receiver));break;"
        }
        6 => "a=S.pop();S.push(Sd([a&65535]));break;",
        _ => "a=S.pop();b=S.pop();o=S.pop();S.push(b);S.push(a);S.push(o);break;",
    }
}

/// Build the `Bin` opcode body with per-file-permuted inner case labels, applying
/// the Stage-3b MBA tangle on the proven-exact integer-domain ops. Operator
/// expressions come from the ONE ISA table.
fn bin_switch_body(div: &VmDiversity, usage: Option<&crate::chunk::InstructionUsage>) -> String {
    let mut s = String::from("op=C[pc++];b=S.pop();a=S.pop();switch(op){");
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
            "case {}:S.push({rendered});break;",
            div.bin_perm[k]
        ));
    }
    s.push_str("}break;");
    s
}

/// Build the `Un` opcode body with per-file-permuted inner case labels. Operator
/// expressions come from the ONE ISA table.
fn un_switch_body(div: &VmDiversity, usage: Option<&crate::chunk::InstructionUsage>) -> String {
    let mut s = String::from("uop=C[pc++];a=S.pop();switch(uop){");
    for (k, opcode) in div.un_perm.iter().enumerate().take(N_UN_OPS) {
        if usage.is_some_and(|u| !u.unary(k)) {
            continue;
        }
        s.push_str(&format!("case {opcode}:S.push({});break;", un_expr_js(k)));
    }
    s.push_str("}break;");
    s
}

/// Rewrite a `switch`-style handler body into a closure-dispatch entry body. The
/// only difference is dispatch-exit control flow: strip the trailing `break;`, route
/// `Ret`'s trailing `return EXPR;` through the shared done-flag/result-slot, and keep
/// `throw` verbatim (it propagates out of the closure/loop/function).
fn to_closure_body(body: &str, done: &str, ret: &str) -> String {
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
    for (k, &label) in div.perm.iter().enumerate().take(N_OPCODES) {
        if spec.usage.is_some_and(|u| !u.opcode(k)) {
            continue;
        }
        if k == 13 {
            handlers.push((
                label,
                format!(
                    "n=C[pc++];a=S.splice(S.length-n,n);f=S.pop();S.push({}(f,a));break;",
                    spec.rc
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
            handlers.push((label, format!("o=S.pop();S.push(o[{}]());break;", spec.sy)));
            continue;
        }
        if k == 35 {
            handlers.push((
                label,
                "n=C[pc++];cl_a=C[pc++];cl_s=C[pc++];cl_p=C[pc++];cl_n=C[pc++];\
cl_u=[];for(j=0;j<cl_n;j++)cl_u.push(C[pc++]);\
S.push(Mk(n,cl_a,cl_s,cl_p,cl_u,L,receiver));break;"
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
cl_u=[];for(j=0;j<cl_n;j++){t=C[pc++];cl_u.push(t===2147483646?receiver:Object.defineProperty({},0,Ref(L,t)));}\
S.push(Reflect.apply(consts[n],null,cl_u));break;"
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
    let sd_decl = format!(
        "function Sd(a){{return String.fromCharCode.apply(null,a.map(function(c){{return c^{ck};}}));}}"
    );
    let used = |opcode| spec.usage.is_none_or(|usage| usage.opcode(opcode));
    // Decoys remain emitted even when real handlers specialize away, so retain
    // the closure helper if a decoy references it.
    let needs_mk = used(35)
        || div.perm[N_OPCODES..]
            .iter()
            .any(|&label| div.decoy_form(label) == 5);
    let mut mk_decl = String::new();
    if used(37) || used(43) {
        mk_decl.push_str("function Copy(target,source,skip){if(source!=null){for(var key of Reflect.ownKeys(Object(source))){if(skip.indexOf(key)>=0)continue;var d=Object.getOwnPropertyDescriptor(source,key);if(d&&d.enumerable)Object.defineProperty(target,key,{value:source[key],writable:true,enumerable:true,configurable:true});}}return target;}");
    }
    if needs_mk || used(36) {
        mk_decl.push_str("function Ref(P,n){var d=Object.getOwnPropertyDescriptor(P,n);return d&&d.get?d:{get:function(){return P[n];},set:function(v){P[n]=v;}};}");
    }
    if used(38) || used(40) {
        mk_decl.push_str("function Lex(n,c){var x,ready=false;B[n]=function(v){x=v;ready=true;};B[n].c=c;Object.defineProperty(L,n,{configurable:true,get:function(){if(!ready)throw ReferenceError('Uninitialized lexical binding');return x;},set:function(v){if(!ready)throw ReferenceError('Uninitialized lexical binding');if(c)throw TypeError('Assignment to constant variable');x=v;}});}");
    }
    if needs_mk {
        mk_decl.push_str(&format!(
            "function Mk(idx,ar,cs,pcnt,sl,PL,prcv){{var up=[],q,clo;for(q=0;q<sl.length;q++)up.push(sl[q]===2147483647?{{get:function(){{return clo;}}}}:Ref(PL,sl[q]));\
clo=ar?((...a)=>{name}({table}[idx][0],{table}[idx][1],a,up,cs,pcnt,prcv,true)):function(){{return {name}({table}[idx][0],{table}[idx][1],arguments,up,cs,pcnt,this,true);}};return clo;}}"
        ));
    }
    let sd_expr = format!(
        "var Sd=function(a){{return String.fromCharCode.apply(null,a.map(function(c){{return c^{ck};}}));}};"
    );
    let helpers = match variant {
        0 => format!("{sd_decl}{mk_decl}"),
        1 => format!("{mk_decl}{sd_decl}"),
        _ => format!("{sd_expr}{mk_decl}"),
    };
    let code_decode = crate::serialize::code_decode_js(key);
    format!(
        "{helpers}\
{code_decode}\
if(!consts.d){{for(i=0;i<consts.length;i++){{t=consts[i];\
if(Array.isArray(t))consts[i]=Sd(t);\
else if(t&&t.q){{a=t.q.map(function(e){{return Array.isArray(e)?Sd(e):undefined;}});a.raw=Object.freeze(t.w.map(Sd));consts[i]=Object.freeze(a);}}}}consts.d=1;}}\
for(i=0;i<args.length&&i<pcount;i++)L[i]=args[i];\
for(i=0;i<caps.length;i++){{if(live)Object.defineProperty(L,capStart+i,caps[i]);else L[capStart+i]=caps[i];}}"
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

    if spec.needs_eh {
        let inner = loop_frame(variant, &format!("switch(C[pc++]){{{cases}}}"));
        let outer_body =
            format!("try{{{inner}}}catch(e){{comp={{t:1,v:e,f:0}};if(!unwind(0))throw e;}}");
        let outer = loop_frame(variant, &outer_body);
        format!(
            "function {name}(code,consts,args,caps,capStart,pcount,receiver,live){{{strict}\
var L=[],B=[],S=[],C=[],pc=0,i,a,b,o,k,v,f,n,t,obj,op,uop,base,r,it,m,j,cl_a,cl_s,cl_p,cl_n,cl_u,H=[],P=[],comp={{t:0,v:0,f:0}},NORMAL={{t:0,v:0,f:0}},h;\
{decode_init}\
function unwind(floor){{while(H.length>floor){{h=H.pop();S.length=h[2];P.length=h[3];\
if(comp.t===1&&h[0]>=0){{S.push(comp.v);pc=h[0];comp=NORMAL;return true;}}\
if(h[1]>=0){{pc=h[1];return true;}}}}return false;}}\
{outer}\
}}"
        )
    } else if div.dispatch_shape(spec.needs_eh) == 1 {
        // Stage-1a: array-of-closures dispatch (lean-only).
        let mut build = String::from("var F=[],dn=0,rv;");
        for (label, body) in &handlers {
            let cb = to_closure_body(body, "dn", "rv");
            build.push_str(&format!("F[{label}]=function(){{{cb}}};"));
        }
        let lean = loop_frame(variant, "F[C[pc++]]();if(dn)return rv;");
        format!(
            "function {name}(code,consts,args,caps,capStart,pcount,receiver,live){{{strict}\
var L=[],B=[],S=[],C=[],pc=0,i,a,b,o,k,v,f,n,t,obj,op,uop,base,j,cl_a,cl_s,cl_p,cl_n,cl_u;\
{decode_init}\
{build}\
{lean}\
}}"
        )
    } else {
        let lean = loop_frame(variant, &format!("switch(C[pc++]){{{cases}}}"));
        format!(
            "function {name}(code,consts,args,caps,capStart,pcount,receiver,live){{{strict}\
var L=[],B=[],S=[],C=[],pc=0,i,a,b,o,k,v,f,n,t,obj,op,uop,base,j,cl_a,cl_s,cl_p,cl_n,cl_u;\
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
    stmt.ok_or_else(|| {
        mangler_core::Error::transform("vm-emit", "interpreter produced no statement")
    })
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
            !src.contains("catch") && !src.contains("H=[]"),
            "lean has no EH:\n{src}"
        );
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
            src.contains("catch") && src.contains("H=[]"),
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

    /// §5a byte-identity proof at the emitter level: a sloppy spec (`is_strict:false`)
    /// must produce EXACTLY the source the pre-strict emitter produced — the only
    /// difference a strict spec introduces is a leading `"use strict";` directive.
    #[test]
    fn strict_spec_only_prepends_use_strict_directive() {
        for seed in [1u64, 7, 42, 999] {
            let div = VmDiversity::draw(&mut mangler_core::Rng::for_pass(seed, "vm"));
            for needs_eh in [false, true] {
                let sloppy = interpreter_src(&spec_for(&div, needs_eh));
                let strict = interpreter_src(&spec_for_strict(&div, needs_eh));
                // The strict body is the sloppy body with `"use strict";` inserted
                // right after the function's opening brace.
                let brace = sloppy.find('{').expect("fn has a body brace");
                let expected = format!(
                    "{}{{\"use strict\";{}",
                    &sloppy[..brace],
                    &sloppy[brace + 1..]
                );
                assert_eq!(strict, expected, "seed {seed} eh {needs_eh}");
                assert!(
                    strict.contains("\"use strict\""),
                    "strict carries the directive"
                );
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
}
