//! Source-function protection accounting, before generated helpers obscure names.
use super::{glob, last_member_key, static_prop_key_name};
use mangler_config::VirtualizeConfig;
use mangler_core::{Error, Note, Notes, Result};
use std::collections::{HashMap, HashSet};
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

#[derive(Clone)]
pub(super) struct Candidate {
    pub name: String,
    pub anonymous: bool,
    pub span: u32,
    pub end: u32,
    pub reason: Option<&'static str>,
}

#[derive(Default)]
struct Collector(Vec<Candidate>, HashSet<u32>);
impl Collector {
    fn push(&mut self, candidate: Candidate) {
        if candidate.span != 0 && self.1.insert(candidate.span) {
            self.0.push(candidate);
        }
    }
    fn add(&mut self, name: String, function: &Function) {
        self.push(Candidate {
            name,
            anonymous: false,
            span: function.span.lo.0,
            end: function.span.hi.0,
            reason: if function.is_async {
                Some("async")
            } else if function.is_generator {
                Some("generator")
            } else {
                None
            },
        });
    }
    fn inferred(&mut self, name: String, expr: &Expr) {
        match expr {
            Expr::Paren(paren) => self.inferred(name, &paren.expr),
            Expr::Fn(f) if f.ident.is_none() => self.add(name, &f.function),
            Expr::Arrow(a) => self.push(Candidate {
                name,
                span: a.span.lo.0,
                end: a.span.hi.0,
                anonymous: false,
                reason: None,
            }),
            _ => {}
        }
    }
}
impl Visit for Collector {
    fn visit_bin_expr(&mut self, binary: &BinExpr) {
        mangler_jsast::deep::walk_binary(binary, self);
    }
    fn visit_export_default_decl(&mut self, export: &ExportDefaultDecl) {
        if let DefaultDecl::Fn(function) = &export.decl {
            self.add(
                function
                    .ident
                    .as_ref()
                    .map(|id| id.sym.to_string())
                    .unwrap_or_else(|| "default".into()),
                &function.function,
            );
        }
        export.visit_children_with(self);
    }
    fn visit_export_default_expr(&mut self, export: &ExportDefaultExpr) {
        self.inferred("default".into(), &export.expr);
        export.visit_children_with(self);
    }
    fn visit_fn_decl(&mut self, f: &FnDecl) {
        self.add(f.ident.sym.to_string(), &f.function);
        f.visit_children_with(self);
    }
    fn visit_fn_expr(&mut self, f: &FnExpr) {
        if let Some(id) = &f.ident {
            self.add(id.sym.to_string(), &f.function);
        } else if !self.1.contains(&f.function.span.lo.0) {
            self.add(format!("<anonymous@{}>", f.function.span.lo.0), &f.function);
            if let Some(candidate) = self
                .0
                .last_mut()
                .filter(|candidate| candidate.span == f.function.span.lo.0)
            {
                candidate.anonymous = true;
            }
        }
        f.visit_children_with(self);
    }
    fn visit_arrow_expr(&mut self, arrow: &ArrowExpr) {
        if !self.1.contains(&arrow.span.lo.0) {
            self.push(Candidate {
                name: format!("<anonymous@{}>", arrow.span.lo.0),
                span: arrow.span.lo.0,
                end: arrow.span.hi.0,
                anonymous: true,
                reason: None,
            });
        }
        arrow.visit_children_with(self);
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
        self.add(
            static_prop_key_name(&p.key).unwrap_or_else(|| "<computed>".into()),
            &p.function,
        );
        p.visit_children_with(self);
    }
    fn visit_class_prop(&mut self, property: &ClassProp) {
        if let Some(value) = &property.value {
            self.inferred(
                static_prop_key_name(&property.key).unwrap_or_else(|| "<computed>".into()),
                value,
            );
        }
        if property.value.is_some() {
            self.push(Candidate {
                name: static_prop_key_name(&property.key).unwrap_or_else(|| "<computed>".into()),
                span: property.span.lo.0,
                end: property.span.hi.0,
                anonymous: false,
                reason: None,
            });
        }
        property.visit_children_with(self);
    }
    fn visit_private_prop(&mut self, property: &PrivateProp) {
        if let Some(value) = &property.value {
            self.inferred(format!("#{}", property.key.name), value);
        }
        if property.value.is_some() {
            self.push(Candidate {
                name: format!("#{}", property.key.name),
                span: property.span.lo.0,
                end: property.span.hi.0,
                anonymous: false,
                reason: None,
            });
        }
        property.visit_children_with(self);
    }
    fn visit_static_block(&mut self, block: &StaticBlock) {
        self.push(Candidate {
            name: "<static>".into(),
            span: block.span.lo.0,
            end: block.span.hi.0,
            reason: None,
            anonymous: false,
        });
        block.visit_children_with(self);
    }
    fn visit_private_method(&mut self, method: &PrivateMethod) {
        self.add(format!("#{}", method.key.name), &method.function);
        method.visit_children_with(self);
    }
    fn visit_constructor(&mut self, constructor: &Constructor) {
        self.add(
            "constructor".into(),
            &Function {
                span: constructor.span,
                body: constructor.body.clone(),
                ..Default::default()
            },
        );
        constructor.visit_children_with(self);
    }
    fn visit_getter_prop(&mut self, getter: &GetterProp) {
        self.add(
            static_prop_key_name(&getter.key).unwrap_or_else(|| "<computed>".into()),
            &Function {
                span: getter.span,
                ..(*getter.function).clone()
            },
        );
        getter.visit_children_with(self);
    }
    fn visit_setter_prop(&mut self, setter: &SetterProp) {
        self.add(
            static_prop_key_name(&setter.key).unwrap_or_else(|| "<computed>".into()),
            &Function {
                span: setter.span,
                ..(*setter.function).clone()
            },
        );
        setter.visit_children_with(self);
    }
    fn visit_method_prop(&mut self, p: &MethodProp) {
        self.add(
            static_prop_key_name(&p.key).unwrap_or_else(|| "<computed>".into()),
            &p.function,
        );
        p.visit_children_with(self);
    }
}

pub(super) fn candidates(program: &Program) -> Vec<Candidate> {
    let mut c = Collector::default();
    program.visit_with(&mut c);
    c.0
}

pub(super) fn function_candidates(function: &Function, include_parameters: bool) -> Vec<Candidate> {
    let mut c = Collector::default();
    if include_parameters {
        function.params.visit_with(&mut c);
    }
    function.body.visit_with(&mut c);
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
    let mut selected_failures = Vec::new();
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
        if selected && reason.is_some_and(|r| r != "excluded") {
            selected_failures.push(format!("{}: {status}", c.name));
        }
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
    if !selected_failures.is_empty() {
        return Err(Error::transform(
            "virtualize",
            format!(
                "selected source bodies could not be virtualized: {}",
                selected_failures.join(", ")
            ),
        ));
    }
    Ok(())
}

/// Merge source exclusion subtrees once; membership checks then stay logarithmic.
pub(super) fn excluded_ranges(candidates: &[Candidate], pattern: Option<&str>) -> Vec<(u32, u32)> {
    let Some(pattern) = pattern else {
        return Vec::new();
    };
    let mut ranges: Vec<_> = candidates
        .iter()
        .filter(|c| glob::matches(pattern, &c.name))
        .map(|c| (c.span, c.end))
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (start, end) in ranges {
        if let Some(previous) = merged.last_mut()
            && start <= previous.1
        {
            previous.1 = previous.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}
pub(super) fn in_ranges(span: u32, ranges: &[(u32, u32)]) -> bool {
    let end = ranges.partition_point(|(start, _)| *start <= span);
    end != 0 && span < ranges[end - 1].1
}
