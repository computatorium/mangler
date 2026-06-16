//! End-to-end behavioral validation: compile real function bodies through the full
//! crate pipeline (compile → TableBuilder → render → execute) and prove the
//! virtualized function behaves IDENTICALLY to the original, via `mangler-testkit`'s
//! rquickjs eval-and-compare and the deterministic VM fuzzer.
//!
//! This is the most important guard in the crate: a VM miscompile is the worst
//! possible bug, so the safety net is "run the compiled program and compare its
//! observable result to the original under `Object.is` semantics".

use std::collections::HashSet;

use mangler_core::Rng;
use mangler_testkit::eval::{assert_behaviorally_equal_with, eval_same_value_with, CaptureMode};
use mangler_vm::diversity::VmDiversity;
use mangler_vm::table::{TableBuilder, VmNames};
use swc_core::common::sync::Lrc;
use swc_core::common::{FileName, SourceMap};
use swc_core::ecma::ast::*;
use swc_core::ecma::codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};
use swc_core::ecma::parser::{lexer::Lexer, EsSyntax, Parser, StringInput, Syntax};

/// Parse a function-expression source into `(name, params, body)`. `name` is the
/// function's own identifier (a named function expression like `function fac(){…}`),
/// or `None` for an anonymous one.
fn parse_fn_named(src: &str) -> (Option<String>, Vec<Param>, BlockStmt) {
    let cm: Lrc<SourceMap> = Default::default();
    let wrapped = format!("var __f = ({src});");
    let fm = cm.new_source_file(Lrc::new(FileName::Custom("t.js".into())), wrapped);
    let lexer = Lexer::new(Syntax::Es(EsSyntax::default()), EsVersion::EsNext, StringInput::from(&*fm), None);
    let mut parser = Parser::new_from(lexer);
    let program = parser.parse_program().unwrap();
    let stmt = match program {
        Program::Script(s) => s.body.into_iter().next().unwrap(),
        Program::Module(m) => match m.body.into_iter().next().unwrap() {
            ModuleItem::Stmt(s) => s,
            _ => panic!("stmt"),
        },
    };
    let init = match stmt {
        Stmt::Decl(Decl::Var(v)) => *v.decls.into_iter().next().unwrap().init.unwrap(),
        _ => panic!("var"),
    };
    let fe = match init {
        Expr::Paren(p) => match *p.expr {
            Expr::Fn(fe) => fe,
            _ => panic!("fn"),
        },
        Expr::Fn(fe) => fe,
        _ => panic!("fn"),
    };
    let name = fe.ident.map(|i| i.sym.to_string());
    (name, fe.function.params, fe.function.body.unwrap())
}

/// Parse a function-expression source into `(params, body)`.
fn parse_fn(src: &str) -> (Vec<Param>, BlockStmt) {
    let cm: Lrc<SourceMap> = Default::default();
    let wrapped = format!("var __f = ({src});");
    let fm = cm.new_source_file(Lrc::new(FileName::Custom("t.js".into())), wrapped);
    let lexer = Lexer::new(Syntax::Es(EsSyntax::default()), EsVersion::EsNext, StringInput::from(&*fm), None);
    let mut parser = Parser::new_from(lexer);
    let program = parser.parse_program().unwrap();
    let stmt = match program {
        Program::Script(s) => s.body.into_iter().next().unwrap(),
        Program::Module(m) => match m.body.into_iter().next().unwrap() {
            ModuleItem::Stmt(s) => s,
            _ => panic!("stmt"),
        },
    };
    let init = match stmt {
        Stmt::Decl(Decl::Var(v)) => *v.decls.into_iter().next().unwrap().init.unwrap(),
        _ => panic!("var"),
    };
    let func = match init {
        Expr::Paren(p) => match *p.expr {
            Expr::Fn(fe) => fe.function,
            _ => panic!("fn"),
        },
        Expr::Fn(fe) => fe.function,
        _ => panic!("fn"),
    };
    (func.params, func.body.unwrap())
}

/// Render a slice of statements to minified JS source.
fn render(stmts: Vec<Stmt>) -> String {
    let cm: Lrc<SourceMap> = Default::default();
    let mut buf = Vec::new();
    {
        let wr = JsWriter::new(cm.clone(), "", &mut buf, None);
        let mut emitter = Emitter {
            cfg: CodegenConfig::default().with_minify(true),
            cm,
            comments: None,
            wr,
        };
        let program = Program::Script(Script {
            span: swc_core::common::DUMMY_SP,
            body: stmts,
            shebang: None,
        });
        emitter.emit_program(&program).unwrap();
    }
    String::from_utf8(buf).unwrap()
}

/// Build a complete program that defines `f` as a VM-virtualized thunk for the given
/// function-expression source, drawing diversification from `seed`. Returns `None` if
/// the body bails (unsupported construct) — a bail is never a miscompile, the caller
/// just skips that body. `pcount`/`caps` are threaded into the thunk's call.
fn virtualize(src: &str, seed: u64) -> Option<String> {
    let (own_name, params, body) = parse_fn_named(src);
    let compiled = mangler_vm::compile_body(&params, &body).ok()?;

    let div = VmDiversity::draw(&mut Rng::for_pass(seed, "vm"));
    let mut tb = TableBuilder::with_diversity(div);
    let chunk = tb.add(compiled);

    let names = VmNames {
        lean_interp: "Vv".into(),
        eh_interp: "Dd".into(),
        lean_interp_strict: "Vs".into(),
        eh_interp_strict: "Ds".into(),
        table: "Tt".into(),
        rc: "rcc".into(),
        sy: "syy".into(),
    };
    let vt = tb.finish(&names).expect("finish");

    // The thunk replaces the BODY of the (possibly named) function, keeping its own
    // name `f` (or the source's name) so a named-fn-expr self-reference capture
    // resolves to the thunk itself — exactly as the real pass replaces a function's
    // body in place. `function f(<params>){ return <interp>(T[i][0],T[i][1],arguments,[caps],capStart,pcount,this); }`
    let fn_name = own_name.as_deref().unwrap_or("f");
    let interp = if chunk.needs_eh { &names.eh_interp } else { &names.lean_interp };
    let caps = format!("[{}]", chunk.captures.join(","));
    let param_src: Vec<String> = (0..chunk.pcount).map(|i| format!("p{i}")).collect();
    let thunk = format!(
        "function {fn_name}({}){{return {interp}({}[{}][0],{}[{}][1],arguments,{caps},{},{},this);}}\nvar f={fn_name};",
        param_src.join(","),
        names.table,
        chunk.index,
        names.table,
        chunk.index,
        chunk.cap_start,
        chunk.pcount,
    );

    let prologue = render(vt.prologue);
    Some(format!("{prologue}\n{thunk}"))
}

/// A program that calls the ORIGINAL function with the given args and sinks a JSON of
/// the result to `globalThis.__out`.
fn original_program(src: &str, call_args: &str) -> String {
    format!("var f=({src});globalThis.__out=JSON.stringify(f({call_args}));")
}

/// A program that calls the VIRTUALIZED function the same way.
fn virtualized_program(src: &str, call_args: &str, seed: u64) -> Option<String> {
    let v = virtualize(src, seed)?;
    Some(format!("{v}\nglobalThis.__out=JSON.stringify(f({call_args}));"))
}

/// Assert the virtualized function produces the same result as the original for the
/// given call args. Skips (does not fail) if the body bails.
fn assert_vm_equiv(src: &str, call_args: &str) {
    for seed in [1u64, 7, 42] {
        let Some(v) = virtualized_program(src, call_args, seed) else {
            return; // bailed: not a miscompile, nothing to compare
        };
        let orig = original_program(src, call_args);
        assert_behaviorally_equal_with(&orig, &v, &CaptureMode::sink());
    }
}

#[test]
fn arithmetic() {
    assert_vm_equiv("function(a,b){ return a + b * 2 - (a % b); }", "7,3");
    assert_vm_equiv("function(a,b){ return (a & b) | (a ^ b); }", "12,10");
    assert_vm_equiv("function(a,b){ return a >>> b; }", "4294967295,2");
    assert_vm_equiv("function(a){ return -a + ~a + !a; }", "5");
    assert_vm_equiv("function(a,b){ return a ** b; }", "2,10");
}

#[test]
fn comparisons_and_logic() {
    assert_vm_equiv("function(a,b){ return a < b ? a : b; }", "3,9");
    assert_vm_equiv("function(a,b){ return (a === b) + (a !== b); }", "3,3");
    assert_vm_equiv("function(a,b){ return a && b || a; }", "0,5");
}

#[test]
fn control_flow() {
    assert_vm_equiv("function(n){ if(n>0){return 'pos';}else if(n<0){return 'neg';}else{return 'zero';} }", "-4");
    assert_vm_equiv("function(n){ var s=0; for(var i=0;i<n;i++){ s+=i; } return s; }", "10");
    assert_vm_equiv("function(n){ var s=0,i=0; while(i<n){ s=s+i*i; i++; } return s; }", "6");
    assert_vm_equiv("function(n){ var s=0; do { s++; n--; } while(n>0); return s; }", "5");
}

#[test]
fn loops_with_break_continue() {
    assert_vm_equiv("function(n){ var s=0; for(var i=0;i<n;i++){ if(i===3)continue; if(i===7)break; s+=i; } return s; }", "10");
    assert_vm_equiv("function(n){ outer: for(var i=0;i<n;i++){ for(var j=0;j<n;j++){ if(i*j>6)break outer; } } return i; }", "5");
}

#[test]
fn switch_stmt() {
    assert_vm_equiv("function(x){ switch(x){ case 1: return 'a'; case 2: return 'b'; default: return 'z'; } }", "2");
    assert_vm_equiv("function(x){ var r=''; switch(x){ case 1: r+='1'; case 2: r+='2'; break; case 3: r+='3'; } return r; }", "1");
}

#[test]
fn try_catch_finally() {
    assert_vm_equiv("function(x){ try { if(x<0) throw 'neg'; return 'ok'; } catch(e){ return 'caught:'+e; } finally { } }", "-1");
    assert_vm_equiv("function(x){ var r=''; try { r+='t'; throw 1; } catch(e){ r+='c'; } finally { r+='f'; } return r; }", "0");
}

#[test]
fn arrays_objects_props() {
    assert_vm_equiv("function(a,b){ var arr=[a,b,a+b]; return arr[2]; }", "3,4");
    assert_vm_equiv("function(a){ var o={x:a,y:a*2}; return o.x + o.y; }", "5");
    assert_vm_equiv("function(a){ var o={}; o['k']=a; return o.k; }", "9");
}

#[test]
fn string_ops_and_template() {
    assert_vm_equiv("function(a,b){ return 'sum=' + (a+b); }", "2,3");
    assert_vm_equiv("function(a){ return `val:${a}:${a*2}`; }", "5");
}

#[test]
fn recursion_via_named_fn_expr() {
    // Named function expression self-reference (SELF_UPVALUE path).
    assert_vm_equiv("function fac(n){ return n<=1 ? 1 : n*fac(n-1); }", "6");
    assert_vm_equiv("function fib(n){ return n<2 ? n : fib(n-1)+fib(n-2); }", "10");
}

#[test]
fn closures_capture() {
    assert_vm_equiv("function(a){ var add=function(b){ return a+b; }; return add(10); }", "5");
    assert_vm_equiv("function(n){ var acc=0; var f=function(){ acc+=1; return acc; }; f(); f(); return f(); }", "0");
}

#[test]
fn for_of_destructure() {
    assert_vm_equiv("function(arr){ var s=0; for(var x of arr){ s+=x; } return s; }", "[1,2,3,4]");
    assert_vm_equiv("function(a,b){ var [x,y]=[a,b]; return x*10+y; }", "3,7");
    assert_vm_equiv("function(o){ var {p,q}=o; return p+q; }", "{p:2,q:5}");
}

#[test]
fn diversity_variants_all_correct() {
    // The same body under several seeds (different perms/skeleton/dispatch/MBA) must
    // all be behaviorally identical to the original.
    let src = "function(a,b){ var s=0; for(var i=0;i<b;i++){ s += (a & i) | (a ^ i); } return s; }";
    let orig = original_program(src, "13,8");
    for seed in 0u64..24 {
        let Some(v) = virtualized_program(src, "13,8", seed) else { continue };
        let r = eval_same_value_with(&orig, &v, &CaptureMode::sink());
        assert!(r.equal, "seed {seed} diverged: {}\n{v}", r.reason);
    }
}

#[test]
fn deterministic_same_seed_same_bytes() {
    // The whole pipeline is deterministic: same seed → byte-identical output.
    let src = "function(a,b){ return a*b+1; }";
    let v1 = virtualize(src, 99).unwrap();
    let v2 = virtualize(src, 99).unwrap();
    assert_eq!(v1, v2, "same seed must produce byte-identical VM output");
}

/// Transform a complete fuzz program: find `function f(a,b,c){…}`, virtualize its
/// body, and re-emit the program with the VM prologue prepended and the function's
/// body replaced by a thunk. If the body bails, return the program UNCHANGED (a bail
/// is never a miscompile — the function simply stays un-virtualized). This mirrors
/// what the real virtualize pass does in-place.
fn fuzz_virtualize(program: &str, seed: u64) -> String {
    fuzz_virtualize_mode(program, seed, false)
}

/// As [`fuzz_virtualize`] but registers `f` STRICT and emits a strict thunk — the
/// §5a route exercised by the strict fuzz net.
fn fuzz_virtualize_strict(program: &str, seed: u64) -> String {
    fuzz_virtualize_mode(program, seed, true)
}

fn fuzz_virtualize_mode(program: &str, seed: u64, strict: bool) -> String {
    use mangler_core::Language;
    use mangler_jsast::lang::{Js, ParseOpts};
    use swc_core::ecma::visit::{VisitMut, VisitMutWith};

    let mut ast = match Js.parse(program, &ParseOpts::default()) {
        Ok(a) => a,
        Err(_) => return program.to_string(),
    };

    // Find the `f` function, compile its body, build the shared table, and replace
    // the body with a thunk. We collect the prologue out of the visitor.
    struct V {
        seed: u64,
        strict: bool,
        prologue: Option<Vec<Stmt>>,
    }
    impl VisitMut for V {
        fn visit_mut_fn_decl(&mut self, n: &mut FnDecl) {
            if n.ident.sym.as_ref() != "f" || self.prologue.is_some() {
                return;
            }
            let Some(body) = n.function.body.clone() else { return };
            let params = n.function.params.clone();
            let Ok(compiled) = mangler_vm::compile_body(&params, &body) else { return };

            let div = VmDiversity::draw(&mut Rng::for_pass(self.seed, "vm"));
            let mut tb = TableBuilder::with_diversity(div);
            let chunk = tb.add_strict(compiled, self.strict);
            let names = VmNames {
                lean_interp: "Vv".into(),
                eh_interp: "Dd".into(),
                lean_interp_strict: "Vs".into(),
                eh_interp_strict: "Ds".into(),
                table: "Tt".into(),
                rc: "rcc".into(),
                sy: "syy".into(),
            };
            let vt = tb.finish(&names).expect("finish");
            let interp = names.interp_for(chunk.needs_eh, chunk.is_strict);
            let caps = format!("[{}]", chunk.captures.join(","));
            let directive = if self.strict { "\"use strict\";" } else { "" };
            let thunk_src = format!(
                "{directive}return {interp}({}[{}][0],{}[{}][1],arguments,{caps},{},{},this);",
                names.table, chunk.index, names.table, chunk.index, chunk.cap_start, chunk.pcount,
            );
            // Parse the thunk body and install it.
            let wrapped = Js.parse(&format!("function _(){{{thunk_src}}}"), &ParseOpts::default()).unwrap();
            let new_body = match wrapped.into_program() {
                Program::Script(s) => match s.body.into_iter().next().unwrap() {
                    Stmt::Decl(Decl::Fn(fd)) => fd.function.body.unwrap(),
                    _ => return,
                },
                _ => return,
            };
            n.function.body = Some(new_body);
            self.prologue = Some(vt.prologue);
        }
        fn visit_mut_arrow_expr(&mut self, _: &mut ArrowExpr) {}
    }

    let mut v = V { seed, strict, prologue: None };
    ast.program_mut().visit_mut_with(&mut v);
    let Some(prologue) = v.prologue else {
        return program.to_string(); // f not found or bailed
    };

    // Prepend the prologue at module scope.
    if let Program::Script(s) = ast.program_mut() {
        let mut new_body = prologue;
        new_body.append(&mut s.body);
        s.body = new_body;
    }
    Js.print(&ast)
}

#[test]
fn fuzz_deterministic_bodies_round_trip() {
    // The deterministic VM fuzzer generates random function bodies over the
    // virtualizer-eligible construct set; every one must behave identically once
    // virtualized (or be left unchanged on a bail). A fixed VM seed keeps the
    // transform deterministic per program.
    mangler_testkit::fuzz::assert_fuzz_transform(400, 0xF0F0_1234, |program| {
        fuzz_virtualize(program, 0xABCD)
    });
}

#[test]
fn fuzz_across_vm_seeds() {
    // Sweep several VM diversification seeds over a smaller program count, so the
    // perms / skeleton / dispatch / MBA variants are all exercised against the
    // fuzzer's bodies.
    for vm_seed in [1u64, 2, 5, 13, 21] {
        mangler_testkit::fuzz::assert_fuzz_transform(120, 0x1234_0000 + vm_seed, move |program| {
            fuzz_virtualize(program, vm_seed)
        });
    }
}

// ---------------------------------------------------------------------------
// §5a strict-mode coverage (Phase 0b).
// ---------------------------------------------------------------------------

/// Build a program that defines `f` as a VM thunk for an anonymous function-expression
/// `src`, virtualized under the given strictness. When `is_strict`, the chunk is
/// registered strict (routing to a strict interpreter variant) and the thunk carries a
/// leading `"use strict"` so a plain call forwards the un-coerced (`undefined`)
/// receiver. Returns `None` on a compile bail.
fn virtualize_with_strict(src: &str, seed: u64, is_strict: bool) -> Option<String> {
    let (params, body) = parse_fn(src);
    let compiled = mangler_vm::compile_body(&params, &body).ok()?;

    let div = VmDiversity::draw(&mut Rng::for_pass(seed, "vm"));
    let mut tb = TableBuilder::with_diversity(div);
    let chunk = tb.add_strict(compiled, is_strict);

    let names = VmNames {
        lean_interp: "Vv".into(),
        eh_interp: "Dd".into(),
        lean_interp_strict: "Vs".into(),
        eh_interp_strict: "Ds".into(),
        table: "Tt".into(),
        rc: "rcc".into(),
        sy: "syy".into(),
    };
    let vt = tb.finish(&names).expect("finish");
    let interp = names.interp_for(chunk.needs_eh, chunk.is_strict);
    let caps = format!("[{}]", chunk.captures.join(","));
    let param_src: Vec<String> = (0..chunk.pcount).map(|i| format!("p{i}")).collect();
    let directive = if is_strict { "\"use strict\";" } else { "" };
    let thunk = format!(
        "function f({}){{{directive}return {interp}({}[{}][0],{}[{}][1],arguments,{caps},{},{},this);}}",
        param_src.join(","),
        names.table,
        chunk.index,
        names.table,
        chunk.index,
        chunk.cap_start,
        chunk.pcount,
    );
    let prologue = render(vt.prologue);
    Some(format!("{prologue}\n{thunk}"))
}

/// §5a case 1: a STRICT virtualized function called plainly (`f()`) must see
/// `this === undefined`, NOT the sloppy `globalThis`. The strict thunk governs this.
#[test]
fn strict_plain_call_this_is_undefined() {
    let src = "function(){ return typeof this; }";
    for seed in [1u64, 7, 42] {
        let v = virtualize_with_strict(src, seed, true).expect("compiles");
        let prog = format!("{v}\nglobalThis.__out=JSON.stringify(f());");
        // The strict source, plainly called, yields `this === undefined`.
        let orig = "var f=(function(){ \"use strict\"; return typeof this; });globalThis.__out=JSON.stringify(f());";
        assert_behaviorally_equal_with(orig, &prog, &CaptureMode::sink());
    }
}

/// §5a case 1 (contrast): a SLOPPY virtualized function called plainly coerces `this`
/// to the global object — the existing behavior, preserved.
#[test]
fn sloppy_plain_call_this_is_global() {
    let src = "function(){ return this === globalThis; }";
    for seed in [1u64, 7, 42] {
        let v = virtualize_with_strict(src, seed, false).expect("compiles");
        let prog = format!("{v}\nglobalThis.__out=JSON.stringify(f());");
        let orig = format!("var f=({src});globalThis.__out=JSON.stringify(f());");
        assert_behaviorally_equal_with(&orig, &prog, &CaptureMode::sink());
    }
}

/// §5a case 2: a store to a `writable:false` property THROWS a TypeError under strict,
/// no-ops under sloppy — governed by the interpreter's strictness. Differential vs the
/// real strict / sloppy source.
#[test]
fn strict_store_to_nonwritable_throws() {
    // Store to a frozen property.
    let src = "function(o){ o.x = 9; return o.x; }";
    let call = "Object.freeze({x:1})";
    for seed in [1u64, 7, 42] {
        // STRICT: both the strict source and the strict-virtualized version throw.
        let v = virtualize_with_strict(src, seed, true).expect("compiles");
        let prog = format!("{v}\nglobalThis.__out=JSON.stringify((function(){{try{{return f({call});}}catch(e){{return 'THROW:'+e.constructor.name;}}}})());");
        let orig = format!(
            "var f=(function(o){{\"use strict\"; o.x = 9; return o.x; }});globalThis.__out=JSON.stringify((function(){{try{{return f({call});}}catch(e){{return 'THROW:'+e.constructor.name;}}}})());"
        );
        assert_behaviorally_equal_with(&orig, &prog, &CaptureMode::sink());

        // SLOPPY: both silently no-op (the frozen value is returned, no throw).
        let v = virtualize_with_strict(src, seed, false).expect("compiles");
        let prog = format!("{v}\nglobalThis.__out=JSON.stringify((function(){{try{{return f({call});}}catch(e){{return 'THROW:'+e.constructor.name;}}}})());");
        let orig = format!(
            "var f=({src});globalThis.__out=JSON.stringify((function(){{try{{return f({call});}}catch(e){{return 'THROW:'+e.constructor.name;}}}})());"
        );
        assert_behaviorally_equal_with(&orig, &prog, &CaptureMode::sink());
    }
}

/// §5a case 2: a store to a getter-only accessor THROWS under strict, no-ops sloppy.
#[test]
fn strict_store_to_getter_only_throws() {
    let src = "function(o){ o.g = 5; return o.g; }";
    let call = "Object.defineProperty({}, 'g', {get:function(){return 7;}, configurable:true})";
    for seed in [1u64, 7, 42] {
        let v = virtualize_with_strict(src, seed, true).expect("compiles");
        let prog = format!("{v}\nglobalThis.__out=JSON.stringify((function(){{try{{return f({call});}}catch(e){{return 'THROW:'+e.constructor.name;}}}})());");
        let orig = format!(
            "var f=(function(o){{\"use strict\"; o.g = 5; return o.g; }});globalThis.__out=JSON.stringify((function(){{try{{return f({call});}}catch(e){{return 'THROW:'+e.constructor.name;}}}})());"
        );
        assert_behaviorally_equal_with(&orig, &prog, &CaptureMode::sink());

        let v = virtualize_with_strict(src, seed, false).expect("compiles");
        let prog = format!("{v}\nglobalThis.__out=JSON.stringify((function(){{try{{return f({call});}}catch(e){{return 'THROW:'+e.constructor.name;}}}})());");
        let orig = format!(
            "var f=({src});globalThis.__out=JSON.stringify((function(){{try{{return f({call});}}catch(e){{return 'THROW:'+e.constructor.name;}}}})());"
        );
        assert_behaviorally_equal_with(&orig, &prog, &CaptureMode::sink());
    }
}

/// §5a case 3: reading `arguments.callee` is an irreducible divergence → the body must
/// BAIL eligibility (stay native), so the VM never miscompiles it.
#[test]
fn arguments_callee_bails() {
    for src in [
        "function(){ return arguments.callee; }",
        "function(){ return arguments.caller; }",
        "function(){ return arguments[\"callee\"]; }",
    ] {
        let (params, body) = parse_fn(src);
        assert!(
            matches!(
                mangler_vm::classify_body(&params, &body),
                mangler_vm::Eligibility::Skip("arguments_callee")
            ),
            "`{src}` must bail arguments_callee"
        );
    }
    // Plain `arguments` use (no callee/caller) stays eligible.
    let (params, body) = parse_fn("function(){ return arguments.length; }");
    assert!(matches!(
        mangler_vm::classify_body(&params, &body),
        mangler_vm::Eligibility::Eligible
    ));
}

/// Determinism: same seed ⇒ byte-identical strict output (incl. the strict thunk +
/// strict interpreter variant).
#[test]
fn strict_deterministic_same_seed_same_bytes() {
    let src = "function(o){ o.x = 1; return typeof this; }";
    let v1 = virtualize_with_strict(src, 99, true).unwrap();
    let v2 = virtualize_with_strict(src, 99, true).unwrap();
    assert_eq!(v1, v2, "same seed must produce byte-identical strict VM output");
}

/// §9.1 strict-divergence fuzz net: the same generated bodies, but each `function f`
/// is `"use strict"` and virtualized via the strict thunk + strict interpreter route.
/// Every one must behave identically to its strict native original — proving the
/// strict variant is never a miscompile across the construct space.
///
/// NOTE (§9.1): the generator emits numeric-return arithmetic bodies, so this net
/// exercises the strict thunk's `this`-forwarding and the strict interpreter's full
/// opcode coverage, but NOT the store-to-frozen/getter divergence (case 2) — those are
/// covered by the targeted hand-written probes (`strict_store_to_nonwritable_throws`,
/// `strict_store_to_getter_only_throws`, `strict_plain_call_this_is_undefined`,
/// `arguments_callee_bails`). A full generator extension that emits frozen-target
/// stores and a sloppy-vs-strict cross-product is left as a follow-up (§9.1).
#[test]
fn fuzz_strict_bodies_round_trip() {
    mangler_testkit::fuzz::assert_fuzz_transform_strict(300, 0xF0F0_5678, |program| {
        fuzz_virtualize_strict(program, 0xABCD)
    });
}

// ---------------------------------------------------------------------------
// Phase 3: native-closure escape hatch (§4)
// ---------------------------------------------------------------------------

/// Virtualize an OUTER (anonymous) function `src` with Phase-3 [`CompileOptions`]
/// (exclude glob + divert-ineligible flag). Returns `(program, rendered_output)`:
/// `program` defines `f` as the virtualized thunk; `rendered_output` is the full
/// prologue+table+thunk source (used to assert the excluded fn's native body appears
/// byte-for-byte). `None` on a compile bail.
fn virtualize_opts(
    src: &str,
    seed: u64,
    exclude: Option<&str>,
    divert_ineligible: bool,
) -> Option<String> {
    use mangler_vm::CompileOptions;
    let (params, body) = parse_fn(src);
    let opts = CompileOptions { exclude, divert_ineligible };
    let compiled = mangler_vm::compile_body_with_opts(&params, &body, opts).ok()?;

    let div = VmDiversity::draw(&mut Rng::for_pass(seed, "vm"));
    let mut tb = TableBuilder::with_diversity(div);
    let chunk = tb.add(compiled);

    let names = VmNames {
        lean_interp: "Vv".into(),
        eh_interp: "Dd".into(),
        lean_interp_strict: "Vs".into(),
        eh_interp_strict: "Ds".into(),
        table: "Tt".into(),
        rc: "rcc".into(),
        sy: "syy".into(),
    };
    let vt = tb.finish(&names).expect("finish");
    let interp = if chunk.needs_eh { &names.eh_interp } else { &names.lean_interp };
    let caps = format!("[{}]", chunk.captures.join(","));
    let param_src: Vec<String> = (0..chunk.pcount).map(|i| format!("p{i}")).collect();
    let thunk = format!(
        "function f({}){{return {interp}({}[{}][0],{}[{}][1],arguments,{caps},{},{},this);}}",
        param_src.join(","),
        names.table,
        chunk.index,
        names.table,
        chunk.index,
        chunk.cap_start,
        chunk.pcount,
    );
    let prologue = render(vt.prologue);
    Some(format!("{prologue}\n{thunk}"))
}

/// Assert the virtualized program (built via `virtualize_opts`) behaves identically
/// to the original for the given call args, across seeds.
fn assert_opts_equiv(src: &str, call_args: &str, exclude: Option<&str>, divert: bool) {
    let orig = original_program(src, call_args);
    for seed in [1u64, 7, 42] {
        let Some(v) = virtualize_opts(src, seed, exclude, divert) else {
            return; // bailed: never a miscompile
        };
        let prog = format!("{v}\nglobalThis.__out=JSON.stringify(f({call_args}));");
        assert_behaviorally_equal_with(&orig, &prog, &CaptureMode::sink());
    }
}

/// An excluded nested `function render` inside a virtualized body stays native: the
/// rendered output carries its body byte-for-byte (a `function render` literal, not a
/// thunk/bytecode), AND it behaves identically.
#[test]
fn excluded_function_decl_stays_native() {
    let src = "function(n){ function render(){ return n * 3; } return render() + 1; }";
    let v = virtualize_opts(src, 7, Some("render"), false).expect("compiles");
    assert!(
        v.contains("function render("),
        "excluded `render` must appear as native source, not bytecode:\n{v}"
    );
    assert_opts_equiv(src, "5", Some("render"), false);
}

/// Binding-name inference (§4.3): `const render = () => …` and `obj.render = …` are
/// excluded by name and stay native.
#[test]
fn excluded_arrow_and_member_binding_stay_native() {
    // `const render = () => …`
    let src = "function(n){ const render = () => n * 2; return render() + render(); }";
    let v = virtualize_opts(src, 7, Some("render"), false).expect("compiles");
    assert!(v.contains("=>"), "excluded arrow must stay native (arrow source):\n{v}");
    assert_opts_equiv(src, "4", Some("render"), false);

    // `obj.render = function(){}`
    let src2 = "function(n){ var obj={}; obj.render = function(){ return n + 7; }; return obj.render(); }";
    let v2 = virtualize_opts(src2, 7, Some("render"), false).expect("compiles");
    assert!(v2.contains("function"), "member-assigned excluded fn stays native:\n{v2}");
    assert_opts_equiv(src2, "10", Some("render"), false);
}

/// An excluded MUTABLE-CAPTURING nested fn, run as a native closure, shares the
/// enclosing local through a cell: the written value is observed after the calls.
#[test]
fn excluded_mutable_capture_native_closure() {
    // `tick` captures-and-mutates `count`; excluded → native closure over the cell.
    let src = "function(){ var count=0; function tick(){ count = count + 1; return count; } \
               tick(); tick(); return tick() + count; }";
    let v = virtualize_opts(src, 7, Some("tick"), false).expect("compiles");
    assert!(v.contains("function tick("), "excluded `tick` stays native:\n{v}");
    // 1,2,3 → returns 3 + count(=3) = 6.
    assert_opts_equiv(src, "", Some("tick"), false);
}

/// An excluded ARROW with lexical `this`, run as a native closure: the enclosing
/// `this` is threaded as an upvalue and the factory closes over it, so `this` is
/// preserved (an arrow ignores the call-time receiver).
#[test]
fn excluded_arrow_lexical_this_native_closure() {
    // The outer fn is called with a receiver; the excluded arrow reads `this.v`.
    let src = "function(){ const get = () => this.v * 2; return get(); }";
    let v = virtualize_opts(src, 7, Some("get"), false).expect("compiles");
    assert!(v.contains("=>"), "excluded arrow stays native:\n{v}");
    // Call with `f.call({v:21})` → 42, for both original and virtualized.
    let orig = format!("var f=({src});globalThis.__out=JSON.stringify(f.call({{v:21}}));");
    let prog = format!("{v}\nglobalThis.__out=JSON.stringify(f.call({{v:21}}));");
    assert_behaviorally_equal_with(&orig, &prog, &CaptureMode::sink());
}

/// Coverage (§4.1): async / generator / `with` / `"use strict"` nested functions
/// divert to native closures under `divert_ineligible`, and the whole program still
/// runs identically (instead of failing the parent compile).
#[test]
fn ineligible_nested_diverts_to_native() {
    // generator
    let g = "function(){ function* gen(){ yield 1; yield 2; } var it=gen(); return it.next().value + it.next().value; }";
    let v = virtualize_opts(g, 7, None, true).expect("compiles with divert");
    assert!(v.contains("function*"), "generator stays native:\n{v}");
    assert_opts_equiv(g, "", None, true);

    // async (observed via a sync wrapper that checks the returned value is a Promise)
    let a = "function(){ async function af(){ return 5; } return typeof af().then; }";
    let v2 = virtualize_opts(a, 7, None, true).expect("compiles with divert");
    assert!(v2.contains("async"), "async stays native:\n{v2}");
    assert_opts_equiv(a, "", None, true);

    // `with` (structurally ineligible) diverts to native.
    let w = "function(o){ function rd(){ with(o){ return x + y; } } return rd(); }";
    let v3 = virtualize_opts(w, 7, None, true).expect("compiles with divert");
    assert!(v3.contains("with("), "with-using fn stays native:\n{v3}");
    assert_opts_equiv(w, "{x:3,y:4}", None, true);

    // own `"use strict"` nested fn diverts to native.
    let s = "function(){ function st(){ \"use strict\"; return typeof this; } return st(); }";
    let v4 = virtualize_opts(s, 7, None, true).expect("compiles with divert");
    assert!(v4.contains("use strict"), "strict nested fn stays native:\n{v4}");
    assert_opts_equiv(s, "", None, true);
}

/// A native closure that reads a module GLOBAL leaves it untouched (resolved at
/// module scope), while threading only the enclosing-frame local it captures.
#[test]
fn native_closure_reads_global_untouched() {
    // `mk` reads global `Math` (untouched) and captures local `base` (threaded).
    let src = "function(base){ function mk(x){ return Math.max(base, x); } return mk(3) + mk(9); }";
    let v = virtualize_opts(src, 7, Some("mk"), false).expect("compiles");
    assert!(v.contains("Math.max"), "global Math left untouched in native body:\n{v}");
    assert_opts_equiv(src, "5", Some("mk"), false);
}

/// Determinism (§4.4): same seed ⇒ byte-identical output, INCLUDING the native
/// factory const and the divert decision.
#[test]
fn native_closure_deterministic_same_seed_same_bytes() {
    let src = "function(n){ function render(){ return n + 1; } return render(); }";
    let v1 = virtualize_opts(src, 99, Some("render"), false).unwrap();
    let v2 = virtualize_opts(src, 99, Some("render"), false).unwrap();
    assert_eq!(v1, v2, "same seed ⇒ byte-identical native-closure output");
}

#[test]
fn bail_leaves_no_output() {
    // `with` is a permanent structural bail; the compiler returns Err, so virtualize
    // returns None (the caller leaves the function un-virtualized — never a
    // miscompile). Confirm classify_body agrees.
    let (params, body) = parse_fn("function(o){ with(o){ return x; } }");
    assert!(matches!(
        mangler_vm::classify_body(&params, &body),
        mangler_vm::Eligibility::Skip(_)
    ));
    let mut used = HashSet::new();
    used.insert("x".to_string());
    // The compiler also bails directly.
    assert!(mangler_vm::compile_body(&params, &body).is_err());
}
