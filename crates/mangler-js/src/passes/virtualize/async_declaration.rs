//! Native async module declarations own Promise creation and await reactions.
//! Their entry body and every resumed state remain compiled bytecode.
use super::*;

pub(super) fn restore(function: &mut Function, cfg: &FileConfig) {
    if function.is_async {
        return;
    }
    let body = function
        .body
        .as_mut()
        .expect("compiled async declaration has a body");
    let Some(Stmt::Return(returned)) = body.stmts.pop() else {
        unreachable!("compiled async declaration ends in its entry thunk")
    };
    let iterator = returned.arg.expect("compiled entry produces an iterator");
    let (driver, result) = drive(iterator, cfg);
    body.stmts.extend(driver);
    body.stmts.push(Stmt::Return(ReturnStmt {
        span: DUMMY_SP,
        arg: Some(result),
    }));
    function.is_async = true;
}

/// Shared by native async declarations and the module's top-level await driver.
/// Return the completion value separately so the caller decides where it flows.
pub(super) fn drive(iterator_value: Box<Expr>, cfg: &FileConfig) -> (Vec<Stmt>, Box<Expr>) {
    let iterator = cfg.fresh_name();
    let step = cfg.fresh_name();
    let value = cfg.fresh_name();
    let ok = cfg.fresh_name();
    let error = cfg.fresh_name();
    let source = format!(
        "async function _driver(){{let {iterator}=void 0,{step}={iterator}.next();while(!{step}.done){{let {ok}=true,{value};try{{{value}=await {step}.value}}catch({error}){{{ok}=false;{value}={error}}}{step}={ok}?{iterator}.next({value}):{iterator}.throw({value})}}}}"
    );
    let mut driver = parse_fn_body_stmts(&source).expect("native await driver parses");
    let Stmt::Decl(Decl::Var(declaration)) = &mut driver[0] else {
        unreachable!()
    };
    declaration.decls[0].init = Some(iterator_value);
    (
        driver,
        Box::new(Expr::Member(MemberExpr {
            span: DUMMY_SP,
            obj: Box::new(Expr::Ident(Ident::new_no_ctxt(step.into(), DUMMY_SP))),
            prop: MemberProp::Ident(IdentName::new("value".into(), DUMMY_SP)),
        })),
    )
}
