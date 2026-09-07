//! Source-function protection accounting, before generated helpers obscure names.
use super::{glob, last_member_key, static_prop_key_name};
use mangler_config::VirtualizeConfig;
use mangler_core::{Error, Note, Notes, Result};
use std::collections::HashMap;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

pub(super) struct Candidate {
    pub name: String,
    pub span: u32,
    pub end: u32,
    pub reason: Option<&'static str>,
}

#[derive(Default)]
struct Collector(Vec<Candidate>);
impl Collector {
    fn add(&mut self, name: String, function: &Function) {
        if !self
            .0
            .iter()
            .any(|c| c.span == function.span.lo.0 && c.name == name)
        {
            self.0.push(Candidate {
                name,
                span: function.span.lo.0,
                end: function.span.hi.0,
                reason: if function.is_async {
                    Some("async")
                } else if function.is_generator {
                    Some("generator")
                } else {
                    function.body.as_ref().and_then(|body| {
                        match mangler_vm::classify_body(&function.params, body) {
                            mangler_vm::Eligibility::Eligible => None,
                            mangler_vm::Eligibility::Skip(reason) => Some(reason),
                        }
                    })
                },
            });
        }
    }
    fn inferred(&mut self, name: String, expr: &Expr) {
        match expr {
            Expr::Fn(f) if f.ident.is_none() => self.add(name, &f.function),
            Expr::Arrow(a) => self.0.push(Candidate {
                name,
                span: a.span.lo.0,
                end: a.span.hi.0,
                reason: Some("arrow"),
            }),
            _ => {}
        }
    }
}
impl Visit for Collector {
    fn visit_fn_decl(&mut self, f: &FnDecl) {
        self.add(f.ident.sym.to_string(), &f.function);
        f.visit_children_with(self);
    }
    fn visit_fn_expr(&mut self, f: &FnExpr) {
        if let Some(id) = &f.ident {
            self.add(id.sym.to_string(), &f.function);
        }
        f.visit_children_with(self);
    }
    fn visit_var_declarator(&mut self, d: &VarDeclarator) {
        if let (Pat::Ident(id), Some(expr)) = (&d.name, &d.init) {
            self.inferred(id.id.sym.to_string(), expr);
        }
        d.visit_children_with(self);
    }
    fn visit_assign_expr(&mut self, a: &AssignExpr) {
        if a.op == AssignOp::Assign
            && let Some(name) = last_member_key(&a.left)
        {
            self.inferred(name, &a.right);
        }
        a.visit_children_with(self);
    }
    fn visit_key_value_prop(&mut self, p: &KeyValueProp) {
        if let Some(name) = static_prop_key_name(&p.key) {
            self.inferred(name, &p.value);
        }
        p.visit_children_with(self);
    }
    fn visit_class_method(&mut self, p: &ClassMethod) {
        if let Some(name) = static_prop_key_name(&p.key) {
            self.add(name, &p.function);
        }
        p.visit_children_with(self);
    }
    fn visit_method_prop(&mut self, p: &MethodProp) {
        if let Some(name) = static_prop_key_name(&p.key) {
            self.add(name, &p.function);
        }
        p.visit_children_with(self);
    }
}

pub(super) fn candidates(program: &Program) -> Vec<Candidate> {
    let mut c = Collector::default();
    program.visit_with(&mut c);
    c.0
}

pub(super) fn function_candidates(function: &Function) -> Vec<Candidate> {
    let mut c = Collector::default();
    function.visit_with(&mut c);
    c.0
}

pub(super) fn report(
    candidates: &[Candidate],
    outcomes: &HashMap<u32, Option<String>>,
    config: &VirtualizeConfig,
    notes: &mut Notes,
) -> Result<()> {
    let mut required_matches = 0;
    let mut failures = Vec::new();
    for c in candidates {
        let required = config
            .required
            .as_deref()
            .is_some_and(|g| glob::matches(g, &c.name));
        let selected = config.whole_program
            || config
                .target
                .as_deref()
                .is_some_and(|g| glob::matches(g, &c.name));
        if !selected && !required {
            continue;
        }
        let reason = match outcomes.get(&c.span) {
            Some(reason) => reason.as_deref(),
            None => Some(c.reason.unwrap_or("not_selected")),
        };
        let status = reason.map_or("virtualized".to_string(), |r| format!("native ({r})"));
        notes.push(Note::from("virtualize", format!("{}: {status}", c.name)));
        if required {
            required_matches += 1;
            if reason.is_some() {
                failures.push(format!("{}: {status}", c.name));
            }
        }
    }
    if let Some(glob) = &config.required {
        if required_matches == 0 {
            return Err(Error::transform(
                "virtualize",
                format!("--require-virtualized '{glob}' matched no source functions"),
            ));
        }
        if !failures.is_empty() {
            return Err(Error::transform(
                "virtualize",
                format!(
                    "--require-virtualized '{glob}' failed: {}",
                    failures.join(", ")
                ),
            ));
        }
    }
    Ok(())
}
