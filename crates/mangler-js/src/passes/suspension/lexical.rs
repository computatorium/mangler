//! Preserve lexical environments while generator lowering turns declarations into
//! state-machine storage. A binding is an explicit cell, allocated on scope entry
//! and initialized at the original declaration. Native block-scoping lowering still
//! owns closure capture and iteration environments around those cells.
use std::collections::{HashMap, HashSet};

use mangler_core::Language;
use mangler_jsast::{Js, ParseOpts};
use swc_core::common::{DUMMY_SP, Mark, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::transforms::base::resolver;
use swc_core::ecma::transforms::compat::es2015;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

#[derive(Clone)]
struct Helpers {
    cell: Ident,
    reference: Ident,
    clone_cell: Ident,
    with_object: Ident,
}
#[derive(Clone)]
struct Binding {
    cell: Ident,
    constant: bool,
    with_depth: usize,
}
type RawAlias = (String, Ident, bool, Vec<Ident>);
type RawScope = (u32, Vec<RawAlias>);
pub(super) type RawScopes = Vec<RawScope>;

fn fresh(name: &str) -> Ident {
    Ident::new(
        name.into(),
        DUMMY_SP,
        SyntaxContext::empty().apply_mark(Mark::new()),
    )
}
fn ident_expr(id: &Ident) -> Box<Expr> {
    Box::new(Expr::Ident(id.clone()))
}
fn undefined() -> Box<Expr> {
    Box::new(Expr::Unary(UnaryExpr {
        span: DUMMY_SP,
        op: UnaryOp::Void,
        arg: Box::new(Expr::Lit(Lit::Num(Number {
            span: DUMMY_SP,
            value: 0.,
            raw: None,
        }))),
    }))
}
fn boolean(value: bool) -> Box<Expr> {
    Box::new(Expr::Lit(Lit::Bool(Bool {
        span: DUMMY_SP,
        value,
    })))
}
fn call(function: &Ident, args: impl IntoIterator<Item = Box<Expr>>) -> Box<Expr> {
    Box::new(Expr::Call(CallExpr {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        callee: Callee::Expr(ident_expr(function)),
        args: args
            .into_iter()
            .map(|expr| ExprOrSpread { spread: None, expr })
            .collect(),
        type_args: None,
    }))
}
fn member(object: Box<Expr>, key: &str) -> MemberExpr {
    MemberExpr {
        span: DUMMY_SP,
        obj: object,
        prop: MemberProp::Ident(IdentName::new(key.into(), DUMMY_SP)),
    }
}
fn assign(left: AssignTarget, right: Box<Expr>) -> Box<Expr> {
    Box::new(Expr::Assign(AssignExpr {
        span: DUMMY_SP,
        op: AssignOp::Assign,
        left,
        right,
    }))
}
fn assign_ident(id: &Ident, value: Box<Expr>) -> Box<Expr> {
    assign(
        AssignTarget::Simple(SimpleAssignTarget::Ident(id.clone().into())),
        value,
    )
}
fn expression(value: Box<Expr>) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: value,
    })
}
fn sequence(expressions: impl IntoIterator<Item = Box<Expr>>) -> Box<Expr> {
    let mut expressions: Vec<_> = expressions.into_iter().collect();
    if expressions.len() == 1 {
        expressions.pop().unwrap()
    } else {
        Box::new(Expr::Seq(SeqExpr {
            span: DUMMY_SP,
            exprs: expressions,
        }))
    }
}
fn declaration(kind: VarDeclKind, bindings: Vec<(Ident, Box<Expr>)>) -> VarDecl {
    VarDecl {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        kind,
        declare: false,
        decls: bindings
            .into_iter()
            .map(|(id, init)| VarDeclarator {
                span: DUMMY_SP,
                name: Pat::Ident(id.into()),
                init: Some(init),
                definite: false,
            })
            .collect(),
    }
}
fn pattern_ids(pattern: &Pat) -> Vec<Ident> {
    struct Names(Vec<Ident>);
    impl Visit for Names {
        fn visit_binding_ident(&mut self, binding: &BindingIdent) {
            self.0.push(binding.id.clone());
        }
        fn visit_expr(&mut self, _: &Expr) {}
    }
    let mut names = Names(Vec::new());
    pattern.visit_with(&mut names);
    names.0
}

/// Generate only the bindings needed by selected generator bodies. The helper
/// identities have fresh resolver contexts and cannot capture source names.
pub(super) fn lower_with_references(
    program: &mut Program,
    unresolved: Mark,
    initial_aliases: RawScopes,
    reference_plans: &mut HashMap<u32, String>,
    initial_references: HashMap<u32, (String, Ident)>,
    switch_declarations: &mangler_jsast::SwitchSuspensionDeclarations,
) -> RawScopes {
    let source = r#"
function _lexical_cell(c){var v,r=false;return {c:c,get v(){if(!r)throw new ReferenceError('Uninitialized lexical binding');return v},set v(n){if(!r)throw new ReferenceError('Uninitialized lexical binding');if(c)throw new TypeError('Assignment to constant variable');v=n},set i(n){if(r)throw new TypeError('Binding already initialized');v=n;r=true}}}
function _lexical_ref(c){if(c===void 0)throw new ReferenceError('Uninitialized lexical binding');return c}
function _lexical_clone(c){var n=_lexical_cell(c.c);n.i=c.v;return n}
function _lexical_with(o){if(o===null||o===void 0)throw new TypeError('Cannot convert null or undefined to object');return Object(o)}
"#;
    let mut helper = Js
        .parse(source, &ParseOpts::default())
        .expect("lexical cell helpers parse")
        .into_program();
    resolver(unresolved, Mark::new(), false).process(&mut helper);
    let Program::Script(mut helper) = helper else {
        unreachable!()
    };
    let ids: Vec<Ident> = helper
        .body
        .iter()
        .map(|stmt| match stmt {
            Stmt::Decl(Decl::Fn(f)) => {
                let mut id = f.ident.clone();
                id.span = DUMMY_SP;
                id
            }
            _ => unreachable!(),
        })
        .collect();
    let helpers = Helpers {
        cell: ids[0].clone(),
        reference: ids[1].clone(),
        clone_cell: ids[2].clone(),
        with_object: ids[3].clone(),
    };
    struct LowerGenerators<'a> {
        switch_declarations: &'a mangler_jsast::SwitchSuspensionDeclarations,
        helpers: Helpers,
        used: bool,
        aliases: HashMap<u32, HashMap<String, Ident>>,
        references: HashMap<u32, String>,
        initial_references: HashMap<u32, (String, Ident)>,
        alias_objects: HashMap<(u32, String), Vec<Ident>>,
        argument_objects: HashMap<u32, Vec<Ident>>,
        has_initial_aliases: bool,
    }
    impl VisitMut for LowerGenerators<'_> {
        fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(e, self);
        }
        fn visit_mut_function(&mut self, function: &mut Function) {
            function.visit_mut_children_with(self);
            if !function.is_generator {
                return;
            }
            let Some(body) = &mut function.body else {
                return;
            };
            // The RHS environment of a lexical iterator head remains permanently
            // uninitialized, including in closures created by that expression.
            let mut heads = IteratorHeads {
                helpers: &self.helpers,
                hoisted: Vec::new(),
                aliases: HashMap::new(),
            };
            body.visit_mut_with(&mut heads);
            if !heads.hoisted.is_empty() {
                body.stmts.insert(
                    directives(&body.stmts),
                    Stmt::Decl(Decl::Var(Box::new(VarDecl {
                        span: DUMMY_SP,
                        ctxt: SyntaxContext::empty(),
                        kind: VarDeclKind::Var,
                        declare: false,
                        decls: heads
                            .hoisted
                            .into_iter()
                            .map(|id| VarDeclarator {
                                span: DUMMY_SP,
                                name: Pat::Ident(id.into()),
                                init: None,
                                definite: false,
                            })
                            .collect(),
                    }))),
                );
            }
            for (span, aliases) in heads.aliases {
                let existing = self.aliases.entry(span).or_default();
                for (name, cell) in aliases {
                    existing.entry(name).or_insert(cell);
                }
            }
            let mut temporary = Program::Script(Script {
                span: DUMMY_SP,
                body: std::mem::take(&mut body.stmts),
                shebang: None,
            });
            es2015::for_of::for_of(Default::default()).process(&mut temporary);
            let Program::Script(script) = temporary else {
                unreachable!()
            };
            body.stmts = script.body;
            let mut collector = Collect {
                switch_declarations: self.switch_declarations,
                bindings: HashMap::new(),
                with_depth: 0,
            };
            body.visit_with(&mut collector);
            let with_ids = with_object_ids(body);
            if collector.bindings.is_empty()
                && self.initial_references.is_empty()
                && !self.has_initial_aliases
            {
                return;
            }
            self.used = true;
            let mut scopes = EvalScopes {
                bindings: &collector.bindings,
                cell_depths: collector
                    .bindings
                    .values()
                    .map(|binding| (binding.cell.to_id(), binding.with_depth))
                    .collect(),
                frames: Vec::new(),
                aliases: HashMap::new(),
                with_ids: &with_ids,
                with_objects: Vec::new(),
                function_with_depth: 0,
                alias_objects: HashMap::new(),
                argument_objects: HashMap::new(),
            };
            scopes.function(&function.params, body);
            for (key, objects) in scopes.alias_objects {
                self.alias_objects.entry(key).or_insert(objects);
            }
            for (key, objects) in scopes.argument_objects {
                self.argument_objects.entry(key).or_insert(objects);
            }
            for (span, aliases) in scopes.aliases {
                let existing = self.aliases.entry(span).or_default();
                for (name, cell) in aliases {
                    existing.entry(name).or_insert(cell);
                }
            }
            let mut rewrite = Rewrite {
                helpers: &self.helpers,
                bindings: collector.bindings,
                current_renames: HashMap::new(),
                eval_renames: HashMap::new(),
                with_objects: Vec::new(),
                references: HashMap::new(),
                initial_references: self.initial_references.clone(),
                function_with_depth: 0,
                with_ids,
            };
            body.visit_mut_with(&mut rewrite);
            self.references.extend(rewrite.references);
            for (span, renames) in rewrite.eval_renames {
                if let Some(aliases) = self.aliases.get_mut(&span) {
                    for cell in aliases.values_mut() {
                        if let Some(renamed) = renames.get(&cell.to_id()) {
                            *cell = renamed.clone();
                        }
                    }
                }
            }
        }
    }
    let mut transform = LowerGenerators {
        switch_declarations,
        helpers,
        used: false,
        aliases: HashMap::new(),
        references: HashMap::new(),
        initial_references,
        alias_objects: HashMap::new(),
        argument_objects: HashMap::new(),
        has_initial_aliases: !initial_aliases.is_empty(),
    };
    program.visit_mut_with(&mut transform);
    reference_plans.extend(transform.references);
    if transform.used {
        struct Generated;
        impl VisitMut for Generated {
            fn visit_mut_span(&mut self, span: &mut swc_core::common::Span) {
                *span = DUMMY_SP;
            }
        }
        helper.visit_mut_with(&mut Generated);
        super::generated::certify_helpers(&mut helper.body);
        match program {
            Program::Script(script) => {
                let at = directives(&script.body);
                script.body.splice(at..at, helper.body);
            }
            Program::Module(module) => {
                let at = module.body.iter().take_while(|s| matches!(s, ModuleItem::Stmt(Stmt::Expr(e)) if matches!(&*e.expr, Expr::Lit(Lit::Str(_))))).count();
                module
                    .body
                    .splice(at..at, helper.body.into_iter().map(ModuleItem::Stmt));
            }
        }
    }
    let mut alias_kinds = HashMap::new();
    for (span, aliases) in initial_aliases {
        let existing = transform.aliases.entry(span).or_default();
        for (name, cell, lexical, objects) in aliases {
            if let std::collections::hash_map::Entry::Vacant(entry) = existing.entry(name.clone()) {
                entry.insert(cell);
                let objects = if objects.is_empty() {
                    transform
                        .argument_objects
                        .get(&span)
                        .cloned()
                        .unwrap_or_default()
                } else {
                    objects
                };
                transform
                    .alias_objects
                    .insert((span, name.clone()), objects);
                alias_kinds.insert((span, name), lexical);
            }
        }
    }
    let mut aliases: Vec<_> = transform
        .aliases
        .into_iter()
        .map(|(span, bindings)| {
            let mut bindings: Vec<_> = bindings
                .into_iter()
                .map(|(name, cell)| {
                    let lexical = alias_kinds
                        .get(&(span, name.clone()))
                        .copied()
                        .unwrap_or(true);
                    let objects = transform
                        .alias_objects
                        .remove(&(span, name.clone()))
                        .unwrap_or_default();
                    (name, cell, lexical, objects)
                })
                .collect();
            bindings.sort_by(|a, b| a.0.cmp(&b.0));
            (span, bindings)
        })
        .collect();
    aliases.sort_by_key(|(span, _)| *span);
    attach_eval_references(program, &aliases);
    aliases
}

pub(super) const EVAL_REFERENCES: &str = "\0mangler_suspension_lexicals";

/// Reference calls retain their source spans while their explicit cell/object
/// arguments follow capture substitution and hygiene in the actual environment.
pub(super) fn take_lexical_references(
    program: &mut Program,
    plans: &HashMap<u32, String>,
) -> mangler_vm::eval::SuspensionLexicalReferences {
    struct Take<'a> {
        plans: &'a HashMap<u32, String>,
        references: mangler_vm::eval::SuspensionLexicalReferences,
    }
    impl Visit for Take<'_> {
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            mangler_jsast::deep::walk_binary(e, self);
        }
        fn visit_call_expr(&mut self, call: &CallExpr) {
            let generated = matches!(&call.callee, Callee::Expr(callee) if matches!(&**callee, Expr::Ident(id) if id.span.is_dummy()));
            if generated && let Some(name) = self.plans.get(&call.span.lo.0) {
                let mut names = call.args.iter().map(|argument| {
                    let Expr::Ident(id) = &*argument.expr else {
                        panic!("projected reference capture lost during suspension lowering")
                    };
                    id.sym.to_string()
                });
                let cell = names.next().expect("projected reference cell");
                self.references.insert(
                    call.span.lo.0,
                    mangler_vm::eval::SuspensionLexicalReference {
                        name: name.clone(),
                        cell,
                        objects: names.collect(),
                    },
                );
            }
            call.visit_children_with(self);
        }
    }
    let mut take = Take {
        plans,
        references: HashMap::new(),
    };
    program.visit_with(&mut take);
    assert_eq!(
        take.references.len(),
        plans.len(),
        "projected references survive suspension lowering"
    );
    take.references
}

/// These temporary arguments make string-only references visible to the upstream
/// closure and iteration transforms. They are removed after hygiene, before any
/// generated program can run or enter bytecode compilation.
fn attach_eval_references(program: &mut Program, aliases: &[RawScope]) {
    struct Attach<'a>(HashMap<u32, &'a Vec<RawAlias>>);
    impl VisitMut for Attach<'_> {
        fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(e, self);
        }
        fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
            call.visit_mut_children_with(self);
            let Some(aliases) = self.0.get(&call.span.lo.0) else {
                return;
            };
            let marker = Box::new(Expr::Lit(Lit::Str(Str {
                span: DUMMY_SP,
                value: EVAL_REFERENCES.into(),
                raw: None,
            })));
            let elems = std::iter::once(marker)
                .chain(aliases.iter().flat_map(|(_, id, _, objects)| {
                    std::iter::once(id).chain(objects.iter()).map(ident_expr)
                }))
                .map(|expr| Some(ExprOrSpread { spread: None, expr }))
                .collect();
            call.args.push(ExprOrSpread {
                spread: None,
                expr: Box::new(Expr::Array(ArrayLit {
                    span: DUMMY_SP,
                    elems,
                })),
            });
        }
    }
    program.visit_mut_with(&mut Attach(
        aliases.iter().map(|(span, ids)| (*span, ids)).collect(),
    ));
}

/// Read names in each actual call environment, including parameters introduced
/// for per-iteration captures. A top-level hygiene sidecar cannot see those
/// context-specific substitutions.
pub(super) fn take_eval_references(
    program: &mut Program,
    scopes: &[RawScope],
) -> mangler_vm::eval::SuspensionLexicalScopes {
    struct Take {
        positions: HashSet<u32>,
        names: HashMap<u32, Vec<String>>,
    }
    impl VisitMut for Take {
        fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(e, self);
        }
        fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
            call.visit_mut_children_with(self);
            if !self.positions.contains(&call.span.lo.0) {
                return;
            }
            let Some(last) = call.args.last() else { return };
            let Expr::Array(array) = &*last.expr else {
                return;
            };
            let Some(Some(marker)) = array.elems.first() else {
                return;
            };
            if !matches!(&*marker.expr, Expr::Lit(Lit::Str(s)) if s.value == EVAL_REFERENCES) {
                return;
            }
            let names = array
                .elems
                .iter()
                .skip(1)
                .map(|element| {
                    let Expr::Ident(id) = &*element.as_ref().expect("lexical reference").expr
                    else {
                        panic!("lexical capture reference lost during suspension lowering")
                    };
                    id.sym.to_string()
                })
                .collect();
            self.names.insert(call.span.lo.0, names);
            call.args.pop();
        }
    }
    let mut take = Take {
        positions: scopes.iter().map(|(span, _)| *span).collect(),
        names: HashMap::new(),
    };
    program.visit_mut_with(&mut take);
    scopes
        .iter()
        .map(|(span, aliases)| {
            let names = take
                .names
                .remove(span)
                .expect("eval lexical references survive suspension lowering");
            assert_eq!(
                names.len(),
                aliases
                    .iter()
                    .map(|(_, _, _, objects)| 1 + objects.len())
                    .sum::<usize>()
            );
            let mut names = names.into_iter();
            (
                *span,
                aliases
                    .iter()
                    .map(
                        |(name, _, lexical, objects)| mangler_vm::eval::SuspensionLexicalAlias {
                            name: name.clone(),
                            cell: names.next().unwrap(),
                            lexical: *lexical,
                            objects: (0..objects.len()).map(|_| names.next().unwrap()).collect(),
                        },
                    )
                    .collect(),
            )
        })
        .collect()
}
fn directives(stmts: &[Stmt]) -> usize {
    stmts
        .iter()
        .take_while(|s| matches!(s, Stmt::Expr(e) if matches!(&*e.expr, Expr::Lit(Lit::Str(_)))))
        .count()
}

/// Keep source lexical visibility alongside eval sites before cells and state
/// callbacks flatten those scopes. Function parameters and non-cell declarations
/// mask an outer lexical alias just as they do in the original environment.
struct EvalScopes<'a> {
    bindings: &'a HashMap<Id, Binding>,
    cell_depths: HashMap<Id, usize>,
    frames: Vec<HashMap<String, Option<Ident>>>,
    aliases: HashMap<u32, HashMap<String, Ident>>,
    with_ids: &'a HashMap<u32, Ident>,
    with_objects: Vec<Ident>,
    function_with_depth: usize,
    alias_objects: HashMap<(u32, String), Vec<Ident>>,
    argument_objects: HashMap<u32, Vec<Ident>>,
}
fn with_object_ids(body: &FunctionBody) -> HashMap<u32, Ident> {
    struct Objects(HashMap<u32, Ident>);
    impl Visit for Objects {
        fn visit_bin_expr(&mut self, e: &BinExpr) {
            mangler_jsast::deep::walk_binary(e, self);
        }
        fn visit_with_stmt(&mut self, statement: &WithStmt) {
            let object = match &*statement.obj {
                Expr::Ident(id) if id.span.is_dummy() => id.clone(),
                _ => fresh("_lex_with_object"),
            };
            self.0.insert(statement.span.lo.0, object);
            statement.visit_children_with(self);
        }
    }
    let mut objects = Objects(HashMap::new());
    body.visit_with(&mut objects);
    objects.0
}
impl EvalScopes<'_> {
    fn binding(&self, id: &Ident) -> (String, Option<Ident>) {
        (
            id.sym.to_string(),
            self.bindings
                .get(&id.to_id())
                .map(|binding| binding.cell.clone()),
        )
    }
    fn block(&self, statements: &[Stmt]) -> HashMap<String, Option<Ident>> {
        let mut frame = HashMap::new();
        for statement in statements {
            match statement {
                Stmt::Decl(Decl::Var(d)) if d.kind != VarDeclKind::Var => {
                    for id in d.decls.iter().flat_map(|d| pattern_ids(&d.name)) {
                        let (name, binding) = self.binding(&id);
                        frame.insert(name, binding);
                    }
                }
                Stmt::Decl(Decl::Class(c)) => {
                    let (name, binding) = self.binding(&c.ident);
                    frame.insert(name, binding);
                }
                Stmt::Decl(Decl::Fn(f)) => {
                    let (name, binding) = self.binding(&f.ident);
                    frame.insert(name, binding);
                }
                _ => {}
            }
        }
        frame
    }
    fn function(&mut self, params: &[Param], body: &FunctionBody) {
        struct Variables(HashMap<String, Option<Ident>>);
        impl Visit for Variables {
            fn visit_function(&mut self, _: &Function) {}
            fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
            fn visit_class(&mut self, _: &Class) {}
            fn visit_var_decl(&mut self, d: &VarDecl) {
                if d.kind == VarDeclKind::Var {
                    for id in d.decls.iter().flat_map(|d| pattern_ids(&d.name)) {
                        self.0.insert(id.sym.to_string(), None);
                    }
                }
                d.visit_children_with(self);
            }
        }
        let mut variables = Variables(HashMap::new());
        body.visit_with(&mut variables);
        let parameters = params
            .iter()
            .flat_map(|p| pattern_ids(&p.pat))
            .map(|id| (id.sym.to_string(), None))
            .collect();
        self.frames.push(parameters);
        params.visit_with(self);
        self.frames.push(variables.0);
        self.frames.push(self.block(&body.stmts));
        body.visit_with(self);
        self.frames.pop();
        self.frames.pop();
        self.frames.pop();
    }
}
impl Visit for EvalScopes<'_> {
    fn visit_with_stmt(&mut self, statement: &WithStmt) {
        statement.obj.visit_with(self);
        if let Some(object) = self.with_ids.get(&statement.span.lo.0) {
            self.with_objects.push(object.clone());
            statement.body.visit_with(self);
            self.with_objects.pop();
        } else {
            statement.body.visit_with(self);
        }
    }
    fn visit_bin_expr(&mut self, e: &BinExpr) {
        mangler_jsast::deep::walk_binary(e, self);
    }
    fn visit_block_stmt(&mut self, block: &BlockStmt) {
        self.frames.push(self.block(&block.stmts));
        block.visit_children_with(self);
        self.frames.pop();
    }
    fn visit_function(&mut self, function: &Function) {
        let previous = self.function_with_depth;
        self.function_with_depth = self.with_objects.len();
        self.frames
            .push(HashMap::from([("arguments".to_string(), None)]));
        if let Some(body) = &function.body {
            self.function(&function.params, body);
        }
        self.frames.pop();
        self.function_with_depth = previous;
    }
    fn visit_fn_expr(&mut self, expression: &FnExpr) {
        let frame = expression
            .ident
            .as_ref()
            .map(|id| HashMap::from([(id.sym.to_string(), None)]))
            .unwrap_or_default();
        self.frames.push(frame);
        expression.function.visit_with(self);
        self.frames.pop();
    }
    fn visit_class_expr(&mut self, expression: &ClassExpr) {
        let frame = expression
            .ident
            .as_ref()
            .map(|id| HashMap::from([(id.sym.to_string(), None)]))
            .unwrap_or_default();
        self.frames.push(frame);
        expression.class.visit_with(self);
        self.frames.pop();
    }
    fn visit_class_decl(&mut self, declaration: &ClassDecl) {
        self.frames
            .push(HashMap::from([(declaration.ident.sym.to_string(), None)]));
        declaration.class.visit_with(self);
        self.frames.pop();
    }
    fn visit_arrow_expr(&mut self, arrow: &ArrowExpr) {
        let params: Vec<Param> = arrow
            .params
            .iter()
            .cloned()
            .map(|pat| Param {
                span: DUMMY_SP,
                decorators: vec![],
                pat,
            })
            .collect();
        match &*arrow.body {
            ArrowFunctionBody::FunctionBody(body) => self.function(&params, body),
            ArrowFunctionBody::Expr(expr) => {
                let frame = params
                    .iter()
                    .flat_map(|p| pattern_ids(&p.pat))
                    .map(|id| (id.sym.to_string(), None))
                    .collect();
                self.frames.push(frame);
                params.visit_with(self);
                expr.visit_with(self);
                self.frames.pop();
            }
        }
    }
    fn visit_for_stmt(&mut self, loop_: &ForStmt) {
        let mut frame = HashMap::new();
        if let Some(VarDeclOrExpr::VarDecl(declaration)) = &loop_.init
            && declaration.kind != VarDeclKind::Var
        {
            for id in declaration.decls.iter().flat_map(|d| pattern_ids(&d.name)) {
                let (name, binding) = self.binding(&id);
                frame.insert(name, binding);
            }
        }
        self.frames.push(frame);
        loop_.visit_children_with(self);
        self.frames.pop();
    }
    fn visit_catch_clause(&mut self, catch: &CatchClause) {
        let frame = catch
            .param
            .as_ref()
            .map(pattern_ids)
            .unwrap_or_default()
            .iter()
            .map(|id| self.binding(id))
            .collect();
        self.frames.push(frame);
        catch.visit_children_with(self);
        self.frames.pop();
    }
    fn visit_switch_stmt(&mut self, switch: &SwitchStmt) {
        switch.discriminant.visit_with(self);
        let frame = switch
            .cases
            .iter()
            .flat_map(|case| self.block(&case.cons))
            .collect();
        self.frames.push(frame);
        switch.cases.visit_with(self);
        self.frames.pop();
    }
    fn visit_call_expr(&mut self, call: &CallExpr) {
        if mangler_jsast::analysis::scope::is_direct_eval_callee(&call.callee) {
            let mut aliases = HashMap::new();
            for frame in &self.frames {
                for (name, binding) in frame {
                    if let Some(binding) = binding {
                        aliases.insert(name.clone(), binding.clone());
                    } else {
                        aliases.remove(name);
                    }
                }
            }
            for (name, cell) in &aliases {
                let depth = self
                    .cell_depths
                    .get(&cell.to_id())
                    .copied()
                    .unwrap_or(usize::MAX);
                self.alias_objects.insert(
                    (call.span.lo.0, name.clone()),
                    self.with_objects.iter().skip(depth).cloned().collect(),
                );
            }
            self.argument_objects.insert(
                call.span.lo.0,
                self.with_objects
                    .iter()
                    .skip(self.function_with_depth)
                    .cloned()
                    .collect(),
            );
            self.aliases.insert(call.span.lo.0, aliases);
        }
        call.visit_children_with(self);
    }
}

struct Collect<'a> {
    switch_declarations: &'a mangler_jsast::SwitchSuspensionDeclarations,
    bindings: HashMap<Id, Binding>,
    with_depth: usize,
}
impl Collect<'_> {
    fn pattern(&mut self, pattern: &Pat, constant: bool) {
        for id in pattern_ids(pattern) {
            self.bindings.entry(id.to_id()).or_insert_with(|| Binding {
                cell: fresh(&format!("_lex_{}", id.sym)),
                constant,
                with_depth: self.with_depth,
            });
        }
    }
}
impl Visit for Collect<'_> {
    fn visit_switch_stmt(&mut self, switch: &SwitchStmt) {
        for case in &switch.cases {
            for statement in &case.cons {
                if let Stmt::Decl(Decl::Fn(function)) = statement
                    && self.switch_declarations.contains(&function.ident)
                {
                    self.pattern(&Pat::Ident(function.ident.clone().into()), false);
                }
            }
        }
        switch.visit_children_with(self);
    }
    fn visit_with_stmt(&mut self, statement: &WithStmt) {
        statement.obj.visit_with(self);
        self.with_depth += 1;
        statement.body.visit_with(self);
        self.with_depth -= 1;
    }
    fn visit_bin_expr(&mut self, e: &BinExpr) {
        mangler_jsast::deep::walk_binary(e, self);
    }
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
    fn visit_class(&mut self, _: &Class) {}
    fn visit_class_decl(&mut self, declaration: &ClassDecl) {
        self.pattern(&Pat::Ident(declaration.ident.clone().into()), false);
    }
    fn visit_var_decl(&mut self, declaration: &VarDecl) {
        if declaration.kind != VarDeclKind::Var {
            for item in &declaration.decls {
                self.pattern(&item.name, declaration.kind == VarDeclKind::Const);
            }
        }
        declaration.visit_children_with(self);
    }
    fn visit_catch_clause(&mut self, catch: &CatchClause) {
        if let Some(param) = &catch.param {
            self.pattern(param, false);
        }
        catch.body.visit_with(self);
    }
}

/// Normalize lexical for-in heads to a per-iteration body declaration. For-of
/// uses the upstream iterator lowering after fixing its RHS TDZ environment.
struct IteratorHeads<'a> {
    helpers: &'a Helpers,
    hoisted: Vec<Ident>,
    aliases: HashMap<u32, HashMap<String, Ident>>,
}
impl IteratorHeads<'_> {
    fn right(&mut self, head: &ForHead, right: &mut Box<Expr>) {
        let ForHead::VarDecl(declaration) = head else {
            return;
        };
        if declaration.kind == VarDeclKind::Var {
            return;
        }
        let mut cells = HashMap::new();
        let mut frame = HashMap::new();
        let mut initializers = Vec::new();
        for id in declaration.decls.iter().flat_map(|d| pattern_ids(&d.name)) {
            let cell = fresh("_lex_head_tdz");
            self.hoisted.push(cell.clone());
            initializers.push(assign_ident(
                &cell,
                call(&self.helpers.cell, vec![boolean(false)]),
            ));
            frame.insert(id.sym.to_string(), Some(cell.clone()));
            cells.insert(
                id.to_id(),
                Binding {
                    cell,
                    constant: true,
                    with_depth: usize::MAX,
                },
            );
        }
        let with_ids = HashMap::new();
        let mut scopes = EvalScopes {
            bindings: &cells,
            cell_depths: cells
                .values()
                .map(|binding| (binding.cell.to_id(), binding.with_depth))
                .collect(),
            frames: vec![frame],
            aliases: HashMap::new(),
            with_ids: &with_ids,
            with_objects: Vec::new(),
            function_with_depth: 0,
            alias_objects: HashMap::new(),
            argument_objects: HashMap::new(),
        };
        right.visit_with(&mut scopes);
        for (span, aliases) in scopes.aliases {
            self.aliases.entry(span).or_default().extend(aliases);
        }
        right.visit_mut_with(&mut Rewrite {
            helpers: self.helpers,
            bindings: cells,
            current_renames: HashMap::new(),
            eval_renames: HashMap::new(),
            with_objects: Vec::new(),
            references: HashMap::new(),
            initial_references: HashMap::new(),
            function_with_depth: 0,
            with_ids,
        });
        initializers.push(std::mem::replace(right, undefined()));
        *right = sequence(initializers);
    }
}
impl VisitMut for IteratorHeads<'_> {
    fn visit_mut_function(&mut self, _: &mut Function) {}
    fn visit_mut_arrow_expr(&mut self, _: &mut ArrowExpr) {}
    fn visit_mut_class(&mut self, _: &mut Class) {}
    fn visit_mut_for_of_stmt(&mut self, loop_: &mut ForOfStmt) {
        self.right(&loop_.left, &mut loop_.right);
        if let ForHead::Pat(pattern) = &loop_.left {
            // IteratorValue precedes target evaluation. Make the assignment
            // explicit before the for-of, lexical-cell and lazy-pattern passes.
            let target =
                AssignTarget::try_from(*pattern.clone()).expect("assignment iteration target");
            let value = fresh("_lex_iteration");
            loop_.left = ForHead::VarDecl(Box::new(VarDecl {
                kind: VarDeclKind::Var,
                decls: vec![VarDeclarator {
                    span: DUMMY_SP,
                    name: Pat::Ident(value.clone().into()),
                    init: None,
                    definite: false,
                }],
                ..Default::default()
            }));
            let body = std::mem::replace(
                loop_.body.as_mut(),
                Stmt::Empty(EmptyStmt { span: DUMMY_SP }),
            );
            loop_.body = Box::new(Stmt::Block(BlockStmt {
                // Preserve the body's own lexical scope, outside the target.
                stmts: vec![
                    Stmt::Expr(ExprStmt {
                        span: DUMMY_SP,
                        expr: assign(target, ident_expr(&value)),
                    }),
                    body,
                ],
                ..Default::default()
            }));
        }
        loop_.visit_mut_children_with(self);
    }
    fn visit_mut_for_in_stmt(&mut self, loop_: &mut ForInStmt) {
        self.right(&loop_.left, &mut loop_.right);
        if let ForHead::VarDecl(declaration) = &loop_.left
            && declaration.kind != VarDeclKind::Var
        {
            let mut binding = (**declaration).clone();
            let value = fresh("_lex_iteration");
            binding.decls[0].init = Some(ident_expr(&value));
            loop_.left = ForHead::VarDecl(Box::new(VarDecl {
                span: DUMMY_SP,
                ctxt: SyntaxContext::empty(),
                kind: VarDeclKind::Var,
                declare: false,
                decls: vec![VarDeclarator {
                    span: DUMMY_SP,
                    name: Pat::Ident(value.into()),
                    init: None,
                    definite: false,
                }],
            }));
            let body = std::mem::replace(
                &mut loop_.body,
                Box::new(Stmt::Empty(EmptyStmt { span: DUMMY_SP })),
            );
            *loop_.body = Stmt::Block(BlockStmt {
                span: DUMMY_SP,
                ctxt: SyntaxContext::empty(),
                stmts: vec![Stmt::Decl(Decl::Var(Box::new(binding))), *body],
            });
        }
        loop_.visit_mut_children_with(self);
    }
}

fn referenced_ident(mut expression: &Expr) -> Option<&Ident> {
    while let Expr::Paren(p) = expression {
        expression = &p.expr;
    }
    if let Expr::Ident(id) = expression {
        Some(id)
    } else {
        None
    }
}
fn unbound(value: Box<Expr>) -> Box<Expr> {
    Box::new(Expr::Paren(ParenExpr {
        span: DUMMY_SP,
        expr: sequence(vec![
            Box::new(Expr::Lit(Lit::Num(Number {
                span: DUMMY_SP,
                value: 0.,
                raw: None,
            }))),
            value,
        ]),
    }))
}

struct Rewrite<'a> {
    helpers: &'a Helpers,
    bindings: HashMap<Id, Binding>,
    current_renames: HashMap<Id, Ident>,
    eval_renames: HashMap<u32, HashMap<Id, Ident>>,
    with_objects: Vec<Ident>,
    references: HashMap<u32, String>,
    initial_references: HashMap<u32, (String, Ident)>,
    function_with_depth: usize,
    with_ids: HashMap<u32, Ident>,
}
impl Rewrite<'_> {
    fn initial_reference(&mut self, expression: &Expr, key: &str) -> Option<Box<Expr>> {
        if self.with_objects.len() <= self.function_with_depth {
            return None;
        }
        let mut expression = expression;
        while let Expr::Paren(p) = expression {
            expression = &p.expr;
        }
        let Expr::Member(member_) = expression else {
            return None;
        };
        let (name, cell) = self.initial_references.get(&member_.span.lo.0)?;
        // An enclosing access such as `arguments[0]` starts at the same source
        // byte as the rewritten `arguments` reference. Match the generated cell
        // member itself, never merely its shared start position.
        if !matches!(&member_.prop, MemberProp::Ident(key) if key.sym == "v")
            || !matches!(&*member_.obj, Expr::Ident(object) if object.to_id() == cell.to_id())
        {
            return None;
        }
        let mut args = vec![ident_expr(cell)];
        args.extend(
            self.with_objects
                .iter()
                .skip(self.function_with_depth)
                .map(ident_expr),
        );
        let mut object = call(&self.helpers.reference, args);
        let Expr::Call(call_) = &mut *object else {
            unreachable!()
        };
        call_.span = member_.span;
        self.references.insert(member_.span.lo.0, name.clone());
        Some(Box::new(Expr::Member(member(object, key))))
    }
    fn lower_loop(&mut self, statement: &mut Stmt) -> bool {
        let mut labels = Vec::new();
        let mut inner = &*statement;
        while let Stmt::Labeled(labeled) = inner {
            labels.push(labeled.label.clone());
            inner = &labeled.body;
        }
        let Stmt::For(loop_) = inner else {
            return false;
        };
        let Some(VarDeclOrExpr::VarDecl(lexical)) = &loop_.init else {
            return false;
        };
        if lexical.kind == VarDeclKind::Var {
            return false;
        }
        let ids: Vec<Ident> = lexical
            .decls
            .iter()
            .flat_map(|d| pattern_ids(&d.name))
            .collect();
        if !ids.iter().any(|id| self.bindings.contains_key(&id.to_id())) {
            return false;
        }
        let mut loop_ = loop_.clone();
        let Some(VarDeclOrExpr::VarDecl(lexical)) = loop_.init.take() else {
            unreachable!()
        };
        let cells: Vec<(Id, Binding)> = ids
            .iter()
            .map(|id| (id.to_id(), self.bindings[&id.to_id()].clone()))
            .collect();
        let prefix =
            self.cell_declaration(cells.iter().map(|(_, binding)| binding.clone()).collect());
        let initializers = expression(sequence(
            lexical.decls.into_iter().map(|d| self.initialization(d)),
        ));
        let previous_renames = self.current_renames.clone();
        let mut entries = Vec::new();
        let mut copies = Vec::new();
        for (id, original) in &cells {
            let iteration = fresh(&format!("{}_iteration", original.cell.sym));
            entries.push((
                iteration.clone(),
                call(&self.helpers.clone_cell, vec![ident_expr(&original.cell)]),
            ));
            copies.push(assign_ident(
                &iteration,
                call(&self.helpers.clone_cell, vec![ident_expr(&iteration)]),
            ));
            self.current_renames
                .insert(original.cell.to_id(), iteration.clone());
            self.bindings.insert(
                id.clone(),
                Binding {
                    cell: iteration,
                    constant: original.constant,
                    with_depth: original.with_depth,
                },
            );
        }
        loop_.init = Some(VarDeclOrExpr::VarDecl(Box::new(declaration(
            VarDeclKind::Let,
            entries,
        ))));
        let mut test = loop_.test.take().unwrap_or_else(|| boolean(true));
        test.visit_mut_with(self);
        let mut update = loop_.update.take().unwrap_or_else(undefined);
        update.visit_mut_with(self);
        copies.push(update);
        loop_.body.visit_mut_with(self);
        // Put the test and update in the per-iteration body as well. Upstream
        // block-scoping only snapshots body captures, so closures created by a
        // header expression otherwise accidentally share the final loop cell.
        let first = fresh("_lex_first_iteration");
        let first_declaration = Stmt::Decl(Decl::Var(Box::new(declaration(
            VarDeclKind::Var,
            vec![(first.clone(), boolean(true))],
        ))));
        let not = |arg| {
            Box::new(Expr::Unary(UnaryExpr {
                span: DUMMY_SP,
                op: UnaryOp::Bang,
                arg,
            }))
        };
        let original_body = std::mem::replace(
            &mut loop_.body,
            Box::new(Stmt::Empty(EmptyStmt { span: DUMMY_SP })),
        );
        loop_.body = Box::new(Stmt::Block(BlockStmt {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            stmts: vec![
                Stmt::If(IfStmt {
                    span: DUMMY_SP,
                    test: not(ident_expr(&first)),
                    cons: Box::new(expression(sequence(copies))),
                    alt: None,
                }),
                expression(assign_ident(&first, boolean(false))),
                Stmt::If(IfStmt {
                    span: DUMMY_SP,
                    test: not(test),
                    cons: Box::new(Stmt::Break(BreakStmt {
                        span: DUMMY_SP,
                        label: None,
                    })),
                    alt: None,
                }),
                *original_body,
            ],
        }));
        for (id, binding) in cells {
            self.bindings.insert(id, binding);
        }
        self.current_renames = previous_renames;
        let mut result = Stmt::For(loop_);
        for label in labels.into_iter().rev() {
            result = Stmt::Labeled(LabeledStmt {
                span: DUMMY_SP,
                label,
                body: Box::new(result),
            });
        }
        *statement = Stmt::Block(BlockStmt {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            stmts: vec![prefix, initializers, first_declaration, result],
        });
        true
    }
    fn reference(&mut self, id: &Ident, initialization: bool) -> Option<MemberExpr> {
        let binding = self.bindings.get(&id.to_id())?;
        let mut arguments = vec![ident_expr(&binding.cell)];
        let projected = !initialization && binding.with_depth < self.with_objects.len();
        if projected {
            arguments.extend(
                self.with_objects
                    .iter()
                    .skip(binding.with_depth)
                    .map(ident_expr),
            );
            self.references.insert(id.span.lo.0, id.sym.to_string());
        }
        let object = if initialization {
            ident_expr(&binding.cell)
        } else {
            let mut expression = call(&self.helpers.reference, arguments);
            if projected {
                let Expr::Call(call) = &mut *expression else {
                    unreachable!()
                };
                call.span = id.span;
            }
            expression
        };
        let mut reference = member(object, if initialization { "i" } else { "v" });
        if projected {
            reference.span = id.span;
        }
        Some(reference)
    }
    fn callable_reference(&mut self, id: &Ident) -> Box<Expr> {
        let mut reference = self.reference(id, false).unwrap();
        if self.references.contains_key(&id.span.lo.0) {
            reference.prop = MemberProp::Ident(IdentName::new("c".into(), DUMMY_SP));
            Box::new(Expr::Member(reference))
        } else {
            unbound(Box::new(Expr::Member(reference)))
        }
    }
    fn cells(&self, statements: &[Stmt]) -> Vec<Binding> {
        let mut cells = Vec::new();
        for stmt in statements {
            match stmt {
                Stmt::Decl(Decl::Var(declaration)) if declaration.kind != VarDeclKind::Var => {
                    for decl in &declaration.decls {
                        for id in pattern_ids(&decl.name) {
                            if let Some(cell) = self.bindings.get(&id.to_id()) {
                                cells.push(cell.clone());
                            }
                        }
                    }
                }
                Stmt::Decl(Decl::Fn(function)) => {
                    if let Some(cell) = self.bindings.get(&function.ident.to_id()) {
                        cells.push(cell.clone());
                    }
                }
                Stmt::Decl(Decl::Class(class)) => {
                    if let Some(cell) = self.bindings.get(&class.ident.to_id()) {
                        cells.push(cell.clone());
                    }
                }
                _ => {}
            }
        }
        cells
    }
    fn cell_declaration(&self, cells: Vec<Binding>) -> Stmt {
        Stmt::Decl(Decl::Var(Box::new(declaration(
            VarDeclKind::Let,
            cells
                .into_iter()
                .map(|b| (b.cell, call(&self.helpers.cell, vec![boolean(b.constant)])))
                .collect(),
        ))))
    }
    fn named(&self, pattern: &Pat, value: Box<Expr>) -> Box<Expr> {
        let Pat::Ident(binding) = pattern else {
            return value;
        };
        let mut direct = &*value;
        while let Expr::Paren(parenthesized) = direct {
            direct = &parenthesized.expr;
        }
        let anonymous = matches!(direct, Expr::Fn(f) if f.ident.is_none())
            || matches!(direct, Expr::Arrow(_))
            || matches!(direct, Expr::Class(c) if c.ident.is_none());
        if !anonymous {
            return value;
        }
        let key = binding.id.sym.clone();
        let object = Expr::Object(ObjectLit {
            span: DUMMY_SP,
            props: vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
                key: if key == "__proto__" {
                    PropName::Computed(ComputedPropName {
                        span: DUMMY_SP,
                        expr: Box::new(Expr::Lit(Lit::Str(Str {
                            span: DUMMY_SP,
                            value: key.clone().into(),
                            raw: None,
                        }))),
                    })
                } else {
                    PropName::Ident(IdentName::new(key.clone(), DUMMY_SP))
                },
                value,
            })))],
        });
        Box::new(Expr::Member(MemberExpr {
            span: DUMMY_SP,
            obj: Box::new(object),
            prop: MemberProp::Ident(IdentName::new(key, DUMMY_SP)),
        }))
    }
    fn target(&mut self, pattern: &mut Pat, initialization: bool) {
        match pattern {
            Pat::Ident(id) => {
                if let Some(reference) = self.reference(&id.id, initialization) {
                    *pattern = Pat::Expr(Box::new(Expr::Member(reference)));
                }
            }
            Pat::Assign(a) => {
                a.right.visit_mut_with(self);
                a.right = self.named(&a.left, a.right.clone());
                self.target(&mut a.left, initialization);
            }
            Pat::Array(a) => {
                for p in a.elems.iter_mut().flatten() {
                    self.target(p, initialization);
                }
            }
            Pat::Object(o) => {
                for prop in &mut o.props {
                    match prop {
                        ObjectPatProp::KeyValue(kv) => {
                            kv.key.visit_mut_with(self);
                            self.target(&mut kv.value, initialization);
                        }
                        ObjectPatProp::Assign(a) => {
                            let id = a.key.clone();
                            let mut value = Pat::Ident(id.clone());
                            if let Some(default) = &mut a.value {
                                default.visit_mut_with(self);
                                value = Pat::Assign(AssignPat {
                                    span: a.span,
                                    left: Box::new(value),
                                    right: self.named(&Pat::Ident(id.clone()), default.clone()),
                                });
                            }
                            self.target(&mut value, initialization);
                            *prop = ObjectPatProp::KeyValue(KeyValuePatProp {
                                key: PropName::Ident(IdentName::new(id.id.sym, id.id.span)),
                                value: Box::new(value),
                            });
                        }
                        ObjectPatProp::Rest(r) => self.target(&mut r.arg, initialization),
                    }
                }
            }
            Pat::Rest(r) => self.target(&mut r.arg, initialization),
            Pat::Expr(e) => e.visit_mut_with(self),
            Pat::Invalid(_) => {}
        }
    }
    fn initialization(&mut self, declaration: VarDeclarator) -> Box<Expr> {
        let mut value = declaration.init.unwrap_or_else(undefined);
        value.visit_mut_with(self);
        let value = self.named(&declaration.name, value);
        let mut pattern = declaration.name;
        self.target(&mut pattern, true);
        let target = match pattern {
            Pat::Expr(e) => AssignTarget::Simple(
                SimpleAssignTarget::try_from(e).expect("lexical initialization reference"),
            ),
            Pat::Array(a) => AssignTarget::Pat(AssignTargetPat::Array(a)),
            Pat::Object(o) => AssignTarget::Pat(AssignTargetPat::Object(o)),
            _ => unreachable!("lexical declaration target"),
        };
        assign(target, value)
    }
    fn list(&mut self, statements: &mut Vec<Stmt>, initialize: bool) {
        let cells = if initialize {
            self.cells(statements)
        } else {
            Vec::new()
        };
        for stmt in statements.iter_mut() {
            stmt.visit_mut_with(self);
        }
        if !cells.is_empty() {
            statements.insert(directives(statements), self.cell_declaration(cells));
        }
    }
}
impl VisitMut for Rewrite<'_> {
    fn visit_mut_function(&mut self, function: &mut Function) {
        let previous = self.function_with_depth;
        self.function_with_depth = self.with_objects.len();
        function.visit_mut_children_with(self);
        self.function_with_depth = previous;
    }
    fn visit_mut_bin_expr(&mut self, e: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(e, self);
    }
    fn visit_mut_stmts(&mut self, statements: &mut Vec<Stmt>) {
        self.list(statements, true);
    }
    fn visit_mut_expr(&mut self, expr: &mut Expr) {
        if let Expr::Unary(unary) = expr
            && unary.op == UnaryOp::Delete
        {
            if let Some(reference) = self.initial_reference(&unary.arg, "d") {
                *expr = *reference;
                return;
            }
            if let Some(id) = referenced_ident(&unary.arg)
                && self.bindings.contains_key(&id.to_id())
            {
                let mut reference = self.reference(id, false).unwrap();
                if self.references.contains_key(&id.span.lo.0) {
                    reference.prop = MemberProp::Ident(IdentName::new("d".into(), DUMMY_SP));
                    *expr = Expr::Member(reference);
                } else {
                    *expr = *boolean(false);
                }
                return;
            }
        }
        if let Some(reference) = self.initial_reference(expr, "v") {
            *expr = *reference;
            return;
        }
        if let Expr::Ident(id) = expr
            && let Some(reference) = self.reference(id, false)
        {
            *expr = Expr::Member(reference);
            return;
        }
        expr.visit_mut_children_with(self);
    }
    fn visit_mut_unary_expr(&mut self, unary: &mut UnaryExpr) {
        unary.visit_mut_children_with(self);
    }
    fn visit_mut_call_expr(&mut self, call_: &mut CallExpr) {
        if mangler_jsast::analysis::scope::is_direct_eval_callee(&call_.callee)
            && !self.current_renames.is_empty()
        {
            self.eval_renames
                .insert(call_.span.lo.0, self.current_renames.clone());
        }
        if let Callee::Expr(callee) = &mut call_.callee {
            if let Some(reference) = self.initial_reference(callee, "c") {
                *callee = reference;
                call_.args.visit_mut_with(self);
                return;
            }
            if let Some(id) = referenced_ident(callee)
                && self.bindings.contains_key(&id.to_id())
            {
                *callee = self.callable_reference(id);
                call_.args.visit_mut_with(self);
                return;
            }
        }
        call_.visit_mut_children_with(self);
    }
    fn visit_mut_opt_call(&mut self, call_: &mut OptCall) {
        if let Some(reference) = self.initial_reference(&call_.callee, "c") {
            call_.callee = reference;
            call_.args.visit_mut_with(self);
            return;
        }
        if let Some(id) = referenced_ident(&call_.callee)
            && self.bindings.contains_key(&id.to_id())
        {
            call_.callee = self.callable_reference(id);
            call_.args.visit_mut_with(self);
            return;
        }
        call_.visit_mut_children_with(self);
    }
    fn visit_mut_tagged_tpl(&mut self, template: &mut TaggedTpl) {
        if let Some(reference) = self.initial_reference(&template.tag, "c") {
            template.tag = reference;
            template.tpl.visit_mut_with(self);
            return;
        }
        if let Some(id) = referenced_ident(&template.tag)
            && self.bindings.contains_key(&id.to_id())
        {
            template.tag = self.callable_reference(id);
            template.tpl.visit_mut_with(self);
            return;
        }
        template.visit_mut_children_with(self);
    }
    fn visit_mut_assign_expr(&mut self, assignment: &mut AssignExpr) {
        if matches!(
            assignment.op,
            AssignOp::Assign | AssignOp::AndAssign | AssignOp::OrAssign | AssignOp::NullishAssign
        ) && let AssignTarget::Simple(SimpleAssignTarget::Ident(id)) = &assignment.left
            && self.bindings.contains_key(&id.id.to_id())
        {
            assignment.right = self.named(&Pat::Ident(id.clone()), assignment.right.clone());
        }
        assignment.visit_mut_children_with(self);
    }
    fn visit_mut_prop(&mut self, prop: &mut Prop) {
        if let Prop::Shorthand(id) = prop
            && let Some(reference) = self.reference(id, false)
        {
            *prop = Prop::KeyValue(KeyValueProp {
                key: PropName::Ident(IdentName::new(id.sym.clone(), id.span)),
                value: Box::new(Expr::Member(reference)),
            });
            return;
        }
        prop.visit_mut_children_with(self);
    }
    fn visit_mut_simple_assign_target(&mut self, target: &mut SimpleAssignTarget) {
        if let SimpleAssignTarget::Member(member_) = target
            && let Some(reference) = self.initial_reference(&Expr::Member(member_.clone()), "v")
        {
            let Expr::Member(reference) = *reference else {
                unreachable!()
            };
            *target = SimpleAssignTarget::Member(reference);
            return;
        }
        if let SimpleAssignTarget::Ident(id) = target
            && let Some(reference) = self.reference(&id.id, false)
        {
            *target = SimpleAssignTarget::Member(reference);
            return;
        }
        target.visit_mut_children_with(self);
    }
    fn visit_mut_assign_target_pat(&mut self, target: &mut AssignTargetPat) {
        match target {
            AssignTargetPat::Array(a) => {
                let mut p = Pat::Array(a.clone());
                self.target(&mut p, false);
                let Pat::Array(a2) = p else { unreachable!() };
                *a = a2;
            }
            AssignTargetPat::Object(o) => {
                let mut p = Pat::Object(o.clone());
                self.target(&mut p, false);
                let Pat::Object(o2) = p else { unreachable!() };
                *o = o2;
            }
            AssignTargetPat::Invalid(_) => {}
        }
    }
    fn visit_mut_catch_clause(&mut self, catch: &mut CatchClause) {
        if catch.param.as_ref().is_some_and(|pattern| {
            pattern_ids(pattern)
                .iter()
                .any(|id| self.bindings.contains_key(&id.to_id()))
        }) {
            let pattern = catch.param.take().unwrap();
            let value = fresh("_lex_exception");
            catch.param = Some(Pat::Ident(value.clone().into()));
            catch.body.stmts.insert(
                0,
                Stmt::Decl(Decl::Var(Box::new(VarDecl {
                    span: DUMMY_SP,
                    ctxt: SyntaxContext::empty(),
                    kind: VarDeclKind::Let,
                    declare: false,
                    decls: vec![VarDeclarator {
                        span: DUMMY_SP,
                        name: pattern,
                        init: Some(ident_expr(&value)),
                        definite: false,
                    }],
                }))),
            );
        }
        catch.body.visit_mut_with(self);
    }
    fn visit_mut_stmt(&mut self, statement: &mut Stmt) {
        if self.lower_loop(statement) {
            return;
        }
        match statement {
            Stmt::With(with) => {
                with.obj.visit_mut_with(self);
                let object = self
                    .with_ids
                    .get(&with.span.lo.0)
                    .cloned()
                    .unwrap_or_else(|| fresh("_lex_with_object"));
                let already_cached =
                    matches!(&*with.obj, Expr::Ident(id) if id.to_id() == object.to_id());
                let init = call(&self.helpers.with_object, vec![with.obj.clone()]);
                with.obj = ident_expr(&object);
                self.with_objects.push(object.clone());
                with.body.visit_mut_with(self);
                self.with_objects.pop();
                if already_cached {
                    return;
                }
                let body = std::mem::replace(statement, Stmt::Empty(EmptyStmt { span: DUMMY_SP }));
                *statement = Stmt::Block(BlockStmt {
                    span: DUMMY_SP,
                    ctxt: SyntaxContext::empty(),
                    stmts: vec![
                        Stmt::Decl(Decl::Var(Box::new(declaration(
                            VarDeclKind::Let,
                            vec![(object, init)],
                        )))),
                        body,
                    ],
                });
            }
            Stmt::Decl(Decl::Var(d))
                if d.kind != VarDeclKind::Var
                    && d.decls.iter().any(|decl| {
                        pattern_ids(&decl.name)
                            .iter()
                            .any(|id| self.bindings.contains_key(&id.to_id()))
                    }) =>
            {
                let values: Vec<_> = d
                    .decls
                    .clone()
                    .into_iter()
                    .map(|decl| self.initialization(decl))
                    .collect();
                *statement = expression(sequence(values));
            }
            Stmt::Decl(Decl::Class(class)) if self.bindings.contains_key(&class.ident.to_id()) => {
                let id = class.ident.clone();
                let binding = self.bindings.remove(&id.to_id()).unwrap();
                let mut value = class.class.clone();
                value.visit_mut_with(self);
                self.bindings.insert(id.to_id(), binding.clone());
                *statement = expression(assign(
                    AssignTarget::Simple(SimpleAssignTarget::Member(member(
                        ident_expr(&binding.cell),
                        "i",
                    ))),
                    Box::new(Expr::Class(ClassExpr {
                        ident: Some(id),
                        class: value,
                    })),
                ));
            }
            Stmt::Switch(switch) => {
                let cells: Vec<_> = switch
                    .cases
                    .iter()
                    .flat_map(|case| self.cells(&case.cons))
                    .collect();
                switch.discriminant.visit_mut_with(self);
                // Declaration closures must capture the same cells as every
                // case lexical binding. Initialize them on CaseBlock entry,
                // before testing cases, even when their textual case is skipped.
                let mut functions = Vec::new();
                for case in &mut switch.cases {
                    for statement in &mut case.cons {
                        if matches!(statement, Stmt::Decl(Decl::Fn(function)) if self.bindings.contains_key(&function.ident.to_id()))
                        {
                            let Stmt::Decl(Decl::Fn(function)) = std::mem::replace(
                                statement,
                                Stmt::Empty(EmptyStmt { span: DUMMY_SP }),
                            ) else {
                                unreachable!()
                            };
                            functions.push(expression(self.initialization(VarDeclarator {
                                span: function.ident.span,
                                name: Pat::Ident(function.ident.into()),
                                init: Some(Box::new(Expr::Fn(FnExpr {
                                    ident: None,
                                    function: function.function,
                                }))),
                                definite: false,
                            })));
                        }
                    }
                    case.test.visit_mut_with(self);
                    self.list(&mut case.cons, false);
                }
                if !cells.is_empty() {
                    // The discriminant belongs to the outer environment and
                    // runs before CaseBlock allocation and declaration setup.
                    let value = fresh("_lex_switch_value");
                    let discriminant =
                        std::mem::replace(&mut switch.discriminant, ident_expr(&value));
                    let mut prefix = vec![
                        Stmt::Decl(Decl::Var(Box::new(declaration(
                            VarDeclKind::Let,
                            vec![(value, discriminant)],
                        )))),
                        self.cell_declaration(cells),
                    ];
                    prefix.extend(functions);
                    prefix.push(std::mem::replace(
                        statement,
                        Stmt::Empty(EmptyStmt { span: DUMMY_SP }),
                    ));
                    *statement = Stmt::Block(BlockStmt {
                        span: DUMMY_SP,
                        ctxt: SyntaxContext::empty(),
                        stmts: prefix,
                    });
                }
            }
            _ => statement.visit_mut_children_with(self),
        }
    }
}

#[cfg(test)]
mod tests {
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};
    use mangler_testkit::{CaptureMode, eval::assert_behaviorally_equal_with};

    fn equivalent(source: &str) {
        let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
        let selected = super::super::suspension_candidates(ast.program())
            .into_keys()
            .collect();
        let lowered = super::super::lower_with_lexicals(ast.program_mut(), &selected);
        mangler_jsast::directives::insert_program_statements(ast.program_mut(), lowered.helpers);
        let output = Js.print(&ast);
        assert_behaviorally_equal_with(
            source,
            &output,
            &CaptureMode::Sink("JSON.stringify(globalThis.__out)".into()),
        );
    }

    #[test]
    fn declarations_keep_tdz_const_defaults_and_names() {
        equivalent(
            r#"
function* g(){
 const log=[];
 try{log.push(typeof x)}catch(e){log.push(e.name)}
 let x=1;yield x;
 const c=2;
 try{c=3}catch(e){log.push(e.name)}
 try{let [a=b,b]=[]}catch(e){log.push(e.name)}
 let f=()=>f;let {named=()=>0}={};
 yield [f.name,f()===f,named.name];
 yield log;
}
globalThis.__out=Array.from(g());
"#,
        );
    }

    #[test]
    fn lexical_loop_environments_survive_resumes() {
        equivalent(
            r#"
function* g(){
 const fs=[];
 for(let i=0;i<3;i++){fs.push(()=>i);yield i}
 yield fs.map(f=>f());
 const updates=[],bodies=[];
 for(let u=0;u<3;(updates.push(()=>u),u++)){bodies.push(()=>u);yield u}
 yield [updates.map(f=>f()),bodies.map(f=>f())];
 const defaults=[];
 for(let d=0;d<3;d++){defaults.push(function(value=d){return value});yield d}
 yield defaults.map(f=>f());
 const gs=[];
 for(let [x,y] of [[1,2],[3,4]]){gs.push(()=>x+y);yield x+y}
 yield gs.map(f=>f());
 const hs=[];
 for(let k in {a:1,b:2}){hs.push(()=>k);yield k}
 yield hs.map(f=>f());
}
globalThis.__out=Array.from(g());
"#,
        );
    }

    #[test]
    fn detached_parameter_defaults_capture_each_iteration_without_running_early() {
        equivalent(
            r#"
function* g(){
 const functions=[],arrows=[],patterns=[],nested=[],log=[];
 for(let i=0;i<3;i++){
  functions.push(function(value=(log.push(i),i)){return value});
  arrows.push((value=i)=>value);
  patterns.push(function({value=i}={}){return value});
  nested.push(function(make=(value=i)=>value){return make()});
  yield log.slice();
 }
 yield functions.map(f=>f());
 yield arrows.map(f=>f());
 yield patterns.map(f=>f());
 yield nested.map(f=>f());
 yield log;
}
async function a(){
 const functions=[],log=[];
 for(let i=0;i<3;i++){
  functions.push((value=(log.push(i),i))=>value);
  await 0;
 }
 const before=log.slice();
 return [before,functions.map(f=>f()),log];
}
const sync=Array.from(g());
a().then(async=>globalThis.__out=[sync,async]);
"#,
        );
    }

    #[test]
    fn initial_loop_environment_and_lexical_rhs_are_distinct() {
        equivalent(
            r#"
function* g(){
 let first;
 for(let i=(first=()=>i,0);i<2;i++){yield [i,first()]}
 const bad=[];
 try{for(let x of x){}}catch(e){bad.push(e.name)}
 try{for(let y in y){}}catch(e){bad.push(e.name)}
 yield bad;
}
globalThis.__out=Array.from(g());
"#,
        );
    }

    #[test]
    fn switch_catch_and_class_bindings_have_their_own_lifetimes() {
        equivalent(
            r#"
function* g(){
 const fs=[];
 for(var i=0;i<2;i++){
   try{throw i}catch(e){fs.push(()=>e);yield e}
   {try{yield typeof x}catch(e){yield e.name}let x=i;fs.push(()=>x)}
 }
 yield fs.map(f=>f());
 switch(0){case 0:try{yield y}catch(e){yield e.name}case 1:let y=4;yield y}
 class C{static self(){return C}}
 yield [C.name,C.self()===C];
}
globalThis.__out=Array.from(g());
"#,
        );
    }
    #[test]
    fn projected_calls_keep_unbound_receivers_and_parenthesized_names() {
        equivalent(
            r#"
function* g(){
 let x=(function(){}),f=function(){'use strict';return this};
 yield [x.name,f(),f?.(),(f)()];
 let n=0;
 outer:for(let i=0;i<3;i++){for(let j=0;j<2;j++){n++;continue outer}}
 yield n;
}
globalThis.__out=Array.from(g());
"#,
        );
    }

    #[test]
    fn eval_observes_original_lexical_scopes_and_iteration_captures() {
        use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
        use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
        let Some(node) = node_path() else { return };
        for source in [
            "function* pay(){let a=[];for(let i=0;i<3;i++){a.push(()=>eval('i'));yield i}yield a.map(f=>f())}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let a=[];for(let i=0;i<3;(a.push(()=>eval('i')),i++)){yield i}yield a.map(f=>f())}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let x=4;yield eval('x');eval('x=5');yield x;const y=6;try{eval('y=7')}catch(e){yield e.name}}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let x=[1];try{for(let x of eval('x')){}}catch(e){yield e.name}}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let x=4;yield (function x(){return eval('typeof x')})();yield (function(a=eval('x')){var x=2;return a})()}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let x=4,eval=globalThis.eval;yield eval('x')}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let x=4;function* nested(){let x=8;yield eval('x')}yield Array.from(nested())}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let x=4;yield eval(...['x'])}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let x=1;with({x:2}){yield eval('x');let y=3;yield eval('y');yield eval('delete x');yield eval('x');try{eval('var x')}catch(e){yield e.name}}}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let x=1,read,obj={x:2};with(obj){read=()=>eval('x');yield read()}obj.x=7;yield read();yield x}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "async function pay(a){with({arguments:[9]}){await 0;return eval('arguments[0]')}}pay(3).then(x=>globalThis.__out=JSON.stringify(x));",
            "async function pay(){await 0;eval('var arguments=3,other=4');return [arguments,other]}pay().then(value=>globalThis.__out=JSON.stringify(value));",
        ] {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(Intensity::Minify),
                seed: Some(42),
                virtualize: Some("pay".into()),
                require_virtualized: Some("pay".into()),
                ..Default::default()
            })
            .unwrap();
            let (output, _) =
                crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
            let results = evaluate_many(&Engine::Node(node.clone()), &[source, &output]).unwrap();
            assert_eq!(results[0], results[1], "{source}");
        }
    }

    #[test]
    fn suspended_with_preserves_source_bindings_receivers_and_proxy_names() {
        use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
        use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
        let Some(node) = node_path() else { return };
        for source in [
            "function* pay(){let x=1;with({x:2}){yield x;x=3;yield x}yield x}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let x=1,read;let obj={x:2};with(obj){read=()=>x;yield read()}obj.x=7;yield read();yield x}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let f=function(){return this.n},obj={n:4,f};with(obj){yield f();yield f?.();yield f`tag`}}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "function* pay(){let x=1,obj={x:2};with(obj){yield typeof x;yield delete x;yield x;{let x=3;yield x;yield delete x}}yield x}globalThis.__out=JSON.stringify(Array.from(pay()));",
            "async function pay(a){with({arguments:[9]}){await 0;return arguments[0]}}pay(3).then(x=>globalThis.__out=JSON.stringify(x));",
            "async function pay(a){with({arguments:[9]}){await 0;arguments[0]=8;return [arguments[0],a]}}pay(3).then(x=>globalThis.__out=JSON.stringify(x));",
            "function* pay(){let f=()=>1,obj={n:7,f(){return this.n}};with(obj){yield function(){return f()} } }let f=pay().next().value;globalThis.__out=JSON.stringify(f());",
        ] {
            let config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(Intensity::Minify),
                seed: Some(42),
                virtualize: Some("pay".into()),
                require_virtualized: Some("pay".into()),
                ..Default::default()
            })
            .unwrap();
            let (output, _) =
                crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
            let results = evaluate_many(&Engine::Node(node.clone()), &[source, &output]).unwrap();
            assert_eq!(results[0], results[1], "{source}");
        }
    }

    #[test]
    fn suspended_object_binding_reads_check_resolution_and_value_separately() {
        use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
        use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
        // Object Environment HasBinding and GetBindingValue each perform
        // HasProperty (ECMA-262 9.1.1.2.1 and 9.1.1.2.6, step 2).
        // Node 26/V8 currently reports one trap for native `with`; use the
        // specified two operations as the oracle, also checking that generated
        // implementation names never enter source object lookup.
        // https://tc39.es/ecma262/#sec-object-environment-records-getbindingvalue-n-s
        let source = "function* pay(){let x=1,names=[];with(new Proxy({x:2},{has(t,k){names.push(k);return Reflect.has(t,k)}})){yield x}yield names}globalThis.__out=JSON.stringify(Array.from(pay()));";
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(42),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
        let oracle = "globalThis.__out=JSON.stringify([2,['x','x']]);";
        let engine =
            Engine::Node(node_path().expect("Node required for suspended object binding reads"));
        let results = evaluate_many(&engine, &[oracle, &output]).unwrap();
        assert_eq!(results[0], results[1], "{source}");
    }

    #[test]
    fn suspended_assignment_retains_the_resolved_object_environment() {
        // ECMA-262 13.15.2 evaluates the left Reference before the RHS. QuickJS
        // follows that order. V8 currently delays bare-identifier `with` writes,
        // including synchronous writes, so this semantic check uses QuickJS.
        let source = "function* pay(o){let x=1;with(o){x=yield 4;}yield x}var obj={x:2};var it=pay(obj);var a=it.next();obj[Symbol.unscopables]={x:true};var b=it.next(8);globalThis.__out=JSON.stringify([a,b,obj.x]);";
        let config = mangler_config::ResolvedConfig::try_from(mangler_config::ConfigFlags {
            preset: Some(mangler_config::Intensity::Minify),
            seed: Some(42),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            ..Default::default()
        })
        .unwrap();
        let (output, _) = crate::runner::process(source, &ParseOpts::default(), &config).unwrap();
        mangler_testkit::assert_behaviorally_equal(source, &output);
        mangler_testkit::assert_behaviorally_equal(
            "globalThis.__out=JSON.stringify([{value:4,done:false},{value:1,done:false},8]);",
            &output,
        );
    }
    #[test]
    fn generator_switch_declarations_are_hoisted_in_the_shared_lexical_cells() {
        use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
        use mangler_testkit::cross_engine::{Engine, evaluate_many, node_path};
        let engine = Engine::Node(node_path().expect("Node required for generator switch scopes"));
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            seed: Some(42),
            virtualize: Some("*".into()),
            require_virtualized: Some("*".into()),
            ..Default::default()
        })
        .unwrap();
        for source in [
            "function* f(){switch(1){default:function* g(){yield 2};yield g.name}yield typeof g}globalThis.__out=Array.from(f())",
            "function* f(){switch(2){case g().next().value:yield 4;break;default:function* g(){yield 2}}}globalThis.__out=Array.from(f())",
            "function* f(){let old;switch(1){case 1:let x=4;old=g;break;default:function* g(){yield x}}yield old().next().value}globalThis.__out=Array.from(f())",
            "function* f(){try{switch(1){case g().next().value:break;default:let x=4;function* g(){yield x}}}catch(e){yield e.name}}globalThis.__out=Array.from(f())",
            "function* f(){var g=3;switch(g){case 3:yield h().next().value;break;default:function* h(){yield 9}}yield typeof h}globalThis.__out=Array.from(f())",
            "function* f(){let a=[];for(let i=0;i<3;i++){switch(i){default:let x=i;function* g(){yield x};a.push(g)}}yield a.map(g=>[g.name,g().next().value])}globalThis.__out=Array.from(f())",
            "function* f(){switch(1){default:function* g(){g=7;yield g};let old=g;yield old().next().value;yield g}}globalThis.__out=Array.from(f())",
            "function* f(){switch(1){default:function* g(){yield h().next().value};function* h(){yield 9};yield g().next().value;yield[g.name,h.name]}}globalThis.__out=Array.from(f())",
            "function* f(){switch(1){default:async function g(){return 2};yield g.name;yield g}}let a=Array.from(f());a[1]().then(x=>globalThis.__out=JSON.stringify([a[0],x]))",
            "function* f(){switch(1){default:async function* g(){yield 2};yield g.name;yield g}}let a=Array.from(f());a[1]().next().then(x=>globalThis.__out=JSON.stringify([a[0],x.value]))",
            "function* f(){let a=[];switch((a.push(\"disc\"),2)){case(a.push(\"first\"),1):break;default:function* g(){yield 9};a.push(g().next().value);case(a.push(\"last\"),3):a.push(\"tail\")}yield a}globalThis.__out=Array.from(f())",
            "function* f(){switch(1){default:function g(){return 2};yield g()}yield typeof g}globalThis.__out=Array.from(f())",
            "function* f(){switch(1){default:function* g(){yield 2};yield eval(\"g.name\")}}globalThis.__out=Array.from(f())",
            "function* f(){let a=[];for(let i=0;i<2;i++){switch(i){default:function* g(){yield i};a.push(()=>eval(\"[g.name,g().next().value]\"))}}yield a.map(f=>f())}globalThis.__out=Array.from(f())",
        ] {
            let source = format!("{source};globalThis.__out=JSON.stringify(globalThis.__out)");
            let (output, _) =
                crate::runner::process(&source, &ParseOpts::default(), &config).unwrap();
            let results = evaluate_many(&engine, &[&source, &output]).unwrap();
            assert_eq!(results[0], results[1], "{source}");
        }
    }
}
