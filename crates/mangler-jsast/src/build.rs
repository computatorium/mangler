//! The ONE node-builder helper set.
//!
//! Before WP3 each pass carried its own private copies of `ident`, `num_lit`,
//! `paren`, `bin`, `bit_not`, `call`, … — divergent in span policy, in whether
//! operands were parenthesized, in `SyntaxContext`. This module collapses them
//! into a single, documented set so a pass writes `b::call(b::ident("f"), …)`
//! instead of hand-spelling `Expr::Call(CallExpr { span: DUMMY_SP, … })`.
//!
//! ## Conventions
//!
//! * Every node's span comes from [`crate::span::injected_span`] — never a literal
//!   `DUMMY_SP`. Changing injected-span policy is a one-line edit there.
//! * Every injected `Ident` carries the empty [`SyntaxContext`]. Passes that run
//!   **before** the resolver get fresh marks from the single resolver run; passes
//!   that run **after** it and need a specific mark set it explicitly on the
//!   returned ident (the field is public swc state). The empty default is the
//!   correct, conservative choice for a freshly-introduced collision-free name.
//! * Builders that compose sub-expressions (`bin`, `bit_not`, `ternary`'s arms)
//!   do **not** auto-parenthesize their operands. Use [`paren`] explicitly where
//!   precedence matters, or rely on the swc `fixer` (run in codegen) to repair
//!   precedence. This keeps the trees minimal; callers that need a guaranteed
//!   grouping ask for it.
//!
//! The runtime-code-emission builder ([`crate::codegen`]) is layered on top of
//! these primitives; downstream crates (WP4 vm, WP6 passes) build all generated
//! AST through one or the other and never hand-spell node structs again.

use crate::span::injected_span;
use swc_core::common::SyntaxContext;
use swc_core::ecma::ast::*;
use swc_core::ecma::atoms::Atom;

// ---------------------------------------------------------------------------
// Leaves
// ---------------------------------------------------------------------------

/// A bare identifier node `name` with the empty `SyntaxContext`.
///
/// For a renamed source binding in a post-resolver pass, set `.ctxt` on the
/// result; for a freshly-introduced collision-free local the empty context is
/// correct (the resolver assigns marks pre-resolver; idnames leaves unmarked
/// injected names alone post-resolver).
#[inline]
pub fn ident(name: &str) -> Ident {
    Ident::new(Atom::from(name), injected_span(), SyntaxContext::empty())
}

/// An expression that is a bare-name reference: `name`.
#[inline]
pub fn ident_expr(name: &str) -> Expr {
    Expr::Ident(ident(name))
}

/// A string-literal expression `"value"` (with `raw: None`, so codegen
/// re-quotes it canonically).
#[inline]
pub fn str_lit(value: &str) -> Expr {
    Expr::Lit(Lit::Str(Str {
        span: injected_span(),
        value: value.into(),
        raw: None,
    }))
}

/// A numeric-literal expression `value` (no `raw`, so codegen formats it).
#[inline]
pub fn num(value: f64) -> Expr {
    Expr::Lit(Lit::Num(Number {
        span: injected_span(),
        value,
        raw: None,
    }))
}

/// A `u32` numeric literal — the common case for the obfuscation passes, which
/// reason in `>>> 0` u32 space.
#[inline]
pub fn num_u32(value: u32) -> Expr {
    num(value as f64)
}

/// A boolean-literal expression `true` / `false`.
#[inline]
pub fn bool_lit(value: bool) -> Expr {
    Expr::Lit(Lit::Bool(Bool {
        span: injected_span(),
        value,
    }))
}

/// The `null` literal.
#[inline]
pub fn null() -> Expr {
    Expr::Lit(Lit::Null(Null {
        span: injected_span(),
    }))
}

// ---------------------------------------------------------------------------
// Grouping / operators
// ---------------------------------------------------------------------------

/// Wrap `e` in parentheses: `(e)`. Use where precedence matters; otherwise rely
/// on the codegen `fixer`.
#[inline]
pub fn paren(e: Expr) -> Expr {
    Expr::Paren(ParenExpr {
        span: injected_span(),
        expr: Box::new(e),
    })
}

/// A binary expression `left op right`. Operands are **not** auto-parenthesized
/// (see module docs); parenthesize explicitly where precedence demands it.
#[inline]
pub fn bin(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Bin(BinExpr {
        span: injected_span(),
        op,
        left: Box::new(left),
        right: Box::new(right),
    })
}

/// A unary expression `op arg` (e.g. `!`, `-`, `void`, `typeof`).
#[inline]
pub fn unary(op: UnaryOp, arg: Expr) -> Expr {
    Expr::Unary(UnaryExpr {
        span: injected_span(),
        op,
        arg: Box::new(arg),
    })
}

/// Bitwise NOT `~arg`.
#[inline]
pub fn bit_not(arg: Expr) -> Expr {
    unary(UnaryOp::Tilde, arg)
}

/// Logical NOT `!arg`.
#[inline]
pub fn not(arg: Expr) -> Expr {
    unary(UnaryOp::Bang, arg)
}

/// Unary minus `-arg`.
#[inline]
pub fn neg(arg: Expr) -> Expr {
    unary(UnaryOp::Minus, arg)
}

/// A ternary `cond ? cons : alt`.
#[inline]
pub fn ternary(cond: Expr, cons: Expr, alt: Expr) -> Expr {
    Expr::Cond(CondExpr {
        span: injected_span(),
        test: Box::new(cond),
        cons: Box::new(cons),
        alt: Box::new(alt),
    })
}

/// A sequence (comma) expression `a, b, …`. Panics on an empty list (a
/// zero-element sequence is not expressible).
#[inline]
pub fn seq(exprs: Vec<Expr>) -> Expr {
    debug_assert!(!exprs.is_empty(), "seq() needs at least one expression");
    Expr::Seq(SeqExpr {
        span: injected_span(),
        exprs: exprs.into_iter().map(Box::new).collect(),
    })
}

// ---------------------------------------------------------------------------
// Access / call
// ---------------------------------------------------------------------------

/// A computed member access `obj[key]` (e.g. `obj["prop"]`). This is the form
/// the member-access + strings passes standardize on.
#[inline]
pub fn member_computed(obj: Expr, key: Expr) -> Expr {
    Expr::Member(MemberExpr {
        span: injected_span(),
        obj: Box::new(obj),
        prop: MemberProp::Computed(ComputedPropName {
            span: injected_span(),
            expr: Box::new(key),
        }),
    })
}

/// A static (dotted) member access `obj.name`.
#[inline]
pub fn member_ident(obj: Expr, name: &str) -> Expr {
    Expr::Member(MemberExpr {
        span: injected_span(),
        obj: Box::new(obj),
        prop: MemberProp::Ident(IdentName::new(Atom::from(name), injected_span())),
    })
}

/// A call `callee(args…)` with the empty call `SyntaxContext`.
#[inline]
pub fn call(callee: Expr, args: Vec<Expr>) -> Expr {
    Expr::Call(CallExpr {
        span: injected_span(),
        ctxt: SyntaxContext::empty(),
        callee: Callee::Expr(Box::new(callee)),
        args: args
            .into_iter()
            .map(|e| ExprOrSpread {
                spread: None,
                expr: Box::new(e),
            })
            .collect(),
        type_args: None,
    })
}

/// A `new callee(args…)` construction.
#[inline]
pub fn new_expr(callee: Expr, args: Vec<Expr>) -> Expr {
    Expr::New(NewExpr {
        span: injected_span(),
        ctxt: SyntaxContext::empty(),
        callee: Box::new(callee),
        args: Some(
            args.into_iter()
                .map(|e| ExprOrSpread {
                    spread: None,
                    expr: Box::new(e),
                })
                .collect(),
        ),
        type_args: None,
    })
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// An array literal `[a, b, …]` (no holes; every element present).
#[inline]
pub fn array(elems: Vec<Expr>) -> Expr {
    Expr::Array(ArrayLit {
        span: injected_span(),
        elems: elems
            .into_iter()
            .map(|e| {
                Some(ExprOrSpread {
                    spread: None,
                    expr: Box::new(e),
                })
            })
            .collect(),
    })
}

/// An object literal from `(key, value)` pairs, each emitted as a **computed**
/// string key `{["key"]: value}` — the prototype-safe form the member-access
/// pass standardizes on (a plain `__proto__` key would set the prototype).
#[inline]
pub fn object_computed(entries: Vec<(&str, Expr)>) -> Expr {
    let props = entries
        .into_iter()
        .map(|(k, v)| {
            PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
                key: PropName::Computed(ComputedPropName {
                    span: injected_span(),
                    expr: Box::new(str_lit(k)),
                }),
                value: Box::new(v),
            })))
        })
        .collect();
    Expr::Object(ObjectLit {
        span: injected_span(),
        props,
    })
}

// ---------------------------------------------------------------------------
// Assignment / statements
// ---------------------------------------------------------------------------

/// A simple assignment expression `name = value`.
#[inline]
pub fn assign(name: &str, value: Expr) -> Expr {
    Expr::Assign(AssignExpr {
        span: injected_span(),
        op: AssignOp::Assign,
        left: AssignTarget::Simple(SimpleAssignTarget::Ident(BindingIdent {
            id: ident(name),
            type_ann: None,
        })),
        right: Box::new(value),
    })
}

/// An assignment with an arbitrary operator and a simple-ident target:
/// `name op= value` (e.g. `+=`, `^=`).
#[inline]
pub fn assign_op(op: AssignOp, name: &str, value: Expr) -> Expr {
    Expr::Assign(AssignExpr {
        span: injected_span(),
        op,
        left: AssignTarget::Simple(SimpleAssignTarget::Ident(BindingIdent {
            id: ident(name),
            type_ann: None,
        })),
        right: Box::new(value),
    })
}

/// Wrap an expression as an expression statement `e;`.
#[inline]
pub fn expr_stmt(e: Expr) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: injected_span(),
        expr: Box::new(e),
    })
}

/// A `return e;` statement.
#[inline]
pub fn return_stmt(e: Expr) -> Stmt {
    Stmt::Return(ReturnStmt {
        span: injected_span(),
        arg: Some(Box::new(e)),
    })
}

/// A block statement `{ stmts… }`.
#[inline]
pub fn block(stmts: Vec<Stmt>) -> BlockStmt {
    BlockStmt {
        span: injected_span(),
        ctxt: SyntaxContext::empty(),
        stmts,
    }
}

/// A single-declarator `var`/`let`/`const` declaration: `<kind> name = init;`.
pub fn var_decl(kind: VarDeclKind, name: &str, init: Expr) -> Stmt {
    Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: injected_span(),
        ctxt: SyntaxContext::empty(),
        kind,
        declare: false,
        decls: vec![VarDeclarator {
            span: injected_span(),
            name: Pat::Ident(BindingIdent {
                id: ident(name),
                type_ann: None,
            }),
            init: Some(Box::new(init)),
            definite: false,
        }],
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use swc_core::common::sync::Lrc;
    use swc_core::common::SourceMap;
    use swc_core::ecma::codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};

    /// Emit a single expression to minified source, for asserting the built shape.
    fn emit(e: Expr) -> String {
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
                body: vec![expr_stmt(e)],
                shebang: None,
            });
            emitter.emit_program(&program).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn primitives_emit() {
        assert_eq!(emit(num_u32(5)), "5;");
        assert_eq!(emit(str_lit("hi")), "\"hi\";");
        assert_eq!(emit(bool_lit(true)), "true;");
        assert_eq!(emit(ident_expr("x")), "x;");
    }

    #[test]
    fn member_and_call_emit() {
        // obj["prop"]
        let m = member_computed(ident_expr("obj"), str_lit("prop"));
        assert_eq!(emit(m), "obj[\"prop\"];");
        // f(1, 2)
        let c = call(ident_expr("f"), vec![num_u32(1), num_u32(2)]);
        assert_eq!(emit(c), "f(1,2);");
    }

    #[test]
    fn bin_bitnot_ternary_emit() {
        let e = bin(BinaryOp::Add, num_u32(1), num_u32(2));
        assert_eq!(emit(e), "1+2;");
        let e = bit_not(num_u32(0));
        assert_eq!(emit(e), "~0;");
        let e = ternary(bool_lit(true), num_u32(1), num_u32(2));
        assert_eq!(emit(e), "true?1:2;");
    }

    #[test]
    fn array_object_emit() {
        let a = array(vec![num_u32(1), num_u32(2)]);
        assert_eq!(emit(a), "[1,2];");
        let o = object_computed(vec![("k", num_u32(1))]);
        assert_eq!(emit(o), "{[\"k\"]:1};");
    }

    #[test]
    fn assign_emit() {
        assert_eq!(emit(assign("x", num_u32(3))), "x=3;");
    }
}
