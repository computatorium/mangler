//! Native closure factories bridge JavaScript functions to live VM-frame bindings.
//!
//! SWC's resolver identifies free references, including parameter defaults and
//! block/catch shadows. Each factory receives fresh-named accessor cells pointing
//! at parent frame slots. Reads and writes remain live; boxed slots add one further
//! dereference. Bare/optional/tagged calls retain their original receiver semantics.
//! Native arrows additionally receive lexical `this`; nested regular functions
//! keep their own receiver. Dynamic eval/with scopes are rejected conservatively.

use std::collections::{HashMap, HashSet};

use swc_core::common::{DUMMY_SP, GLOBALS, Globals, Mark, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::codegen::text_writer::JsWriter;
use swc_core::ecma::codegen::{Config as CodegenConfig, Emitter};
use swc_core::ecma::transforms::base::resolver;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

/// One upvalue the factory takes as a parameter (`u_i`).
#[derive(Debug, Clone)]
pub(crate) struct Upvalue {
    /// The original enclosing-frame name this upvalue stands in for. `None` for the
    /// synthetic arrow-`this` upvalue (no source name; never rewritten by name).
    pub name: Option<String>,
    /// True if the enclosing-frame slot itself holds a binding cell. The native
    /// function dereferences the accessor and then this cell: `u_i[0][0]`.
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

/// Dynamic scope can resolve names hidden inside strings or with objects. Walk
/// through nested functions and parameter defaults: moving any such closure into
/// a factory would change its lexical environment.
pub(crate) fn unsupported_scope(params: &[Pat], body: &FunctionBody) -> Option<&'static str> {
    struct Scan(Option<&'static str>);
    impl Visit for Scan {
        fn visit_bin_expr(&mut self, n: &BinExpr) {
            super::walk_binary_chain(n, self);
        }
        fn visit_call_expr(&mut self, call: &CallExpr) {
            if mangler_jsast::analysis::scope::is_direct_eval_callee(&call.callee) {
                self.0.get_or_insert("native_direct_eval");
            }
            call.visit_children_with(self);
        }
        fn visit_with_stmt(&mut self, stmt: &WithStmt) {
            self.0.get_or_insert("native_with");
            stmt.visit_children_with(self);
        }
    }
    let mut scan = Scan(None);
    params.visit_with(&mut scan);
    body.visit_with(&mut scan);
    scan.0
}

fn resolve_native_expression(expression: &mut Expr, unresolved: Mark) {
    let mut program = Program::Script(Script {
        body: vec![Stmt::Expr(ExprStmt {
            span: DUMMY_SP,
            expr: Box::new(std::mem::replace(
                expression,
                Expr::Invalid(Invalid { span: DUMMY_SP }),
            )),
        })],
        ..Default::default()
    });
    program.visit_mut_with(&mut resolver(unresolved, Mark::new(), false));
    mangler_jsast::Js::repair_resolver_scopes(&mut program);
    let Program::Script(mut script) = program else {
        unreachable!()
    };
    let Stmt::Expr(statement) = script.body.remove(0) else {
        unreachable!()
    };
    *expression = *statement.expr;
}

/// Build the factory function-expression SOURCE for `is_arrow` original, given its
/// `params`/`body` and the ordered `upvalues`. `self_name` is the original named
/// function expression's own name (re-attached so its self-recursion resolves
/// natively). Returns `None` if codegen produces something that fails to reparse
/// (defensive — never expected; the caller then bails to native-subtree).
pub(crate) fn build_factory_src(
    params: &[Pat],
    body: &FunctionBody,
    is_arrow: bool,
    is_async: bool,
    is_generator: bool,
    self_name: Option<&str>,
    inherited_strict: bool,
    upvalues: &[Upvalue],
) -> Option<String> {
    // Dynamic scope cannot be represented by static capture rewrites.
    if unsupported_scope(params, body).is_some() {
        return None;
    }
    GLOBALS.set(&Globals::new(), || {
        let mut inner_expr =
            build_inner_expr(params, body, is_arrow, is_async, is_generator, self_name);
        let unresolved = Mark::new();
        resolve_native_expression(&mut inner_expr, unresolved);
        let unresolved = SyntaxContext::empty().apply_mark(unresolved);
        let mut reserved = Names::default();
        inner_expr.visit_with(&mut reserved);
        let mut parameter_names = Vec::new();
        for i in 0..upvalues.len() {
            let mut name = up_param(i);
            while reserved.0.contains(&name) {
                name.push('_');
            }
            reserved.0.insert(name.clone());
            parameter_names.push(name);
        }
        // Map each celled / plain frame-local NAME to its factory-param ident, and find
        // the arrow-`this` param (if any).
        let mut rename: HashMap<String, RewriteTarget> = HashMap::new();
        let mut this_param: Option<String> = None;
        for (i, uv) in upvalues.iter().enumerate() {
            let p = parameter_names[i].clone();
            if uv.is_this {
                this_param = Some(p);
            } else if let Some(n) = &uv.name {
                rename.insert(
                    n.clone(),
                    RewriteTarget {
                        param: p,
                        celled: uv.celled,
                    },
                );
            }
        }

        // Clone + rewrite the inner function/arrow so frame-local refs become the
        // factory params (`u_i` / `u_i[0]`), `this` becomes the arrow-this param, and
        // module globals are left untouched. Locals the inner fn itself binds shadow a
        // same-named upvalue and are NOT rewritten.
        let mut rw = Rewriter {
            rename: &rename,
            this_param: this_param.as_deref(),
            unresolved,
            own_arguments: false,
            strict: inherited_strict,
        };
        inner_expr.visit_mut_with(&mut rw);

        // Wrap: `function ($u0,$u1,…){ return <inner>; }`. The factory itself is plain
        // (sloppy) — the inner fn carries its OWN strictness directive, so it runs in
        // its correct mode regardless (§5a.2).
        let factory_params: Vec<Pat> = (0..upvalues.len())
            .map(|i| {
                Pat::Ident(BindingIdent {
                    id: Ident::new(
                        parameter_names[i].clone().into(),
                        DUMMY_SP,
                        Default::default(),
                    ),
                    type_ann: None,
                })
            })
            .collect();
        let factory = Expr::Fn(FnExpr {
            ident: None,
            function: Box::new(Function {
                this_param: None,
                params: factory_params
                    .into_iter()
                    .map(|pat| Param {
                        span: DUMMY_SP,
                        decorators: vec![],
                        pat,
                    })
                    .collect(),
                decorators: vec![],
                span: DUMMY_SP,
                ctxt: Default::default(),
                body: Some(FunctionBody {
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
        if mangler_jsast::lang::Js::reparse(&check, &mangler_jsast::lang::ParseOpts::default())
            .is_err()
        {
            return None;
        }
        Some(src)
    })
}

/// Reconstruct the original inner function/arrow as an [`Expr`] from its parts. A
/// named function expression re-attaches `self_name` so its native self-reference
/// resolves; an arrow rebuilds with a block body (the caller already wrapped an
/// expression-bodied arrow into `{ return e; }`).
fn build_inner_expr(
    params: &[Pat],
    body: &FunctionBody,
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
            body: Box::new(ArrowFunctionBody::FunctionBody(body.clone())),
            is_async,
            is_generator,
            type_params: None,
            return_type: None,
        })
    } else {
        Expr::Fn(FnExpr {
            ident: self_name.map(|n| Ident::new(n.into(), DUMMY_SP, Default::default())),
            function: Box::new(Function {
                this_param: None,
                params: params
                    .iter()
                    .map(|pat| Param {
                        span: DUMMY_SP,
                        decorators: vec![],
                        pat: pat.clone(),
                    })
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

#[derive(Default)]
struct Names(HashSet<String>);
impl Visit for Names {
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        super::walk_binary_chain(n, self);
    }
    fn visit_ident(&mut self, id: &Ident) {
        self.0.insert(id.sym.to_string());
    }
}

/// Native escapes and their factories use one scope authority: SWC's resolver.
/// Parameter defaults, block/catch shadows and nested scopes therefore agree.
pub(crate) fn free_names(
    params: &[Pat],
    body: &FunctionBody,
    self_name: Option<&str>,
) -> Vec<String> {
    GLOBALS.set(&Globals::new(), || {
        let mut expr = build_inner_expr(params, body, true, false, false, self_name);
        let mark = Mark::new();
        resolve_native_expression(&mut expr, mark);
        struct Free {
            unresolved: SyntaxContext,
            names: Vec<String>,
            seen: HashSet<String>,
            own_arguments: bool,
        }
        impl Visit for Free {
            fn visit_bin_expr(&mut self, n: &BinExpr) {
                super::walk_binary_chain(n, self);
            }
            fn visit_function(&mut self, function: &Function) {
                let old = self.own_arguments;
                self.own_arguments = true;
                function.visit_children_with(self);
                self.own_arguments = old;
            }
            fn visit_constructor(&mut self, constructor: &Constructor) {
                let old = self.own_arguments;
                self.own_arguments = true;
                constructor.visit_children_with(self);
                self.own_arguments = old;
            }
            fn visit_ident(&mut self, id: &Ident) {
                let name = id.sym.to_string();
                if id.ctxt == self.unresolved
                    && !(self.own_arguments && name == "arguments")
                    && self.seen.insert(name.clone())
                {
                    self.names.push(name);
                }
            }
        }
        let mut free = Free {
            unresolved: SyntaxContext::empty().apply_mark(mark),
            names: Vec::new(),
            seen: HashSet::new(),
            own_arguments: false,
        };
        expr.visit_with(&mut free);
        free.names.retain(|name| Some(name.as_str()) != self_name);
        free.names
    })
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
    unresolved: SyntaxContext,
    own_arguments: bool,
    strict: bool,
}

impl Rewriter<'_> {
    fn is_free(&self, id: &Ident) -> bool {
        id.ctxt == self.unresolved && !(self.own_arguments && id.sym.as_ref() == "arguments")
    }

    /// A free call must keep an undefined receiver after becoming a cell member.
    fn rewrite_callee(&self, callee: &mut Box<Expr>) {
        if let Expr::Ident(id) = callee.as_ref()
            && self.is_free(id)
            && let Some(target) = self.target_expr(id.sym.as_ref(), id.span)
        {
            if let Some(target) = self.rename.get(id.sym.as_ref())
                && !target.celled
            {
                **callee = Expr::Member(MemberExpr {
                    span: id.span,
                    obj: Box::new(Expr::Ident(Ident::new(
                        target.param.clone().into(),
                        id.span,
                        Default::default(),
                    ))),
                    prop: MemberProp::Computed(ComputedPropName {
                        span: id.span,
                        expr: Box::new(Expr::Lit(Lit::Num(Number {
                            span: id.span,
                            value: if self.strict { 3.0 } else { 1.0 },
                            raw: None,
                        }))),
                    }),
                });
                return;
            }
            **callee = Expr::Paren(ParenExpr {
                span: id.span,
                expr: Box::new(Expr::Seq(SeqExpr {
                    span: id.span,
                    exprs: vec![
                        Box::new(Expr::Lit(Lit::Num(Number {
                            span: id.span,
                            value: 0.0,
                            raw: None,
                        }))),
                        Box::new(target),
                    ],
                })),
            });
        }
    }

    /// Rewrite a `for-in`/`for-of` head whose target is a bare upvalue ident
    /// (`for (x of …)`) to its member form (`for ($u_i[0] of …)`). A `var`/`let` head
    /// declares a fresh local (never an upvalue), left for the normal walk.
    fn cellify_for_head(&mut self, head: &mut ForHead) {
        if let ForHead::Pat(p) = head
            && let Pat::Ident(bi) = &**p
            && self.is_free(&bi.id)
            && let Some(Expr::Member(m)) = self.target_expr(bi.id.sym.as_ref(), bi.id.span)
        {
            *head = ForHead::Pat(Box::new(Pat::Expr(Box::new(Expr::Member(m)))));
        } else {
            head.visit_mut_with(self);
        }
    }

    /// Dereference a live slot accessor, and then the binding cell if boxed.
    fn target_expr(&self, name: &str, span: swc_core::common::Span) -> Option<Expr> {
        let t = self.rename.get(name)?;
        let mut expr = Expr::Ident(Ident::new(t.param.clone().into(), span, Default::default()));
        // Every native upvalue is a live accessor cell over its parent frame slot.
        // Boxed slots add one further dereference to the shared binding cell.
        for depth in 0..if t.celled { 2 } else { 1 } {
            expr = Expr::Member(MemberExpr {
                span,
                obj: Box::new(expr),
                prop: MemberProp::Computed(ComputedPropName {
                    span,
                    expr: Box::new(Expr::Lit(Lit::Num(Number {
                        span,
                        value: if depth == 0 && self.strict { 2.0 } else { 0.0 },
                        raw: None,
                    }))),
                }),
            });
        }
        Some(expr)
    }
}

impl VisitMut for Rewriter<'_> {
    // Regular functions own their receiver; nested arrows inherit the active one.
    fn visit_mut_function(&mut self, f: &mut Function) {
        let previous = self.this_param.take();
        let arguments = self.own_arguments;
        let strict = self.strict;
        self.strict |= f
            .body
            .as_ref()
            .is_some_and(|body| mangler_jsast::directives::has_use_strict(&body.stmts));
        self.own_arguments = true;
        f.visit_mut_children_with(self);
        self.strict = strict;
        self.own_arguments = arguments;
        self.this_param = previous;
    }

    fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
        let strict = self.strict;
        if let ArrowFunctionBody::FunctionBody(body) = &*arrow.body {
            self.strict |= mangler_jsast::directives::has_use_strict(&body.stmts);
        }
        arrow.visit_mut_children_with(self);
        self.strict = strict;
    }

    fn visit_mut_class(&mut self, class: &mut Class) {
        let strict = self.strict;
        self.strict = true;
        class.visit_mut_children_with(self);
        self.strict = strict;
    }

    // Computed class keys and heritage evaluate in the enclosing receiver scope;
    // field initializers, constructors and static blocks own a new receiver.
    fn visit_mut_class_prop(&mut self, property: &mut ClassProp) {
        property.key.visit_mut_with(self);
        property.decorators.visit_mut_with(self);
        let previous = self.this_param.take();
        property.value.visit_mut_with(self);
        self.this_param = previous;
    }

    fn visit_mut_private_prop(&mut self, property: &mut PrivateProp) {
        property.decorators.visit_mut_with(self);
        let previous = self.this_param.take();
        property.value.visit_mut_with(self);
        self.this_param = previous;
    }

    fn visit_mut_constructor(&mut self, constructor: &mut Constructor) {
        let previous = self.this_param.take();
        let arguments = self.own_arguments;
        self.own_arguments = true;
        constructor.visit_mut_children_with(self);
        self.own_arguments = arguments;
        self.this_param = previous;
    }

    fn visit_mut_static_block(&mut self, block: &mut StaticBlock) {
        let previous = self.this_param.take();
        block.visit_mut_children_with(self);
        self.this_param = previous;
    }

    fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
        if let Callee::Expr(callee) = &mut call.callee {
            self.rewrite_callee(callee);
        }
        call.visit_mut_children_with(self);
    }

    fn visit_mut_opt_call(&mut self, call: &mut OptCall) {
        self.rewrite_callee(&mut call.callee);
        call.visit_mut_children_with(self);
    }

    fn visit_mut_tagged_tpl(&mut self, tag: &mut TaggedTpl) {
        self.rewrite_callee(&mut tag.tag);
        tag.visit_mut_children_with(self);
    }

    // Assignment / compound-assignment target `x = v` / `x op= v` → `$u_i[0] op= v`
    // (a celled upvalue) or `$u_i = v` (a plain frame-local — though a written plain
    // local would have been rejected by the §4.4 guard, so in practice only the
    // celled case reaches here). A non-upvalue target is left untouched.
    fn visit_mut_assign_expr(&mut self, n: &mut AssignExpr) {
        if let AssignTarget::Simple(SimpleAssignTarget::Ident(bi)) = &n.left {
            let name = bi.id.sym.as_ref();
            if self.is_free(&bi.id)
                && let Some(rep) = self.target_expr(name, bi.id.span)
            {
                if let Expr::Member(m) = rep {
                    n.left = AssignTarget::Simple(SimpleAssignTarget::Member(m));
                } else if let Expr::Ident(id) = rep {
                    n.left = AssignTarget::Simple(SimpleAssignTarget::Ident(BindingIdent {
                        id,
                        type_ann: None,
                    }));
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
            if self.is_free(id)
                && let Some(rep) = self.target_expr(name, id.span)
            {
                *n.arg = rep;
                return;
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
                if self.is_free(id)
                    && let Some(rep) = self.target_expr(name, id.span)
                {
                    *e = rep;
                    return;
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
            if self.is_free(id)
                && let Some(rep) = self.target_expr(name, id.span)
            {
                *p = Prop::KeyValue(KeyValueProp {
                    key: PropName::Ident(IdentName::new(id.sym.clone(), id.span)),
                    value: Box::new(rep),
                });
                return;
            }
        }
        p.visit_mut_children_with(self);
    }
}

/// Print an expression to minified JS source (as `(<expr>)`-ready text). Used for
/// the factory const; deterministic for a given AST.
fn print_expr(e: &Expr) -> String {
    use swc_core::common::SourceMap;
    use swc_core::common::sync::Lrc;
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
        let mut program = Program::Script(Script {
            span: DUMMY_SP,
            body: vec![Stmt::Expr(ExprStmt {
                span: DUMMY_SP,
                expr: Box::new(e.clone()),
            })],
            shebang: None,
        });
        program.visit_mut_with(&mut swc_core::ecma::transforms::base::fixer::fixer(None));
        use swc_core::ecma::codegen::Node;
        program
            .emit_with(&mut emitter)
            .expect("codegen of factory expr");
    }
    let mut s = String::from_utf8(buf).expect("utf8 codegen");
    // Strip a trailing `;` (the ExprStmt terminator) so the source is the bare
    // expression the serializer wraps in `(...)`.
    if s.ends_with(';') {
        s.pop();
    }
    s
}

#[cfg(test)]
mod dynamic_scope_tests {
    use super::unsupported_scope;
    use crate::test_support::parse_fn_with_params;

    #[test]
    fn strict_native_scopes_select_strict_reference_views() {
        use mangler_testkit::eval::{CaptureMode, assert_behaviorally_equal_with};
        let (_, body) = parse_fn_with_params(
            "function(){return [x,function(){'use strict';return [x,()=>x,x()]},()=>{'use strict';return x},class {m(){return x}}]}",
        );
        for inherited_strict in [false, true] {
            let source = super::build_factory_src(
                &[],
                &body,
                false,
                false,
                false,
                None,
                inherited_strict,
                &[super::Upvalue {
                    name: Some("x".into()),
                    celled: false,
                    is_this: false,
                }],
            )
            .unwrap();
            let transformed = format!(
                "var cells={{0:7,2:9,1:function(){{return 11}},3:function(){{return 13}}}};var a=({source})(cells)(),b=a[1]();globalThis.__out=JSON.stringify([a[0],b[0],b[1](),b[2],a[2](),new (a[3])().m()]);"
            );
            let expected = if inherited_strict { 9 } else { 7 };
            assert_behaviorally_equal_with(
                &format!("globalThis.__out='[{expected},9,9,13,9,9]';"),
                &transformed,
                &CaptureMode::sink(),
            );
        }
    }

    #[test]
    fn class_receiver_and_arguments_scopes_survive_factory_rewrite() {
        use mangler_testkit::eval::{CaptureMode, assert_behaviorally_equal_with};
        let (_, body) = parse_fn_with_params(
            "function(){return class {static x=this; x=this; [this.key]=this; m(){return arguments[0]}}}",
        );
        assert!(!super::free_names(&[], &body, None).contains(&"arguments".to_string()));
        let source = super::build_factory_src(
            &[],
            &body,
            true,
            false,
            false,
            None,
            false,
            &[super::Upvalue {
                name: None,
                celled: false,
                is_this: true,
            }],
        )
        .unwrap();
        let transformed = format!(
            "var C=({source})({{key:'field'}})();var c=new C;globalThis.__out=JSON.stringify([C.x===C,c.x===c,c.field===c,c.m(7)]);"
        );
        let original = "var C=(function(){return class {static x=this;x=this;[this.key]=this;m(){return arguments[0]}}}).call({key:'field'});var c=new C;globalThis.__out=JSON.stringify([C.x===C,c.x===c,c.field===c,c.m(7)]);";
        assert_behaviorally_equal_with(original, &transformed, &CaptureMode::sink());
    }

    #[test]
    fn parenthesized_eval_retains_direct_eval_semantics() {
        for source in [
            "function(){ return (eval)('x'); }",
            "function(){ return ((eval))('x'); }",
            "function(a=(eval)('x')){ return a; }",
            "function(){ return ()=>((eval))('x'); }",
        ] {
            let (params, body) = parse_fn_with_params(source);
            let params: Vec<_> = params.into_iter().map(|p| p.pat).collect();
            assert_eq!(
                unsupported_scope(&params, &body),
                Some("native_direct_eval"),
                "{source}"
            );
        }
    }

    #[test]
    fn indirect_eval_does_not_require_caller_environment() {
        for source in [
            "function(){ return (0, eval)('x'); }",
            "function(){ return eval?.('x'); }",
            "function(){ return globalThis.eval('x'); }",
        ] {
            let (params, body) = parse_fn_with_params(source);
            let params: Vec<_> = params.into_iter().map(|p| p.pat).collect();
            assert_eq!(unsupported_scope(&params, &body), None, "{source}");
        }
    }
}
