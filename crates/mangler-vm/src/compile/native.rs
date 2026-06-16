//! Phase 3 native-closure escape hatch (§4): build the factory function
//! expression for an excluded / ineligible nested function that must run as
//! **native JS at full speed** while still capturing enclosing VM-frame locals.
//!
//! The compiler ([`super::emit_nested_closure`]) decides WHICH nested functions
//! divert here (exclude-glob match, or async/generator/`"use strict"`/structurally
//! ineligible under the divert-ineligible option). This module owns the
//! representation: given the original function/arrow AST, the set of free names
//! that resolve to enclosing-VM-frame bindings (the upvalues, in deterministic
//! order) and which of those are CELLS (boxed mutable captures), it produces the
//! factory source
//!
//! ```js
//! function (u0, u1, …) { return <original, frame-locals rewritten to u0,u1,…>; }
//! ```
//!
//! Free **module globals** (names NOT bound in any enclosing VM frame) are left
//! untouched — the factory lives at module scope in the shared program-table, so
//! they resolve to the real globals (no threading, no obfuscation lost). A celled
//! upvalue is rewritten to `u_i[0]` (read) / `u_i[0] = v` (write) so the native fn
//! shares the same one-element cell array the VM frame holds (mutation
//! propagates). For an arrow, the enclosing `this` is threaded as an extra trailing
//! upvalue and the factory closes over it lexically (an arrow ignores the call-time
//! receiver, so this preserves lexical `this`).
//!
//! The output is a pure function of the AST + the rename map, so the same diversity
//! seed yields byte-identical factory source (§4.4 determinism).

use std::collections::{HashMap, HashSet};

use swc_core::common::DUMMY_SP;
use swc_core::ecma::ast::*;
use swc_core::ecma::codegen::text_writer::JsWriter;
use swc_core::ecma::codegen::{Config as CodegenConfig, Emitter};
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

/// One upvalue the factory takes as a parameter (`u_i`).
#[derive(Debug, Clone)]
pub(crate) struct Upvalue {
    /// The original enclosing-frame name this upvalue stands in for. `None` for the
    /// synthetic arrow-`this` upvalue (no source name; never rewritten by name).
    pub name: Option<String>,
    /// True if the enclosing-frame slot holds a one-element CELL `[v]` (a boxed
    /// mutable capture): the native fn reads/writes `u_i[0]`.
    pub celled: bool,
    /// True if this is the synthetic arrow-`this` upvalue: every `this` in the
    /// original arrow body becomes `u_i`.
    pub is_this: bool,
}

/// The lexically-named factory parameter for upvalue index `i` (`$u0`, `$u1`, …).
/// The leading `$` keeps it from colliding with a real source identifier the
/// downstream renamer might also pick (the renamer never produces a `$`-prefixed
/// name from this generator), and with the factory's own scope it is fresh anyway.
fn up_param(i: usize) -> String {
    format!("$u{i}")
}

/// Build the factory function-expression SOURCE for `is_arrow` original, given its
/// `params`/`body` and the ordered `upvalues`. `self_name` is the original named
/// function expression's own name (re-attached so its self-recursion resolves
/// natively). Returns `None` if codegen produces something that fails to reparse
/// (defensive — never expected; the caller then bails to native-subtree).
pub(crate) fn build_factory_src(
    params: &[Pat],
    body: &BlockStmt,
    is_arrow: bool,
    is_async: bool,
    is_generator: bool,
    self_name: Option<&str>,
    upvalues: &[Upvalue],
) -> Option<String> {
    // Map each celled / plain frame-local NAME to its factory-param ident, and find
    // the arrow-`this` param (if any).
    let mut rename: HashMap<String, RewriteTarget> = HashMap::new();
    let mut this_param: Option<String> = None;
    for (i, uv) in upvalues.iter().enumerate() {
        let p = up_param(i);
        if uv.is_this {
            this_param = Some(p);
        } else if let Some(n) = &uv.name {
            rename.insert(
                n.clone(),
                RewriteTarget { param: p, celled: uv.celled },
            );
        }
    }

    // Clone + rewrite the inner function/arrow so frame-local refs become the
    // factory params (`u_i` / `u_i[0]`), `this` becomes the arrow-this param, and
    // module globals are left untouched. Locals the inner fn itself binds shadow a
    // same-named upvalue and are NOT rewritten.
    let inner_expr = build_inner_expr(
        params, body, is_arrow, is_async, is_generator, self_name,
    );
    let mut rw = Rewriter {
        rename: &rename,
        this_param: this_param.as_deref(),
        scopes: vec![local_names(params, body, self_name)],
    };
    let mut inner_expr = inner_expr;
    inner_expr.visit_mut_with(&mut rw);

    // Wrap: `function ($u0,$u1,…){ return <inner>; }`. The factory itself is plain
    // (sloppy) — the inner fn carries its OWN strictness directive, so it runs in
    // its correct mode regardless (§5a.2).
    let factory_params: Vec<Pat> = (0..upvalues.len())
        .map(|i| {
            Pat::Ident(BindingIdent {
                id: Ident::new(up_param(i).into(), DUMMY_SP, Default::default()),
                type_ann: None,
            })
        })
        .collect();
    let factory = Expr::Fn(FnExpr {
        ident: None,
        function: Box::new(Function {
            params: factory_params
                .into_iter()
                .map(|pat| Param { span: DUMMY_SP, decorators: vec![], pat })
                .collect(),
            decorators: vec![],
            span: DUMMY_SP,
            ctxt: Default::default(),
            body: Some(BlockStmt {
                span: DUMMY_SP,
                stmts: vec![Stmt::Return(ReturnStmt {
                    span: DUMMY_SP,
                    arg: Some(Box::new(inner_expr)),
                })],
                ..Default::default()
            }),
            is_generator: false,
            is_async: false,
            type_params: None,
            return_type: None,
        }),
    });

    let src = print_expr(&factory);
    // Defensive reparse: a malformed factory would corrupt the program table. The
    // serializer renders this const PARENTHESIZED (`(<src>)`) into the consts-array
    // expression position, so validate it in that exact form — an anonymous
    // `function(...)` at bare statement position is a syntax error, but `(function
    // (...))` is a valid expression.
    let check = format!("({src})");
    if mangler_jsast::lang::Js::reparse(&check, &mangler_jsast::lang::ParseOpts::default()).is_err()
    {
        return None;
    }
    Some(src)
}

/// Reconstruct the original inner function/arrow as an [`Expr`] from its parts. A
/// named function expression re-attaches `self_name` so its native self-reference
/// resolves; an arrow rebuilds with a block body (the caller already wrapped an
/// expression-bodied arrow into `{ return e; }`).
fn build_inner_expr(
    params: &[Pat],
    body: &BlockStmt,
    is_arrow: bool,
    is_async: bool,
    is_generator: bool,
    self_name: Option<&str>,
) -> Expr {
    if is_arrow {
        Expr::Arrow(ArrowExpr {
            span: DUMMY_SP,
            ctxt: Default::default(),
            params: params.to_vec(),
            body: Box::new(BlockStmtOrExpr::BlockStmt(body.clone())),
            is_async,
            is_generator,
            type_params: None,
            return_type: None,
        })
    } else {
        Expr::Fn(FnExpr {
            ident: self_name.map(|n| Ident::new(n.into(), DUMMY_SP, Default::default())),
            function: Box::new(Function {
                params: params
                    .iter()
                    .map(|pat| Param { span: DUMMY_SP, decorators: vec![], pat: pat.clone() })
                    .collect(),
                decorators: vec![],
                span: DUMMY_SP,
                ctxt: Default::default(),
                body: Some(body.clone()),
                is_generator,
                is_async,
                type_params: None,
                return_type: None,
            }),
        })
    }
}

/// The names the inner fn binds itself (params + locals + own self-name): these
/// SHADOW a same-named upvalue and must NOT be rewritten.
fn local_names(params: &[Pat], body: &BlockStmt, self_name: Option<&str>) -> HashSet<String> {
    let mut names = HashSet::new();
    for p in params {
        mangler_jsast::analysis::binding_names(p, &mut |id| {
            names.insert(id.sym.to_string());
        });
    }
    if let Some(n) = self_name {
        names.insert(n.to_string());
    }
    super::stmt::collect_body_local_decls(body, &mut names);
    names
}

struct RewriteTarget {
    param: String,
    celled: bool,
}

/// Rewrites the inner fn/arrow: a free frame-local name → its factory param
/// (`$u_i`, or `$u_i[0]` for a cell); `this` → the arrow-this param. Module
/// globals (not in `rename`) and inner-bound locals (in the current `scopes`) are
/// left untouched. Tracks nested binding scopes so a deeper local shadowing an
/// upvalue name is not rewritten.
struct Rewriter<'a> {
    rename: &'a HashMap<String, RewriteTarget>,
    this_param: Option<&'a str>,
    /// Stack of binding-name sets; a name in ANY frame is a local (not rewritten).
    scopes: Vec<HashSet<String>>,
}

impl Rewriter<'_> {
    fn is_local(&self, name: &str) -> bool {
        self.scopes.iter().any(|s| s.contains(name))
    }

    /// Rewrite a `for-in`/`for-of` head whose target is a bare upvalue ident
    /// (`for (x of …)`) to its member form (`for ($u_i[0] of …)`). A `var`/`let` head
    /// declares a fresh local (never an upvalue), left for the normal walk.
    fn cellify_for_head(&mut self, head: &mut ForHead) {
        if let ForHead::Pat(p) = head
            && let Pat::Ident(bi) = &**p
            && !self.is_local(bi.id.sym.as_ref())
            && let Some(Expr::Member(m)) = self.target_expr(bi.id.sym.as_ref(), bi.id.span)
        {
            *head = ForHead::Pat(Box::new(Pat::Expr(Box::new(Expr::Member(m)))));
        } else {
            head.visit_mut_with(self);
        }
    }

    /// `$u_i` (plain) or `$u_i[0]` (cell) for the upvalue `name` stands in for.
    fn target_expr(&self, name: &str, span: swc_core::common::Span) -> Option<Expr> {
        let t = self.rename.get(name)?;
        let base = Expr::Ident(Ident::new(t.param.clone().into(), span, Default::default()));
        if t.celled {
            Some(Expr::Member(MemberExpr {
                span,
                obj: Box::new(base),
                prop: MemberProp::Computed(ComputedPropName {
                    span,
                    expr: Box::new(Expr::Lit(Lit::Num(Number { span, value: 0.0, raw: None }))),
                }),
            }))
        } else {
            Some(base)
        }
    }
}

impl VisitMut for Rewriter<'_> {
    // A nested function/arrow opens its own scope: collect its bound names so a
    // same-named upvalue is shadowed there. We still descend (free names of the
    // nested fn that are OUR upvalues must be rewritten too — they are captures of
    // the same enclosing VM frame).
    fn visit_mut_function(&mut self, f: &mut Function) {
        let mut names = HashSet::new();
        for p in &f.params {
            mangler_jsast::analysis::binding_names(&p.pat, &mut |id| {
                names.insert(id.sym.to_string());
            });
        }
        if let Some(b) = &f.body {
            super::stmt::collect_body_local_decls(b, &mut names);
        }
        self.scopes.push(names);
        f.visit_mut_children_with(self);
        self.scopes.pop();
    }
    fn visit_mut_arrow_expr(&mut self, a: &mut ArrowExpr) {
        let mut names = HashSet::new();
        for p in &a.params {
            mangler_jsast::analysis::binding_names(p, &mut |id| {
                names.insert(id.sym.to_string());
            });
        }
        if let BlockStmtOrExpr::BlockStmt(b) = &*a.body {
            super::stmt::collect_body_local_decls(b, &mut names);
        }
        self.scopes.push(names);
        a.visit_mut_children_with(self);
        self.scopes.pop();
        // NOTE: a nested arrow's `this` is still the enclosing lexical `this`, so the
        // arrow-this rewrite below (which fires on the outer `Expr::This`) is correct
        // — `visit_mut_expr` handles `this` uniformly across nested arrows.
    }

    // Assignment / compound-assignment target `x = v` / `x op= v` → `$u_i[0] op= v`
    // (a celled upvalue) or `$u_i = v` (a plain frame-local — though a written plain
    // local would have been rejected by the §4.4 guard, so in practice only the
    // celled case reaches here). A non-upvalue target is left untouched.
    fn visit_mut_assign_expr(&mut self, n: &mut AssignExpr) {
        if let AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) = &n.left {
            let name = bi.id.sym.as_ref();
            if !self.is_local(name) {
                if let Some(rep) = self.target_expr(name, bi.id.span) {
                    if let Expr::Member(m) = rep {
                        n.left = AssignTarget::Simple(SimpleAssignTarget::Member(m));
                    } else if let Expr::Ident(id) = rep {
                        n.left = AssignTarget::Simple(SimpleAssignTarget::Ident(BindingIdent {
                            id,
                            type_ann: None,
                        }));
                    }
                }
            }
        } else {
            n.left.visit_mut_with(self);
        }
        n.right.visit_mut_with(self);
    }

    // `++x` / `x--` → `++$u_i[0]` / `$u_i[0]--` for a celled upvalue.
    fn visit_mut_update_expr(&mut self, n: &mut UpdateExpr) {
        if let Expr::Ident(id) = &*n.arg {
            let name = id.sym.as_ref();
            if !self.is_local(name) {
                if let Some(rep) = self.target_expr(name, id.span) {
                    *n.arg = rep;
                    return;
                }
            }
        }
        n.arg.visit_mut_with(self);
    }

    // for-head bare-ident target `for (x in/of …)` → `for ($u_i[0] in/of …)`.
    fn visit_mut_for_in_stmt(&mut self, n: &mut ForInStmt) {
        self.cellify_for_head(&mut n.left);
        n.right.visit_mut_with(self);
        n.body.visit_mut_with(self);
    }
    fn visit_mut_for_of_stmt(&mut self, n: &mut ForOfStmt) {
        self.cellify_for_head(&mut n.left);
        n.right.visit_mut_with(self);
        n.body.visit_mut_with(self);
    }

    fn visit_mut_expr(&mut self, e: &mut Expr) {
        match e {
            Expr::Ident(id) => {
                let name = id.sym.as_ref();
                if !self.is_local(name) {
                    if let Some(rep) = self.target_expr(name, id.span) {
                        *e = rep;
                        return;
                    }
                }
                // global / local / non-upvalue → untouched
            }
            Expr::This(t) => {
                if let Some(p) = self.this_param {
                    *e = Expr::Ident(Ident::new(p.into(), t.span, Default::default()));
                    return;
                }
            }
            _ => {}
        }
        e.visit_mut_children_with(self);
    }

    // Do not rewrite member-property identifiers (`o.render`) or non-computed
    // object/class keys — they are not value references.
    fn visit_mut_member_expr(&mut self, m: &mut MemberExpr) {
        m.obj.visit_mut_with(self);
        if let MemberProp::Computed(c) = &mut m.prop {
            c.visit_mut_with(self);
        }
    }
    fn visit_mut_prop_name(&mut self, p: &mut PropName) {
        if let PropName::Computed(c) = p {
            c.visit_mut_with(self);
        }
    }
    // Object-literal shorthand `{ x }` means `{ x: x }`; if `x` is an upvalue it must
    // become `{ x: $u_i }`. Rewrite shorthand explicitly (the value is a real ref).
    fn visit_mut_prop(&mut self, p: &mut Prop) {
        if let Prop::Shorthand(id) = p {
            let name = id.sym.as_ref();
            if !self.is_local(name) {
                if let Some(rep) = self.target_expr(name, id.span) {
                    *p = Prop::KeyValue(KeyValueProp {
                        key: PropName::Ident(IdentName::new(id.sym.clone(), id.span)),
                        value: Box::new(rep),
                    });
                    return;
                }
            }
        }
        p.visit_mut_children_with(self);
    }
}

/// Print an expression to minified JS source (as `(<expr>)`-ready text). Used for
/// the factory const; deterministic for a given AST.
fn print_expr(e: &Expr) -> String {
    use swc_core::common::sync::Lrc;
    use swc_core::common::SourceMap;
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
            span: DUMMY_SP,
            body: vec![Stmt::Expr(ExprStmt { span: DUMMY_SP, expr: Box::new(e.clone()) })],
            shebang: None,
        });
        use swc_core::ecma::codegen::Node;
        program.emit_with(&mut emitter).expect("codegen of factory expr");
    }
    let mut s = String::from_utf8(buf).expect("utf8 codegen");
    // Strip a trailing `;` (the ExprStmt terminator) so the source is the bare
    // expression the serializer wraps in `(...)`.
    if s.ends_with(';') {
        s.pop();
    }
    s
}
