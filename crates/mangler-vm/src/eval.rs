//! Compile an eval StatementList with its completion value and caller var scope.
//!
//! The host creates `declared_vars` in the appropriate variable environment before
//! installing the returned capture descriptors. Strict eval keeps declarations in
//! its own frame. Captures are references supplied by the host lexical environment.
use std::collections::HashSet;

use swc_core::common::{DUMMY_SP, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

use crate::chunk::Compiled;
use crate::compile::{CompileOptions, compile_body_with_opts, walk_binary_chain};

/// Grammar and variable-environment context of the original source entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum SourceContext {
    #[default]
    Function = 0,
    Script = 1,
    Module = 2,
}

pub struct EvalCompiled {
    pub compiled: Compiled,
    pub declared_vars: Vec<String>,
    /// Source-level function declarations requiring global function preflight.
    pub declared_functions: Vec<String>,
    pub strict: bool,
}

/// Compile eval using the grammar context supplied by its source entry.
pub fn compile_eval_body_with_context(
    statements: Vec<Stmt>,
    inherited_strict: bool,
    source_context: SourceContext,
) -> Result<EvalCompiled, &'static str> {
    compile_prepared_eval_body(
        prepare_eval_body(statements, inherited_strict, source_context),
        CompileOptions::default(),
    )
}

/// Eval's declaration and completion semantics, retained while a shared frontend
/// lowers classes and suspension syntax in `body`. Prepare this exactly once.
pub struct PreparedEvalBody {
    pub body: FunctionBody,
    pub declared_vars: Vec<String>,
    pub declared_functions: Vec<String>,
    pub strict: bool,
    pub source_context: SourceContext,
    pub internal_bindings: HashSet<String>,
}

pub fn prepare_eval_body(
    statements: Vec<Stmt>,
    inherited_strict: bool,
    source_context: SourceContext,
) -> PreparedEvalBody {
    let strict = inherited_strict || mangler_jsast::directives::has_use_strict(&statements);
    let scan =
        mangler_jsast::analysis::declarations::function_var_declarations(&statements, strict);
    let mut names = Names::default();
    statements.visit_with(&mut names);
    let mut completion = Completion {
        names: names.0,
        generated: Vec::new(),
        next: 0,
    };
    let result = completion.fresh();
    let mut body = Vec::new();
    for statement in statements {
        body.push(completion.statement(statement, &result));
    }
    let declarations = completion
        .generated
        .iter()
        .map(|name| VarDeclarator {
            span: DUMMY_SP,
            name: Pat::Ident(BindingIdent {
                id: ident(name),
                type_ann: None,
            }),
            init: None,
            definite: false,
        })
        .collect();
    body.insert(
        0,
        Stmt::Decl(Decl::Var(Box::new(VarDecl {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            kind: VarDeclKind::Var,
            declare: false,
            decls: declarations,
        }))),
    );
    body.push(Stmt::Return(ReturnStmt {
        span: DUMMY_SP,
        arg: Some(Box::new(Expr::Ident(ident(&result)))),
    }));
    let block = FunctionBody {
        span: DUMMY_SP,
        stmts: body,
    };
    PreparedEvalBody {
        body: block,
        declared_vars: if strict { Vec::new() } else { scan.variables },
        declared_functions: if strict { Vec::new() } else { scan.functions },
        strict,
        source_context,
        internal_bindings: completion.generated.into_iter().collect(),
    }
}

/// Finish a prepared eval body with the same compiler as static functions.
/// Supplemental options carry frontend capture and suspension metadata; eval's
/// binding and completion contracts remain authoritative here.
pub fn compile_prepared_eval_body(
    prepared: PreparedEvalBody,
    opts: CompileOptions<'_>,
) -> Result<EvalCompiled, &'static str> {
    let external: HashSet<String> = prepared.declared_vars.iter().cloned().collect();
    let mut internal = prepared.internal_bindings;
    if let Some(bindings) = opts.internal_bindings {
        internal.extend(bindings.iter().cloned());
    }
    let compiled = compile_body_with_opts(
        &[],
        &prepared.body,
        CompileOptions {
            live_captures: true,
            eval_context: true,
            lexical_entry: false,
            native_parameters: false,
            source_context: prepared.source_context,
            lexical_arguments: true,
            strict: prepared.strict,
            external_var_bindings: Some(&external),
            internal_bindings: Some(&internal),
            ..opts
        },
    )?;
    Ok(EvalCompiled {
        compiled,
        declared_vars: prepared.declared_vars,
        declared_functions: prepared.declared_functions,
        strict: prepared.strict,
    })
}

#[derive(Default)]
struct Names(HashSet<String>);
impl Visit for Names {
    fn visit_ident(&mut self, id: &Ident) {
        self.0.insert(id.sym.to_string());
    }
    fn visit_bin_expr(&mut self, n: &BinExpr) {
        walk_binary_chain(n, self);
    }
}

fn ident(name: &str) -> Ident {
    Ident::new(name.into(), DUMMY_SP, SyntaxContext::empty())
}
fn undefined() -> Expr {
    Expr::Unary(UnaryExpr {
        span: DUMMY_SP,
        op: UnaryOp::Void,
        arg: Box::new(Expr::Lit(Lit::Num(Number {
            span: DUMMY_SP,
            value: 0.,
            raw: None,
        }))),
    })
}
fn assign(name: &str, value: Expr) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: Box::new(Expr::Assign(AssignExpr {
            span: DUMMY_SP,
            op: AssignOp::Assign,
            left: AssignTarget::Simple(SimpleAssignTarget::Ident(BindingIdent {
                id: ident(name),
                type_ann: None,
            })),
            right: Box::new(value),
        })),
    })
}
fn block(statements: Vec<Stmt>) -> Stmt {
    Stmt::Block(BlockStmt {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        stmts: statements,
    })
}

struct Completion {
    names: HashSet<String>,
    generated: Vec<String>,
    next: u32,
}
impl Completion {
    fn fresh(&mut self) -> String {
        loop {
            let name = format!("__mangler_eval_completion_{}", self.next);
            self.next += 1;
            if self.names.insert(name.clone()) {
                self.generated.push(name.clone());
                return name;
            }
        }
    }
    fn statements(&mut self, statements: &mut Vec<Stmt>, result: &str) {
        *statements = std::mem::take(statements)
            .into_iter()
            .map(|s| self.statement(s, result))
            .collect();
    }
    fn statement(&mut self, statement: Stmt, result: &str) -> Stmt {
        let (statement, clear) = match statement {
            Stmt::Expr(expression) => return assign(result, *expression.expr),
            Stmt::Block(mut body) => {
                self.statements(&mut body.stmts, result);
                (Stmt::Block(body), false)
            }
            Stmt::If(mut node) => {
                node.cons = Box::new(self.statement(*node.cons, result));
                node.alt = node.alt.map(|s| Box::new(self.statement(*s, result)));
                (Stmt::If(node), true)
            }
            Stmt::While(mut node) => {
                node.body = Box::new(self.statement(*node.body, result));
                (Stmt::While(node), true)
            }
            Stmt::DoWhile(mut node) => {
                node.body = Box::new(self.statement(*node.body, result));
                (Stmt::DoWhile(node), true)
            }
            Stmt::For(mut node) => {
                node.body = Box::new(self.statement(*node.body, result));
                (Stmt::For(node), true)
            }
            Stmt::ForIn(mut node) => {
                node.body = Box::new(self.statement(*node.body, result));
                (Stmt::ForIn(node), true)
            }
            Stmt::ForOf(mut node) => {
                node.body = Box::new(self.statement(*node.body, result));
                (Stmt::ForOf(node), true)
            }
            Stmt::With(mut node) => {
                node.body = Box::new(self.statement(*node.body, result));
                (Stmt::With(node), true)
            }
            Stmt::Switch(mut node) => {
                for case in &mut node.cases {
                    self.statements(&mut case.cons, result);
                }
                (Stmt::Switch(node), true)
            }
            Stmt::Try(mut node) => {
                self.statements(&mut node.block.stmts, result);
                if let Some(catch) = &mut node.handler {
                    self.statements(&mut catch.body.stmts, result);
                    catch.body.stmts.insert(0, assign(result, undefined()));
                }
                if let Some(finalizer) = &mut node.finalizer {
                    self.statements(&mut finalizer.stmts, result);
                    let saved = self.fresh();
                    finalizer
                        .stmts
                        .insert(0, assign(&saved, Expr::Ident(ident(result))));
                    finalizer.stmts.insert(1, assign(result, undefined()));
                    finalizer
                        .stmts
                        .push(assign(result, Expr::Ident(ident(&saved))));
                }
                (Stmt::Try(node), true)
            }
            Stmt::Labeled(node) => {
                let mut labels = vec![node.label];
                let mut body = *node.body;
                while let Stmt::Labeled(next) = body {
                    labels.push(next.label);
                    body = *next.body;
                }
                let loop_body = matches!(
                    body,
                    Stmt::While(_)
                        | Stmt::DoWhile(_)
                        | Stmt::For(_)
                        | Stmt::ForIn(_)
                        | Stmt::ForOf(_)
                );
                let mut body = self.statement(body, result);
                let reset = if loop_body {
                    let Stmt::Block(mut wrapper) = body else {
                        unreachable!("loop completion wrapper")
                    };
                    let reset = wrapper.stmts.remove(0);
                    body = wrapper.stmts.remove(0);
                    Some(reset)
                } else {
                    None
                };
                for label in labels.into_iter().rev() {
                    body = Stmt::Labeled(LabeledStmt {
                        span: DUMMY_SP,
                        label,
                        body: Box::new(body),
                    });
                }
                return if let Some(reset) = reset {
                    block(vec![reset, body])
                } else {
                    body
                };
            }
            statement => (statement, false),
        };
        if clear {
            block(vec![assign(result, undefined()), statement])
        } else {
            statement
        }
    }
}

/// Class grammar and its opaque native capability capsule at a source eval call.
/// The capsule is compiler-owned and never becomes a source-visible binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalClassContext {
    pub capsule_binding: String,
    pub private_names: Vec<String>,
    pub allow_super_property: bool,
    pub allow_super_call: bool,
    pub arguments_forbidden: bool,
}

pub type EvalClassContexts = std::collections::HashMap<u32, EvalClassContext>;

/// Resolved frame reference for a class capsule. Only grammar and slot metadata
/// are serialized; the native capability value stays in its original realm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentClassContext {
    pub capsule_slot: u32,
    pub capsule_cell: bool,
    pub private_names: Vec<String>,
    pub allow_super_property: bool,
    pub allow_super_call: bool,
    pub arguments_forbidden: bool,
}

/// Static layout of the lexical records visible at one VM evaluation point.
/// Binding names index the containing program's decoded constant pool.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvironmentMetadata {
    pub source_context: SourceContext,
    pub class_context: Option<EnvironmentClassContext>,
    pub scopes: Vec<EnvironmentScope>,
}
#[derive(Debug, Clone, PartialEq)]
pub enum EnvironmentScope {
    Bindings(Vec<EnvironmentBinding>),
    WithObject(u32),
    Variables,
}
#[derive(Debug, Clone, PartialEq)]
pub struct EnvironmentBinding {
    pub name_const: u32,
    pub slot: u32,
    pub cell: bool,
    pub lexical: bool,
    /// A suspension lowering cell exposing the original lexical binding.
    pub accessor_cell: bool,
    /// Source object records which can intercept this projected binding.
    pub objects: Vec<(u32, bool)>,
}

/// Original lexical names available to eval after suspension lowering rewrites
/// those bindings to generated cells. Keys are original eval call positions.
#[derive(Debug, Clone)]
pub struct SuspensionLexicalAlias {
    pub name: String,
    pub cell: String,
    /// Whether eval var declarations conflict with this source binding.
    pub lexical: bool,
    pub objects: Vec<String>,
}
pub type SuspensionLexicalScopes = std::collections::HashMap<u32, Vec<SuspensionLexicalAlias>>;

/// Whether this chunk tree needs an ambient lexical environment supplied by its
/// native entry shell. Ordinary chunks do not pay for environment dictionaries.
pub fn requires_environment(compiled: &Compiled) -> bool {
    let mut pending = vec![compiled];
    while let Some(chunk) = pending.pop() {
        if chunk.code.iter().any(|instruction| {
            matches!(
                instruction,
                crate::isa::Instr::BeginVarEnvironment(_)
                    | crate::isa::Instr::CaptureClosureEnvironment(_)
                    | crate::isa::Instr::EvalCall(_)
                    | crate::isa::Instr::EnvironmentRef(_)
            )
        }) {
            return true;
        }
        pending.extend(chunk.children.iter().map(|child| &child.compiled));
    }
    false
}

/// A source lexical reference rewritten to an accessor cell by suspension
/// lowering, with the object environments that can intercept its name.
#[derive(Debug, Clone)]
pub struct SuspensionLexicalReference {
    pub name: String,
    pub cell: String,
    pub objects: Vec<String>,
}
pub type SuspensionLexicalReferences = std::collections::HashMap<u32, SuspensionLexicalReference>;
