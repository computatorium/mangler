//! Lexical grammar at original direct-eval call sites. Source spans survive
//! lowering; the native capability names are allocated before source rewriting.
use crate::config::FileConfig;
use mangler_vm::eval::{EvalClassContext, EvalClassContexts};
use std::collections::{BTreeSet, HashMap};
use swc_core::ecma::{
    ast::*,
    visit::{Visit, VisitWith},
};

#[derive(Clone, Debug, Default)]
pub(crate) struct SourceClassContexts {
    pub(crate) calls: EvalClassContexts,
    providers: HashMap<String, Provider>,
    owners: HashMap<u32, Vec<EvalClassContext>>,
}
#[derive(Clone, Debug)]
struct Provider {
    inherited_binding: Option<String>,
    local_private_names: Vec<String>,
}
impl SourceClassContexts {
    pub(crate) fn for_owner(&self, span: u32) -> Vec<&EvalClassContext> {
        self.owners.get(&span).into_iter().flatten().collect()
    }
    pub(super) fn native_capsule_for(
        &self,
        original_binding: &str,
        context: &EvalClassContext,
        cfg: &FileConfig,
        apply: &str,
        iterator: &super::classes::IteratorAlias<'_>,
    ) -> Vec<Stmt> {
        if let Some(provider) = self.providers.get(original_binding) {
            super::classes::eval_context::native_capsule_with_parent(
                context,
                provider.inherited_binding.as_deref(),
                &provider.local_private_names,
                cfg,
                apply,
                iterator,
            )
        } else if context.capsule_binding != original_binding {
            super::classes::eval_context::native_capsule_with_parent(
                context,
                Some(original_binding),
                &[],
                cfg,
                apply,
                iterator,
            )
        } else {
            Vec::new()
        }
    }
}

#[derive(Clone, Default, Hash, PartialEq, Eq)]
struct Scope {
    private_names: Vec<String>,
    local_private_names: Vec<String>,
    home: u32,
    super_property: bool,
    super_call: bool,
    arguments_forbidden: bool,
}

pub(super) fn collect(
    program: &Program,
    cfg: &FileConfig,
    seed: Option<&EvalClassContext>,
) -> SourceClassContexts {
    struct Collector<'a> {
        cfg: &'a FileConfig,
        scope: Scope,
        capsules: HashMap<Scope, String>,
        calls: EvalClassContexts,
        providers: HashMap<String, Provider>,
        owners: HashMap<u32, std::collections::BTreeMap<String, EvalClassContext>>,
        inherited_binding: Option<String>,
        derived: bool,
    }
    impl Collector<'_> {
        fn function(&mut self, function: &Function, home: bool) {
            let previous = self.scope.clone();
            self.scope.home = function.span.lo.0;
            self.scope.super_property = home;
            self.scope.super_call = false;
            self.scope.arguments_forbidden = false;
            function.visit_children_with(self);
            self.scope = previous;
        }
        fn initializer(&mut self, span: u32, visit: impl FnOnce(&mut Self)) {
            let previous = self.scope.clone();
            self.scope.home = span;
            self.scope.super_property = true;
            self.scope.super_call = false;
            self.scope.arguments_forbidden = true;
            visit(self);
            self.scope = previous;
        }
    }
    impl Visit for Collector<'_> {
        fn visit_bin_expr(&mut self, binary: &BinExpr) {
            mangler_jsast::deep::walk_binary(binary, self);
        }
        fn visit_function(&mut self, function: &Function) {
            if mangler_jsast::span::is_eval_entry_span(function.span) {
                function.visit_children_with(self);
            } else {
                self.function(function, false);
            }
        }
        fn visit_class(&mut self, class: &Class) {
            class.decorators.visit_with(self);
            let previous = self.scope.clone();
            let mut names: BTreeSet<_> = self.scope.private_names.iter().cloned().collect();
            let mut local: BTreeSet<_> = self.scope.local_private_names.iter().cloned().collect();
            for member in &class.body {
                match member {
                    ClassMember::PrivateProp(property) => {
                        names.insert(property.key.name.to_string());
                        local.insert(property.key.name.to_string());
                    }
                    ClassMember::PrivateMethod(method) => {
                        names.insert(method.key.name.to_string());
                        local.insert(method.key.name.to_string());
                    }
                    ClassMember::AutoAccessor(accessor) => {
                        if let Key::Private(name) = &accessor.key {
                            names.insert(name.name.to_string());
                            local.insert(name.name.to_string());
                        }
                    }
                    _ => {}
                }
            }
            self.scope.private_names = names.into_iter().collect();
            self.scope.local_private_names = local.into_iter().collect();
            let derived = std::mem::replace(&mut self.derived, class.super_class.is_some());
            class.super_class.visit_with(self);
            class.body.visit_with(self);
            self.derived = derived;
            self.scope = previous;
        }
        fn visit_class_method(&mut self, method: &ClassMethod) {
            method.key.visit_with(self);
            self.function(&method.function, true);
        }
        fn visit_private_method(&mut self, method: &PrivateMethod) {
            self.function(&method.function, true);
        }
        fn visit_constructor(&mut self, constructor: &Constructor) {
            constructor.key.visit_with(self);
            let previous = self.scope.clone();
            self.scope.home = constructor.span.lo.0;
            self.scope.super_property = true;
            self.scope.super_call = self.derived;
            self.scope.arguments_forbidden = false;
            constructor.params.visit_with(self);
            constructor.body.visit_with(self);
            self.scope = previous;
        }
        fn visit_class_prop(&mut self, property: &ClassProp) {
            property.key.visit_with(self);
            property.decorators.visit_with(self);
            self.initializer(property.span.lo.0, |this| property.value.visit_with(this));
        }
        fn visit_private_prop(&mut self, property: &PrivateProp) {
            property.decorators.visit_with(self);
            self.initializer(property.span.lo.0, |this| property.value.visit_with(this));
        }
        fn visit_auto_accessor(&mut self, property: &AutoAccessor) {
            property.key.visit_with(self);
            property.decorators.visit_with(self);
            self.initializer(property.span.lo.0, |this| property.value.visit_with(this));
        }
        fn visit_static_block(&mut self, block: &StaticBlock) {
            self.initializer(block.span.lo.0, |this| block.body.visit_with(this));
        }
        fn visit_method_prop(&mut self, method: &MethodProp) {
            method.key.visit_with(self);
            self.function(&method.function, true);
        }
        fn visit_getter_prop(&mut self, getter: &GetterProp) {
            getter.key.visit_with(self);
            let previous = self.scope.clone();
            self.scope.home = getter.span.lo.0;
            self.scope.super_property = true;
            self.scope.super_call = false;
            self.scope.arguments_forbidden = false;
            getter.function.body.visit_with(self);
            self.scope = previous;
        }
        fn visit_setter_prop(&mut self, setter: &SetterProp) {
            setter.key.visit_with(self);
            let previous = self.scope.clone();
            self.scope.home = setter.span.lo.0;
            self.scope.super_property = true;
            self.scope.super_call = false;
            self.scope.arguments_forbidden = false;
            setter.function.params.visit_with(self);
            setter.function.body.visit_with(self);
            self.scope = previous;
        }
        fn visit_call_expr(&mut self, call: &CallExpr) {
            if call.span.lo.0 != 0
                && mangler_jsast::analysis::scope::is_direct_eval_callee(&call.callee)
                && (!self.scope.private_names.is_empty()
                    || self.scope.super_property
                    || self.scope.super_call
                    || self.scope.arguments_forbidden)
            {
                let inherited = self.scope.local_private_names.is_empty()
                    && (!self.scope.super_property || self.scope.home == 0);
                let binding = if inherited && let Some(binding) = &self.inherited_binding {
                    binding.clone()
                } else {
                    let binding = self
                        .capsules
                        .entry(self.scope.clone())
                        .or_insert_with(|| self.cfg.fresh_name())
                        .clone();
                    self.providers
                        .entry(binding.clone())
                        .or_insert_with(|| Provider {
                            inherited_binding: self.inherited_binding.clone(),
                            local_private_names: self.scope.local_private_names.clone(),
                        });
                    binding
                };
                let context = EvalClassContext {
                    capsule_binding: binding.clone(),
                    private_names: self.scope.private_names.clone(),
                    allow_super_property: self.scope.super_property,
                    allow_super_call: self.scope.super_call,
                    arguments_forbidden: self.scope.arguments_forbidden,
                };
                self.owners
                    .entry(self.scope.home)
                    .or_default()
                    .entry(binding)
                    .or_insert_with(|| context.clone());
                self.calls.insert(call.span.lo.0, context);
            }
            call.visit_children_with(self);
        }
    }
    let mut collector = Collector {
        cfg,
        scope: seed.map_or_else(Scope::default, |context| Scope {
            private_names: context.private_names.clone(),
            local_private_names: Vec::new(),
            home: 0,
            super_property: context.allow_super_property,
            super_call: context.allow_super_call,
            arguments_forbidden: context.arguments_forbidden,
        }),
        capsules: HashMap::new(),
        calls: HashMap::new(),
        providers: HashMap::new(),
        owners: HashMap::new(),
        inherited_binding: seed.map(|context| context.capsule_binding.clone()),
        derived: false,
    };
    program.visit_with(&mut collector);
    SourceClassContexts {
        calls: collector.calls,
        providers: collector.providers,
        owners: collector
            .owners
            .into_iter()
            .map(|(owner, contexts)| (owner, contexts.into_values().collect()))
            .collect(),
    }
}

/// Materialize only capabilities referenced by this surviving source envelope.
/// Nested object methods need their own VM HomeObject and are handled by the
/// object-method preparation hook, rather than capturing this envelope's super.
pub(super) fn native_bridges(
    function: &Function,
    contexts: &SourceClassContexts,
    cfg: &FileConfig,
    apply: &str,
    iterator: &super::classes::IteratorAlias<'_>,
) -> Vec<Stmt> {
    struct Used<'a> {
        contexts: &'a SourceClassContexts,
        bindings: std::collections::BTreeMap<String, &'a EvalClassContext>,
        method_depth: usize,
    }
    impl Visit for Used<'_> {
        fn visit_bin_expr(&mut self, binary: &BinExpr) {
            mangler_jsast::deep::walk_binary(binary, self);
        }
        fn visit_class(&mut self, _: &Class) {}
        fn visit_method_prop(&mut self, method: &MethodProp) {
            self.method_depth += 1;
            method.visit_children_with(self);
            self.method_depth -= 1;
        }
        fn visit_getter_prop(&mut self, method: &GetterProp) {
            self.method_depth += 1;
            method.visit_children_with(self);
            self.method_depth -= 1;
        }
        fn visit_setter_prop(&mut self, method: &SetterProp) {
            self.method_depth += 1;
            method.visit_children_with(self);
            self.method_depth -= 1;
        }
        fn visit_call_expr(&mut self, call: &CallExpr) {
            if let Some(context) = self.contexts.calls.get(&call.span.lo.0)
                && (self.method_depth == 0 || !context.allow_super_property)
            {
                self.bindings
                    .insert(context.capsule_binding.clone(), context);
            }
            call.visit_children_with(self);
        }
    }
    let mut used = Used {
        contexts,
        bindings: Default::default(),
        method_depth: 0,
    };
    function.params.visit_with(&mut used);
    function.body.visit_with(&mut used);
    used.bindings
        .values()
        .flat_map(|context| {
            contexts.native_capsule_for(&context.capsule_binding, context, cfg, apply, iterator)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};

    fn contexts(
        source: &str,
        seed: Option<&EvalClassContext>,
    ) -> (SourceClassContexts, HashMap<String, u32>) {
        let ast = Js.parse(source, &ParseOpts::default()).unwrap();
        let cfg = FileConfig::new(
            ResolvedConfig::try_from(ConfigFlags {
                preset: Some(Intensity::Minify),
                virtualize: Some("*".into()),
                ..Default::default()
            })
            .unwrap(),
            42,
            Default::default(),
        );
        let contexts = collect(ast.program(), &cfg, seed);
        struct Calls(HashMap<String, u32>);
        impl Visit for Calls {
            fn visit_call_expr(&mut self, call: &CallExpr) {
                if let Some(argument) = call.args.first()
                    && let Expr::Lit(Lit::Str(label)) = &*argument.expr
                {
                    self.0
                        .insert(label.value.to_string_lossy().into_owned(), call.span.lo.0);
                }
                call.visit_children_with(self);
            }
        }
        let mut calls = Calls(Default::default());
        ast.program().visit_with(&mut calls);
        (contexts, calls.0)
    }

    #[test]
    fn native_class_grammar_tracks_arrows_and_ordinary_functions() {
        let (contexts, spans) = contexts(
            "class C extends B{#secret;constructor(){eval('constructor');super()}m(){eval('method');(()=>eval('arrow'))();function nested(){eval('ordinary')}}field=(()=>eval('field'))();other=function(){eval('fieldFunction')};static{eval('static')}[eval('key')](){}}",
            None,
        );
        let get = |name: &str| &contexts.calls[&spans[name]];
        assert!(get("constructor").allow_super_call);
        assert!(get("method").allow_super_property);
        assert_eq!(get("method").capsule_binding, get("arrow").capsule_binding);
        assert!(!get("ordinary").allow_super_property);
        assert!(!get("ordinary").arguments_forbidden);
        assert!(get("field").arguments_forbidden);
        assert!(!get("fieldFunction").arguments_forbidden);
        assert!(get("static").arguments_forbidden);
        assert!(!get("key").allow_super_property);
        for context in contexts.calls.values() {
            assert_eq!(context.private_names, ["secret"]);
        }
    }

    #[test]
    fn class_private_environment_is_present_in_heritage() {
        let (contexts, spans) =
            contexts("class C extends (eval('heritage'),Object){#secret}", None);
        assert_eq!(contexts.calls[&spans["heritage"]].private_names, ["secret"]);
        assert!(!contexts.calls[&spans["heritage"]].allow_super_property);
    }

    #[test]
    fn seeded_private_banks_compose_local_shadowing_without_native_outer_names() {
        let seed = EvalClassContext {
            capsule_binding: "parentCapsule".into(),
            private_names: vec!["outer".into(), "parentOnly".into()],
            allow_super_property: true,
            allow_super_call: false,
            arguments_forbidden: true,
        };
        let (contexts, spans) = contexts(
            "eval('outside');(()=>eval('arrow'))();function nested(){eval('ordinary')}class I{#outer;#local;m(){eval('inside')}}",
            Some(&seed),
        );
        for label in ["outside", "arrow", "ordinary"] {
            assert_eq!(
                contexts.calls[&spans[label]].capsule_binding,
                "parentCapsule"
            );
        }
        assert!(contexts.calls[&spans["arrow"]].arguments_forbidden);
        assert!(!contexts.calls[&spans["ordinary"]].arguments_forbidden);
        let inner = &contexts.calls[&spans["inside"]];
        assert_eq!(inner.private_names, ["local", "outer", "parentOnly"]);
        let provider = &contexts.providers[&inner.capsule_binding];
        assert_eq!(provider.inherited_binding.as_deref(), Some("parentCapsule"));
        assert_eq!(provider.local_private_names, ["local", "outer"]);
    }
}
