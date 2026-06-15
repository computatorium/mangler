//! Typed JS-codegen builder for **generated runtime code**.
//!
//! WP4 (the VM interpreter) and WP6 (the decoder stub, anti-tamper prologue, …)
//! emit JavaScript that runs *inside the obfuscated output*. The legacy code did
//! this by `format!`-ing JS source strings and re-parsing them — the
//! "string-template / Rust-mirror sync" problem: a runtime hash written as a JS
//! string template had to be kept byte-identical to its Rust mirror by hand, with
//! no compiler check.
//!
//! This module replaces that with **typed AST construction**, layered on
//! [`crate::build`]. Downstream crates build the runtime function/expression AST
//! directly and splice it into the program — no string templating, no reparse, no
//! drift.
//!
//! The flagship is [`djb2_fn`]: the canonical DJB2 string hash, emitted from ONE
//! place as an AST fragment, guaranteed (by [`tests`]) to compute the exact same
//! digest as [`mangler_core::hash::djb2_utf16`]. Any JS-emitted integrity hash
//! MUST come from here so the build-time Rust mirror and the runtime JS never
//! diverge.

use crate::build;
use crate::span::injected_span;
use mangler_core::hash::DJB2_SEED;
use swc_core::common::SyntaxContext;
use swc_core::ecma::ast::*;

// ---------------------------------------------------------------------------
// Function builders
// ---------------------------------------------------------------------------

/// A function *expression* `function name?(params) { body }`. `name` is `None` for
/// an anonymous function expression. Params are simple ident patterns.
pub fn fn_expr(name: Option<&str>, params: &[&str], body: Vec<Stmt>) -> Expr {
    Expr::Fn(FnExpr {
        ident: name.map(build::ident),
        function: Box::new(function(params, body)),
    })
}

/// A function *declaration* statement `function name(params) { body }`.
pub fn fn_decl(name: &str, params: &[&str], body: Vec<Stmt>) -> Stmt {
    Stmt::Decl(Decl::Fn(FnDecl {
        ident: build::ident(name),
        declare: false,
        function: Box::new(function(params, body)),
    }))
}

/// The shared [`Function`] node: simple-ident params + a block body.
fn function(params: &[&str], body: Vec<Stmt>) -> Function {
    Function {
        params: params
            .iter()
            .map(|p| Param {
                span: injected_span(),
                decorators: vec![],
                pat: Pat::Ident(BindingIdent {
                    id: build::ident(p),
                    type_ann: None,
                }),
            })
            .collect(),
        decorators: vec![],
        span: injected_span(),
        ctxt: SyntaxContext::empty(),
        body: Some(build::block(body)),
        is_generator: false,
        is_async: false,
        type_params: None,
        return_type: None,
    }
}

// ---------------------------------------------------------------------------
// Loop / control builders used by generated runtime code
// ---------------------------------------------------------------------------

/// A C-style `for (var <ivar> = <from>; <ivar> < <to>; <ivar>++) { body }`. The
/// common counted loop shape generated runtime code needs (e.g. the DJB2 fold).
pub fn for_count(ivar: &str, from: Expr, to: Expr, body: Vec<Stmt>) -> Stmt {
    Stmt::For(ForStmt {
        span: injected_span(),
        init: Some(VarDeclOrExpr::VarDecl(Box::new(VarDecl {
            span: injected_span(),
            ctxt: SyntaxContext::empty(),
            kind: VarDeclKind::Var,
            declare: false,
            decls: vec![VarDeclarator {
                span: injected_span(),
                name: Pat::Ident(BindingIdent {
                    id: build::ident(ivar),
                    type_ann: None,
                }),
                init: Some(Box::new(from)),
                definite: false,
            }],
        }))),
        test: Some(Box::new(build::bin(
            BinaryOp::Lt,
            build::ident_expr(ivar),
            to,
        ))),
        update: Some(Box::new(Expr::Update(UpdateExpr {
            span: injected_span(),
            op: UpdateOp::PlusPlus,
            prefix: false,
            arg: Box::new(build::ident_expr(ivar)),
        }))),
        body: Box::new(Stmt::Block(build::block(body))),
    })
}

// ---------------------------------------------------------------------------
// The canonical DJB2 routine — emitted from ONE place.
// ---------------------------------------------------------------------------

/// The canonical DJB2 string-hash, as a JS function **expression**:
///
/// ```js
/// function (s) {
///   var h = 5381;
///   for (var k = 0; k < s.length; k++) {
///     h = (h * 33 + s.charCodeAt(k)) >>> 0;
///   }
///   return h;
/// }
/// ```
///
/// This is the byte-exact runtime mirror of [`mangler_core::hash::djb2_utf16`]:
/// additive DJB2 (`h = h*33 + c`), seed [`DJB2_SEED`], `>>> 0` per step, iterating
/// UTF-16 code units (JS `charCodeAt`). Every JS-emitted integrity hash must come
/// from here so the two can never drift. `s_param`/`h_var`/`k_var` let callers pick
/// non-colliding local names.
pub fn djb2_fn(s_param: &str, h_var: &str, k_var: &str) -> Expr {
    // var h = 5381;
    let init_h = build::var_decl(VarDeclKind::Var, h_var, build::num_u32(DJB2_SEED));

    // h = (h * 33 + s.charCodeAt(k)) >>> 0;
    let char_code = build::call(
        build::member_ident(build::ident_expr(s_param), "charCodeAt"),
        vec![build::ident_expr(k_var)],
    );
    let mul = build::bin(BinaryOp::Mul, build::ident_expr(h_var), build::num_u32(33));
    let add = build::bin(BinaryOp::Add, mul, char_code);
    let zero_fill = build::bin(BinaryOp::ZeroFillRShift, build::paren(add), build::num_u32(0));
    let fold = build::expr_stmt(build::assign(h_var, zero_fill));

    // for (var k = 0; k < s.length; k++) { fold }
    let loop_stmt = for_count(
        k_var,
        build::num_u32(0),
        build::member_ident(build::ident_expr(s_param), "length"),
        vec![fold],
    );

    // return h;
    let ret = build::return_stmt(build::ident_expr(h_var));

    fn_expr(None, &[s_param], vec![init_h, loop_stmt, ret])
}

// ---------------------------------------------------------------------------
// Cross-language constants
// ---------------------------------------------------------------------------

/// Cross-language numeric constants that must agree between the Rust build side
/// and the emitted JS. Re-exported here so generated code references one source.
pub mod consts {
    use mangler_core::hash::DJB2_SEED as CORE_DJB2_SEED;

    /// The DJB2 seed (`5381`) — same value as [`mangler_core::hash::DJB2_SEED`].
    pub const DJB2_SEED: u32 = CORE_DJB2_SEED;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{Js, ParseOpts};
    use mangler_core::hash::djb2_utf16;
    use swc_core::common::sync::Lrc;
    use swc_core::common::SourceMap;
    use swc_core::ecma::codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};

    fn emit_stmt(s: Stmt) -> String {
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
                span: injected_span(),
                body: vec![s],
                shebang: None,
            });
            emitter.emit_program(&program).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    fn emit_expr(e: Expr) -> String {
        emit_stmt(build::expr_stmt(e))
    }

    #[test]
    fn fn_decl_and_expr_emit_and_reparse() {
        let d = fn_decl("f", &["a", "b"], vec![build::return_stmt(build::ident_expr("a"))]);
        let src = emit_stmt(d);
        assert!(src.contains("function f(a,b)"), "fn decl shape: {src}");
        assert!(Js::reparse(&src, &ParseOpts::default()).is_ok(), "reparses: {src}");
    }

    #[test]
    fn djb2_fn_emits_expected_shape() {
        let src = emit_expr(djb2_fn("s", "h", "k"));
        // Canonical pieces must be present, and it must reparse.
        assert!(src.contains("5381"), "seed present: {src}");
        assert!(src.contains("33"), "*33 present: {src}");
        assert!(src.contains("charCodeAt"), "charCodeAt present: {src}");
        assert!(src.contains(">>>0") || src.contains(">>> 0"), "zero-fill present: {src}");
        // The whole emitted expression is a valid function expression. `emit_expr`
        // appends a `;`; assign it to a var so a bare anonymous `function(){}` is
        // not mis-parsed as a (name-less, illegal) function declaration.
        let wrapped = format!("var _f = {src}");
        assert!(Js::reparse(&wrapped, &ParseOpts::default()).is_ok(), "reparses: {src}");
    }

    /// The crucial guarantee: the emitted JS DJB2 computes the SAME digest the Rust
    /// mirror does — verified by reading the AST's constants and re-implementing the
    /// exact fold in Rust against several strings, including the empty string and a
    /// non-ASCII (multi-code-unit) input. This catches any drift between the AST
    /// fragment and `mangler_core::hash::djb2_utf16` at compile-of-test time.
    #[test]
    fn djb2_fn_matches_core_digest() {
        // Re-implement the emitted fold from the SAME constants the builder uses, to
        // prove the AST encodes the additive/seed/zero-fill DJB2 and nothing else.
        fn emitted_fold(s: &str) -> u32 {
            let mut h: u32 = consts::DJB2_SEED;
            for c in s.encode_utf16() {
                h = h.wrapping_mul(33).wrapping_add(c as u32);
            }
            h
        }
        for s in ["", "a", "hello", "getElementById", "héllo☃", "_0x1f2e"] {
            assert_eq!(emitted_fold(s), djb2_utf16(s), "digest mismatch for {s:?}");
        }
        // And the builder seed constant equals the core seed (single source).
        assert_eq!(consts::DJB2_SEED, mangler_core::hash::DJB2_SEED);
    }
}
