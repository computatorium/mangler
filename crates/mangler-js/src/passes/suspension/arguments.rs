//! Keep source arguments bindings outside generated suspension callbacks.
//! Accessor cells preserve object identity and rebinding across suspended resumes.
use mangler_core::Language;
use mangler_jsast::{Js, ParseOpts};
use std::collections::{HashMap, HashSet};
use swc_core::common::util::take::Take;
use swc_core::common::{DUMMY_SP, Mark, Spanned, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::transforms::base::resolver;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

type Aliases = Vec<(u32, Vec<(String, Ident, bool, Vec<Ident>)>)>;
pub(super) type References = HashMap<u32, (String, Ident)>;
pub(super) struct Plan {
    entries: HashMap<u32, Entry>,
    aliases: Aliases,
    references: References,
    unresolved: Mark,
    strict: HashSet<u32>,
    generators: GeneratorArguments,
}
struct Entry {
    parameter: Ident,
    body: Option<Ident>,
    original: Ident,
    nonsimple: bool,
    original_params: Vec<Param>,
    parameter_aliases: HashSet<u32>,
    source_generator: bool,
}
/// Source generator parameters and live arguments accessors must remain in the
/// outer activation. SWC's generator hoister otherwise snapshots `arguments`
/// inside its state callback, changing rebinding and parameter-default closures.
#[derive(Default)]
pub(super) struct GeneratorArguments {
    frames: HashMap<(u32, SyntaxContext), GeneratorFrame>,
}
struct GeneratorFrame {
    prefix: Vec<Stmt>,
    params: Option<Vec<Param>>,
}
impl GeneratorArguments {
    pub(super) fn shield(&mut self, program: &mut Program) {
        struct Shield<'a>(&'a mut GeneratorArguments);
        impl VisitMut for Shield<'_> {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_function(&mut self, function: &mut Function) {
                function.visit_mut_children_with(self);
                if let Some(frame) = self.0.frames.get_mut(&(function.span.lo.0, function.ctxt)) {
                    frame.params = Some(std::mem::take(&mut function.params));
                }
            }
        }
        program.visit_mut_with(&mut Shield(self));
    }
    pub(super) fn restore(mut self, program: &mut Program) {
        struct Restore<'a>(&'a mut GeneratorArguments);
        impl VisitMut for Restore<'_> {
            fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(binary, self);
            }
            fn visit_mut_function(&mut self, function: &mut Function) {
                function.visit_mut_children_with(self);
                if let Some(frame) = self.0.frames.remove(&(function.span.lo.0, function.ctxt)) {
                    function.params = frame.params.expect("source generator parameters shielded");
                    let body = function.body.as_mut().expect("source generator body");
                    let at = insertion(body);
                    body.stmts.splice(at..at, frame.prefix);
                }
            }
        }
        program.visit_mut_with(&mut Restore(&mut self));
        assert!(
            self.frames.is_empty(),
            "source generator arguments activation retained"
        );
    }
}

fn fresh(name: &str) -> Ident {
    Ident::new(
        name.into(),
        DUMMY_SP,
        SyntaxContext::empty().apply_mark(Mark::new()),
    )
}
fn member(cell: &Ident) -> MemberExpr {
    MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(Expr::Ident(cell.clone())),
        prop: MemberProp::Ident(IdentName::new("v".into(), DUMMY_SP)),
    }
}
fn expr_stmt(expr: Expr) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(expr),
    })
}
fn assignment(cell: &Ident, value: Box<Expr>) -> Expr {
    Expr::Assign(AssignExpr {
        span: DUMMY_SP,
        op: AssignOp::Assign,
        left: AssignTarget::Simple(SimpleAssignTarget::Member(member(cell))),
        right: value,
    })
}
fn insertion(body: &FunctionBody) -> usize {
    body.stmts
        .iter()
        .take_while(|s| matches!(s,Stmt::Expr(e) if matches!(&*e.expr,Expr::Lit(Lit::Str(_)))))
        .count()
}
fn names(pattern: &Pat) -> Vec<Ident> {
    struct Names(Vec<Ident>);
    impl Visit for Names {
        fn visit_binding_ident(&mut self, id: &BindingIdent) {
            self.0.push(id.id.clone());
        }
    }
    let mut names = Names(Vec::new());
    pattern.visit_with(&mut names);
    names.0
}

#[derive(Default)]
struct Scan {
    excluded: HashSet<Id>,
    original: Option<Ident>,
    references: Vec<Ident>,
    body_var: bool,
    arrows: u32,
    depth: u32,
    uses: bool,
    eval: bool,
    eval_calls: HashSet<u32>,
}
impl Visit for Scan {
    fn visit_bin_expr(&mut self, b: &BinExpr) {
        mangler_jsast::deep::walk_binary(b, self)
    }
    fn visit_function(&mut self, _: &Function) {}
    fn visit_stmt(&mut self, s: &Stmt) {
        self.depth += 1;
        s.visit_children_with(self);
        self.depth -= 1;
    }
    fn visit_ident(&mut self, id: &Ident) {
        if id.sym == *"arguments" {
            self.original.get_or_insert_with(|| id.clone());
            self.references.push(id.clone());
            self.uses = true;
        }
    }
    fn visit_var_decl(&mut self, v: &VarDecl) {
        for d in &v.decls {
            for id in names(&d.name) {
                if id.sym == *"arguments" {
                    if v.kind != VarDeclKind::Var || self.arrows > 0 {
                        self.excluded.insert(id.to_id());
                    } else {
                        self.body_var = true;
                        self.original.get_or_insert(id);
                    }
                }
            }
        }
        v.visit_children_with(self);
    }
    fn visit_fn_decl(&mut self, f: &FnDecl) {
        if f.ident.sym == *"arguments" {
            if self.depth == 1 && self.arrows == 0 {
                self.body_var = true;
                self.original.get_or_insert_with(|| f.ident.clone());
            } else {
                self.excluded.insert(f.ident.to_id());
            }
        }
    }
    fn visit_catch_clause(&mut self, c: &CatchClause) {
        if let Some(p) = &c.param {
            for id in names(p) {
                if id.sym == *"arguments" {
                    self.excluded.insert(id.to_id());
                }
            }
        }
        c.visit_children_with(self);
    }
    fn visit_arrow_expr(&mut self, a: &ArrowExpr) {
        for p in &a.params {
            for id in names(p) {
                if id.sym == *"arguments" {
                    self.excluded.insert(id.to_id());
                }
            }
        }
        self.arrows += 1;
        a.visit_children_with(self);
        self.arrows -= 1;
    }
    fn visit_call_expr(&mut self, c: &CallExpr) {
        if mangler_jsast::analysis::scope::is_direct_eval_callee(&c.callee) {
            self.eval = true;
            self.eval_calls.insert(c.span.lo.0);
        }
        c.visit_children_with(self);
    }
}
struct Rewrite<'a> {
    cell: &'a Ident,
    excluded: &'a HashSet<Id>,
    aliases: &'a mut Aliases,
    references: &'a mut References,
    hoisted: Vec<Ident>,
    functions: Vec<Stmt>,
}
impl Rewrite<'_> {
    fn reference(&mut self, id: &Ident) -> MemberExpr {
        let mut reference = member(self.cell);
        reference.span = id.span;
        self.references
            .insert(id.span.lo.0, ("arguments".into(), self.cell.clone()));
        reference
    }
    fn matches(&self, id: &Ident) -> bool {
        id.sym == *"arguments" && !self.excluded.contains(&id.to_id())
    }
    fn pattern(&mut self, p: &mut Pat) {
        match p {
            Pat::Ident(id) if self.matches(&id.id) => {
                *p = Pat::Expr(Box::new(Expr::Member(self.reference(&id.id))))
            }
            Pat::Object(object) => {
                for property in &mut object.props {
                    match property {
                        ObjectPatProp::Assign(a) if self.matches(&a.key.id) => {
                            let mut value =
                                Pat::Expr(Box::new(Expr::Member(self.reference(&a.key.id))));
                            if let Some(default) = a.value.take() {
                                value = Pat::Assign(AssignPat {
                                    span: a.span,
                                    left: Box::new(value),
                                    right: default,
                                });
                            }
                            *property = ObjectPatProp::KeyValue(KeyValuePatProp {
                                key: PropName::Ident(IdentName::new(
                                    a.key.id.sym.clone(),
                                    a.key.id.span,
                                )),
                                value: Box::new(value),
                            });
                        }
                        ObjectPatProp::KeyValue(k) => self.pattern(&mut k.value),
                        ObjectPatProp::Rest(r) => self.pattern(&mut r.arg),
                        _ => {}
                    }
                }
            }
            Pat::Array(a) => {
                for p in a.elems.iter_mut().flatten() {
                    self.pattern(p)
                }
            }
            Pat::Rest(r) => self.pattern(&mut r.arg),
            Pat::Assign(a) => self.pattern(&mut a.left),
            _ => {}
        }
    }
    fn head(&mut self, head: &mut ForHead) {
        if let ForHead::VarDecl(v) = head
            && v.kind == VarDeclKind::Var
            && v.decls
                .iter()
                .any(|d| names(&d.name).iter().any(|id| self.matches(id)))
        {
            let mut declaration = v.decls.remove(0);
            let ordinary: Vec<Ident> = names(&declaration.name)
                .into_iter()
                .filter(|id| !self.matches(id))
                .collect();
            self.hoisted.extend(ordinary);
            self.pattern(&mut declaration.name);
            *head = ForHead::Pat(Box::new(declaration.name));
        }
    }
}
impl VisitMut for Rewrite<'_> {
    fn visit_mut_bin_expr(&mut self, b: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(b, self)
    }
    fn visit_mut_function(&mut self, _: &mut Function) {}
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        if let Expr::Ident(id) = e
            && self.matches(id)
        {
            *e = Expr::Member(self.reference(id));
            return;
        }
        e.visit_mut_children_with(self);
    }
    fn visit_mut_simple_assign_target(&mut self, t: &mut SimpleAssignTarget) {
        if let SimpleAssignTarget::Ident(id) = t
            && self.matches(&id.id)
        {
            *t = SimpleAssignTarget::Member(self.reference(&id.id));
            return;
        }
        t.visit_mut_children_with(self);
    }
    fn visit_mut_assign_target_pat(&mut self, p: &mut AssignTargetPat) {
        let mut pat = match p.take() {
            AssignTargetPat::Array(a) => Pat::Array(a),
            AssignTargetPat::Object(o) => Pat::Object(o),
            AssignTargetPat::Invalid(i) => Pat::Invalid(i),
        };
        self.pattern(&mut pat);
        pat.visit_mut_children_with(self);
        *p = match pat {
            Pat::Array(a) => AssignTargetPat::Array(a),
            Pat::Object(o) => AssignTargetPat::Object(o),
            _ => unreachable!(),
        };
    }
    fn visit_mut_prop(&mut self, p: &mut Prop) {
        if let Prop::Shorthand(id) = p
            && self.matches(id)
        {
            *p = Prop::KeyValue(KeyValueProp {
                key: PropName::Ident(IdentName::new(id.sym.clone(), id.span)),
                value: Box::new(Expr::Member(self.reference(id))),
            });
            return;
        }
        p.visit_mut_children_with(self);
    }
    fn visit_mut_var_decl(&mut self, v: &mut VarDecl) {
        if v.kind != VarDeclKind::Var {
            v.visit_mut_children_with(self);
            return;
        }
        let mut result = Vec::new();
        for mut d in std::mem::take(&mut v.decls) {
            if names(&d.name).iter().any(|id| self.matches(id)) {
                for id in names(&d.name).into_iter().filter(|id| !self.matches(id)) {
                    result.push(VarDeclarator {
                        span: DUMMY_SP,
                        name: Pat::Ident(id.into()),
                        init: None,
                        definite: false,
                    });
                }
                let dummy = fresh("_arguments_unused");
                self.pattern(&mut d.name);
                d.name.visit_mut_children_with(self);
                let init = d.init.take().map(|mut value| {
                    value.visit_mut_with(self);
                    Box::new(Expr::Assign(AssignExpr {
                        span: d.span,
                        op: AssignOp::Assign,
                        left: d.name.try_into().expect("argument assignment pattern"),
                        right: value,
                    }))
                });
                result.push(VarDeclarator {
                    span: d.span,
                    name: Pat::Ident(dummy.into()),
                    init,
                    definite: false,
                });
            } else {
                d.visit_mut_children_with(self);
                result.push(d);
            }
        }
        v.decls = result;
    }
    fn visit_mut_stmt(&mut self, s: &mut Stmt) {
        if let Stmt::Decl(Decl::Fn(f)) = s
            && self.matches(&f.ident)
        {
            let function = std::mem::take(&mut f.function);
            let id = f.ident.clone();
            self.functions.push(expr_stmt(assignment(
                self.cell,
                Box::new(Expr::Fn(FnExpr {
                    ident: Some(id),
                    function,
                })),
            )));
            *s = Stmt::Empty(EmptyStmt { span: DUMMY_SP });
            return;
        }
        s.visit_mut_children_with(self);
    }
    fn visit_mut_for_of_stmt(&mut self, f: &mut ForOfStmt) {
        self.head(&mut f.left);
        f.visit_mut_children_with(self);
    }
    fn visit_mut_for_in_stmt(&mut self, f: &mut ForInStmt) {
        self.head(&mut f.left);
        f.visit_mut_children_with(self);
    }
    fn visit_mut_call_expr(&mut self, c: &mut CallExpr) {
        if mangler_jsast::analysis::scope::is_direct_eval_callee(&c.callee) {
            self.aliases.push((
                c.span.lo.0,
                vec![("arguments".into(), self.cell.clone(), false, Vec::new())],
            ));
        }
        c.visit_mut_children_with(self);
    }
}

pub(super) fn prepare(program: &mut Program, unresolved: Mark) -> Plan {
    fn strict_body(body: &FunctionBody) -> bool {
        mangler_jsast::directives::has_use_strict(&body.stmts)
    }
    struct Strict {
        active: bool,
        spans: HashSet<u32>,
    }
    impl Visit for Strict {
        fn visit_bin_expr(&mut self, b: &BinExpr) {
            mangler_jsast::deep::walk_binary(b, self)
        }
        fn visit_function(&mut self, f: &Function) {
            let old = self.active;
            self.active |= f.body.as_ref().is_some_and(strict_body);
            if self.active {
                self.spans.insert(f.span.lo.0);
            }
            f.visit_children_with(self);
            self.active = old;
        }
        fn visit_arrow_expr(&mut self, a: &ArrowExpr) {
            let old = self.active;
            self.active |=
                matches!(&*a.body,ArrowFunctionBody::FunctionBody(body) if strict_body(body));
            if self.active {
                self.spans.insert(a.span.lo.0);
            }
            a.visit_children_with(self);
            self.active = old;
        }
        fn visit_class(&mut self, c: &Class) {
            let old = self.active;
            self.active = true;
            c.visit_children_with(self);
            self.active = old;
        }
    }
    let initial = matches!(program, Program::Module(_))
        || matches!(program, Program::Script(script) if mangler_jsast::directives::has_use_strict(&script.body));
    let mut strict = Strict {
        active: initial,
        spans: HashSet::new(),
    };
    program.visit_with(&mut strict);
    let mut plan = Plan {
        entries: HashMap::new(),
        aliases: Vec::new(),
        references: HashMap::new(),
        unresolved,
        strict: strict.spans,
        generators: GeneratorArguments::default(),
    };
    struct Prepare<'a> {
        plan: &'a mut Plan,
        arrow: bool,
    }
    impl VisitMut for Prepare<'_> {
        fn visit_mut_bin_expr(&mut self, b: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(b, self)
        }
        fn visit_mut_function(&mut self, f: &mut Function) {
            let arrow = self.arrow;
            self.arrow = false;
            if !(f.is_async || f.is_generator) || f.span.is_dummy() {
                f.visit_mut_children_with(self);
                return;
            }
            let mut original = Function {
                params: std::mem::take(&mut f.params),
                ..Default::default()
            };
            f.params = mangler_jsast::deep::clone_function(&mut original).params;
            let original_params = original.params;
            let Some(body) = &mut f.body else { return };
            let mut scan = Scan::default();
            body.visit_with(&mut scan);
            f.params.visit_with(&mut scan);
            // An enclosing suspension activation already owns these eval
            // aliases. An arrow adds no arguments binding of its own.
            let inherited_eval = arrow
                && !scan.uses
                && !scan.body_var
                && scan.eval
                && scan.eval_calls.iter().all(|position| {
                    self.plan.aliases.iter().any(|(span, aliases)| {
                        span == position
                            && aliases.iter().any(|(name, _, _, _)| name == "arguments")
                    })
                });
            if (!scan.uses && !scan.eval) || inherited_eval {
                f.visit_mut_children_with(self);
                return;
            }
            struct Expressions(bool);
            impl Visit for Expressions {
                fn visit_expr(&mut self, _: &Expr) {
                    self.0 = true;
                }
            }
            let mut expressions = Expressions(false);
            f.params.visit_with(&mut expressions);
            let parameter = fresh("_source_arguments");
            let separate = (arrow || expressions.0) && scan.body_var;
            let body_cell = separate.then(|| fresh("_body_arguments"));
            let body_binding = body_cell.as_ref().unwrap_or(&parameter);
            let mut rewrite = Rewrite {
                cell: body_binding,
                excluded: &scan.excluded,
                aliases: &mut self.plan.aliases,
                references: &mut self.plan.references,
                hoisted: Vec::new(),
                functions: Vec::new(),
            };
            body.visit_mut_with(&mut rewrite);
            let mut prefix = Vec::new();
            if separate && !arrow {
                prefix.push(expr_stmt(assignment(
                    body_binding,
                    Box::new(Expr::Member(member(&parameter))),
                )));
            }
            if !rewrite.hoisted.is_empty() {
                prefix.push(Stmt::Decl(Decl::Var(Box::new(VarDecl {
                    span: DUMMY_SP,
                    ctxt: SyntaxContext::empty(),
                    kind: VarDeclKind::Var,
                    declare: false,
                    decls: rewrite
                        .hoisted
                        .into_iter()
                        .map(|id| VarDeclarator {
                            span: DUMMY_SP,
                            name: Pat::Ident(id.into()),
                            init: None,
                            definite: false,
                        })
                        .collect(),
                }))));
            }
            prefix.extend(rewrite.functions);
            let at = insertion(body);
            body.stmts.splice(at..at, prefix);
            let parameter_alias_start = self.plan.aliases.len();
            f.params.visit_mut_with(&mut Rewrite {
                cell: &parameter,
                excluded: &scan.excluded,
                aliases: &mut self.plan.aliases,
                references: &mut self.plan.references,
                hoisted: Vec::new(),
                functions: Vec::new(),
            });
            self.plan.entries.insert(
                f.span.lo.0,
                Entry {
                    parameter,
                    body: body_cell,
                    original: scan
                        .references
                        .into_iter()
                        .find(|id| !scan.excluded.contains(&id.to_id()))
                        .or(scan.original)
                        .unwrap_or_else(|| {
                            Ident::new("arguments".into(), DUMMY_SP, SyntaxContext::empty())
                        }),
                    source_generator: f.is_generator && !f.is_async,
                    nonsimple: f.params.iter().any(|p| !matches!(p.pat, Pat::Ident(_))),
                    original_params,
                    parameter_aliases: self.plan.aliases[parameter_alias_start..]
                        .iter()
                        .map(|(span, _)| *span)
                        .collect(),
                },
            );
            // Rewrite lexical arrows before preparing them independently. Their
            // late-restored accessors must not capture a generated generator's
            // own arguments object instead of this source activation.
            f.visit_mut_children_with(self);
        }
        fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
            if !arrow.is_async {
                arrow.visit_mut_children_with(self);
                return;
            }
            let body = match *arrow.body.take() {
                ArrowFunctionBody::FunctionBody(body) => body,
                ArrowFunctionBody::Expr(expr) => FunctionBody {
                    span: arrow.span,
                    stmts: vec![Stmt::Return(ReturnStmt {
                        span: arrow.span,
                        arg: Some(expr),
                    })],
                },
            };
            let mut function = Function {
                span: arrow.span,
                ctxt: arrow.ctxt,
                params: std::mem::take(&mut arrow.params)
                    .into_iter()
                    .map(|pat| Param {
                        span: DUMMY_SP,
                        decorators: Vec::new(),
                        pat,
                    })
                    .collect(),
                body: Some(body),
                is_async: true,
                ..Default::default()
            };
            self.arrow = true;
            self.visit_mut_function(&mut function);
            arrow.params = function.params.into_iter().map(|param| param.pat).collect();
            *arrow.body = ArrowFunctionBody::FunctionBody(function.body.unwrap());
        }
    }
    program.visit_mut_with(&mut Prepare {
        plan: &mut plan,
        arrow: false,
    });
    plan
}
impl Plan {
    /// Module generator declarations retain their native parameter activation
    /// when their transformed producer body is moved back into the declaration.
    pub(super) fn native_generators(&mut self, spans: impl IntoIterator<Item = u32>) {
        for span in spans {
            if let Some(entry) = self.entries.get_mut(&span) {
                entry.source_generator = true;
            }
        }
    }

    pub(super) fn restore_with_references(
        mut self,
        program: &mut Program,
    ) -> (Aliases, References, GeneratorArguments) {
        struct Restore<'a>(&'a mut Plan);
        impl VisitMut for Restore<'_> {
            fn visit_mut_bin_expr(&mut self, b: &mut BinExpr) {
                mangler_jsast::deep::walk_binary_mut(b, self)
            }
            fn visit_mut_function(&mut self, f: &mut Function) {
                if let Some(entry) = self.0.entries.remove(&f.span.lo.0) {
                    let stayed = f.params.len() == entry.original_params.len()
                        && f.params
                            .iter()
                            .zip(&entry.original_params)
                            .all(|(a, b)| a.pat.span() == b.pat.span());
                    if stayed {
                        f.params = entry.original_params;
                        self.0.aliases.retain(|(span, aliases)| {
                            !entry.parameter_aliases.contains(span)
                                || !aliases
                                    .iter()
                                    .any(|(_, cell, _, _)| cell.to_id() == entry.parameter.to_id())
                        });
                    }
                    let mut prefix = self.0.cell(
                        &entry.parameter,
                        Some(&entry.original),
                        self.0.strict.contains(&f.span.lo.0),
                    );
                    if let Some(body) = entry.body {
                        prefix.extend(self.0.cell(&body, None, false));
                    }
                    if entry.nonsimple && !f.params.iter().any(|p| matches!(p.pat, Pat::Rest(_))) {
                        f.params.push(Param {
                            span: DUMMY_SP,
                            decorators: Vec::new(),
                            pat: Pat::Rest(RestPat {
                                span: DUMMY_SP,
                                dot3_token: DUMMY_SP,
                                arg: Box::new(Pat::Ident(fresh("_source_rest").into())),
                                type_ann: None,
                            }),
                        });
                    }
                    if entry.source_generator {
                        self.0.generators.frames.insert(
                            (f.span.lo.0, f.ctxt),
                            GeneratorFrame {
                                prefix,
                                params: None,
                            },
                        );
                    } else if let Some(body) = &mut f.body {
                        let at = insertion(body);
                        body.stmts.splice(at..at, prefix);
                    }
                }
                f.visit_mut_children_with(self);
            }
            fn visit_mut_arrow_expr(&mut self, arrow: &mut ArrowExpr) {
                if let Some(entry) = self.0.entries.remove(&arrow.span.lo.0) {
                    let stayed = arrow.params.len() == entry.original_params.len()
                        && arrow
                            .params
                            .iter()
                            .zip(&entry.original_params)
                            .all(|(a, b)| a.span() == b.pat.span());
                    if stayed {
                        arrow.params = entry
                            .original_params
                            .into_iter()
                            .map(|param| param.pat)
                            .collect();
                        self.0.aliases.retain(|(span, aliases)| {
                            !entry.parameter_aliases.contains(span)
                                || !aliases
                                    .iter()
                                    .any(|(_, cell, _, _)| cell.to_id() == entry.parameter.to_id())
                        });
                    }
                    let mut prefix = self.0.cell(
                        &entry.parameter,
                        Some(&entry.original),
                        self.0.strict.contains(&arrow.span.lo.0),
                    );
                    if let Some(body) = entry.body {
                        prefix.extend(self.0.cell(&body, None, false));
                    }
                    let mut body = match *arrow.body.take() {
                        ArrowFunctionBody::FunctionBody(body) => body,
                        ArrowFunctionBody::Expr(expr) => FunctionBody {
                            span: arrow.span,
                            stmts: vec![Stmt::Return(ReturnStmt {
                                span: arrow.span,
                                arg: Some(expr),
                            })],
                        },
                    };
                    let at = insertion(&body);
                    body.stmts.splice(at..at, prefix);
                    *arrow.body = ArrowFunctionBody::FunctionBody(body);
                }
                arrow.visit_mut_children_with(self);
            }
        }
        program.visit_mut_with(&mut Restore(&mut self));
        (self.aliases, self.references, self.generators)
    }
    fn cell(&self, id: &Ident, argument: Option<&Ident>, strict: bool) -> Vec<Stmt> {
        let source = if argument.is_some() && strict {
            "var CELL=((get)=>({get v(){return get()}}))(()=>SOURCE);"
        } else if argument.is_some() {
            "var CELL=((get,set)=>({get v(){return get()},set v(value){set(value)}}))(()=>SOURCE,value=>SOURCE=value);"
        } else {
            "var CELL=((value)=>({get v(){return value},set v(next){value=next}}))(void 0);"
        };
        let mut helper = Js
            .parse(source, &ParseOpts::default())
            .expect("argument cell helper")
            .into_program();
        helper.visit_mut_with(&mut mangler_jsast::span::GeneratedSpans);
        resolver(self.unresolved, Mark::new(), false).process(&mut helper);
        struct Replace<'a> {
            id: &'a Ident,
            source: Option<&'a Ident>,
        }
        impl VisitMut for Replace<'_> {
            fn visit_mut_ident(&mut self, id: &mut Ident) {
                if id.sym == *"CELL" {
                    *id = self.id.clone();
                } else if id.sym == *"SOURCE" {
                    *id = self.source.unwrap().clone();
                }
            }
        }
        helper.visit_mut_with(&mut Replace {
            id,
            source: argument,
        });
        match helper {
            Program::Script(script) => script.body,
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn equivalent(source: &str) {
        let source = source.replace(
            "globalThis.__out=value",
            "globalThis.__out=JSON.stringify(value)",
        );
        let source = source.as_str();
        let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
        let selected = super::super::suspension_candidates(ast.program())
            .into_keys()
            .collect();
        let lowered = super::super::lower_with_lexicals(ast.program_mut(), &selected);
        mangler_jsast::directives::insert_program_statements(ast.program_mut(), lowered.helpers);
        let transformed = Js.print(&ast);
        {
            let node = mangler_testkit::cross_engine::node_path()
                .expect("Node is required to verify async argument environments");
            let values = mangler_testkit::cross_engine::evaluate_many(
                &mangler_testkit::cross_engine::Engine::Node(node),
                &[source, &transformed],
            )
            .expect("V8 suspended arguments");
            assert_eq!(values[0], values[1], "{source}\n{transformed}");
        }
    }
    #[test]
    fn nested_async_arrows_keep_source_arguments_and_new_target() {
        for source in [
            "async function pay(a){let saved=arguments;return async()=>async()=>saved===arguments}pay(3).then(f=>f()).then(f=>f()).then(value=>globalThis.__out=value)",
            "async function pay(){let f=async()=>arguments;arguments={v:9};return[(await f())===arguments,(await f()).v]}pay().then(value=>globalThis.__out=value)",
            "async function pay(){let read=()=>arguments;return async()=>{arguments=8;return read()}}pay().then(f=>f()).then(value=>globalThis.__out=value)",
            "async function pay(arguments){return async()=>arguments}pay(9).then(f=>f()).then(value=>globalThis.__out=value)",
            "function pay(){return async()=>async()=>new.target===pay}new pay()().then(f=>f()).then(value=>globalThis.__out=value)",
            "function pay(){return async(a=new.target)=>a===pay}new pay()().then(value=>globalThis.__out=value)",
            "async function pay(){return async()=>new.target===undefined}pay().then(f=>f()).then(value=>globalThis.__out=value)",
            "function* pay(){return async()=>new.target===undefined}pay().next().value().then(value=>globalThis.__out=value)",
            "async function pay(){function C(){return async()=>new.target===C}return new C()()}pay().then(value=>globalThis.__out=value)",
        ] {
            equivalent(source);
        }
    }

    #[test]
    fn nested_async_eval_reuses_the_enclosing_arguments_cell() {
        let mut ast = Js.parse("async function pay(){let saved=arguments;return async()=>saved===eval('arguments')}", &ParseOpts::default()).unwrap();
        let selected = super::super::suspension_candidates(ast.program())
            .into_keys()
            .collect();
        let lowered = super::super::lower_with_lexicals(ast.program_mut(), &selected);
        let cells: HashSet<_> = lowered
            .lexicals
            .values()
            .flatten()
            .filter(|alias| alias.name == "arguments")
            .map(|alias| &alias.cell)
            .collect();
        assert_eq!(
            cells.len(),
            1,
            "arrows must reuse their source activation's live arguments reference"
        );
    }

    #[test]
    fn generator_arguments_remain_live_in_the_original_activation() {
        for source in [
            "function* pay(a){var old=arguments;yield 0;a=8;return[old===arguments,arguments[0],arguments.callee===pay]}var g=pay(2);g.next();globalThis.__out=JSON.stringify(g.next().value)",
            "function* pay(a=()=>arguments){yield 0;arguments={value:3};return[a()===arguments,arguments.value]}var g=pay();g.next();globalThis.__out=JSON.stringify(g.next().value)",
            "function* pay(a=()=>arguments){var arguments=3;yield 0;return[a().length,arguments]}var g=pay();g.next();globalThis.__out=JSON.stringify(g.next().value)",
            "function* pay(){yield 0;arguments=4;return arguments}var g=pay();g.next();globalThis.__out=JSON.stringify(g.next().value)",
        ] {
            equivalent(source);
        }
    }

    #[test]
    fn eval_only_async_generator_defaults_keep_implicit_arguments_binding() {
        equivalent(
            "async function* pay(a=eval('var a=42')){}try{pay();globalThis.__out='missing error'}catch(error){globalThis.__out=error.name}",
        );
    }

    #[test]
    fn async_arguments_keep_original_identity_callee_and_parameter_map() {
        equivalent(
            "async function pay(a){var before=arguments;a=8;await 0;return [before===arguments,arguments[0],arguments.callee===pay]}pay(2).then(value=>globalThis.__out=value)",
        );
        equivalent(
            "async function pay(a){arguments={value:3};await 0;return [arguments.value,a]}pay(2).then(value=>globalThis.__out=value)",
        );
        equivalent(
            "async function pay(a){var {arguments,x}={arguments:3,x:4};await 0;return [arguments,x,a]}pay(2).then(value=>globalThis.__out=value)",
        );
        equivalent(
            "async function pay(){for(var arguments of [1,2])await 0;return arguments}pay().then(value=>globalThis.__out=value)",
        );
    }
    #[test]
    fn async_parameter_and_body_arguments_environments_remain_distinct() {
        equivalent(
            "async function pay(a=1){await 0;try{return arguments.callee}catch(e){return e.name}}pay().then(value=>globalThis.__out=value)",
        );
        equivalent(
            "async function pay(a=()=>arguments){var arguments=3;await 0;return [a().length,arguments]}pay().then(value=>globalThis.__out=value)",
        );
        equivalent(
            "async function pay(a=arguments){await 0;return a===arguments}pay().then(value=>globalThis.__out=value)",
        );
    }
    #[test]
    fn async_arrows_keep_lexical_arguments_and_local_shadowing() {
        equivalent(
            "function parent(a){return (async()=>{await 0;return arguments===saved})();var unused}var saved;function start(a){saved=arguments;return (async()=>{await 0;return [arguments===saved,arguments.callee===start]})()}start(2).then(value=>globalThis.__out=value)",
        );
        equivalent(
            "function start(){return (async()=>{var arguments;await 0;return typeof arguments})()}start(2).then(value=>globalThis.__out=value)",
        );
        equivalent(
            "function start(){return (async()=>{var arguments=3;await 0;return arguments})()}start(2).then(value=>globalThis.__out=value)",
        );
    }
    #[test]
    fn strict_async_arguments_getters_do_not_generate_invalid_assignments() {
        equivalent(
            "'use strict';async function pay(a){await 0;return [arguments[0],a]}pay(2).then(value=>globalThis.__out=value)",
        );
    }
}
