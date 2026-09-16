//! Parse the two independent grammar inputs of CreateDynamicFunction.
//!
//! Callers perform JavaScript ToString and host compilation checks before this
//! boundary. No constructor argument is evaluated by a native JavaScript engine.
use mangler_core::Language;
use mangler_jsast::{Js, ParseOpts};
use swc_core::ecma::ast::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstructorKind {
    Normal,
    Generator,
    Async,
    AsyncGenerator,
}

impl ConstructorKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "normal" => Some(Self::Normal),
            "generator" => Some(Self::Generator),
            "async" => Some(Self::Async),
            "async-generator" => Some(Self::AsyncGenerator),
            _ => None,
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Generator => 2,
            Self::Async => 1,
            Self::AsyncGenerator => 3,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Normal => "function",
            Self::Generator => "function*",
            Self::Async => "async function",
            Self::AsyncGenerator => "async function*",
        }
    }
}

pub struct ConstructorSource {
    /// No named-expression self binding: `anonymous` is a reflection name only.
    pub function: Function,
    /// Exact function source prescribed by CreateDynamicFunction.
    pub source: String,
    pub parameters: String,
    pub length: u32,
    pub strict: bool,
    pub kind: ConstructorKind,
}

pub fn parse(
    kind: ConstructorKind,
    parameters: &[String],
    body: &str,
) -> Result<ConstructorSource, String> {
    let parameters = parameters.join(",");
    // Separate parses reject comment/token fragments that would become valid
    // only by consuming text from the other constructor argument.
    let params_only = parse_function(kind, &parameters, "")?;
    if !params_only
        .body
        .as_ref()
        .is_some_and(|body| body.stmts.is_empty())
    {
        return Err("constructor parameters escaped their grammar boundary".into());
    }
    let body_only = parse_function(kind, "", body)?;
    if !body_only.params.is_empty() {
        return Err("constructor body escaped its grammar boundary".into());
    }
    let function = parse_function(kind, &parameters, body)?;
    let length = function
        .params
        .iter()
        .take_while(|p| !matches!(p.pat, Pat::Assign(_) | Pat::Rest(_)))
        .count() as u32;
    let strict = function
        .body
        .as_ref()
        .is_some_and(|body| mangler_jsast::directives::has_use_strict(&body.stmts));
    validate_parameters(&function, strict)?;
    crate::eval_context::validate_constructor(&function)?;
    let source = source(kind, &parameters, body);
    Ok(ConstructorSource {
        function,
        source,
        parameters,
        length,
        strict,
        kind,
    })
}

/// SWC's parser leaves parameter/body binding checks to consumers. Constructor
/// inputs need those combined Early Errors before the function can be created.
fn validate_parameters(function: &Function, strict: bool) -> Result<(), String> {
    use std::collections::HashSet;
    let simple = function
        .params
        .iter()
        .all(|param| matches!(param.pat, Pat::Ident(_)));
    if strict && !simple {
        return Err("use strict directive is not allowed with non-simple parameters".into());
    }
    let mut names = HashSet::new();
    let mut duplicate = false;
    let mut strict_name = false;
    for param in &function.params {
        mangler_jsast::analysis::binding_names(&param.pat, &mut |id| {
            duplicate |= !names.insert(id.sym.to_string());
            strict_name |= matches!(
                id.sym.as_ref(),
                "eval"
                    | "arguments"
                    | "implements"
                    | "interface"
                    | "let"
                    | "package"
                    | "private"
                    | "protected"
                    | "public"
                    | "static"
                    | "yield"
            );
        });
    }
    if duplicate && (strict || !simple) {
        return Err("duplicate parameter name in strict or non-simple parameter list".into());
    }
    if strict && strict_name {
        return Err("invalid strict-mode parameter binding".into());
    }
    if let Some(body) = &function.body {
        let mut collision = false;
        for statement in &body.stmts {
            match statement {
                Stmt::Decl(Decl::Var(declaration)) if declaration.kind != VarDeclKind::Var => {
                    for declarator in &declaration.decls {
                        mangler_jsast::analysis::binding_names(&declarator.name, &mut |id| {
                            collision |= names.contains(id.sym.as_ref())
                        });
                    }
                }
                Stmt::Decl(Decl::Using(declaration)) => {
                    for declarator in &declaration.decls {
                        mangler_jsast::analysis::binding_names(&declarator.name, &mut |id| {
                            collision |= names.contains(id.sym.as_ref())
                        });
                    }
                }
                Stmt::Decl(Decl::Class(declaration)) => {
                    collision |= names.contains(declaration.ident.sym.as_ref())
                }
                _ => {}
            }
        }
        if collision {
            return Err(
                "parameter conflicts with a lexical declaration in the function body".into(),
            );
        }
    }
    Ok(())
}

fn source(kind: ConstructorKind, parameters: &str, body: &str) -> String {
    format!("{} anonymous({parameters}\n) {{\n{body}\n}}", kind.prefix())
}

fn parse_function(kind: ConstructorKind, parameters: &str, body: &str) -> Result<Function, String> {
    let source = source(kind, parameters, body);
    let ast = Js
        .parse(&source, &ParseOpts::default())
        .map_err(|error| error.to_string())?;
    let Program::Script(mut script) = ast.into_program() else {
        return Err("dynamic function requires script grammar".into());
    };
    if script.body.len() != 1 {
        return Err("constructor source escaped its grammar boundary".into());
    }
    let Stmt::Decl(Decl::Fn(declaration)) = script.body.remove(0) else {
        return Err("expected one dynamic function".into());
    };
    if declaration.ident.sym != "anonymous" {
        return Err("dynamic function name was replaced".into());
    }
    Ok(*declaration.function)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parameters(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn preserves_joined_parameters_source_and_reflection_metadata() {
        let parsed = parse(
            ConstructorKind::Normal,
            &parameters(&["a,b", "{x}", "c=2"]),
            "return a+b+x+c",
        )
        .unwrap();
        assert_eq!(parsed.parameters, "a,b,{x},c=2");
        assert_eq!(parsed.length, 3);
        assert_eq!(
            parsed.source,
            "function anonymous(a,b,{x},c=2\n) {\nreturn a+b+x+c\n}"
        );
        assert!(!parsed.strict);
        assert_eq!(parsed.function.params.len(), 4);
        assert!(
            !parse(
                ConstructorKind::Normal,
                &parameters(&["a", "a"]),
                "'use\\x20strict';return a"
            )
            .unwrap()
            .strict
        );
    }

    #[test]
    fn parameter_and_body_fragments_cannot_complete_each_other() {
        for (params, body) in [
            (vec!["/*"], "*/ ) {"),
            (vec!["a) {"], "return a"),
            (vec!["a){} function injected("], "return 1"),
            (vec![], "} function injected(){"),
            (vec!["a=/*"], "*/1) {return a"),
        ] {
            assert!(
                parse(ConstructorKind::Normal, &parameters(&params), body).is_err(),
                "accepted grammar boundary escape: {params:?} / {body}"
            );
        }
        assert!(
            parse(
                ConstructorKind::Normal,
                &parameters(&["a/*comment*/", "b//comment"]),
                "return a+b// trailing comment"
            )
            .is_ok()
        );
    }

    #[test]
    fn enforces_combined_early_errors_and_callable_grammar() {
        assert!(
            parse(
                ConstructorKind::Normal,
                &parameters(&["a", "a"]),
                "return a"
            )
            .is_ok()
        );
        for (kind, params, body) in [
            (
                ConstructorKind::Normal,
                vec!["a", "a"],
                "'use strict';return a",
            ),
            (
                ConstructorKind::Normal,
                vec!["a=1"],
                "'use strict';return a",
            ),
            (ConstructorKind::Normal, vec!["a"], "let a"),
            (ConstructorKind::Normal, vec!["a"], "class a {}"),
            (ConstructorKind::Normal, vec!["a", "{a}"], "return a"),
            (
                ConstructorKind::Normal,
                vec!["eval"],
                "'use strict';return eval",
            ),
            (
                ConstructorKind::Normal,
                vec!["arguments"],
                "'use strict';return arguments",
            ),
            (ConstructorKind::Generator, vec!["yield"], "yield 1"),
            (ConstructorKind::Generator, vec!["a=yield 1"], "yield a"),
            (ConstructorKind::Async, vec!["await"], "return 1"),
            (ConstructorKind::Async, vec!["a=await 1"], "return a"),
            (ConstructorKind::Normal, vec![], "return super.value"),
        ] {
            assert!(
                parse(kind, &parameters(&params), body).is_err(),
                "accepted invalid {kind:?}: {params:?} / {body}"
            );
        }
        for (kind, body) in [
            (ConstructorKind::Normal, "return new.target"),
            (ConstructorKind::Generator, "yield new.target"),
            (ConstructorKind::Async, "return await new.target"),
            (ConstructorKind::AsyncGenerator, "yield await new.target"),
        ] {
            let parsed = parse(kind, &[], body).unwrap();
            assert_eq!(parsed.kind, kind);
            assert_eq!(
                parsed.function.is_async,
                matches!(
                    kind,
                    ConstructorKind::Async | ConstructorKind::AsyncGenerator
                )
            );
            assert_eq!(
                parsed.function.is_generator,
                matches!(
                    kind,
                    ConstructorKind::Generator | ConstructorKind::AsyncGenerator
                )
            );
        }
    }

    #[test]
    fn private_names_belong_to_source_classes_not_constructor_callers() {
        for kind in [
            ConstructorKind::Normal,
            ConstructorKind::Generator,
            ConstructorKind::Async,
            ConstructorKind::AsyncGenerator,
        ] {
            for (params, body) in [
                (vec![], "return o.#missing"),
                (vec!["p=o.#missing"], "return p"),
                (vec![], "return () => o.#missing"),
                (vec![], "return class C extends o.#x { #x; }"),
                (vec![], "class C { #x; } return o.#x"),
                (
                    vec![],
                    "return class C { #x; m(){ return delete this.#x; } }",
                ),
            ] {
                assert!(
                    parse(kind, &parameters(&params), body).is_err(),
                    "accepted private name escape {kind:?}: {params:?} / {body}"
                );
            }
            for (params, body) in [
                (vec![], "return class C { #x; m(){return this.#x} }"),
                (
                    vec![],
                    "return class C { #x; m(){return class D { m(o){return o.#x} }} }",
                ),
                (vec!["p=class C { #x; m(){return this.#x} }"], "return p"),
            ] {
                parse(kind, &parameters(&params), body)
                    .unwrap_or_else(|error| panic!("rejected valid {kind:?}: {body}: {error}"));
            }
        }
    }
}
