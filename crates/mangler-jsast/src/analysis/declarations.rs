//! Var-declaration instantiation shared by program partitions and runtime eval.
//! Lexical barriers suppress Annex B aliases without suppressing ordinary vars.
use super::binding_names;
use std::collections::HashSet;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

#[derive(Default)]
pub struct VarDeclarations {
    /// Unique var-environment bindings in source encounter order.
    pub variables: Vec<String>,
    /// Top-level function declarations in declaration order (including repeats).
    pub functions: Vec<String>,
}

/// Function/eval statement-list declarations. Parameters are supplied separately
/// by the caller's declaration-instantiation protocol.
pub fn function_var_declarations(statements: &[Stmt], strict: bool) -> VarDeclarations {
    let strict = strict || crate::directives::has_use_strict(statements);
    let mut scan = Declarations {
        annex_b: !strict,
        top_functions: true,
        ..Default::default()
    };
    scan.lexical.push(lexical_names(statements));
    statements.visit_with(&mut scan);
    scan.result
}

/// Bindings whose storage must stay native when a program is split into chunks.
/// Strict program function declarations already have native declaration storage;
/// sloppy functions additionally participate in the Annex B variable envelope.
pub fn program_var_declarations(program: &Program, strict: bool) -> Vec<String> {
    let strict = strict
        || match program {
            Program::Module(_) => true,
            Program::Script(script) => crate::directives::has_use_strict(&script.body),
        };
    let mut scan = Declarations {
        annex_b: !strict,
        top_functions: !strict,
        ..Default::default()
    };
    if let Program::Script(script) = program {
        scan.lexical.push(lexical_names(&script.body));
    }
    program.visit_with(&mut scan);
    scan.result.variables
}

#[derive(Default)]
struct Declarations {
    result: VarDeclarations,
    annex_b: bool,
    top_functions: bool,
    seen: HashSet<String>,
    lexical: Vec<HashSet<String>>,
    depth: u32,
}
impl Declarations {
    fn add(&mut self, name: String) {
        if self.seen.insert(name.clone()) {
            self.result.variables.push(name);
        }
    }
}
fn lexical_names(statements: &[Stmt]) -> HashSet<String> {
    let mut names = HashSet::new();
    for statement in statements {
        match statement {
            Stmt::Decl(Decl::Var(declaration)) if declaration.kind != VarDeclKind::Var => {
                names.extend(declarator_names(&declaration.decls));
            }
            Stmt::Decl(Decl::Using(declaration)) => {
                names.extend(declarator_names(&declaration.decls));
            }
            Stmt::Decl(Decl::Class(declaration)) => {
                names.insert(declaration.ident.sym.to_string());
            }
            _ => {}
        }
    }
    names
}
fn variable_names(declaration: &VarDecl) -> HashSet<String> {
    if declaration.kind == VarDeclKind::Var {
        return HashSet::new();
    }
    declarator_names(&declaration.decls)
}
fn declarator_names(declarations: &[VarDeclarator]) -> HashSet<String> {
    let mut names = HashSet::new();
    for variable in declarations {
        binding_names(&variable.name, &mut |id| {
            names.insert(id.sym.to_string());
        });
    }
    names
}
fn head_names(head: &ForHead) -> HashSet<String> {
    match head {
        ForHead::VarDecl(declaration) => variable_names(declaration),
        ForHead::UsingDecl(declaration) => declarator_names(&declaration.decls),
        _ => HashSet::new(),
    }
}

impl Visit for Declarations {
    fn visit_stmt(&mut self, n: &Stmt) {
        self.depth += 1;
        n.visit_children_with(self);
        self.depth -= 1;
    }
    fn visit_block_stmt(&mut self, n: &BlockStmt) {
        self.lexical.push(lexical_names(&n.stmts));
        n.visit_children_with(self);
        self.lexical.pop();
    }
    fn visit_for_stmt(&mut self, n: &ForStmt) {
        let names = if let Some(VarDeclOrExpr::VarDecl(declaration)) = &n.init {
            variable_names(declaration)
        } else {
            HashSet::new()
        };
        self.lexical.push(names);
        n.visit_children_with(self);
        self.lexical.pop();
    }
    fn visit_for_in_stmt(&mut self, n: &ForInStmt) {
        self.lexical.push(head_names(&n.left));
        n.visit_children_with(self);
        self.lexical.pop();
    }
    fn visit_for_of_stmt(&mut self, n: &ForOfStmt) {
        self.lexical.push(head_names(&n.left));
        n.visit_children_with(self);
        self.lexical.pop();
    }
    fn visit_switch_stmt(&mut self, n: &SwitchStmt) {
        let names = n
            .cases
            .iter()
            .flat_map(|case| lexical_names(&case.cons))
            .collect();
        self.lexical.push(names);
        n.visit_children_with(self);
        self.lexical.pop();
    }
    fn visit_catch_clause(&mut self, catch: &CatchClause) {
        let mut names = HashSet::new();
        if let Some(pattern @ (Pat::Array(_) | Pat::Object(_))) = &catch.param {
            binding_names(pattern, &mut |id| {
                names.insert(id.sym.to_string());
            });
        }
        self.lexical.push(names);
        catch.visit_children_with(self);
        self.lexical.pop();
    }
    fn visit_var_decl(&mut self, n: &VarDecl) {
        if n.kind == VarDeclKind::Var {
            for declaration in &n.decls {
                binding_names(&declaration.name, &mut |id| self.add(id.sym.to_string()));
            }
        }
        n.visit_children_with(self);
    }
    fn visit_fn_decl(&mut self, n: &FnDecl) {
        if self.depth <= 1 {
            self.result.functions.push(n.ident.sym.to_string());
        }
        if (self.depth <= 1 && self.top_functions)
            || (self.depth > 1
                && self.annex_b
                && !n.function.is_async
                && !n.function.is_generator
                && !self
                    .lexical
                    .iter()
                    .any(|scope| scope.contains(n.ident.sym.as_ref())))
        {
            self.add(n.ident.sym.to_string());
        }
    }
    fn visit_class(&mut self, _: &Class) {}
    fn visit_function(&mut self, _: &Function) {}
    fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        crate::deep::walk_binary(n, self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Js, ParseOpts};
    use mangler_core::Language;

    #[test]
    fn program_and_eval_share_annex_b_scope_barriers() {
        for (source, expected) in [
            ("{function f(){}}", vec!["f"]),
            ("let f;{function f(){}}", vec![]),
            ("{let f;{function f(){}}}", vec![]),
            ("{using f=null;{function f(){}}}", vec![]),
            ("using f=null;{function f(){}}", vec![]),
            ("for(using f of []){function f(){}}", vec![]),
            ("class f{};{function f(){}}", vec![]),
            ("for(let f=0;f<1;f++){function f(){}}", vec![]),
            ("for(let f of []){function f(){}}", vec![]),
            ("for(let f in {}){function f(){}}", vec![]),
            ("switch(1){case 1:let f;{function f(){}}}", vec![]),
            ("try{}catch({f}){{function f(){}}}", vec![]),
            ("try{}catch(f){{function f(){}}}", vec!["f"]),
            ("{async function f(){}}", vec![]),
            ("{function* f(){}}", vec![]),
            ("class C{static{var hidden}}var visible", vec!["visible"]),
        ] {
            let ast = Js.parse(source, &ParseOpts::default()).unwrap();
            let Program::Script(script) = ast.program() else {
                panic!("Script")
            };
            assert_eq!(
                program_var_declarations(ast.program(), false),
                expected,
                "{source}"
            );
            assert_eq!(
                function_var_declarations(&script.body, false).variables,
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn top_function_declarations_keep_their_native_program_storage() {
        let ast = Js
            .parse(
                "var a;function f(){}var b;function f(){}",
                &ParseOpts::default(),
            )
            .unwrap();
        let Program::Script(script) = ast.program() else {
            panic!("Script")
        };
        let eval = function_var_declarations(&script.body, false);
        assert_eq!(eval.variables, ["a", "f", "b"]);
        assert_eq!(eval.functions, ["f", "f"]);
        assert_eq!(program_var_declarations(ast.program(), true), ["a", "b"]);
        assert_eq!(
            program_var_declarations(ast.program(), false),
            eval.variables
        );
    }
}
