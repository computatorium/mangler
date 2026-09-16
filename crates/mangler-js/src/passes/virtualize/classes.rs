//! Lexical host operations for bytecode bodies inside native class envelopes.
//!
//! JavaScript owns private brands, home objects, constructor initialization and
//! descriptors. Small lexical arrows expose those operations to the VM; operands,
//! argument evaluation, branches and business logic remain bytecode.
use super::*;
#[path = "classes/eval_context.rs"]
pub(super) mod eval_context;
#[path = "classes/method_shells.rs"]
mod method_shells;
#[path = "classes/suspension_parameters.rs"]
mod suspension_parameters;
pub(super) use suspension_parameters::Parameters as SuspendedParameters;
#[path = "classes/object_eval.rs"]
mod object_eval;
use mangler_vm::eval_class::Operation;
pub(super) use object_eval::prepare_nested_object_eval;

/// Demand for the canonical captured iterator key shared with bytecode GetIter.
/// Only constructing a native super capability requires the additional alias.
pub(super) struct IteratorAlias<'a> {
    name: &'a str,
    required: std::cell::Cell<bool>,
}
impl<'a> IteratorAlias<'a> {
    pub(super) fn new(name: &'a str) -> Self {
        Self {
            name,
            required: std::cell::Cell::new(false),
        }
    }
    pub(super) fn required(&self) -> bool {
        self.required.get()
    }
    fn require(&self) -> &str {
        self.required.set(true);
        self.name
    }
}

/// Source lexical reads already use lazy native capabilities. An interpreter
/// entry in a derived environment must not read `this` merely to forward an
/// unused receiver, including protected arrows produced inside direct eval.
pub(super) fn defer_derived_receiver(function: &mut Function, names: &VmNames) {
    struct LazyThis<'a>(&'a VmNames);
    impl VisitMut for LazyThis<'_> {
        fn visit_mut_function(&mut self, _: &mut Function) {}
        fn visit_mut_class(&mut self, _: &mut swc_core::ecma::ast::Class) {}
        fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
            call.visit_mut_children_with(self);
            let interpreter = match &call.callee {
                Callee::Expr(callee) => {
                    matches!(&**callee, Expr::Ident(id) if [self.0.lean_interp.as_str(), self.0.eh_interp.as_str(), self.0.lean_interp_strict.as_str(), self.0.eh_interp_strict.as_str()].contains(&id.sym.as_ref()))
                }
                _ => false,
            };
            if interpreter
                && call
                    .args
                    .get(6)
                    .is_some_and(|argument| matches!(&*argument.expr, Expr::This(_)))
            {
                *call.args[6].expr = Expr::Unary(UnaryExpr {
                    span: DUMMY_SP,
                    op: UnaryOp::Void,
                    arg: Box::new(Expr::Lit(Lit::Num(Number {
                        span: DUMMY_SP,
                        value: 0.,
                        raw: None,
                    }))),
                });
            }
        }
    }
    let mut lazy = LazyThis(names);
    function.params.visit_mut_with(&mut lazy);
    if let Some(body) = &mut function.body {
        body.visit_mut_with(&mut lazy);
    }
}

fn generated_expression(span: swc_core::common::Span) -> bool {
    span.is_dummy() || mangler_jsast::span::is_runtime_span(span)
}

pub(super) fn prepare(
    function: &mut Function,
    cfg: &FileConfig,
    apply: &str,
    iterator: &IteratorAlias<'_>,
) -> (Function, Vec<Stmt>) {
    let mut function = mangler_jsast::deep::clone_function(function);
    let mut lower = LexicalBridges {
        cfg,
        apply,
        iterator,
        declarations: Vec::new(),
        cache: std::collections::HashMap::new(),
        lexical: true,
        external: None,
        masked_private: Default::default(),
    };
    function.params.visit_mut_with(&mut lower);
    if let Some(body) = &mut function.body {
        body.visit_mut_with(&mut lower);
    }
    (function, lower.declarations)
}

/// Lower eval lexical references against opaque native capabilities supplied by
/// its caller. The returned declarations contain capability lookups only.
pub(super) fn prepare_external_eval(
    function: &mut Function,
    cfg: &FileConfig,
    apply: &str,
    iterator: &IteratorAlias<'_>,
    context: &mangler_vm::eval::EvalClassContext,
) -> (Function, Vec<Stmt>) {
    prepare_external(function, cfg, apply, iterator, context, true)
}

/// Resolve caller-private names before nested source functions are compiled.
/// Their own this/super environments still belong to the later class pass.
pub(super) fn prepare_external_private(
    function: &mut Function,
    cfg: &FileConfig,
    apply: &str,
    iterator: &IteratorAlias<'_>,
    context: &mangler_vm::eval::EvalClassContext,
) -> (Function, Vec<Stmt>) {
    prepare_external(function, cfg, apply, iterator, context, false)
}

fn prepare_external(
    function: &mut Function,
    cfg: &FileConfig,
    apply: &str,
    iterator: &IteratorAlias<'_>,
    context: &mangler_vm::eval::EvalClassContext,
    lexical: bool,
) -> (Function, Vec<Stmt>) {
    let mut function = mangler_jsast::deep::clone_function(function);
    let mut lower = LexicalBridges {
        cfg,
        apply,
        iterator,
        declarations: Vec::new(),
        cache: Default::default(),
        lexical,
        external: Some(context.clone()),
        masked_private: Default::default(),
    };
    function.params.visit_mut_with(&mut lower);
    function.body.visit_mut_with(&mut lower);
    (function, lower.declarations)
}

struct LexicalBridges<'a> {
    cfg: &'a FileConfig,
    apply: &'a str,
    iterator: &'a IteratorAlias<'a>,
    declarations: Vec<Stmt>,
    cache: std::collections::HashMap<String, String>,
    lexical: bool,
    external: Option<mangler_vm::eval::EvalClassContext>,
    masked_private: std::collections::HashSet<String>,
}

fn ident(name: &str) -> Expr {
    Expr::Ident(Ident::new_no_ctxt(name.into(), DUMMY_SP))
}
fn quoted_private_name(name: &str) -> String {
    swc_core::ecma::codegen::to_code(&Expr::Lit(Lit::Str(Str {
        span: DUMMY_SP,
        value: name.into(),
        raw: None,
    })))
}
fn call(name: &str, args: impl IntoIterator<Item = Box<Expr>>) -> Expr {
    Expr::Call(CallExpr {
        span: DUMMY_SP,
        callee: Callee::Expr(Box::new(ident(name))),
        args: args
            .into_iter()
            .map(|expr| ExprOrSpread { spread: None, expr })
            .collect(),
        ..Default::default()
    })
}
fn value(object: Expr) -> Expr {
    Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(object),
        prop: MemberProp::Ident(IdentName::new("value".into(), DUMMY_SP)),
    })
}

impl LexicalBridges<'_> {
    fn lower_private(&self, name: &str) -> bool {
        !self.masked_private.contains(name)
            && self.external.as_ref().is_none_or(|context| {
                context.private_names.iter().any(|private| private == name)
            })
    }
    /// A nested native class owns its declared brands; references to enclosing
    /// brands must remain capabilities in the surrounding lexical envelope.
    fn prepare_class(&mut self, mut class: ClassExpr) -> ClassExpr {
        class.class.visit_mut_with(self);
        class
    }
    fn class_factory_named(
        &mut self,
        class: ClassExpr,
        inferred: Option<swc_core::atoms::Wtf8Atom>,
    ) -> Expr {
        class_factory_named(self.prepare_class(class), inferred)
    }
    fn class_factory(&mut self, class: ClassExpr) -> Expr {
        self.class_factory_named(class, None)
    }
    fn bridge(&mut self, operation: Operation, private: Option<&str>) -> String {
        let iterator = if matches!(operation, Operation::Construct) {
            self.iterator.require()
        } else {
            self.iterator.name
        };
        let source = operation.source(private, self.apply, iterator);
        if let Some(name) = self.cache.get(&source) {
            return name.clone();
        }
        let name = self.cfg.fresh_name();
        self.cache.insert(source.clone(), name.clone());
        if let Some(context) = &self.external {
            let capsule = &context.capsule_binding;
            let accessor = match operation {
                Operation::This => format!("{capsule}.t"),
                Operation::NewTarget => format!("{capsule}.n"),
                _ => match private {
                    Some(private) => format!(
                        "{capsule}.p[{}]({})",
                        quoted_private_name(private),
                        operation.id()
                    ),
                    None => format!("{capsule}.s({})", operation.id()),
                },
            };
            let mut declarations =
                parse_fn_body_stmts(&format!("function _(){{const {name}={accessor};}}"))
                    .expect("opaque class capability lookup parses");
            declarations.visit_mut_with(&mut GeneratedSpans);
            self.declarations.extend(declarations);
            return name;
        }
        let field = private.map_or(String::new(), |p| format!("#{p};"));
        // Parse in a disposable class scope, then retain only its constructor
        // statement. This validates lexical super/private syntax without lowering
        // class creation or weakening private-brand checks.
        let source = format!(
            "class _Bridge extends _Base{{{field}constructor(){{const {name}={source};}}}}"
        );
        let ast = Js
            .parse(&source, &ParseOpts::default())
            .expect("generated lexical bridge parses");
        let program = ast.into_program();
        let class = match program {
            Program::Script(s) => match s.body.into_iter().next().unwrap() {
                Stmt::Decl(Decl::Class(c)) => c.class,
                _ => unreachable!(),
            },
            Program::Module(m) => match m.body.into_iter().next().unwrap() {
                ModuleItem::Stmt(Stmt::Decl(Decl::Class(c))) => c.class,
                _ => unreachable!(),
            },
        };
        for member in class.body {
            if let ClassMember::Constructor(c) = member {
                let mut stmts = c.body.unwrap().stmts;
                struct Generated;
                impl VisitMut for Generated {
                    fn visit_mut_span(&mut self, span: &mut swc_core::common::Span) {
                        *span = DUMMY_SP;
                    }
                }
                stmts.visit_mut_with(&mut Generated);
                self.declarations.extend(stmts);
            }
        }
        name
    }

    fn private(&mut self, member: &MemberExpr, callable: bool) -> Expr {
        let MemberProp::PrivateName(p) = &member.prop else {
            unreachable!()
        };
        let key = p.name.to_string();
        let operation = if callable {
            Operation::Call { optional: false }
        } else {
            Operation::Reference
        };
        let name = self.bridge(operation, Some(&key));
        let mut object = member.obj.clone();
        object.visit_mut_with(self);
        let expr = call(&name, vec![object]);
        if callable { expr } else { value(expr) }
    }

    fn super_prop(&mut self, member: &SuperPropExpr, callable: bool) -> Expr {
        let operation = if callable {
            Operation::Call { optional: false }
        } else {
            Operation::Reference
        };
        let name = self.bridge(operation, None);
        let mut key = match &member.prop {
            SuperProp::Ident(i) => Box::new(Expr::Lit(Lit::Str(Str {
                span: DUMMY_SP,
                value: i.sym.clone().into(),
                raw: None,
            }))),
            SuperProp::Computed(c) => c.expr.clone(),
        };
        key.visit_mut_with(self);
        let expr = call(&name, vec![key]);
        if callable { expr } else { value(expr) }
    }

    fn callable(&mut self, expr: &Expr) -> Option<Expr> {
        match expr {
            Expr::Member(m) if matches!(&m.prop, MemberProp::PrivateName(p) if self.lower_private(p.name.as_ref())) => {
                Some(self.private(m, true))
            }
            Expr::SuperProp(m) if self.lexical => Some(self.super_prop(m, true)),
            Expr::Paren(p) => self.callable(&p.expr),
            _ => None,
        }
    }
}

fn protected_class(class: &swc_core::ecma::ast::Class, exclude: Option<&str>) -> bool {
    use swc_core::common::Spanned;
    fn value_is_protected(value: &Option<Box<Expr>>, exclude: Option<&str>) -> bool {
        value.as_ref().is_none_or(|value| {
            if generated_expression(value.span()) {
                return true;
            }
            match &**value {
                Expr::Fn(f) => f.function.body.as_ref().is_some_and(|b| b.span.is_dummy()),
                Expr::Arrow(a) => {
                    matches!(&*a.body, ArrowFunctionBody::FunctionBody(b) if b.span.is_dummy())
                }
                Expr::Class(c) => protected_class(&c.class, exclude),
                _ => false,
            }
        })
    }
    let excluded = |name: &str| exclude.is_some_and(|pattern| glob::matches(pattern, name));
    let protected_key = |key: &PropName| {
        method_shells::original_key_is_protected(key).unwrap_or_else(
            || !matches!(key, PropName::Computed(key) if !generated_expression(key.expr.span())),
        )
    };
    if class
        .super_class
        .as_ref()
        .is_some_and(|heritage| !generated_expression(heritage.span()))
    {
        return false;
    }
    class.body.iter().all(|member| match member {
        ClassMember::Constructor(c) => {
            excluded("constructor") || c.body.as_ref().is_none_or(|b| b.span.is_dummy())
        }
        ClassMember::Method(m) => {
            if !protected_key(&m.key) {
                return false;
            }
            excluded(
                &method_shells::original_method_name(&m.key)
                    .or_else(|| static_prop_key_name(&m.key))
                    .unwrap_or_else(|| "<computed>".into()),
            ) || m.function.body.as_ref().is_none_or(|b| b.span.is_dummy())
        }
        ClassMember::PrivateMethod(m) => {
            excluded(&format!("#{}", m.key.name))
                || m.function.body.as_ref().is_none_or(|b| b.span.is_dummy())
        }
        ClassMember::ClassProp(p) => {
            if !protected_key(&p.key) {
                return false;
            }
            excluded(&static_prop_key_name(&p.key).unwrap_or_else(|| "<computed>".into()))
                || value_is_protected(&p.value, exclude)
        }
        ClassMember::PrivateProp(p) => {
            excluded(&format!("#{}", p.key.name)) || value_is_protected(&p.value, exclude)
        }
        ClassMember::StaticBlock(b) => excluded("<static>") || b.body.span.is_dummy(),
        ClassMember::Empty(_) => true,
        _ => false,
    })
}

fn class_factory_named(class: ClassExpr, inferred: Option<swc_core::atoms::Wtf8Atom>) -> Expr {
    let inferred = class.ident.is_none().then_some(inferred).flatten();
    let class = Box::new(Expr::Class(class));
    let class = match inferred {
        Some(value) => mangler_jsast::assignment_target::named_value_key(
            Str {
                span: DUMMY_SP,
                value,
                raw: None,
            },
            class,
        ),
        None => class,
    };
    class_factory_result(class, Vec::new(), Vec::new())
}

fn class_factory_result(result: Box<Expr>, params: Vec<Pat>, args: Vec<ExprOrSpread>) -> Expr {
    Expr::Call(CallExpr {
        span: DUMMY_SP,
        callee: Callee::Expr(Box::new(Expr::Arrow(ArrowExpr {
            span: DUMMY_SP,
            params,
            body: Box::new(ArrowFunctionBody::FunctionBody(FunctionBody {
                // Reserved compiler protocol marker. Parsed source starts at byte
                // one, while ordinary generated arrow bodies use 0..0.
                span: swc_core::common::Span::new(
                    swc_core::common::BytePos(0),
                    swc_core::common::BytePos(1),
                ),
                stmts: vec![Stmt::Return(ReturnStmt {
                    span: DUMMY_SP,
                    arg: Some(result),
                })],
                ..Default::default()
            })),
            ..Default::default()
        }))),
        args,
        ..Default::default()
    })
}

impl VisitMut for LexicalBridges<'_> {
    fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
        mangler_jsast::deep::walk_binary_mut(binary, self);
    }
    fn visit_mut_unary_expr(&mut self, unary: &mut UnaryExpr) {
        let mut argument = &*unary.arg;
        while let Expr::Paren(parentheses) = argument {
            argument = &parentheses.expr;
        }
        if self.lexical
            && unary.op == UnaryOp::Delete
            && let Expr::SuperProp(member) = argument
        {
            // A SuperReference cannot be deleted. Pass its raw key to the
            // native primitive; neither property lookup nor ToPropertyKey occurs.
            let mut key = match &member.prop {
                SuperProp::Ident(name) => Box::new(Expr::Lit(Lit::Str(Str {
                    span: DUMMY_SP,
                    value: name.sym.clone().into(),
                    raw: None,
                }))),
                SuperProp::Computed(key) => key.expr.clone(),
            };
            key.visit_mut_with(self);
            let operation = self.bridge(Operation::Delete, None);
            unary.arg = Box::new(call(&operation, [key]));
            return;
        }
        unary.visit_mut_children_with(self);
    }
    fn visit_mut_assign_pat(&mut self, pattern: &mut AssignPat) {
        if let (Pat::Ident(binding), Expr::Class(class)) = (&*pattern.left, &*pattern.right)
            && protected_class(
                &class.class,
                self.cfg.resolved().passes.virtualize.exclude.as_deref(),
            )
        {
            *pattern.right =
                self.class_factory_named(class.clone(), Some(binding.id.sym.clone().into()));
            return;
        }
        pattern.visit_mut_children_with(self);
    }
    fn visit_mut_assign_pat_prop(&mut self, pattern: &mut AssignPatProp) {
        if let Some(value) = &mut pattern.value
            && let Expr::Class(class) = &**value
            && protected_class(
                &class.class,
                self.cfg.resolved().passes.virtualize.exclude.as_deref(),
            )
        {
            **value = self.class_factory_named(class.clone(), Some(pattern.key.id.sym.clone().into()));
            return;
        }
        pattern.visit_mut_children_with(self);
    }
    fn visit_mut_var_declarator(&mut self, declaration: &mut VarDeclarator) {
        if let (Pat::Ident(binding), Some(value)) = (&declaration.name, &mut declaration.init)
            && let Expr::Class(class) = &**value
            && protected_class(
                &class.class,
                self.cfg.resolved().passes.virtualize.exclude.as_deref(),
            )
        {
            **value = self.class_factory_named(class.clone(), Some(binding.id.sym.clone().into()));
            return;
        }
        declaration.visit_mut_children_with(self);
    }
    fn visit_mut_prop_or_spread(&mut self, property: &mut PropOrSpread) {
        if let PropOrSpread::Prop(prop) = property
            && let Prop::KeyValue(value) = &**prop
            && !matches!(&value.key, PropName::Ident(name) if name.sym == "__proto__")
            && !matches!(&value.key, PropName::Str(name) if name.value.as_str() == Some("__proto__"))
            && let Expr::Class(class) = mangler_jsast::assignment_target::unparen(&value.value)
            && protected_class(
                &class.class,
                self.cfg.resolved().passes.virtualize.exclude.as_deref(),
            )
        {
            // Keep key evaluation in the source activation (it can suspend),
            // then let native PropertyDefinitionEvaluation convert it once and
            // name the anonymous class before static initialization. Returning
            // the one-property object avoids a second key lookup/conversion.
            let (key, params, args) = if let PropName::Computed(key) = &value.key {
                let mut argument = key.expr.clone();
                argument.visit_mut_with(self);
                let parameter = Ident::new_no_ctxt(self.cfg.fresh_name().into(), DUMMY_SP);
                (
                    PropName::Computed(ComputedPropName {
                        span: DUMMY_SP,
                        expr: Box::new(Expr::Ident(parameter.clone())),
                    }),
                    vec![Pat::Ident(parameter.into())],
                    vec![ExprOrSpread {
                        spread: None,
                        expr: argument,
                    }],
                )
            } else {
                (value.key.clone(), Vec::new(), Vec::new())
            };
            let result = Box::new(Expr::Object(ObjectLit {
                span: DUMMY_SP,
                props: vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
                    key,
                    value: Box::new(Expr::Class(self.prepare_class(class.clone()))),
                })))],
            }));
            *property = PropOrSpread::Spread(SpreadElement {
                dot3_token: DUMMY_SP,
                expr: Box::new(class_factory_result(result, params, args)),
            });
            return;
        }
        property.visit_mut_children_with(self);
    }
    fn visit_mut_stmt(&mut self, stmt: &mut Stmt) {
        if let Stmt::Decl(Decl::Class(class)) = stmt {
            if !protected_class(
                &class.class,
                self.cfg.resolved().passes.virtualize.exclude.as_deref(),
            ) {
                class.class.visit_mut_with(self);
                return;
            }
            let expression = self.class_factory(ClassExpr {
                ident: Some(class.ident.clone()),
                class: class.class.clone(),
            });
            *stmt = Stmt::Decl(Decl::Var(Box::new(VarDecl {
                span: class.class.span,
                kind: VarDeclKind::Let,
                decls: vec![VarDeclarator {
                    span: class.class.span,
                    name: Pat::Ident(class.ident.clone().into()),
                    init: Some(Box::new(expression)),
                    definite: false,
                }],
                ..Default::default()
            })));
        } else {
            stmt.visit_mut_children_with(self);
        }
    }
    fn visit_mut_class(&mut self, class: &mut swc_core::ecma::ast::Class) {
        class.super_class.visit_mut_with(self);
        let previous = self.masked_private.clone();
        for member in &class.body {
            match member {
                ClassMember::PrivateMethod(method) => {
                    self.masked_private.insert(method.key.name.to_string());
                }
                ClassMember::PrivateProp(property) => {
                    self.masked_private.insert(property.key.name.to_string());
                }
                _ => {}
            }
        }
        for member in &mut class.body {
            match member {
                ClassMember::Method(method) => method.key.visit_mut_with(self),
                ClassMember::ClassProp(property) => property.key.visit_mut_with(self),
                _ => {}
            }
            let lexical = self.lexical;
            self.lexical = false;
            match member {
                ClassMember::Method(method) => method.function.visit_mut_with(self),
                ClassMember::PrivateMethod(method) => method.function.visit_mut_with(self),
                ClassMember::ClassProp(property) => property.value.visit_mut_with(self),
                ClassMember::PrivateProp(property) => property.value.visit_mut_with(self),
                ClassMember::Constructor(constructor) => constructor.visit_mut_children_with(self),
                ClassMember::StaticBlock(block) => block.body.visit_mut_with(self),
                _ => {}
            }
            self.lexical = lexical;
        }
        self.masked_private = previous;
    }
    fn visit_mut_function(&mut self, f: &mut Function) {
        let lexical = self.lexical;
        self.lexical = false;
        f.visit_mut_children_with(self);
        self.lexical = lexical;
    }
    fn visit_mut_simple_assign_target(&mut self, target: &mut SimpleAssignTarget) {
        use mangler_jsast::assignment_target::{self, Reference};
        let expr = match assignment_target::simple_reference(target) {
            Reference::Member(m) if matches!(&m.prop, MemberProp::PrivateName(p) if self.lower_private(p.name.as_ref())) => {
                Some(self.private(m, false))
            }
            Reference::Super(m) if self.lexical => Some(self.super_prop(m, false)),
            _ => None,
        };
        if let Some(Expr::Member(m)) = expr {
            *target = SimpleAssignTarget::Member(m);
        } else {
            target.visit_mut_children_with(self);
        }
    }
    fn visit_mut_expr(&mut self, expr: &mut Expr) {
        if matches!(expr, Expr::Bin(_) | Expr::Paren(_)) {
            mangler_jsast::deep::rewrite_binary_spine(expr, self, |lower, expression| {
                let Expr::Bin(binary) = expression else {
                    return;
                };
                if binary.op != BinaryOp::In {
                    return;
                }
                let Expr::PrivateName(private) = &*binary.left else {
                    return;
                };
                if !lower.lower_private(private.name.as_ref()) {
                    return;
                }
                let key = private.name.to_string();
                let name = lower.bridge(Operation::In, Some(&key));
                let rhs = std::mem::replace(
                    &mut binary.right,
                    Box::new(Expr::Invalid(Invalid { span: DUMMY_SP })),
                );
                *expression = call(&name, vec![rhs]);
            });
            return;
        }
        if self.lexical
            && matches!(expr, Expr::Call(call) if mangler_jsast::span::is_lexical_class_reference_span(call.span))
        {
            let Expr::Call(reference) = expr else {
                unreachable!()
            };
            let operation = match reference.args.first().map(|argument| &*argument.expr) {
                Some(Expr::Lit(Lit::Num(id)))
                    if id.value == f64::from(Operation::Construct.id()) =>
                {
                    Operation::Construct
                }
                Some(Expr::Lit(Lit::Num(id))) if id.value == f64::from(Operation::This.id()) => {
                    Operation::This
                }
                _ => unreachable!("generated lexical class operation"),
            };
            *expr = ident(&self.bridge(operation, None));
            return;
        }
        match expr {
            Expr::Class(class) => {
                if protected_class(
                    &class.class,
                    self.cfg.resolved().passes.virtualize.exclude.as_deref(),
                ) {
                    *expr = self.class_factory(class.clone());
                } else {
                    class.class.visit_mut_with(self);
                }
                return;
            }
            Expr::Update(u) => {
                let (mut operand, private) = match mangler_jsast::assignment_target::unparen(&u.arg)
                {
                    Expr::Member(m) => {
                        if let MemberProp::PrivateName(p) = &m.prop
                            && self.lower_private(p.name.as_ref())
                        {
                            (m.obj.clone(), Some(p.name.to_string()))
                        } else {
                            expr.visit_mut_children_with(self);
                            return;
                        }
                    }
                    Expr::SuperProp(m) if self.lexical => {
                        let key = match &m.prop {
                            SuperProp::Ident(i) => Box::new(Expr::Lit(Lit::Str(Str {
                                span: DUMMY_SP,
                                value: i.sym.clone().into(),
                                raw: None,
                            }))),
                            SuperProp::Computed(c) => c.expr.clone(),
                        };
                        (key, None)
                    }
                    _ => {
                        expr.visit_mut_children_with(self);
                        return;
                    }
                };
                let name = self.bridge(
                    Operation::Update {
                        increment: u.op == UpdateOp::PlusPlus,
                        prefix: u.prefix,
                    },
                    private.as_deref(),
                );
                operand.visit_mut_with(self);
                *expr = call(&name, vec![operand]);
                return;
            }
            Expr::Assign(a) => {
                if matches!(
                    a.op,
                    AssignOp::Assign
                        | AssignOp::AndAssign
                        | AssignOp::OrAssign
                        | AssignOp::NullishAssign
                ) && let (Some(name), Expr::Class(class)) = (
                    mangler_jsast::assignment_target::inferred_name(&a.left),
                    mangler_jsast::assignment_target::unparen(&a.right),
                ) && protected_class(
                    &class.class,
                    self.cfg.resolved().passes.virtualize.exclude.as_deref(),
                ) {
                    *a.right = self.class_factory_named(class.clone(), Some(name.into()));
                    return;
                }
                use mangler_jsast::assignment_target::{self, Reference};
                let (mut operands, private) = match assignment_target::reference(&a.left) {
                    Reference::Member(m) => {
                        if let MemberProp::PrivateName(p) = &m.prop
                            && self.lower_private(p.name.as_ref())
                        {
                            (vec![m.obj.clone()], Some(p.name.to_string()))
                        } else {
                            expr.visit_mut_children_with(self);
                            return;
                        }
                    }
                    Reference::Super(m) if self.lexical => {
                        let key = match &m.prop {
                            SuperProp::Ident(i) => Box::new(Expr::Lit(Lit::Str(Str {
                                span: DUMMY_SP,
                                value: i.sym.clone().into(),
                                raw: None,
                            }))),
                            SuperProp::Computed(c) => c.expr.clone(),
                        };
                        (vec![key], None)
                    }
                    _ => {
                        expr.visit_mut_children_with(self);
                        return;
                    }
                };
                let name = self.bridge(Operation::Assign(a.op), private.as_deref());
                operands.visit_mut_with(self);
                let mut rhs = a.right.clone();
                rhs.visit_mut_with(self);
                operands.push(Box::new(Expr::Arrow(ArrowExpr {
                    span: DUMMY_SP,
                    body: Box::new(ArrowFunctionBody::Expr(rhs)),
                    ..Default::default()
                })));
                *expr = call(&name, operands);
                return;
            }
            Expr::Call(c) => {
                match &mut c.callee {
                    Callee::Super(_) if self.lexical => {
                        let name = self.bridge(Operation::Construct, None);
                        c.callee = Callee::Expr(Box::new(ident(&name)));
                    }
                    Callee::Expr(e) => {
                        if let Some(replacement) = self.callable(e) {
                            **e = replacement;
                        } else {
                            e.visit_mut_with(self);
                        }
                    }
                    _ => {}
                }
                c.args.visit_mut_with(self);
                return;
            }
            Expr::OptChain(chain) => {
                match &mut *chain.base {
                    OptChainBase::Call(c) => {
                        // Optional calls must test the actual private value before
                        // allocating the bound host callable.
                        if let Expr::OptChain(inner) = &mut *c.callee
                            && let OptChainBase::Member(m) = &mut *inner.base
                            && let MemberProp::PrivateName(p) = &m.prop
                            && self.lower_private(p.name.as_ref())
                        {
                            let key = p.name.to_string();
                            let name = self.bridge(
                                Operation::OptionalCallReference {
                                    receiver: inner.optional,
                                    value: chain.optional,
                                },
                                Some(&key),
                            );
                            let mut object = m.obj.clone();
                            object.visit_mut_with(self);
                            *m.obj = call(&name, vec![object]);
                            m.prop = MemberProp::Ident(IdentName::new("value".into(), DUMMY_SP));
                            c.args.visit_mut_with(self);
                            return;
                        }
                        if let Expr::SuperProp(m) = &*c.callee
                            && self.lexical
                        {
                            let name = self.bridge(
                                Operation::Call {
                                    optional: chain.optional,
                                },
                                None,
                            );
                            let mut key = match &m.prop {
                                SuperProp::Ident(i) => Box::new(Expr::Lit(Lit::Str(Str {
                                    span: DUMMY_SP,
                                    value: i.sym.clone().into(),
                                    raw: None,
                                }))),
                                SuperProp::Computed(k) => k.expr.clone(),
                            };
                            key.visit_mut_with(self);
                            *c.callee = call(&name, vec![key]);
                            c.args.visit_mut_with(self);
                            return;
                        }
                        if let Expr::Member(m) = &*c.callee
                            && let MemberProp::PrivateName(p) = &m.prop
                            && self.lower_private(p.name.as_ref())
                        {
                            let key = p.name.to_string();
                            let name = self.bridge(
                                Operation::Call {
                                    optional: chain.optional,
                                },
                                Some(&key),
                            );
                            let mut object = m.obj.clone();
                            object.visit_mut_with(self);
                            *c.callee = call(&name, vec![object]);
                            c.args.visit_mut_with(self);
                            return;
                        }
                    }
                    OptChainBase::Member(m) => {
                        if let MemberProp::PrivateName(p) = &m.prop
                            && self.lower_private(p.name.as_ref())
                        {
                            let key = p.name.to_string();
                            let name = self.bridge(
                                if chain.optional {
                                    Operation::OptionalReference
                                } else {
                                    Operation::Reference
                                },
                                Some(&key),
                            );
                            let mut object = m.obj.clone();
                            object.visit_mut_with(self);
                            *m.obj = call(&name, vec![object]);
                            m.prop = MemberProp::Ident(IdentName::new("value".into(), DUMMY_SP));
                            return;
                        }
                    }
                }
            }
            Expr::TaggedTpl(t) => {
                if let Some(replacement) = self.callable(&t.tag) {
                    *t.tag = replacement;
                } else {
                    t.tag.visit_mut_with(self);
                }
                t.tpl.visit_mut_with(self);
                return;
            }
            Expr::Member(m) if matches!(&m.prop, MemberProp::PrivateName(p) if self.lower_private(p.name.as_ref())) =>
            {
                *expr = self.private(m, false);
                return;
            }
            Expr::SuperProp(m) if self.lexical => {
                *expr = self.super_prop(m, false);
                return;
            }
            Expr::This(_)
                if self.lexical
                    && self
                        .external
                        .as_ref()
                        .is_none_or(|context| context.allow_super_call) =>
            {
                let name = self.bridge(Operation::This, None);
                *expr = call(&name, Vec::new());
                return;
            }
            Expr::MetaProp(m)
                if self.lexical && self.external.is_none() && m.kind == MetaPropKind::NewTarget =>
            {
                let name = self.bridge(Operation::NewTarget, None);
                *expr = call(&name, Vec::new());
                return;
            }
            _ => {}
        }
        expr.visit_mut_children_with(self);
    }
}

impl Virtualizer<'_> {
    /// Native class construction owns the class-name TDZ and key coercion, while
    /// each source heritage/key expression executes in a bytecode callback at its
    /// original evaluation position.
    pub(super) fn class_headers(&mut self, class: &mut swc_core::ecma::ast::Class) {
        use swc_core::common::Spanned;
        let mut protect = |expression: &mut Box<Expr>, label: &str| {
            if generated_expression(expression.span()) {
                return;
            }
            expression.visit_mut_with(self);
            if let Err(reason) = self.virtualize_initializer(expression, None) {
                self.top_level_errors
                    .push(format!("class {label}: {reason}"));
            }
        };
        if let Some(heritage) = &mut class.super_class {
            protect(heritage, "heritage");
        }
        for member in &mut class.body {
            let key = match member {
                ClassMember::Method(method) => Some(&mut method.key),
                ClassMember::ClassProp(property) => Some(&mut property.key),
                _ => None,
            };
            if let Some(PropName::Computed(key)) = key {
                protect(&mut key.expr, "computed key");
            }
        }
    }

    fn field_initializer(
        &mut self,
        name: &str,
        span: swc_core::common::Span,
        value: &mut Option<Box<Expr>>,
    ) {
        let Some(expression) = value else {
            return;
        };
        if generated_expression(expression.span()) {
            return;
        }
        // Field initializers and static blocks are implicit native function
        // contexts: nested lexical arrows and direct eval may use new.target.
        let initializer_context = self.class_initializer_context;
        self.class_initializer_context = true;
        self.strict_stack.push(true);
        let only = self.class_methods_only;
        self.class_methods_only = false;
        let direct = match mangler_jsast::assignment_target::unparen_mut(expression) {
            Expr::Class(class) => {
                class.class.visit_mut_with(self);
                Some(protected_class(
                    &class.class,
                    self.cfg.resolved().passes.virtualize.exclude.as_deref(),
                ))
            }
            Expr::Arrow(arrow) => Some(self.try_virtualize_arrow(name, arrow)),
            Expr::Fn(function) => Some(self.try_virtualize(name, &mut function.function)),
            _ => None,
        };
        if let Some(protected) = direct {
            if protected {
                self.outcomes.insert(span.lo.0, None);
                // Function-valued fields use the same callable envelope as other
                // expressions; their inferred field name remains source metadata.
                if matches!(
                    mangler_jsast::assignment_target::unparen(expression),
                    Expr::Fn(_) | Expr::Arrow(_)
                ) {
                    expression.visit_mut_with(self);
                }
            } else {
                expression.visit_mut_with(self);
            }
        } else {
            let mut function = Function {
                span,
                body: Some(FunctionBody {
                    span,
                    stmts: vec![Stmt::Return(ReturnStmt {
                        span,
                        arg: Some(expression.clone()),
                    })],
                    ..Default::default()
                }),
                ..Default::default()
            };
            if self.try_virtualize_kind(name, &mut function, true, None) {
                **expression = Expr::Call(CallExpr {
                    span: DUMMY_SP,
                    callee: Callee::Expr(Box::new(Expr::Arrow(ArrowExpr {
                        span: DUMMY_SP,
                        params: function.params.into_iter().map(|p| p.pat).collect(),
                        body: Box::new(ArrowFunctionBody::FunctionBody(function.body.unwrap())),
                        ..Default::default()
                    }))),
                    ..Default::default()
                });
            } else {
                expression.visit_mut_with(self);
            }
        }
        self.class_methods_only = only;
        self.strict_stack.pop();
        self.class_initializer_context = initializer_context;
    }
    pub(super) fn class_prop(&mut self, property: &mut ClassProp) {
        let name = static_prop_key_name(&property.key).unwrap_or_else(|| "<computed>".into());
        self.field_initializer(&name, property.span, &mut property.value);
        property.key.visit_mut_with(self);
    }
    pub(super) fn private_prop(&mut self, property: &mut PrivateProp) {
        self.field_initializer(
            &format!("#{}", property.key.name),
            property.span,
            &mut property.value,
        );
    }
    pub(super) fn static_block(&mut self, block: &mut StaticBlock) {
        // Field initializers and static blocks are implicit native function
        // contexts: nested lexical arrows and direct eval may use new.target.
        let initializer_context = self.class_initializer_context;
        self.class_initializer_context = true;
        self.strict_stack.push(true);
        let only = self.class_methods_only;
        self.class_methods_only = false;
        let mut function = Function {
            span: block.span,
            body: Some(FunctionBody {
                span: block.body.span,
                stmts: block.body.stmts.clone(),
            }),
            ..Default::default()
        };
        if self.try_virtualize_kind("<static>", &mut function, true, None) {
            block.body = BlockStmt {
                span: DUMMY_SP,
                stmts: vec![Stmt::Expr(ExprStmt {
                    span: DUMMY_SP,
                    expr: Box::new(Expr::Call(CallExpr {
                        span: DUMMY_SP,
                        callee: Callee::Expr(Box::new(Expr::Arrow(ArrowExpr {
                            span: DUMMY_SP,
                            params: function.params.into_iter().map(|p| p.pat).collect(),
                            body: Box::new(ArrowFunctionBody::FunctionBody(function.body.unwrap())),
                            ..Default::default()
                        }))),
                        ..Default::default()
                    })),
                })],
                ..Default::default()
            };
        } else {
            block.visit_mut_children_with(self);
        }
        self.class_methods_only = only;
        self.strict_stack.pop();
        self.class_initializer_context = initializer_context;
    }
    fn setter_parameters(&self, function: &mut Function, original: &[Param]) {
        let length = original
            .iter()
            .take_while(|p| !matches!(p.pat, Pat::Assign(_) | Pat::Rest(_)))
            .count();
        let simple = original.iter().all(|p| matches!(p.pat, Pat::Ident(_)));
        let binding = function
            .params
            .first()
            .and_then(|parameter| match &parameter.pat {
                Pat::Ident(binding) => Some(Pat::Ident(binding.clone())),
                Pat::Rest(rest) => Some(*rest.arg.clone()),
                Pat::Assign(assign) => Some(*assign.left.clone()),
                _ => None,
            })
            .unwrap_or_else(|| {
                Pat::Ident(Ident::new_no_ctxt(self.cfg.fresh_name().into(), DUMMY_SP).into())
            });
        function.params = vec![Param {
            span: DUMMY_SP,
            decorators: vec![],
            pat: if length == 0 {
                Pat::Assign(AssignPat {
                    span: DUMMY_SP,
                    left: Box::new(binding),
                    // An unknown intrinsic call keeps the minifier from erasing
                    // the default and incorrectly changing setter.length to one.
                    right: {
                        let mut statements = parse_fn_body_stmts(&format!(
                            "function _(){{return {}(function(){{}},void 0,[])}}",
                            self.native_apply
                        ))
                        .expect("setter arity adapter parses");
                        let Stmt::Return(value) = statements.remove(0) else {
                            unreachable!()
                        };
                        value.arg.unwrap()
                    },
                })
            } else {
                binding
            },
        }];
        if !simple {
            // Source non-simple parameter lists expose an unmapped arguments
            // object. A setter cannot forward through a rest parameter; retain its
            // one-parameter native shape and clone the actual list in strict mode.
            let mut statements = parse_fn_body_stmts(&format!("function _(){{return {}(function(){{'use strict';return arguments}},void 0,arguments)}}", self.native_apply)).expect("setter argument adapter parses");
            let Stmt::Return(adapter) = statements.remove(0) else {
                unreachable!()
            };
            if let Some(body) = &mut function.body
                && let Some(Stmt::Return(ReturnStmt {
                    arg: Some(expression),
                    ..
                })) = body.stmts.last_mut()
                && let Expr::Call(call) = &mut **expression
            {
                call.args[2].expr = adapter.arg.unwrap();
            }
        }
    }

    pub(super) fn class_method(&mut self, method: &mut ClassMethod) {
        self.strict_stack.push(true);
        let only = self.class_methods_only;
        self.class_methods_only = false;
        let name = static_prop_key_name(&method.key).unwrap_or_else(|| "<computed>".into());
        let original_params = method.function.params.clone();
        let replaced = self.try_virtualize(&name, &mut method.function);
        if replaced && method.kind == MethodKind::Setter {
            self.setter_parameters(&mut method.function, &original_params);
        }
        self.class_methods_only = only;
        if !replaced {
            method.visit_mut_children_with(self);
        }
        self.strict_stack.pop();
    }
    pub(super) fn private_method(&mut self, method: &mut PrivateMethod) {
        self.strict_stack.push(true);
        let only = self.class_methods_only;
        self.class_methods_only = false;
        let original_params = method.function.params.clone();
        let replaced = self.try_virtualize(&format!("#{}", method.key.name), &mut method.function);
        if replaced && method.kind == MethodKind::Setter {
            self.setter_parameters(&mut method.function, &original_params);
        }
        self.class_methods_only = only;
        if !replaced {
            method.visit_mut_children_with(self);
        }
        self.strict_stack.pop();
    }
    pub(super) fn constructor(&mut self, constructor: &mut Constructor) {
        self.strict_stack.push(true);
        let only = self.class_methods_only;
        self.class_methods_only = false;
        let mut function = Function {
            span: constructor.span,
            params: constructor
                .params
                .iter()
                .filter_map(|p| match p {
                    ParamOrTsParamProp::Param(p) => Some(p.clone()),
                    _ => None,
                })
                .collect(),
            body: constructor.body.clone(),
            ..Default::default()
        };
        let replaced = self.try_virtualize("constructor", &mut function);
        self.class_methods_only = only;
        if replaced {
            defer_derived_receiver(&mut function, self.names);
            constructor.params = function
                .params
                .into_iter()
                .map(ParamOrTsParamProp::Param)
                .collect();
            constructor.body = function.body;
        } else {
            constructor.visit_mut_children_with(self);
        }
        self.strict_stack.pop();
    }
    pub(super) fn getter(&mut self, getter: &mut GetterProp) {
        let mut f = (*getter.function).clone();
        f.span = getter.span;
        let name = static_prop_key_name(&getter.key).unwrap_or_else(|| "<computed>".into());
        if self.try_virtualize(&name, &mut f) {
            getter.function = Box::new(f);
        } else {
            getter.visit_mut_children_with(self);
        }
    }
    pub(super) fn setter(&mut self, setter: &mut SetterProp) {
        let mut f = (*setter.function).clone();
        f.span = setter.span;
        let name = static_prop_key_name(&setter.key).unwrap_or_else(|| "<computed>".into());
        let original_params = f.params.clone();
        if self.try_virtualize(&name, &mut f) {
            self.setter_parameters(&mut f, &original_params);
            setter.function = Box::new(f);
        } else {
            setter.visit_mut_children_with(self);
        }
    }
}
