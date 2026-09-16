//! Resolve a private analysis copy of the runtime before selecting rewrites.
//! Plans contain no hygiene marks: applying them to the original AST preserves
//! source-factory identifiers and the caller's existing resolver contexts.
use std::collections::{HashMap, HashSet, VecDeque};
use swc_core::common::{DUMMY_SP, GLOBALS, Globals, Mark, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::transforms::base::resolver;
use swc_core::ecma::visit::{VisitMut, VisitMutWith};

pub(super) type Plan<T> = VecDeque<Option<T>>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Intrinsic {
    pub path: Vec<String>,
    pub bind_receiver: bool,
}

fn resolved<T>(statements: &[Stmt], inspect: impl FnOnce(&mut Vec<Stmt>, SyntaxContext) -> T) -> T {
    GLOBALS.set(&Globals::new(), || {
        let mut program = Program::Script(Script {
            span: DUMMY_SP,
            body: statements.to_vec(),
            shebang: None,
        });
        struct Reset;
        impl VisitMut for Reset {
            fn visit_mut_syntax_context(&mut self, context: &mut SyntaxContext) {
                *context = SyntaxContext::empty();
            }
        }
        program.visit_mut_with(&mut Reset);
        let unresolved = Mark::new();
        program.visit_mut_with(&mut resolver(unresolved, Mark::new(), false));
        mangler_jsast::Js::repair_resolver_scopes(&mut program);
        let Program::Script(script) = &mut program else {
            unreachable!()
        };
        inspect(
            &mut script.body,
            SyntaxContext::empty().apply_mark(unresolved),
        )
    })
}

/// Visit generated machinery, including native arguments factories, without
/// rewriting source closures embedded in constant-table fields.
pub(super) fn visit_runtime<V: VisitMut>(statements: &mut [Stmt], table: &str, visitor: &mut V) {
    for statement in statements {
        let source_factories = matches!(statement, Stmt::Decl(Decl::Var(v)) if v.decls.iter().any(|d| matches!(&d.name, Pat::Ident(b) if b.id.sym.as_ref() == table)));
        if source_factories {
            if let Stmt::Decl(Decl::Var(v)) = statement {
                for d in &mut v.decls {
                    if let Some(Expr::Array(chunks)) = d.init.as_deref_mut() {
                        for chunk in chunks.elems.iter_mut().flatten() {
                            if let Expr::Array(fields) = chunk.expr.as_mut()
                                && let Some(Some(factory)) = fields.elems.get_mut(5)
                            {
                                factory.expr.visit_mut_with(visitor);
                            }
                        }
                    }
                }
            }
        } else {
            statement.visit_mut_with(visitor);
        }
    }
}

pub(super) fn intrinsic_plan(statements: &[Stmt], table: &str) -> Plan<Intrinsic> {
    resolved(statements, |statements, unresolved| {
        struct Record {
            unresolved: SyntaxContext,
            plan: Plan<Intrinsic>,
            callee: bool,
        }
        impl VisitMut for Record {
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                let path = intrinsic_path(expression, self.unresolved);
                let descend = path.is_none();
                self.plan.push_back(path.map(|path| Intrinsic {
                    bind_receiver:
                        self.callee
                            && path.first().is_some_and(|root| root != "Reflect")
                            && path.last().is_some_and(|key| {
                                matches!(key.as_str(), "call" | "apply" | "bind")
                            }),
                    path,
                }));
                if descend {
                    let previous = self.callee;
                    if !matches!(expression, Expr::Paren(_)) {
                        self.callee = false;
                    }
                    expression.visit_mut_children_with(self);
                    self.callee = previous;
                }
            }

            fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
                let previous = self.callee;
                self.callee = true;
                call.callee.visit_mut_with(self);
                self.callee = false;
                call.args.visit_mut_with(self);
                call.type_args.visit_mut_with(self);
                self.callee = previous;
            }
        }
        let mut record = Record {
            unresolved,
            plan: Plan::new(),
            callee: false,
        };
        visit_runtime(statements, table, &mut record);
        record.plan
    })
}

fn intrinsic_path(expression: &Expr, unresolved: SyntaxContext) -> Option<Vec<String>> {
    match expression {
        Expr::Ident(id) if id.ctxt == unresolved && super::intrinsic(id.sym.as_ref()) => {
            Some(vec![id.sym.to_string()])
        }
        Expr::Member(member) => {
            let MemberProp::Ident(key) = &member.prop else {
                return None;
            };
            let mut path = intrinsic_path(&member.obj, unresolved)?;
            path.push(key.sym.to_string());
            Some(path)
        }
        _ => None,
    }
}

/// Pool only declarations whose free bindings remain available at the prologue.
/// The declaration plan follows candidate order separately from expression order:
/// an unsafe declaration and every reference to it remain in their original scope.
pub(super) fn helper_plan(
    statements: &[Stmt],
    table: &str,
    helpers: &[&str],
) -> (Plan<String>, HashSet<String>, Plan<String>) {
    resolved(statements, |statements, unresolved| {
        #[derive(Default)]
        struct Bindings(HashSet<Id>);
        impl VisitMut for Bindings {
            fn visit_mut_binding_ident(&mut self, binding: &mut BindingIdent) {
                self.0.insert(binding.id.to_id());
                binding.visit_mut_children_with(self);
            }
            fn visit_mut_fn_decl(&mut self, function: &mut FnDecl) {
                self.0.insert(function.ident.to_id());
                function.visit_mut_children_with(self);
            }
            fn visit_mut_fn_expr(&mut self, function: &mut FnExpr) {
                if let Some(name) = &function.ident {
                    self.0.insert(name.to_id());
                }
                function.visit_mut_children_with(self);
            }
            fn visit_mut_class_decl(&mut self, class: &mut ClassDecl) {
                self.0.insert(class.ident.to_id());
                class.visit_mut_children_with(self);
            }
            fn visit_mut_class_expr(&mut self, class: &mut ClassExpr) {
                if let Some(name) = &class.ident {
                    self.0.insert(name.to_id());
                }
                class.visit_mut_children_with(self);
            }
        }
        #[derive(Default)]
        struct References(HashSet<Id>, bool);
        impl VisitMut for References {
            fn visit_mut_ident(&mut self, id: &mut Ident) {
                self.0.insert(id.to_id());
            }
            fn visit_mut_labeled_stmt(&mut self, statement: &mut LabeledStmt) {
                statement.body.visit_mut_with(self);
            }
            fn visit_mut_break_stmt(&mut self, _: &mut BreakStmt) {}
            fn visit_mut_continue_stmt(&mut self, _: &mut ContinueStmt) {}
            fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
                // Direct eval can name locals absent from the static reference set.
                if matches!(&call.callee, Callee::Expr(e) if matches!(e.as_ref(), Expr::Ident(i) if i.sym == *"eval"))
                {
                    self.1 = true;
                }
                call.visit_mut_children_with(self);
            }
        }
        struct Candidate {
            id: Id,
            key: String,
            free: HashSet<Id>,
            safe: bool,
        }
        let mut outer = HashSet::new();
        for statement in statements.iter_mut() {
            match statement {
                Stmt::Decl(Decl::Fn(function)) => {
                    outer.insert(function.ident.to_id());
                }
                Stmt::Decl(Decl::Class(class)) => {
                    outer.insert(class.ident.to_id());
                }
                Stmt::Decl(Decl::Var(declaration)) => {
                    for variable in &mut declaration.decls {
                        let mut bindings = Bindings::default();
                        variable.name.visit_mut_with(&mut bindings);
                        outer.extend(bindings.0);
                    }
                }
                _ => {}
            }
        }
        let mut candidates = Vec::new();
        for statement in statements.iter_mut() {
            let Stmt::Decl(Decl::Fn(interpreter)) = statement else {
                continue;
            };
            let Some(body) = &mut interpreter.function.body else {
                continue;
            };
            let mut add = |id: &Ident, function: &mut Function, expression_name: Option<&Ident>| {
                let Some(index) = helpers.iter().position(|name| *name == id.sym.as_ref()) else {
                    return;
                };
                let mut bindings = Bindings::default();
                function.visit_mut_with(&mut bindings);
                if let Some(name) = expression_name {
                    bindings.0.insert(name.to_id());
                }
                let mut references = References::default();
                function.visit_mut_with(&mut references);
                references.0.retain(|id| !bindings.0.contains(id));
                candidates.push(Candidate {
                    id: id.to_id(),
                    key: ((b'a' + index as u8) as char).to_string(),
                    free: references.0,
                    safe: !references.1,
                });
            };
            for statement in &mut body.stmts {
                match statement {
                    Stmt::Decl(Decl::Fn(helper)) => add(&helper.ident, &mut helper.function, None),
                    Stmt::Decl(Decl::Var(declaration)) => {
                        for variable in &mut declaration.decls {
                            if let Pat::Ident(binding) = &variable.name
                                && let Some(Expr::Fn(function)) = variable.init.as_deref_mut()
                            {
                                add(&binding.id, &mut function.function, function.ident.as_ref());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let ids: HashSet<_> = candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect();
        for candidate in &mut candidates {
            candidate.safe &= candidate
                .free
                .iter()
                .all(|id| id.1 == unresolved || outer.contains(id) || ids.contains(id));
        }
        // Same-spelling helpers may share code only when they share their outer
        // environment. A local shadow never becomes an outer alias by spelling.
        let mut environments: HashMap<String, HashSet<Id>> = HashMap::new();
        let mut conflicting = HashSet::new();
        for candidate in candidates.iter().filter(|candidate| candidate.safe) {
            let environment = candidate
                .free
                .iter()
                .filter(|id| !ids.contains(*id))
                .cloned()
                .collect();
            if let Some(previous) = environments.get(&candidate.key) {
                if previous != &environment {
                    conflicting.insert(candidate.key.clone());
                }
            } else {
                environments.insert(candidate.key.clone(), environment);
            }
        }
        for candidate in &mut candidates {
            candidate.safe &= !conflicting.contains(&candidate.key);
        }
        loop {
            let unsafe_ids: HashSet<_> = candidates
                .iter()
                .filter(|candidate| !candidate.safe)
                .map(|candidate| candidate.id.clone())
                .collect();
            let mut changed = false;
            for candidate in &mut candidates {
                if candidate.safe && candidate.free.iter().any(|id| unsafe_ids.contains(id)) {
                    candidate.safe = false;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let names: HashMap<_, _> = candidates
            .iter()
            .filter(|candidate| candidate.safe)
            .map(|candidate| (candidate.id.clone(), candidate.key.clone()))
            .collect();
        let declarations = candidates
            .iter()
            .map(|candidate| candidate.safe.then(|| candidate.key.clone()))
            .collect();
        struct Record {
            names: HashMap<Id, String>,
            plan: Plan<String>,
            owner: Option<String>,
            roots: HashSet<String>,
            edges: HashMap<String, HashSet<String>>,
        }
        impl VisitMut for Record {
            fn visit_mut_fn_decl(&mut self, function: &mut FnDecl) {
                let previous = self.owner.clone();
                if let Some(key) = self.names.get(&function.ident.to_id()) {
                    self.owner = Some(key.clone());
                }
                function.visit_mut_children_with(self);
                self.owner = previous;
            }
            fn visit_mut_var_declarator(&mut self, variable: &mut VarDeclarator) {
                let previous = self.owner.clone();
                if let Pat::Ident(binding) = &variable.name
                    && let Some(key) = self.names.get(&binding.id.to_id())
                {
                    self.owner = Some(key.clone());
                }
                variable.visit_mut_children_with(self);
                self.owner = previous;
            }
            fn visit_mut_expr(&mut self, expression: &mut Expr) {
                let key = match expression {
                    Expr::Ident(id) => self.names.get(&id.to_id()).cloned(),
                    _ => None,
                };
                if let Some(key) = &key {
                    if let Some(owner) = &self.owner {
                        self.edges
                            .entry(owner.clone())
                            .or_default()
                            .insert(key.clone());
                    } else {
                        self.roots.insert(key.clone());
                    }
                }
                let descend = key.is_none();
                self.plan.push_back(key);
                if descend {
                    expression.visit_mut_children_with(self);
                }
            }
        }
        let mut record = Record {
            names,
            plan: Plan::new(),
            owner: None,
            roots: HashSet::new(),
            edges: HashMap::new(),
        };
        visit_runtime(statements, table, &mut record);
        let mut reachable = record.roots;
        let mut pending: Vec<_> = reachable.iter().cloned().collect();
        while let Some(owner) = pending.pop() {
            for key in record.edges.get(&owner).into_iter().flatten() {
                if reachable.insert(key.clone()) {
                    pending.push(key.clone());
                }
            }
        }
        (record.plan, reachable, declarations)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};

    fn statements(source: &str) -> Vec<Stmt> {
        match Js
            .parse(source, &ParseOpts::default())
            .unwrap()
            .into_program()
        {
            Program::Script(script) => script.body,
            Program::Module(module) => module
                .body
                .into_iter()
                .map(|item| match item {
                    ModuleItem::Stmt(statement) => statement,
                    _ => panic!("expected statements"),
                })
                .collect(),
        }
    }

    fn paths(source: &str) -> Vec<String> {
        intrinsic_plan(&statements(source), "table")
            .into_iter()
            .flatten()
            .map(|intrinsic| intrinsic.path.join("."))
            .collect()
    }

    #[test]
    fn intrinsic_identity_reads_and_receiver_calls_have_distinct_captures() {
        let selected: Vec<_> = intrinsic_plan(
            &statements("const b=Function.prototype.bind; Function.prototype.bind(null); (Function.prototype.call)(null); const c=Function.prototype.call; Object.prototype.hasOwnProperty.call({},'x'); Reflect.apply(b,null,[]);"),
            "table",
        )
        .into_iter()
        .flatten()
        .map(|intrinsic| (intrinsic.path.join("."), intrinsic.bind_receiver))
        .collect();
        assert_eq!(
            selected,
            vec![
                ("Function.prototype.bind".into(), false),
                ("Function.prototype.bind".into(), true),
                ("Function.prototype.call".into(), true),
                ("Function.prototype.call".into(), false),
                ("Object.prototype.hasOwnProperty.call".into(), true),
                ("Reflect.apply".into(), false),
            ]
        );
    }

    #[test]
    fn only_unbound_intrinsics_are_isolated_across_lexical_scopes() {
        assert_eq!(
            paths(
                r#"
            function interpreter() {
                Map([], x => x);
                function Map(array, fn) { return array.map(fn); }
                function local(Object, { Reflect }) { return Object.keys(Reflect); }
                { let Array = { from() {} }; Array.from([]); }
                try {} catch (Symbol) { Symbol.iterator; }
                let named = function String() { return String.name; };
                function defaults(value = Object.keys({})) {
                    let Object = {}; return Object;
                }
                return [Object.keys({}), Reflect.apply(local, null, [])];
            }
        "#
            ),
            ["Object.keys", "Object.keys", "Reflect.apply"]
        );
    }

    #[test]
    fn class_heritage_and_switch_cases_use_their_real_lexical_scope() {
        assert_eq!(
            paths(
                r#"
            let cls = class Object extends Object {};
            switch (Object.keys({})) {
                case Array.from([]): break;
                default: let Array = {}; Array.from([]);
            }
            Array.from([]);
        "#
            ),
            ["Object.keys", "Array.from"]
        );
    }

    #[test]
    fn source_factories_keep_dynamic_intrinsics_but_argument_factories_are_isolated() {
        assert_eq!(
            paths(
                r#"
            var table = [[[], [function(){return Object.keys({})}], false, 0, null,
                function(invoke){return Object.defineProperty(invoke, 'name', {value:'f'})}]];
            function interpreter(){return Reflect.apply(function(){}, null, [])}
        "#
            ),
            ["Object.defineProperty", "Reflect.apply"]
        );
    }

    #[test]
    fn helper_pooling_follows_declarations_without_rewriting_shadows() {
        let source = statements(
            r#"
            function interpreter() {
                function MapItems(array, fn) { return array.map(fn); }
                MapItems([], x => x);
                function nested() { return MapItems([], x => x); }
                function parameter(MapItems) { return MapItems(); }
                try {} catch (MapItems) { MapItems(); }
                let named = function MapItems() { return MapItems(); };
                { const MapItems = () => 1; MapItems(); }
            }
        "#,
        );
        let refs: Vec<_> = helper_plan(&source, "table", &["MapItems"])
            .0
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(refs, ["a", "a"]);
    }

    #[test]
    fn helper_pooling_discards_unreachable_cycles_but_retains_transitive_dependencies() {
        let source = statements(
            r#"
            function interpreter() {
                function List(){return Push();}
                function Push(){return 1;}
                function Pop(){return Check();}
                function Check(){return Pop();}
                return List();
            }
        "#,
        );
        let (_, reachable, _) = helper_plan(&source, "table", &["List", "Push", "Pop", "Check"]);
        assert_eq!(reachable, HashSet::from(["a".into(), "b".into()]));
    }

    #[test]
    fn variable_helpers_share_the_same_binding_identity_and_reachability_rules() {
        let source = statements(
            r#"
            function interpreter() {
                var Sd = function(value){ return List(value); }, untouched = 7;
                function List(value){return value;}
                var Pop = function(){return Pop();};
                function shadow(Sd){return Sd();}
                return Sd(untouched);
            }
        "#,
        );
        let (plan, reachable, _) = helper_plan(&source, "table", &["Sd", "List", "Pop"]);
        assert_eq!(reachable, HashSet::from(["a".into(), "b".into()]));
        assert_eq!(
            plan.into_iter().flatten().collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
    }

    #[test]
    fn helper_pooling_keeps_local_and_transitive_captures_in_the_interpreter() {
        let source = statements(
            "function interpreter(parameter){var local=parameter;             function Append(value){return local+value;}             function Slice(value){return Append(value);}             function List(value){return value;}             function Push(){return List(parameter);}             return [Slice(1),Push()];}",
        );
        let (references, reachable, declarations) =
            helper_plan(&source, "table", &["Append", "Slice", "List", "Push"]);
        assert_eq!(
            declarations.into_iter().collect::<Vec<_>>(),
            [None, None, Some("c".into()), None]
        );
        // The safe List remains reachable from the unpooled Push.
        assert_eq!(reachable, HashSet::from(["c".into()]));
        assert_eq!(references.into_iter().flatten().collect::<Vec<_>>(), ["c"]);
    }

    #[test]
    fn helper_pooling_distinguishes_local_shadows_from_shared_outer_aliases() {
        let source = statements(
            "var alias=Object.defineProperty;             function first(){function Append(value){return alias(value);}return Append(1);}             function second(alias){function Append(value){return alias(value);}return Append(2);}",
        );
        let (references, reachable, declarations) = helper_plan(&source, "table", &["Append"]);
        assert_eq!(
            declarations.into_iter().collect::<Vec<_>>(),
            [Some("a".into()), None]
        );
        assert_eq!(reachable, HashSet::from(["a".into()]));
        assert_eq!(references.into_iter().flatten().collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn helper_pooling_preserves_parameters_nested_bindings_and_direct_eval() {
        let source = statements(
            "function interpreter(local){             function List(local){function nested(){return local;}return nested();}             var Append=function(value){return (()=>local+value)();};             function Check(){return eval('local');}             return [List(1),Append(2),Check()];}",
        );
        let (_, reachable, declarations) =
            helper_plan(&source, "table", &["List", "Append", "Check"]);
        assert_eq!(
            declarations.into_iter().collect::<Vec<_>>(),
            [Some("a".into()), None, None]
        );
        assert_eq!(reachable, HashSet::from(["a".into()]));
    }

    #[test]
    fn analysis_preserves_the_callers_existing_hygiene() {
        Js::with_globals(|| {
            let mut ast = Js
                .parse(
                    "function f(Object){return [Object.keys({}),Reflect.apply(f,null,[])]}",
                    &ParseOpts::default(),
                )
                .unwrap();
            Js::resolve(&mut ast);
            let Program::Script(script) = ast.program() else {
                panic!("expected script")
            };
            let before = format!("{:?}", script.body);
            let selected: Vec<_> = intrinsic_plan(&script.body, "table")
                .into_iter()
                .flatten()
                .map(|intrinsic| intrinsic.path)
                .collect();
            assert_eq!(selected, [vec!["Reflect".to_string(), "apply".to_string()]]);
            assert_eq!(format!("{:?}", script.body), before);
        });
    }
}
