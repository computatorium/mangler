//! Dynamic source grammar and its lexical class privileges.
//! Private brands and home objects never enter this module or the source request.
use std::collections::HashSet;

use serde_json::Value;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

#[derive(Clone, Debug, Default)]
pub(crate) struct Grammar {
    pub private_names: Vec<String>,
    pub allow_super_property: bool,
    pub allow_super_call: bool,
    pub arguments_forbidden: bool,
}

impl Grammar {
    pub fn bind(&self, script: &Script) -> mangler_vm::eval::EvalClassContext {
        struct Names(HashSet<String>);
        impl Visit for Names {
            fn visit_ident(&mut self, name: &Ident) {
                self.0.insert(name.sym.to_string());
            }
            fn visit_bin_expr(&mut self, expression: &BinExpr) {
                mangler_jsast::deep::walk_binary(expression, self);
            }
        }
        let mut names = Names(HashSet::new());
        script.visit_with(&mut names);
        let mut capsule_binding = "__mangler_eval_capsule".to_owned();
        while names.0.contains(&capsule_binding) {
            capsule_binding.push('_');
        }
        mangler_vm::eval::EvalClassContext {
            capsule_binding,
            private_names: self.private_names.clone(),
            allow_super_property: self.allow_super_property,
            allow_super_call: self.allow_super_call,
            arguments_forbidden: self.arguments_forbidden,
        }
    }

    pub fn from_request(request: &Value) -> Result<Option<Self>, String> {
        let Some(value) = request.get("classContext") else {
            return Ok(None);
        };
        if value.is_null() {
            return Ok(None);
        }
        if !value.is_object() {
            return Err("classContext must be an object".into());
        }
        let private_names = value["privateNames"]
            .as_array()
            .ok_or("privateNames must be an array")?
            .iter()
            .map(|name| {
                name.as_str()
                    .map(str::to_owned)
                    .ok_or("private names must be strings")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let flag = |name: &str| {
            value.get(name).map_or(Ok(false), |value| {
                value
                    .as_bool()
                    .ok_or_else(|| format!("{name} must be boolean"))
            })
        };
        Ok(Some(Self {
            private_names,
            allow_super_property: flag("allowSuperProperty")?,
            allow_super_call: flag("allowSuperCall")?,
            arguments_forbidden: flag("argumentsForbidden")?,
        }))
    }
}

pub(crate) fn parse(
    source: &str,
    request: &Value,
    grammar: Option<&Grammar>,
) -> Result<Script, String> {
    use swc_core::common::{FileName, SourceMap, sync::Lrc};
    use swc_core::ecma::parser::{Context, EsSyntax, Lexer, Parser, StringInput, Syntax};
    let cm: Lrc<SourceMap> = Default::default();
    let file = cm.new_source_file(
        Lrc::new(FileName::Custom("eval.js".into())),
        source.to_string(),
    );
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax {
            explicit_resource_management: true,
            ..Default::default()
        }),
        EsVersion::EsNext,
        StringInput::from(&*file),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let mut context = parser.ctx();
    if request["strict"].as_bool().unwrap_or(false) {
        context.insert(Context::Strict);
    }
    if request["allowNewTarget"].as_bool().unwrap_or(false) {
        context.insert(Context::InsideNonArrowFunctionScope);
    }
    if grammar.is_some_and(|grammar| grammar.allow_super_property) {
        context.insert(Context::AllowDirectSuper);
    }
    parser.set_ctx(context);
    parser.set_allow_super_call(grammar.is_some_and(|grammar| grammar.allow_super_call));
    // SWC's unambiguous entry point distinguishes an ordinary `await` binding
    // from an AwaitExpression. Eval still accepts only the Script result.
    let mut program = parser
        .parse_program()
        .map_err(|error| format!("{:?}", error.kind()))?;
    let mut errors = parser.take_errors();
    mangler_jsast::Js::repair_annex_b(
        &mut program,
        request["strict"].as_bool().unwrap_or(false),
        &mut errors,
    );
    if let Some(error) = errors.first() {
        return Err(format!("{:?}", error.kind()));
    }
    mangler_jsast::Js::repair_pattern_elisions(&mut program, source, file.start_pos);
    let Program::Script(script) = program else {
        return Err("eval source must use Script grammar".into());
    };
    let grammar = grammar.cloned().unwrap_or_default();
    let mut validate = Validate {
        private_scopes: vec![grammar.private_names.into_iter().collect()],
        flags: Flags {
            property: grammar.allow_super_property,
            call: grammar.allow_super_call,
            arguments: grammar.arguments_forbidden,
        },
        error: None,
    };
    script.visit_with(&mut validate);
    match validate.error {
        Some(error) => Err(error),
        None => Ok(script),
    }
}

#[derive(Clone, Copy, Default)]
struct Flags {
    property: bool,
    call: bool,
    arguments: bool,
}

/// Function constructors start with an empty private environment, regardless
/// of the calling class. Nested source classes establish their own environments.
pub(crate) fn validate_constructor(function: &Function) -> Result<(), String> {
    let mut validate = Validate::default();
    function.visit_with(&mut validate);
    validate.error.map_or(Ok(()), Err)
}

#[derive(Default)]
struct Validate {
    private_scopes: Vec<HashSet<String>>,
    flags: Flags,
    error: Option<String>,
}
impl Validate {
    fn reject(&mut self, reason: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(reason.into());
        }
    }
    fn function(&mut self, function: &Function, flags: Flags) {
        let old = std::mem::replace(&mut self.flags, flags);
        function.visit_children_with(self);
        self.flags = old;
    }
    fn field(&mut self, value: Option<&Expr>) {
        let old = std::mem::replace(
            &mut self.flags,
            Flags {
                property: true,
                call: false,
                arguments: true,
            },
        );
        if let Some(value) = value {
            value.visit_with(self);
        }
        self.flags = old;
    }
}
impl Visit for Validate {
    fn visit_bin_expr(&mut self, expression: &BinExpr) {
        mangler_jsast::deep::walk_binary(expression, self);
    }
    fn visit_ident(&mut self, identifier: &Ident) {
        if self.flags.arguments && identifier.sym == *"arguments" {
            self.reject("arguments is not allowed in a class field initializer or static initialization block");
        }
    }
    fn visit_labeled_stmt(&mut self, statement: &LabeledStmt) {
        statement.body.visit_with(self);
    }
    fn visit_break_stmt(&mut self, _: &BreakStmt) {}
    fn visit_continue_stmt(&mut self, _: &ContinueStmt) {}
    fn visit_private_name(&mut self, name: &PrivateName) {
        if !self
            .private_scopes
            .iter()
            .rev()
            .any(|scope| scope.contains(name.name.as_ref()))
        {
            self.reject(format!(
                "Private field #{} must be declared in an enclosing class",
                name.name
            ));
        }
    }
    fn visit_super_prop_expr(&mut self, expression: &SuperPropExpr) {
        if !self.flags.property {
            self.reject("super property is not allowed in this eval context");
        }
        expression.prop.visit_with(self);
    }
    fn visit_call_expr(&mut self, expression: &CallExpr) {
        if matches!(expression.callee, Callee::Super(_)) && !self.flags.call {
            self.reject("super() is not allowed in this eval context");
        }
        expression.visit_children_with(self);
    }
    fn visit_unary_expr(&mut self, expression: &UnaryExpr) {
        fn private_reference(expression: &Expr) -> bool {
            match expression {
                Expr::Paren(parentheses) => private_reference(&parentheses.expr),
                Expr::Member(member) => matches!(member.prop, MemberProp::PrivateName(_)),
                Expr::OptChain(chain) => {
                    matches!(&*chain.base, OptChainBase::Member(member) if matches!(member.prop, MemberProp::PrivateName(_)))
                }
                _ => false,
            }
        }
        if expression.op == UnaryOp::Delete && private_reference(&expression.arg) {
            self.reject("Private fields cannot be deleted");
        }
        expression.visit_children_with(self);
    }
    fn visit_function(&mut self, function: &Function) {
        self.function(function, Flags::default());
    }
    fn visit_method_prop(&mut self, method: &MethodProp) {
        method.key.visit_with(self);
        self.function(
            &method.function,
            Flags {
                property: true,
                ..Default::default()
            },
        );
    }
    fn visit_getter_prop(&mut self, getter: &GetterProp) {
        getter.key.visit_with(self);
        self.function(
            &getter.function,
            Flags {
                property: true,
                ..Default::default()
            },
        );
    }
    fn visit_setter_prop(&mut self, setter: &SetterProp) {
        setter.key.visit_with(self);
        self.function(
            &setter.function,
            Flags {
                property: true,
                ..Default::default()
            },
        );
    }
    fn visit_class(&mut self, class: &Class) {
        // Heritage is evaluated before the new class private environment exists.
        class.super_class.visit_with(self);
        class.decorators.visit_with(self);
        let names = class
            .body
            .iter()
            .filter_map(|member| match member {
                ClassMember::PrivateProp(property) => Some(property.key.name.to_string()),
                ClassMember::PrivateMethod(method) => Some(method.key.name.to_string()),
                ClassMember::AutoAccessor(accessor) => match &accessor.key {
                    Key::Private(name) => Some(name.name.to_string()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        self.private_scopes.push(names);
        for member in &class.body {
            match member {
                ClassMember::Constructor(constructor) => {
                    let old = std::mem::replace(
                        &mut self.flags,
                        Flags {
                            property: true,
                            call: class.super_class.is_some(),
                            arguments: false,
                        },
                    );
                    constructor.params.visit_with(self);
                    constructor.body.visit_with(self);
                    self.flags = old;
                }
                ClassMember::Method(method) => {
                    method.key.visit_with(self);
                    self.function(
                        &method.function,
                        Flags {
                            property: true,
                            ..Default::default()
                        },
                    );
                }
                ClassMember::PrivateMethod(method) => self.function(
                    &method.function,
                    Flags {
                        property: true,
                        ..Default::default()
                    },
                ),
                ClassMember::ClassProp(property) => {
                    property.key.visit_with(self);
                    self.field(property.value.as_deref());
                }
                ClassMember::PrivateProp(property) => self.field(property.value.as_deref()),
                ClassMember::AutoAccessor(accessor) => {
                    if let Key::Public(key) = &accessor.key {
                        key.visit_with(self);
                    }
                    self.field(accessor.value.as_deref());
                }
                ClassMember::StaticBlock(block) => {
                    let old = std::mem::replace(
                        &mut self.flags,
                        Flags {
                            property: true,
                            call: false,
                            arguments: true,
                        },
                    );
                    block.body.visit_with(self);
                    self.flags = old;
                }
                ClassMember::Empty(_) | ClassMember::TsIndexSignature(_) => {}
            }
        }
        self.private_scopes.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn eval_annex_b_branches_respect_inherited_strictness() {
        for source in [
            "if(true) function f(){}",
            "if(false){}else function f(){}",
            "function outer(){if(true) function f(){}}",
        ] {
            assert!(
                parse(source, &json!({"strict":false}), None).is_ok(),
                "{source}"
            );
            assert!(
                parse(source, &json!({"strict":true}), None).is_err(),
                "{source}"
            );
        }
    }
    #[test]
    fn eval_assignment_loop_heads_preserve_trailing_elisions() {
        let script = parse("for ([,,] of values) {}", &json!({}), None).unwrap();
        let Stmt::ForOf(loop_) = &script.body[0] else {
            panic!("for-of statement")
        };
        let ForHead::Pat(pattern) = &loop_.left else {
            panic!("assignment head")
        };
        let Pat::Array(pattern) = &**pattern else {
            panic!("array assignment")
        };
        assert_eq!(pattern.elems.len(), 2);
        assert!(pattern.elems.iter().all(Option::is_none));
    }

    #[test]
    fn class_privileges_do_not_change_eval_script_grammar() {
        let grammar = Grammar {
            private_names: vec!["x".into()],
            allow_super_property: true,
            allow_super_call: true,
            arguments_forbidden: true,
        };
        let request = json!({"strict":true,"allowNewTarget":true});
        for source in [
            "this.#x",
            "#x in this",
            "super.x",
            "super()",
            "()=>super.x",
            "function f(){return this.#x}",
            "class C{#y; m(){return this.#x+this.#y}}",
            "(function(){return arguments})(1)",
            "var await=1;await",
            "var await=1;await+=2;await+1",
            "var await=x=>x;await(3)",
            "var await={x:3};await.x",
            "var await=2;await/2/g",
            "function f(await){return await+1}",
            "new.target",
            "arguments:1",
        ] {
            assert!(parse(source, &request, Some(&grammar)).is_ok(), "{source}");
        }
        for source in [
            "return 1",
            "await 1",
            "async function f(await){}",
            "export {}",
            "this.#missing",
            "function f(){return super.x}",
            "function f(){super()}",
            "arguments",
            "()=>arguments",
            "class C extends (this.#y){#y}",
            "delete this.#x",
            "delete this?.#x",
        ] {
            assert!(parse(source, &request, Some(&grammar)).is_err(), "{source}");
        }
    }
}
