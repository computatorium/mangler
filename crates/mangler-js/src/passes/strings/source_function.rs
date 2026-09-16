//! Resolve a generated callable through private bootstrap exports in final source.
//! Resolver identities distinguish same-spelled bindings in nested scopes. Only
//! static function/array/alias shapes are interpreted; source is never executed.
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use mangler_core::Language;
use mangler_jsast::{Js, ParseOpts};
use swc_core::common::{Span, Spanned};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

#[derive(Clone)]
enum Value {
    Unknown,
    Binding(Id),
    Function(Range<usize>),
    Array(Vec<Value>),
    Index(Box<Value>, usize),
}

fn range(span: Span) -> Range<usize> {
    span.lo.0.saturating_sub(1) as usize..span.hi.0.saturating_sub(1) as usize
}

fn returned(body: &FunctionBody) -> Value {
    match body.stmts.last() {
        Some(Stmt::Return(ret)) => ret.arg.as_deref().map(value).unwrap_or(Value::Unknown),
        _ => Value::Unknown,
    }
}

fn value(expression: &Expr) -> Value {
    match expression {
        Expr::Ident(id) => Value::Binding(id.to_id()),
        Expr::Fn(function) => Value::Function(range(function.span())),
        Expr::Arrow(function) => Value::Function(range(function.span)),
        Expr::Paren(paren) => value(&paren.expr),
        Expr::Seq(sequence) => sequence
            .exprs
            .last()
            .map(|e| value(e))
            .unwrap_or(Value::Unknown),
        Expr::Array(array) => Value::Array(
            array
                .elems
                .iter()
                .map(|element| {
                    element
                        .as_ref()
                        .filter(|element| element.spread.is_none())
                        .map(|element| value(&element.expr))
                        .unwrap_or(Value::Unknown)
                })
                .collect(),
        ),
        Expr::Member(member) => {
            if let MemberProp::Computed(key) = &member.prop
                && let Expr::Lit(Lit::Num(index)) = key.expr.as_ref()
                && index.value >= 0.0
                && index.value.fract() == 0.0
            {
                return Value::Index(Box::new(value(&member.obj)), index.value as usize);
            }
            Value::Unknown
        }
        Expr::Call(call) => {
            let Callee::Expr(callee) = &call.callee else {
                return Value::Unknown;
            };
            let mut callee = callee.as_ref();
            while let Expr::Paren(paren) = callee {
                callee = &paren.expr;
            }
            match callee {
                Expr::Arrow(arrow) if arrow.params.is_empty() => match arrow.body.as_ref() {
                    ArrowFunctionBody::FunctionBody(body) => returned(body),
                    ArrowFunctionBody::Expr(expression) => value(expression),
                },
                Expr::Fn(function) if function.function.params.is_empty() => function
                    .function
                    .body
                    .as_ref()
                    .map(returned)
                    .unwrap_or(Value::Unknown),
                _ => Value::Unknown,
            }
        }
        _ => Value::Unknown,
    }
}

pub(super) fn resolve(output: &str, callee: &str, wrapper: Range<usize>) -> Option<Range<usize>> {
    Js::with_globals(|| {
        let mut ast = Js.parse(output, &ParseOpts::default()).ok()?;
        Js::resolve(&mut ast);
        struct Bindings<'a> {
            values: HashMap<Id, Value>,
            callee: &'a str,
            wrapper: Range<usize>,
            called: Option<Id>,
        }
        impl Visit for Bindings<'_> {
            fn visit_bin_expr(&mut self, binary: &BinExpr) {
                mangler_jsast::deep::walk_binary(binary, self);
            }
            fn visit_fn_decl(&mut self, function: &FnDecl) {
                self.values.insert(
                    function.ident.to_id(),
                    Value::Function(range(function.function.span)),
                );
                function.visit_children_with(self);
            }
            fn visit_var_declarator(&mut self, declaration: &VarDeclarator) {
                if let Pat::Ident(binding) = &declaration.name
                    && let Some(initializer) = &declaration.init
                {
                    self.values.insert(binding.id.to_id(), value(initializer));
                }
                declaration.visit_children_with(self);
            }
            fn visit_call_expr(&mut self, call: &CallExpr) {
                if self.wrapper.contains(&range(call.span).start)
                    && let Callee::Expr(callee) = &call.callee
                    && let Expr::Ident(id) = callee.as_ref()
                    && id.sym == self.callee
                {
                    self.called = Some(id.to_id());
                }
                call.visit_children_with(self);
            }
        }
        let mut bindings = Bindings {
            values: HashMap::new(),
            callee,
            wrapper,
            called: None,
        };
        ast.program().visit_with(&mut bindings);
        let mut current = Value::Binding(bindings.called?);
        let mut indices = Vec::new();
        let mut seen = HashSet::new();
        loop {
            current = match current {
                Value::Binding(id) => {
                    if !seen.insert(id.clone()) {
                        return None;
                    }
                    bindings.values.get(&id)?.clone()
                }
                Value::Index(object, index) => {
                    indices.push(index);
                    *object
                }
                Value::Array(elements) => elements.get(indices.pop()?)?.clone(),
                Value::Function(source) if indices.is_empty() => return Some(source),
                _ => return None,
            };
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_bootstrap_alias_in_its_lexical_scope() {
        let output = "function inner(){return 0}var tuple=(()=>{function inner(a){return a}return[0,inner]})(),vm=tuple[1];function decode(){return vm(table[0][0])}";
        let wrapper = output.find("function decode").unwrap()..output.len();
        let source = resolve(output, "vm", wrapper).unwrap();
        assert_eq!(&output[source], "function inner(a){return a}");
    }

    #[test]
    fn does_not_execute_dynamic_initializers() {
        let output = "var vm=fetch('/code');function decode(){return vm(table[0][0])}";
        assert!(
            resolve(
                output,
                "vm",
                output.find("function decode").unwrap()..output.len()
            )
            .is_none()
        );
    }
}
