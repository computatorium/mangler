//! Static scope layouts for runtime-compiled direct eval and its closures.
use super::*;
use crate::eval::{EnvironmentBinding, EnvironmentMetadata, EnvironmentScope};
use std::collections::HashSet;

pub(crate) struct EnvironmentUse {
    pub(crate) direct: bool,
    pub(crate) descendants: bool,
}
pub(crate) fn environment_usage(
    params: &[Param],
    body: &FunctionBody,
    aliases: Option<&crate::eval::SuspensionLexicalScopes>,
) -> EnvironmentUse {
    struct Scan<'a> {
        aliases: Option<&'a crate::eval::SuspensionLexicalScopes>,
        depth: u32,
        usage: EnvironmentUse,
    }
    impl Visit for Scan<'_> {
        fn visit_call_expr(&mut self, call: &CallExpr) {
            if self
                .aliases
                .is_some_and(|aliases| aliases.contains_key(&call.span.lo.0))
            {
                self.usage.descendants = true;
                self.usage.direct |= self.depth == 0;
            }
            if let Callee::Expr(callee) = &call.callee {
                let mut callee = &**callee;
                while let Expr::Paren(parenthesized) = callee {
                    callee = &parenthesized.expr;
                }
                if matches!(callee, Expr::Ident(name) if name.sym == *"eval" || name.sym == *"\0mangler_eval_invoke")
                {
                    self.usage.descendants = true;
                    self.usage.direct |= self.depth == 0;
                }
            }
            call.visit_children_with(self);
        }
        fn visit_function(&mut self, function: &Function) {
            self.depth += 1;
            function.visit_children_with(self);
            self.depth -= 1;
        }
        fn visit_arrow_expr(&mut self, arrow: &ArrowExpr) {
            self.depth += 1;
            arrow.visit_children_with(self);
            self.depth -= 1;
        }
        fn visit_bin_expr(&mut self, binary: &BinExpr) {
            walk_binary_chain(binary, self);
        }
    }
    let mut scan = Scan {
        aliases,
        depth: 0,
        usage: EnvironmentUse {
            direct: false,
            descendants: false,
        },
    };
    params.visit_with(&mut scan);
    body.visit_with(&mut scan);
    scan.usage
}

fn bindings(
    cx: &mut Cx<'_>,
    values: &HashMap<String, u32>,
    lexical: bool,
) -> Vec<EnvironmentBinding> {
    let mut values: Vec<_> = values
        .iter()
        .filter(|(name, _)| !cx.hidden_environment_bindings.contains(*name))
        .map(|(name, slot)| (name.clone(), *slot))
        .collect();
    values.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    values
        .into_iter()
        .map(|(name, slot)| {
            let cell = if slot >= cx.cap_floor {
                cx.boxed_caps.contains(&name)
            } else {
                cx.boxed_locals.contains(&name) && !cx.lexical_slots.contains_key(&slot)
            };
            EnvironmentBinding {
                name_const: cx.const_str(name),
                slot,
                cell,
                lexical: lexical && !cx.simple_catch_slots.contains(&slot),
                accessor_cell: false,
                objects: Vec::new(),
            }
        })
        .collect()
}

pub(crate) fn emit_variable_environment(cx: &mut Cx<'_>, variables: &HashMap<String, u32>) -> u32 {
    let values = bindings(cx, variables, false);
    let metadata = cx.consts.len() as u32;
    cx.consts.push(Const::Environment(EnvironmentMetadata {
        source_context: cx.opts.source_context,
        class_context: None,
        scopes: vec![EnvironmentScope::Bindings(values)],
    }));
    cx.emit(Instr::BeginVarEnvironment(metadata));
    metadata
}

/// Freeze lexical record identity while retaining live binding accessors and the
/// shared variable dictionary. The order follows actual scope nesting, including
/// multiple with records introduced at the same lexical depth.
pub(crate) fn environment_snapshot(cx: &mut Cx<'_>) -> u32 {
    let mut scopes = Vec::new();
    for depth in (0..cx.scopes.len()).rev() {
        for &(entry, slot) in cx.with_scopes.iter().rev() {
            if entry == depth + 1 {
                scopes.push(EnvironmentScope::WithObject(slot));
            }
        }
        let lexical: HashMap<String, u32> = cx.scopes[depth]
            .iter()
            .filter(|(_, slot)| **slot < cx.cap_floor && !cx.var_binding_slots.contains(slot))
            .map(|(name, slot)| (name.clone(), *slot))
            .collect();
        if !lexical.is_empty() {
            scopes.push(EnvironmentScope::Bindings(bindings(cx, &lexical, true)));
        }
    }
    scopes.push(EnvironmentScope::Variables);
    let index = cx.consts.len();
    cx.consts.push(Const::Environment(EnvironmentMetadata {
        scopes,
        source_context: cx.opts.source_context,
        class_context: None,
    }));
    cx.environment_snapshots.push(index);
    index as u32
}

/// Capture discovery is lazy, so append the complete ambient fallback reference
/// map after emission. Eval before a later static read still sees its host binding.
pub(crate) fn finish_environment_snapshots(cx: &mut Cx<'_>) {
    if cx.environment_snapshots.is_empty() {
        return;
    }
    let first = cx.next_slot - cx.captures.len() as u32;
    let captures = cx
        .captures
        .iter()
        .enumerate()
        .map(|(i, name)| (name.clone(), first + i as u32))
        .collect();
    let fallback = bindings(cx, &captures, false);
    for &index in &cx.environment_snapshots {
        if let Const::Environment(metadata) = &mut cx.consts[index] {
            metadata
                .scopes
                .push(EnvironmentScope::Bindings(fallback.clone()));
        }
    }
}

/// Recover the source lexical names which suspension lowering represents using
/// generated accessor cells. Resolving those cells also forces capture threading
/// when their only source reference occurs inside the eval string.
pub(crate) fn eval_environment_snapshot(cx: &mut Cx<'_>, position: u32) -> u32 {
    let aliases = cx
        .opts
        .suspension_lexicals
        .and_then(|scopes| scopes.get(&position))
        .cloned()
        .unwrap_or_default();
    let mut projected = Vec::with_capacity(aliases.len());
    for alias in aliases {
        let slot = cx.resolve(&alias.cell);
        let objects = alias
            .objects
            .iter()
            .map(|name| (cx.resolve(name), cx.is_celled(name)))
            .collect();
        projected.push(EnvironmentBinding {
            name_const: cx.const_str(alias.name),
            slot,
            cell: cx.is_celled(&alias.cell),
            lexical: alias.lexical,
            accessor_cell: true,
            objects,
        });
    }
    let class_context = cx
        .opts
        .eval_class_contexts
        .and_then(|contexts| contexts.get(&position))
        .cloned()
        .map(|context| crate::eval::EnvironmentClassContext {
            capsule_slot: cx.resolve(&context.capsule_binding),
            capsule_cell: cx.is_celled(&context.capsule_binding),
            private_names: context.private_names,
            allow_super_property: context.allow_super_property,
            allow_super_call: context.allow_super_call,
            arguments_forbidden: context.arguments_forbidden,
        });
    let snapshot = environment_snapshot(cx);
    if let Const::Environment(metadata) = &mut cx.consts[snapshot as usize] {
        metadata.class_context = class_context;
    }
    if !projected.is_empty()
        && let Const::Environment(metadata) = &mut cx.consts[snapshot as usize]
    {
        metadata
            .scopes
            .insert(0, EnvironmentScope::Bindings(projected));
    }
    snapshot
}

/// Metadata-only source references must participate in capture planning before
/// deciding whether the child receives an enclosing boxed cell.
pub(crate) fn metadata_capture_names(
    params: &[Param],
    body: &FunctionBody,
    opts: CompileOptions<'_>,
) -> HashSet<String> {
    struct Scan<'a> {
        opts: CompileOptions<'a>,
        names: HashSet<String>,
    }
    impl Visit for Scan<'_> {
        fn visit_call_expr(&mut self, call: &CallExpr) {
            if let Some(context) = self
                .opts
                .eval_class_contexts
                .and_then(|map| map.get(&call.span.lo.0))
            {
                self.names.insert(context.capsule_binding.clone());
            }
            if let Some(aliases) = self
                .opts
                .suspension_lexicals
                .and_then(|map| map.get(&call.span.lo.0))
            {
                self.names.extend(aliases.iter().flat_map(|alias| {
                    std::iter::once(alias.cell.clone()).chain(alias.objects.iter().cloned())
                }));
            }
            if let Some(reference) = self
                .opts
                .suspension_references
                .and_then(|map| map.get(&call.span.lo.0))
            {
                self.names.insert(reference.cell.clone());
                self.names.extend(reference.objects.iter().cloned());
            }
            call.visit_children_with(self);
        }
        fn visit_bin_expr(&mut self, binary: &BinExpr) {
            walk_binary_chain(binary, self);
        }
    }
    let mut scan = Scan {
        opts,
        names: HashSet::new(),
    };
    params.visit_with(&mut scan);
    body.visit_with(&mut scan);
    scan.names
}

/// Preserve the evaluated callable and its implicit object receiver before an
/// eval argument suspends. The tuple is internal VM data, never a source call.
pub(crate) fn emit_suspended_eval_marker(cx: &mut Cx<'_>, call: &CallExpr) -> bool {
    let Callee::Expr(callee) = &call.callee else {
        return false;
    };
    let Expr::Ident(marker) = &**callee else {
        return false;
    };
    match marker.sym.as_ref() {
        "\0mangler_eval_reference" => {
            let mut callee = &*call.args[0].expr;
            while let Expr::Paren(parenthesized) = callee {
                callee = &parenthesized.expr;
            }
            emit_eval_reference(cx, callee);
            cx.emit(Instr::MakeArray(2));
        }
        "\0mangler_eval_invoke" => {
            // Both expressions are compiler-created, private local identifiers.
            // Loading the saved tuple twice cannot repeat source side effects.
            emit_expr(cx, &call.args[0].expr);
            let receiver = cx.const_num(0.0);
            cx.emit(Instr::PushConst(receiver));
            cx.emit(Instr::GetProp);
            emit_expr(cx, &call.args[0].expr);
            let function = cx.const_num(1.0);
            cx.emit(Instr::PushConst(function));
            cx.emit(Instr::GetProp);
            emit_expr(cx, &call.args[1].expr);
            let environment = eval_environment_snapshot(cx, call.span.lo.0);
            cx.emit(Instr::EvalCall(environment));
        }
        _ => return false,
    }
    true
}

pub(crate) fn emit_eval_reference(cx: &mut Cx<'_>, callee: &Expr) {
    if let Expr::Member(member) = callee
        && matches!(&member.prop, MemberProp::Ident(property) if property.sym == *"c")
        && let Expr::Call(reference) = &*member.obj
        && super::projection::emit_projected_call_reference(cx, reference)
    {
        return;
    }
    if let Expr::Ident(id) = callee {
        emit_binding_ref(cx, id.sym.as_ref());
        cx.emit(Instr::RefCall);
    } else {
        cx.emit(Instr::PushUndef);
        emit_expr(cx, callee);
    }
}

/// The source resolver owns dynamic-code identity. This pass only carries its
/// marked expression sites through compilation, including nested chunk bodies.
pub(crate) fn source_compiler_usage(
    params: &[Param],
    body: &FunctionBody,
    sites: Option<&HashSet<u32>>,
) -> bool {
    let Some(sites) = sites.filter(|sites| !sites.is_empty()) else {
        return false;
    };
    struct Scan<'a> {
        sites: &'a HashSet<u32>,
        found: bool,
    }
    impl Visit for Scan<'_> {
        fn visit_expr(&mut self, expression: &Expr) {
            if self.found {
                return;
            }
            self.found = self
                .sites
                .contains(&swc_core::common::Spanned::span(expression).lo.0);
            if !self.found {
                expression.visit_children_with(self);
            }
        }
        fn visit_bin_expr(&mut self, binary: &BinExpr) {
            walk_binary_chain(binary, self);
        }
    }
    let mut scan = Scan {
        sites,
        found: false,
    };
    params.visit_with(&mut scan);
    body.visit_with(&mut scan);
    scan.found
}
