//! Per-invocation eval capabilities for object methods created by bytecode.
use super::*;
use crate::passes::virtualize::eval_contexts::SourceClassContexts;

pub(in crate::passes::virtualize) fn prepare_nested_object_eval(
    function: &mut Function,
    contexts: &SourceClassContexts,
    cfg: &FileConfig,
    apply: &str,
    iterator: &IteratorAlias<'_>,
) -> (Vec<Stmt>, std::collections::HashSet<String>) {
    struct Prepare<'a> {
        contexts: &'a SourceClassContexts,
        cfg: &'a FileConfig,
        apply: &'a str,
        iterator: &'a IteratorAlias<'a>,
        native: Vec<Stmt>,
        hidden: std::collections::HashSet<String>,
    }
    impl Prepare<'_> {
        fn entry(&mut self, owner: u32, body: &mut FunctionBody) {
            let mut declarations = Vec::new();
            for context in self.contexts.for_owner(owner) {
                let binding = &context.capsule_binding;
                if body.stmts.iter().any(|statement| {
                    matches!(statement, Stmt::Decl(Decl::Var(declaration))
                        if declaration.span.is_dummy() && declaration.decls.iter().any(|declaration|
                            matches!(&declaration.name, Pat::Ident(name) if name.id.sym.as_ref() == binding)))
                }) {
                    continue;
                }
                let private = if context.private_names.is_empty() {
                    "{__proto__:null}".to_string()
                } else {
                    let bank = self.cfg.fresh_name();
                    let mut bank_context = context.clone();
                    bank_context.capsule_binding = bank.clone();
                    bank_context.allow_super_property = false;
                    bank_context.allow_super_call = false;
                    self.native.extend(self.contexts.native_capsule_for(
                        binding,
                        &bank_context,
                        self.cfg,
                        self.apply,
                        self.iterator,
                    ));
                    self.hidden.insert(bank.clone());
                    format!("{bank}.p")
                };
                let source = format!(
                    "function _(){{const {binding}={{__proto__:null,p:{private},s:_mangler_object_super_provider(),t:()=>this,n:()=>new.target}};}}"
                );
                let mut statements = parse_fn_body_stmts(&source)
                    .expect("object eval capability initializer parses");
                struct Generated;
                impl VisitMut for Generated {
                    fn visit_mut_span(&mut self, span: &mut swc_core::common::Span) {
                        *span = DUMMY_SP;
                    }
                    fn visit_mut_ident(&mut self, ident: &mut Ident) {
                        ident.span = DUMMY_SP;
                        if ident.sym == *"_mangler_object_super_provider" {
                            ident.sym = "\0mangler_object_super_provider".into();
                        }
                    }
                }
                statements.visit_mut_with(&mut Generated);
                declarations.extend(statements);
                self.hidden.insert(binding.clone());
            }
            let at = body
                .stmts
                .iter()
                .take_while(|statement| mangler_jsast::directives::is_directive(statement))
                .count();
            body.stmts.splice(at..at, declarations);
        }
    }
    impl VisitMut for Prepare<'_> {
        fn visit_mut_bin_expr(&mut self, binary: &mut BinExpr) {
            mangler_jsast::deep::walk_binary_mut(binary, self);
        }
        // A nested class owns its own native private environment. Its envelope
        // preparation will materialize capabilities at that lexical boundary.
        fn visit_mut_class(&mut self, _: &mut Class) {}
        fn visit_mut_method_prop(&mut self, method: &mut MethodProp) {
            method.visit_mut_children_with(self);
            if let Some(body) = &mut method.function.body {
                self.entry(method.function.span.lo.0, body);
            }
        }
        fn visit_mut_getter_prop(&mut self, getter: &mut GetterProp) {
            getter.visit_mut_children_with(self);
            if let Some(body) = &mut getter.function.body {
                self.entry(getter.span.lo.0, body);
            }
        }
        fn visit_mut_setter_prop(&mut self, setter: &mut SetterProp) {
            setter.visit_mut_children_with(self);
            if let Some(body) = &mut setter.function.body {
                self.entry(setter.span.lo.0, body);
            }
        }
    }
    let mut prepare = Prepare {
        contexts,
        cfg,
        apply,
        iterator,
        native: Vec::new(),
        hidden: Default::default(),
    };
    function.params.visit_mut_with(&mut prepare);
    function.body.visit_mut_with(&mut prepare);
    (prepare.native, prepare.hidden)
}

#[cfg(test)]
mod tests {
    #[test]
    fn nested_object_eval_preserves_home_receiver_defaults_and_escaped_arrows() {
        use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
        use mangler_jsast::ParseOpts;
        use mangler_testkit::cross_engine::{Engine, evaluate_many};
        let node = std::env::var_os("MANGLER_TESTKIT_NODE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "node".into());
        for source in [
            "function pay(){let base={x:4,add(n){return this.n+n}},o={__proto__:base,n:7,m(a=eval('super.x')){return[a,eval('super.add(3)'),eval('super.x+=2'),this.x]}};return[o.m(),o.m.call({n:9})]}globalThis.__out=JSON.stringify(pay());",
            "function pay(){let o={__proto__:{x:3},m(){return eval('()=>super.x')}};let f=o.m();Object.setPrototypeOf(o,{x:8});return f()}globalThis.__out=JSON.stringify(pay());",
            "function pay(){let objects=[];for(let i=0;i<3;i++)objects.push({__proto__:{x:i},m(a=eval('super.x')){return a}});return objects.map(o=>o.m())}globalThis.__out=JSON.stringify(pay());",
            "function pay(){let o={__proto__:{x:5},get value(){return eval('super.x')},set value([v=eval('super.x')]){this.saved=v}};o.value=[];return[o.value,o.saved]}globalThis.__out=JSON.stringify(pay());",
            "function pay(){class Account{#x=9;object(){let self=this;return{__proto__:{x:2},m(){return[eval('self.#x'),eval('super.x')]}}}}return new Account().object().m()}globalThis.__out=JSON.stringify(pay());",
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
}
