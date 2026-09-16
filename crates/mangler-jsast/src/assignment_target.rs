//! Assignment references and syntax-dependent named evaluation.
//! Parentheses preserve references but do not preserve IsIdentifierRef.
use swc_core::ecma::ast::*;

#[derive(Clone, Copy)]
pub enum Reference<'a> {
    Ident(&'a Ident),
    Member(&'a MemberExpr),
    Super(&'a SuperPropExpr),
    Call(&'a Expr),
    Object(&'a ObjectPat),
    Array(&'a ArrayPat),
    Other,
}

pub fn unparen(mut expression: &Expr) -> &Expr {
    while let Expr::Paren(paren) = expression {
        expression = &paren.expr;
    }
    expression
}

pub fn unparen_mut(mut expression: &mut Expr) -> &mut Expr {
    while let Expr::Paren(paren) = expression {
        expression = &mut paren.expr;
    }
    expression
}

pub fn expression_reference(expression: &Expr) -> Reference<'_> {
    match unparen(expression) {
        Expr::Ident(id) => Reference::Ident(id),
        Expr::Member(member) => Reference::Member(member),
        Expr::SuperProp(property) => Reference::Super(property),
        expression @ Expr::Call(call) if matches!(call.callee, Callee::Expr(_)) => {
            Reference::Call(expression)
        }
        _ => Reference::Other,
    }
}

pub fn simple_reference(target: &SimpleAssignTarget) -> Reference<'_> {
    match target {
        SimpleAssignTarget::Ident(binding) => Reference::Ident(&binding.id),
        SimpleAssignTarget::Member(member) => Reference::Member(member),
        SimpleAssignTarget::SuperProp(property) => Reference::Super(property),
        SimpleAssignTarget::Paren(paren) => expression_reference(&paren.expr),
        _ => Reference::Other,
    }
}

pub fn reference(target: &AssignTarget) -> Reference<'_> {
    match target {
        AssignTarget::Simple(target) => simple_reference(target),
        AssignTarget::Pat(AssignTargetPat::Object(object)) => Reference::Object(object),
        AssignTarget::Pat(AssignTargetPat::Array(array)) => Reference::Array(array),
        _ => Reference::Other,
    }
}

/// Only a syntactically bare identifier requests NamedEvaluation of an
/// anonymous RHS. In particular, `(name) = function(){}` has an empty name.
pub fn inferred_name(target: &AssignTarget) -> Option<&str> {
    match target {
        AssignTarget::Simple(SimpleAssignTarget::Ident(binding)) => Some(binding.id.sym.as_ref()),
        _ => None,
    }
}

/// Whether this syntax requests NamedEvaluation when assigned to an identifier.
pub fn is_anonymous_definition(value: &Expr) -> bool {
    matches!(
        unparen(value),
        Expr::Arrow(_)
            | Expr::Fn(FnExpr { ident: None, .. })
            | Expr::Class(ClassExpr { ident: None, .. })
    )
}

/// Preserve a name across binding renaming without adding a lexical self binding.
/// Computed-property NamedEvaluation sets it before class static initialization.
/// Arbitrary non-anonymous values are also safe and keep their existing names.
pub fn named_value(name: &str, value: Box<Expr>) -> Box<Expr> {
    named_value_key(
        Str {
            span: swc_core::common::DUMMY_SP,
            value: name.into(),
            raw: None,
        },
        value,
    )
}

/// The string-key form retains lone UTF-16 surrogates from source syntax.
pub fn named_value_key(key: Str, value: Box<Expr>) -> Box<Expr> {
    named_value_expression(Box::new(Expr::Lit(Lit::Str(key))), value)
}

/// The key must be a side-effect-free literal or a cached ToPropertyKey result.
/// It is read for both property creation and lookup; conversion belongs to the caller.
pub fn named_value_expression(key: Box<Expr>, value: Box<Expr>) -> Box<Expr> {
    use swc_core::common::DUMMY_SP;
    Box::new(Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(Expr::Object(ObjectLit {
            span: DUMMY_SP,
            props: vec![PropOrSpread::Prop(Box::new(Prop::KeyValue(KeyValueProp {
                key: PropName::Computed(ComputedPropName {
                    span: DUMMY_SP,
                    expr: key.clone(),
                }),
                value,
            })))],
        })),
        prop: MemberProp::Computed(ComputedPropName {
            span: DUMMY_SP,
            expr: key,
        }),
    }))
}
