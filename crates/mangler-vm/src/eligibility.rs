//! Structural eligibility for VM lowering.
//!
//! Object environments and mapped arguments are modeled by runtime references;
//! neither requires a syntactic exclusion. Direct eval still requires a retained
//! native evaluation activation. Suspension is lowered by the caller.

use mangler_jsast::analysis::eligibility::{Probe, SkipMethodWrappers, body_classify};
use swc_core::ecma::ast::*;

pub use mangler_jsast::analysis::eligibility::Eligibility;

pub fn classify_body(_params: &[Param], body: &FunctionBody) -> Eligibility {
    body_classify(body, SkipMethodWrappers::InnerOnly, &reject)
}

/// virtualize's structural reject set, evaluated at the shared walker's visit points.
/// D4 removed tagged-template and D5 removed the nested-fn bails, so only the
/// direct eval and suspension constructs remain structural rejects.
fn reject(p: Probe) -> Option<&'static str> {
    match p {
        Probe::With(_) => None,
        Probe::Await(_) => Some("await"),
        Probe::Yield(_) => Some("yield"),
        Probe::DirectEvalCall(_) => None,
        // cfflatten's control-flow constructs are all virtualize-eligible (Tier-1).
        Probe::Try(_)
        | Probe::Switch(_)
        | Probe::DoWhile(_)
        | Probe::ForIn(_)
        | Probe::ForOf(_)
        | Probe::Labeled(_)
        | Probe::For(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Eligibility, classify_body};
    use crate::test_support::{parse_fn_body, parse_fn_with_params};

    /// Classify a body parsed via `parse_fn_body` (no params).
    fn classify(body: &swc_core::ecma::ast::FunctionBody) -> Eligibility {
        classify_body(&[], body)
    }

    #[test]
    fn rejects_remaining_unsupported() {
        // Object scopes, try/catch and for-of are eligible;
        // nested functions/arrows are now eligible too (D5 closures).
        assert!(matches!(
            classify(&parse_fn_body("with(o){ x; }")),
            Eligibility::Eligible
        ));
        assert!(matches!(
            classify(&parse_fn_body("function g(){ return 1; }")),
            Eligibility::Eligible
        ));
        assert!(matches!(
            classify(&parse_fn_body("var h = () => 1;")),
            Eligibility::Eligible
        ));
    }

    #[test]
    fn direct_eval_and_object_scopes_are_eligible() {
        // EvalCall snapshots object scopes and lexical records in source order.
        assert!(matches!(
            classify(&parse_fn_body("with(o){ x; } var r = eval(s);")),
            Eligibility::Eligible
        ));
        // Reversed source order: the direct `eval` now precedes the `with`.
        assert!(matches!(
            classify(&parse_fn_body("var r = eval(s); with(o){ x; }")),
            Eligibility::Eligible
        ));
    }

    #[test]
    fn accepts_try_catch_and_for_of() {
        assert!(matches!(
            classify(&parse_fn_body("try{ f(); }catch(e){ g(e); }")),
            Eligibility::Eligible
        ));
        assert!(matches!(
            classify(&parse_fn_body("for (var x of y) { g(x); }")),
            Eligibility::Eligible
        ));
    }

    #[test]
    fn accepts_loops_and_calls() {
        let body = parse_fn_body("var s=0; for(var i=0;i<n;i++){ s=s+i; } return s;");
        assert!(matches!(classify(&body), Eligibility::Eligible));
    }

    #[test]
    fn arguments_read_only_is_eligible() {
        // Read-only `arguments` uses no longer bail (D2): the VM snapshots a plain
        // array. length / index-read / iterate / `.apply` forwarding are all fine.
        for src in [
            "function(){ return arguments.length; }",
            "function(){ var s=0; for(var i=0;i<arguments.length;i++) s+=arguments[i]; return s; }",
            "function(){ return f.apply(null, arguments); }",
            "function(a,b){ return a + b + arguments.length; }",
        ] {
            let (params, body) = parse_fn_with_params(src);
            assert!(
                matches!(classify_body(&params, &body), Eligibility::Eligible),
                "`{src}` must be eligible"
            );
        }
    }

    #[test]
    fn mapped_arguments_aliases_are_eligible() {
        // MapArgument retains the alias through both native property updates and
        // VM parameter writes, including deletion and descriptor changes.
        for src in [
            "function(a){ a = 9; return arguments[0]; }", // simple param write
            "function(a){ a++; return arguments.length; }", // ++ on param
            "function(a){ a += 1; return arguments[0]; }", // compound on param
            "function(a){ arguments[0] = 7; return a; }", // write arguments elem
            "function(a){ [a] = [5]; return arguments[0]; }", // destructuring target
            "function(a){ arguments[0]++; return a; }",   // ++ on arguments elem
        ] {
            let (params, body) = parse_fn_with_params(src);
            assert!(
                matches!(classify_body(&params, &body), Eligibility::Eligible),
                "`{src}` uses VM mapped argument bindings"
            );
        }
    }

    #[test]
    fn unmapped_and_native_owned_arguments_are_eligible() {
        for source in [
            "function(a){ 'use strict'; a=2; return arguments[0]; }",
            "function(a=1){ a=2; return arguments[0]; }",
            "function(...a){ arguments[0]=2; return a; }",
            "function(){ arguments[0]=2; return arguments[0]; }",
            "function(arguments){ arguments[0]=2; return arguments; }",
        ] {
            let (params, body) = parse_fn_with_params(source);
            assert!(
                matches!(classify_body(&params, &body), Eligibility::Eligible),
                "{source}"
            );
        }
        let (params, body) = parse_fn_with_params("function(a){ a=2; return arguments[0]; }");
        assert!(matches!(
            classify_body(&params, &body),
            Eligibility::Eligible
        ));
        let (params, body) =
            parse_fn_with_params("function(a){ var g=()=>{a=2;}; g(); return arguments[0]; }");
        assert!(matches!(
            classify_body(&params, &body),
            Eligibility::Eligible
        ));
    }

    #[test]
    fn native_arguments_callee_caller_is_eligible() {
        // The native arguments object owns strict throwing accessors and the
        // sloppy callee reference; no reconstruction or special case is needed.
        for src in [
            "function(){ return arguments.callee; }",
            "function(){ return arguments.caller; }",
            "function(){ return arguments['callee']; }",
            "function(){ return arguments[\"caller\"].x; }",
        ] {
            let (params, body) = parse_fn_with_params(src);
            assert!(
                matches!(classify_body(&params, &body), Eligibility::Eligible),
                "`{src}` uses the native arguments object"
            );
        }
        // A nested function's `arguments.callee` does NOT bail the outer body (each
        // function has its own `arguments`, checked when its own chunk compiles).
        let (params, body) = parse_fn_with_params(
            "function(){ var g = function(){ return arguments.callee; }; return g; }",
        );
        assert!(matches!(
            classify_body(&params, &body),
            Eligibility::Eligible
        ));
        // Plain `arguments.length` / element read is still eligible.
        let (params, body) = parse_fn_with_params("function(){ return arguments.length; }");
        assert!(matches!(
            classify_body(&params, &body),
            Eligibility::Eligible
        ));
    }

    #[test]
    fn param_write_without_arguments_is_eligible() {
        // Parameter writes use the same cells as mapped arguments when needed.
        let (params, body) = parse_fn_with_params("function(a){ a = 9; return a; }");
        assert!(matches!(
            classify_body(&params, &body),
            Eligibility::Eligible
        ));
    }

    #[test]
    fn nested_functions_and_arrows_are_eligible() {
        // D5: nested `function`/`arrow` are virtualized as their own chunks, so the
        // OUTER function's structural eligibility no longer rejects them. (The nested
        // body's own unsupported constructs bail when it is compiled, not here.)
        for src in [
            "function inc(){ c++; return c; } return inc;",
            "return x => y => x + y;",
            "var f = function self(n){ return n<=1?1:n*self(n-1); }; return f(5);",
            "function a(){ return 1; } function b(){ return 2; } return a()+b();",
        ] {
            assert!(
                matches!(classify(&parse_fn_body(src)), Eligibility::Eligible),
                "`{src}` must be eligible"
            );
        }
    }

    #[test]
    fn accepts_tagged_template() {
        // D4: tagged templates are now structurally eligible (lowered to a cached
        // frozen template object + call on the tag). Both bare and member tags.
        for src in [
            "var t = tag`x${1}`;",
            "var t = obj.tag`a${b}c`;",
            "var t = tag`only-quasi`;",
        ] {
            assert!(
                matches!(classify(&parse_fn_body(src)), Eligibility::Eligible),
                "`{src}` must be eligible"
            );
        }
    }

    #[test]
    fn accepts_all_tier1_constructs() {
        // Every Tier-1 construct passes the structural gate (compile_body is the
        // authority that bails on residual unsupported shapes).
        for src in [
            "switch (x) { case 1: break; default: g(); }", // switch
            "outer: for (;;) { break outer; }",            // labeled
            "var a = o?.b?.c;",                            // optional chaining
            "delete o.k;",                                 // delete
            "for (var k in o) { g(k); }",                  // for-in
            "for (var v of o) { g(v); }",                  // for-of
            "try { g(); } catch (e) { h(e); } finally { k(); }", // try/catch/finally
            "throw e;",                                    // throw
            "var { a, b: c, ...r } = o;",                  // object destructuring
            "var [p, , q = 1, ...t] = o;",                 // array destructuring
            "var z = f(...a);",                            // call spread
            "var m = { ...o, k: 1 };",                     // object spread
            "var e = ev; e(s);",                           // indirect eval
        ] {
            assert!(
                matches!(classify(&parse_fn_body(src)), Eligibility::Eligible),
                "`{src}` must be eligible"
            );
        }
    }
}
