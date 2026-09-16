//! Property descriptors belonging to generated machinery have own fields only.
//! Source descriptors keep JavaScript's inherited-field semantics. All generated
//! entry points share this adapter, including the standalone VM and runtime host.
use mangler_core::Language;
use mangler_jsast::{Js, ParseOpts, build};
use std::collections::{BTreeMap, HashSet};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitMut, VisitMutWith, VisitWith};

pub fn factory_source() -> &'static str {
    include_str!("descriptors.js")
}

/// Adapter codes are private support ABI, independent of the bytecode ISA.
pub fn adapter_kind(path: &[String]) -> Option<u32> {
    match path
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["Object" | "Reflect", "defineProperty"] => Some(0),
        ["Object", "defineProperties"] => Some(1),
        ["Object", "create"] => Some(2),
        ["Object" | "Reflect", "getOwnPropertyDescriptor"] => Some(3),
        ["Object", "getOwnPropertyDescriptors"] => Some(4),
        _ => None,
    }
}

/// Bind the canonical factory to captured, unmodified support intrinsics.
pub fn factory_expression() -> mangler_core::Result<Expr> {
    let source = format!(
        "({})(Object.getPrototypeOf,Object.getOwnPropertyNames,Object.getOwnPropertySymbols,Object.prototype.hasOwnProperty,Reflect.apply,Object.prototype.propertyIsEnumerable)",
        factory_source()
    );
    let mut program = Js.parse(&source, &ParseOpts::default())?.into_program();
    let Program::Script(script) = &mut program else {
        unreachable!()
    };
    let Stmt::Expr(expression) = script.body.remove(0) else {
        unreachable!()
    };
    let mut expression = *expression.expr;
    expression.visit_mut_with(&mut mangler_jsast::span::GeneratedSpans);
    Ok(expression)
}

fn path(expression: &Expr) -> Option<Vec<String>> {
    match expression {
        Expr::Ident(id) => Some(vec![id.sym.to_string()]),
        Expr::Member(member) => {
            let MemberProp::Ident(key) = &member.prop else {
                return None;
            };
            let mut result = path(&member.obj)?;
            result.push(key.sym.to_string());
            Some(result)
        }
        _ => None,
    }
}

/// Protect compiler-owned statements before embedding them. Constant-pool source
/// factories are excluded; native argument shells in table field five are support.
pub(crate) fn protect(statements: &mut Vec<Stmt>, table: &str) -> mangler_core::Result<()> {
    struct Names(HashSet<String>);
    impl Visit for Names {
        fn visit_ident(&mut self, id: &Ident) {
            self.0.insert(id.sym.to_string());
        }
    }
    let mut names = Names(HashSet::new());
    statements.visit_with(&mut names);
    fn fresh(names: &mut HashSet<String>) -> String {
        for n in 0.. {
            let name = format!("_manglerDescriptor{n}");
            if names.insert(name.clone()) {
                return name;
            }
        }
        unreachable!()
    }
    let factory = fresh(&mut names.0);
    struct Rewrite<'a> {
        names: &'a mut HashSet<String>,
        aliases: BTreeMap<Vec<String>, (String, Expr, u32)>,
    }
    impl VisitMut for Rewrite<'_> {
        fn visit_mut_expr(&mut self, expression: &mut Expr) {
            if let Some(path) = path(expression)
                && let Some(kind) = adapter_kind(&path)
            {
                let alias = self
                    .aliases
                    .entry(path)
                    .or_insert_with(|| (fresh(self.names), expression.clone(), kind));
                *expression = build::ident_expr(&alias.0);
            } else {
                expression.visit_mut_children_with(self);
            }
        }
    }
    let mut rewrite = Rewrite {
        names: &mut names.0,
        aliases: BTreeMap::new(),
    };
    for statement in statements.iter_mut() {
        if let Stmt::Decl(Decl::Var(var)) = statement
            && var
                .decls
                .iter()
                .any(|d| matches!(&d.name,Pat::Ident(id) if id.id.sym.as_ref()==table))
        {
            for declaration in &mut var.decls {
                if let Some(Expr::Array(rows)) = declaration.init.as_deref_mut() {
                    for row in rows.elems.iter_mut().flatten() {
                        if let Expr::Array(fields) = row.expr.as_mut()
                            && let Some(Some(factory)) = fields.elems.get_mut(5)
                        {
                            factory.expr.visit_mut_with(&mut rewrite);
                        }
                    }
                }
            }
        } else {
            statement.visit_mut_with(&mut rewrite);
        }
    }
    if rewrite.aliases.is_empty() {
        return Ok(());
    }
    let mut declarations = vec![build::var_decl(
        VarDeclKind::Var,
        &factory,
        factory_expression()?,
    )];
    for (_, (name, intrinsic, kind)) in rewrite.aliases {
        declarations.push(build::var_decl(
            VarDeclKind::Var,
            &name,
            build::call(
                build::ident_expr(&factory),
                vec![build::num(kind as f64), intrinsic],
            ),
        ));
    }
    for declaration in &mut declarations {
        if let Stmt::Decl(Decl::Var(variable)) = declaration {
            variable.span = mangler_jsast::span::private_runtime_declaration_span();
        }
    }
    let directives = statements.iter().take_while(|statement| matches!(statement, Stmt::Expr(expression) if matches!(expression.expr.as_ref(), Expr::Lit(Lit::Str(_))))).count();
    statements.splice(directives..directives, declarations);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_core::Rng;

    #[test]
    fn standalone_table_protects_descriptors_and_live_captures() {
        let (params, body) = crate::test_support::parse_fn_with_params(
            "function(n){var f=()=>n;n=8;let x=f();return x}",
        );
        let compiled = crate::compile_body(&params, &body).unwrap();
        let mut table = crate::TableBuilder::with_diversity(crate::VmDiversity::draw(
            &mut Rng::for_pass(42, "descriptor-test"),
        ));
        let chunk = table.add(compiled);
        let names = crate::VmNames {
            lean_interp: "Vm".into(),
            eh_interp: "VmEh".into(),
            lean_interp_strict: "VmStrict".into(),
            eh_interp_strict: "VmStrictEh".into(),
            table: "Programs".into(),
            rc: "Construct".into(),
            sy: "IteratorKey".into(),
        };
        let prologue = table.finish(&names).unwrap().prologue;
        let mut ast = Js.parse("", &ParseOpts::default()).unwrap();
        let Program::Script(script) = ast.program_mut() else {
            unreachable!()
        };
        script.body = prologue;
        let output = Js.print(&ast);
        for (field, value) in [
            ("value", "19"),
            ("writable", "true"),
            ("get", "function(){return 19}"),
            ("set", "function(){}"),
        ] {
            let program = format!(
                "{output}var result;Object.prototype.{field}={value};try{{var row=Programs[{}];result=row[2](row[0],row[1],[7],[],{},1,undefined,false);}}finally{{delete Object.prototype.{field}}}globalThis.__out=result;",
                chunk.index, chunk.cap_start,
            );
            mangler_testkit::assert_behaviorally_equal("globalThis.__out=8", &program);
        }
    }
}
