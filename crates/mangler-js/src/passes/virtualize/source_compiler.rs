//! Source-known dynamic compilation dependencies, independent of eval scope.
//! A resolver copy gives each lexical binding its own identity. The monotone
//! provenance analysis follows aliases conservatively; it never substitutes a
//! callable. Runtime identity remains the authority for eval and constructors.
use std::collections::{HashMap, HashSet, VecDeque};
use swc_core::common::{GLOBALS, Mark, Spanned, SyntaxContext};
use swc_core::ecma::ast::*;
use swc_core::ecma::transforms::base::resolver;
use swc_core::ecma::visit::{Visit, VisitMutWith, VisitWith};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Value(u64);
const DYNAMIC: u64 = 1;
const CALLABLE: u64 = 2;
const GLOBAL: u64 = 4;
const REFLECT: u64 = 8;
const OBJECT: u64 = 16;
const ARRAY: u64 = 32;
const RECORD: u64 = 64;
const CONTAINS_DYNAMIC: u64 = 128;
const FUNCTION_PROTO: u64 = 256;
const CALL: u64 = 512;
const APPLY: u64 = 1024;
const BIND: u64 = 2048;
const REFLECT_APPLY: u64 = 4096;
const REFLECT_CONSTRUCT: u64 = 8192;
const REFLECT_GET: u64 = 16384;
const GET_PROTOTYPE: u64 = 32768;
const DYNAMIC_RECEIVER: u64 = 65536;
const PRIMITIVE: u64 = 131072;
const CONTAINS_CALLABLE: u64 = 262144;
const SOURCE_ADAPTER: u64 = 524288;
const CONTAINS_ADAPTER: u64 = 1048576;
const INVOCATION_ADAPTERS: u64 = CALL | APPLY | BIND | REFLECT_APPLY | SOURCE_ADAPTER;
impl Value {
    fn has(self, kinds: u64) -> bool {
        self.0 & kinds != 0
    }
    fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    fn dynamic(self) -> bool {
        self.has(DYNAMIC | DYNAMIC_RECEIVER | CONTAINS_DYNAMIC)
    }
}
fn key(mut expression: &Expr) -> Option<String> {
    while let Expr::Paren(parenthesis) = expression {
        expression = &parenthesis.expr;
    }
    match expression {
        Expr::Lit(Lit::Str(value)) => value.value.as_str().map(str::to_owned),
        Expr::Lit(Lit::Num(value)) => Some(value.value.to_string()),
        Expr::Seq(sequence) => sequence.exprs.last().and_then(|value| key(value)),
        Expr::Tpl(value) if value.exprs.is_empty() => value
            .quasis
            .first()?
            .cooked
            .as_ref()?
            .as_str()
            .map(str::to_owned),
        _ => None,
    }
}
fn member_key(member: &MemberProp) -> Option<String> {
    match member {
        MemberProp::Ident(id) => Some(id.sym.to_string()),
        MemberProp::Computed(value) => key(&value.expr),
        _ => None,
    }
}
fn property_key(property: &PropName) -> Option<String> {
    match property {
        PropName::Ident(id) => Some(id.sym.to_string()),
        PropName::Str(value) => value.value.as_str().map(str::to_owned),
        PropName::Num(value) => Some(value.value.to_string()),
        PropName::Computed(value) => key(&value.expr),
        _ => None,
    }
}

type Node = usize;
#[derive(Default)]
struct Operation {
    seed: Value,
    inputs: Vec<Node>,
    arguments: Option<Node>,
    kind: Kind,
}
#[derive(Default)]
enum Kind {
    #[default]
    Union,
    Property(String),
    Container(u64),
    Function,
    Call(Option<String>),
    DynamicContainer,
}
struct Provenance {
    bindings: HashMap<Id, Node>,
    expressions: HashMap<usize, Node>,
    operations: Vec<Operation>,
    sites: Vec<(u32, Node)>,
    producer_calls: Vec<(u32, Node)>,
    unresolved: SyntaxContext,
    projected: bool,
}
impl Provenance {
    fn global(&self, name: &str) -> Value {
        Value(match name {
            "eval" | "Function" => DYNAMIC | CALLABLE,
            "globalThis" | "window" | "self" => GLOBAL,
            "Reflect" => REFLECT,
            "Object" => OBJECT | CALLABLE,
            "Array" | "String" | "Number" | "Boolean" | "RegExp" | "Date" | "Map" | "Set"
            | "WeakMap" | "WeakSet" | "Promise" | "Error" | "TypeError" | "Uint8Array" => CALLABLE,
            _ => 0,
        })
    }
    fn property(&self, object: Value, name: &str) -> Value {
        let mut value = Value::default();
        if object.has(GLOBAL) {
            value = value.union(self.global(name));
        }
        if object.has(REFLECT) {
            value = value.union(Value(match name {
                "apply" => CALLABLE | REFLECT_APPLY,
                "construct" => CALLABLE | REFLECT_CONSTRUCT,
                "get" => CALLABLE | REFLECT_GET,
                _ => 0,
            }));
        }
        if object.has(OBJECT) && name == "getPrototypeOf" {
            value = value.union(Value(CALLABLE | GET_PROTOTYPE));
        }
        if object.has(CONTAINS_CALLABLE) {
            value = value.union(Value(CALLABLE));
        }
        if object.has(CONTAINS_DYNAMIC) {
            value = value.union(Value(DYNAMIC | CONTAINS_DYNAMIC));
        }
        if object.has(CONTAINS_ADAPTER) {
            value = value.union(Value(CALLABLE | SOURCE_ADAPTER));
        }
        if name == "constructor" {
            if object.has(CALLABLE | FUNCTION_PROTO) {
                value = value.union(Value(DYNAMIC | CALLABLE));
            }
            if object.has(ARRAY | RECORD | PRIMITIVE) {
                value = value.union(Value(CALLABLE));
            }
        }
        if object.has(CALLABLE) {
            value = value.union(Value(match name {
                "prototype" if object.has(DYNAMIC) => CALLABLE | FUNCTION_PROTO,
                "prototype" => RECORD,
                "call" => {
                    CALLABLE
                        | CALL
                        | if object.has(INVOCATION_ADAPTERS) {
                            SOURCE_ADAPTER
                        } else {
                            0
                        }
                        | if object.has(DYNAMIC | DYNAMIC_RECEIVER) {
                            DYNAMIC_RECEIVER
                        } else {
                            0
                        }
                }
                "apply" => {
                    CALLABLE
                        | APPLY
                        | if object.has(INVOCATION_ADAPTERS) {
                            SOURCE_ADAPTER
                        } else {
                            0
                        }
                        | if object.has(DYNAMIC | DYNAMIC_RECEIVER) {
                            DYNAMIC_RECEIVER
                        } else {
                            0
                        }
                }
                "bind" => {
                    CALLABLE
                        | BIND
                        | if object.has(INVOCATION_ADAPTERS) {
                            SOURCE_ADAPTER
                        } else {
                            0
                        }
                        | if object.has(DYNAMIC | DYNAMIC_RECEIVER) {
                            DYNAMIC_RECEIVER
                        } else {
                            0
                        }
                }
                _ => 0,
            }));
        }
        if object.has(ARRAY)
            && matches!(
                name,
                "map"
                    | "filter"
                    | "forEach"
                    | "reduce"
                    | "reduceRight"
                    | "some"
                    | "every"
                    | "find"
                    | "findIndex"
                    | "findLast"
                    | "findLastIndex"
                    | "slice"
                    | "splice"
                    | "concat"
                    | "push"
                    | "pop"
                    | "shift"
                    | "unshift"
                    | "sort"
                    | "reverse"
                    | "toSorted"
                    | "toReversed"
                    | "toSpliced"
                    | "with"
                    | "join"
                    | "keys"
                    | "values"
                    | "entries"
                    | "flat"
                    | "flatMap"
                    | "includes"
                    | "indexOf"
                    | "lastIndexOf"
                    | "at"
                    | "fill"
                    | "copyWithin"
                    | "toString"
                    | "toLocaleString"
            )
        {
            value = value.union(Value(CALLABLE));
        }
        if object.has(PRIMITIVE)
            && matches!(
                name,
                "toString"
                    | "valueOf"
                    | "charAt"
                    | "charCodeAt"
                    | "codePointAt"
                    | "slice"
                    | "substring"
                    | "split"
                    | "replace"
                    | "replaceAll"
                    | "match"
                    | "matchAll"
                    | "indexOf"
                    | "includes"
                    | "trim"
                    | "toUpperCase"
                    | "toLowerCase"
            )
        {
            value = value.union(Value(CALLABLE));
        }
        value
    }

    fn node(&mut self, operation: Operation) -> Node {
        let node = self.operations.len();
        self.operations.push(operation);
        node
    }
    fn binding(&mut self, id: &Ident) -> Node {
        let identity = id.to_id();
        if let Some(node) = self.bindings.get(&identity) {
            return *node;
        }
        let seed = if id.ctxt == self.unresolved {
            self.global(id.sym.as_ref())
        } else {
            Value::default()
        };
        let node = self.node(Operation {
            seed,
            ..Default::default()
        });
        self.bindings.insert(identity, node);
        node
    }
    fn expression(&mut self, expression: &Expr) -> Node {
        let identity = expression as *const Expr as usize;
        if let Some(node) = self.expressions.get(&identity) {
            return *node;
        }
        let node = self.node(Operation::default());
        self.expressions.insert(identity, node);
        node
    }
    fn project(&mut self, input: Node, name: String) -> Node {
        self.node(Operation {
            inputs: vec![input],
            kind: Kind::Property(name),
            ..Default::default()
        })
    }
    fn connect(&mut self, id: &Ident, input: Node) {
        let binding = self.binding(id);
        self.operations[binding].inputs.push(input);
    }
    fn pattern(&mut self, pattern: &Pat, input: Node) {
        match pattern {
            Pat::Ident(binding) => self.connect(&binding.id, input),
            Pat::Assign(assign) => {
                self.pattern(&assign.left, input);
                let fallback = self.expression(&assign.right);
                self.pattern(&assign.left, fallback);
            }
            Pat::Rest(rest) => self.pattern(&rest.arg, input),
            Pat::Array(array) => {
                let element = self.project(input, "0".into());
                for pattern in array.elems.iter().flatten() {
                    self.pattern(pattern, element);
                }
            }
            Pat::Object(object) => self.object_pattern(object, input),
            _ => {}
        }
    }
    fn object_pattern(&mut self, object: &ObjectPat, input: Node) {
        for property in &object.props {
            match property {
                ObjectPatProp::KeyValue(property) => {
                    if let Some(key) = property_key(&property.key) {
                        let projected = self.project(input, key);
                        self.pattern(&property.value, projected);
                    }
                }
                ObjectPatProp::Assign(property) => {
                    let projected = self.project(input, property.key.id.sym.to_string());
                    self.connect(&property.key.id, projected);
                    if let Some(fallback) = &property.value {
                        let fallback = self.expression(fallback);
                        self.connect(&property.key.id, fallback);
                    }
                }
                ObjectPatProp::Rest(rest) => self.pattern(&rest.arg, input),
            }
        }
    }
    fn returns(&mut self, body: &FunctionBody) -> Vec<Node> {
        struct Returns<'a> {
            graph: &'a mut Provenance,
            nodes: Vec<Node>,
        }
        impl Visit for Returns<'_> {
            fn visit_bin_expr(&mut self, binary: &BinExpr) {
                mangler_jsast::deep::walk_binary(binary, self);
            }
            fn visit_expr(&mut self, _: &Expr) {}
            fn visit_function(&mut self, _: &Function) {}
            fn visit_arrow_expr(&mut self, _: &ArrowExpr) {}
            fn visit_class(&mut self, _: &Class) {}
            fn visit_return_stmt(&mut self, statement: &ReturnStmt) {
                if let Some(value) = &statement.arg {
                    self.nodes.push(self.graph.expression(value));
                }
            }
        }
        let mut returns = Returns {
            graph: self,
            nodes: Vec::new(),
        };
        body.visit_with(&mut returns);
        returns.nodes
    }
    fn function(&mut self, function: &Function) -> Operation {
        Operation {
            seed: Value(CALLABLE),
            inputs: function
                .body
                .as_ref()
                .map_or_else(Vec::new, |body| self.returns(body)),
            kind: Kind::Function,
            ..Default::default()
        }
    }
    fn call(&mut self, callee: &Expr, args: &[ExprOrSpread]) -> Operation {
        let mut inputs = vec![self.expression(callee)];
        inputs.extend(args.iter().map(|arg| self.expression(&arg.expr)));
        // Keep argument provenance separately aggregated in the worklist. This
        // avoids rescanning a long argument vector every time one input changes.
        let arguments = self.node(Operation {
            inputs: inputs[1..].to_vec(),
            ..Default::default()
        });
        inputs.push(arguments);
        Operation {
            inputs,
            arguments: Some(arguments),
            kind: Kind::Call(args.get(1).and_then(|arg| key(&arg.expr))),
            ..Default::default()
        }
    }
    fn operation(&mut self, expression: &Expr) -> Operation {
        let mut operation = Operation::default();
        match expression {
            Expr::Ident(id) => operation.inputs.push(self.binding(id)),
            Expr::Fn(function) => return self.function(&function.function),
            Expr::Arrow(arrow) => {
                operation.seed = Value(CALLABLE);
                operation.kind = Kind::Function;
                operation.inputs = match &*arrow.body {
                    ArrowFunctionBody::Expr(expr) => vec![self.expression(expr)],
                    ArrowFunctionBody::FunctionBody(body) => self.returns(body),
                };
            }
            Expr::Class(_) => operation.seed = Value(CALLABLE),
            Expr::Paren(value) => operation.inputs.push(self.expression(&value.expr)),
            Expr::Seq(value) => {
                if let Some(value) = value.exprs.last() {
                    operation.inputs.push(self.expression(value));
                }
            }
            Expr::Cond(value) => {
                operation.inputs = vec![self.expression(&value.cons), self.expression(&value.alt)];
            }
            Expr::Assign(value) => operation.inputs.push(self.expression(&value.right)),
            Expr::Bin(value)
                if matches!(
                    value.op,
                    BinaryOp::LogicalAnd | BinaryOp::LogicalOr | BinaryOp::NullishCoalescing
                ) =>
            {
                operation.inputs =
                    vec![self.expression(&value.left), self.expression(&value.right)];
            }
            Expr::Member(value) => {
                if let Some(name) = member_key(&value.prop) {
                    operation.kind = Kind::Property(name);
                    operation.inputs.push(self.expression(&value.obj));
                }
            }
            Expr::OptChain(chain) => match &*chain.base {
                OptChainBase::Member(member) => {
                    if let Some(name) = member_key(&member.prop) {
                        operation.kind = Kind::Property(name);
                        operation.inputs.push(self.expression(&member.obj));
                    }
                }
                OptChainBase::Call(call) => return self.call(&call.callee, &call.args),
            },
            Expr::Array(array) => {
                operation.kind = Kind::Container(ARRAY);
                operation.inputs = array
                    .elems
                    .iter()
                    .flatten()
                    .map(|value| self.expression(&value.expr))
                    .collect();
            }
            Expr::Object(object) => {
                operation.kind = Kind::Container(RECORD);
                for property in &object.props {
                    match property {
                        PropOrSpread::Spread(spread) => {
                            operation.inputs.push(self.expression(&spread.expr))
                        }
                        PropOrSpread::Prop(property) => match &**property {
                            Prop::KeyValue(value) => {
                                operation.inputs.push(self.expression(&value.value))
                            }
                            Prop::Shorthand(id) => operation.inputs.push(self.binding(id)),
                            Prop::Assign(value) => {
                                operation.inputs.push(self.expression(&value.value))
                            }
                            Prop::Method(method) => {
                                let callable = self.function(&method.function);
                                operation.inputs.push(self.node(callable));
                            }
                            Prop::Getter(getter) => {
                                if let Some(body) = &getter.function.body {
                                    operation.inputs.extend(self.returns(body));
                                }
                            }
                            Prop::Setter(_) => {}
                        },
                    }
                }
            }
            Expr::Lit(_) | Expr::Tpl(_) => operation.seed = Value(PRIMITIVE),
            Expr::Call(call) => {
                if let Callee::Expr(callee) = &call.callee {
                    return self.call(callee, &call.args);
                }
            }
            _ => {}
        }
        operation
    }
    fn evaluate(&self, operation: &Operation, values: &[Value], combined: Value) -> Value {
        let result = match &operation.kind {
            Kind::Union => combined,
            Kind::Property(name) => self.property(values[operation.inputs[0]], name),
            Kind::Container(kind) => Value(
                *kind
                    | if combined.has(INVOCATION_ADAPTERS | CONTAINS_ADAPTER) {
                        CONTAINS_ADAPTER
                    } else {
                        0
                    }
                    | if combined.has(CALLABLE | CONTAINS_CALLABLE) {
                        CONTAINS_CALLABLE
                    } else {
                        0
                    }
                    | if combined.dynamic() {
                        CONTAINS_DYNAMIC
                    } else {
                        0
                    },
            ),
            Kind::DynamicContainer => Value(if combined.dynamic() {
                CONTAINS_DYNAMIC
            } else {
                0
            }),
            Kind::Function => Value(
                ((combined.0 & 0xffff_ffff) << 32)
                    | if combined.dynamic() {
                        CONTAINS_DYNAMIC
                    } else {
                        0
                    },
            ),
            Kind::Call(key) => {
                let target = values[operation.inputs[0]];
                let mut result = Value(
                    (target.0 >> 32)
                        | if target.has(CONTAINS_DYNAMIC) || combined.dynamic() {
                            CONTAINS_DYNAMIC
                        } else {
                            0
                        },
                );
                if target.has(GET_PROTOTYPE)
                    && operation
                        .inputs
                        .get(1)
                        .is_some_and(|node| values[*node].has(CALLABLE))
                {
                    result = result.union(Value(FUNCTION_PROTO));
                }
                if target.has(REFLECT_GET)
                    && let (Some(node), Some(key)) = (operation.inputs.get(1), key)
                {
                    result = result.union(self.property(values[*node], key));
                }
                if target.has(INVOCATION_ADAPTERS) {
                    let adapter = target.has(SOURCE_ADAPTER)
                        || operation.arguments.is_some_and(|node| {
                            values[node].has(INVOCATION_ADAPTERS | CONTAINS_ADAPTER)
                        });
                    result = result.union(Value(
                        if target.has(BIND) || combined.dynamic() || adapter {
                            CALLABLE
                        } else {
                            0
                        } | if target.has(DYNAMIC_RECEIVER) || combined.dynamic() {
                            DYNAMIC_RECEIVER
                        } else {
                            0
                        } | if adapter { SOURCE_ADAPTER } else { 0 },
                    ));
                }
                result
            }
        };
        operation.seed.union(result)
    }
    /// Each node can gain at most 64 bits. Only changed inputs revisit their
    /// consumers; no whole-program rounds or recursive expression queries.
    fn solve(&self) -> (Vec<Value>, usize) {
        let mut dependents = vec![Vec::new(); self.operations.len()];
        for (node, operation) in self.operations.iter().enumerate() {
            for input in &operation.inputs {
                dependents[*input].push(node);
            }
        }
        let mut values = vec![Value::default(); self.operations.len()];
        let mut aggregates = vec![Value::default(); self.operations.len()];
        let mut queue: VecDeque<_> = (0..self.operations.len()).collect();
        let mut queued = vec![true; self.operations.len()];
        let mut evaluations = 0;
        while let Some(node) = queue.pop_front() {
            queued[node] = false;
            evaluations += 1;
            let value = values[node].union(self.evaluate(
                &self.operations[node],
                &values,
                aggregates[node],
            ));
            if value != values[node] {
                values[node] = value;
                for next in &dependents[node] {
                    aggregates[*next] = aggregates[*next].union(value);
                    if !queued[*next] {
                        queued[*next] = true;
                        queue.push_back(*next);
                    }
                }
            }
        }
        (values, evaluations)
    }
    fn expression_site(&mut self, expression: &Expr) {
        let node = self.expression(expression);
        self.operations[node] = self.operation(expression);
        if (matches!(expression, Expr::Call(_))
            || matches!(expression, Expr::OptChain(chain) if matches!(&*chain.base, OptChainBase::Call(_))))
            && expression.span().lo.0 != 0
        {
            self.producer_calls.push((expression.span().lo.0, node));
        }
        if !self.projected && expression.span().lo.0 != 0 {
            self.sites.push((expression.span().lo.0, node));
        }
    }
}
impl Visit for Provenance {
    fn visit_bin_expr(&mut self, binary: &BinExpr) {
        mangler_jsast::deep::walk_binary(binary, self);
    }
    fn visit_expr(&mut self, expression: &Expr) {
        let previous = self.projected;
        let mut pending = vec![(expression, previous)];
        while let Some((current, projected)) = pending.pop() {
            self.projected = projected;
            self.expression_site(current);
            match current {
                Expr::Member(member) => {
                    pending.push((&member.obj, true));
                    if let MemberProp::Computed(property) = &member.prop {
                        pending.push((&property.expr, false));
                    }
                }
                Expr::OptChain(chain) => match &*chain.base {
                    OptChainBase::Member(member) => {
                        pending.push((&member.obj, true));
                        if let MemberProp::Computed(property) = &member.prop {
                            pending.push((&property.expr, false));
                        }
                    }
                    OptChainBase::Call(_) => {
                        self.projected = false;
                        current.visit_children_with(self);
                    }
                },
                Expr::Paren(parenthesis) => pending.push((&parenthesis.expr, projected)),
                Expr::Bin(binary) => {
                    pending.push((&binary.right, projected));
                    pending.push((&binary.left, projected));
                }
                Expr::Call(_) | Expr::New(_) => {
                    self.projected = false;
                    current.visit_children_with(self);
                }
                _ => current.visit_children_with(self),
            }
        }
        self.projected = previous;
    }
    fn visit_var_declarator(&mut self, declaration: &VarDeclarator) {
        if let Some(value) = &declaration.init {
            let input = self.expression(value);
            self.pattern(&declaration.name, input);
        }
        declaration.visit_children_with(self);
    }
    fn visit_assign_pat(&mut self, assignment: &AssignPat) {
        let input = self.expression(&assignment.right);
        self.pattern(&assignment.left, input);
        assignment.visit_children_with(self);
    }
    fn visit_assign_expr(&mut self, assignment: &AssignExpr) {
        let input = self.expression(&assignment.right);
        match &assignment.left {
            AssignTarget::Simple(SimpleAssignTarget::Ident(binding)) => {
                self.connect(&binding.id, input)
            }
            AssignTarget::Pat(AssignTargetPat::Array(array)) => {
                let element = self.project(input, "0".into());
                for pattern in array.elems.iter().flatten() {
                    self.pattern(pattern, element);
                }
            }
            AssignTarget::Pat(AssignTargetPat::Object(object)) => {
                self.object_pattern(object, input)
            }
            AssignTarget::Simple(SimpleAssignTarget::Member(member)) => {
                let mut object = &*member.obj;
                while let Expr::Member(parent) = object {
                    object = &parent.obj;
                }
                if let Expr::Ident(id) = object {
                    let container = self.node(Operation {
                        inputs: vec![input],
                        kind: Kind::DynamicContainer,
                        ..Default::default()
                    });
                    self.connect(id, container);
                }
            }
            _ => {}
        }
        assignment.visit_children_with(self);
    }
    fn visit_fn_decl(&mut self, declaration: &FnDecl) {
        let operation = self.function(&declaration.function);
        let input = self.node(operation);
        self.connect(&declaration.ident, input);
        declaration.visit_children_with(self);
    }
    fn visit_fn_expr(&mut self, expression: &FnExpr) {
        if let Some(id) = &expression.ident {
            let operation = self.function(&expression.function);
            let input = self.node(operation);
            self.connect(id, input);
        }
        expression.visit_children_with(self);
    }
    fn visit_class_decl(&mut self, declaration: &ClassDecl) {
        let input = self.node(Operation {
            seed: Value(CALLABLE),
            ..Default::default()
        });
        self.connect(&declaration.ident, input);
        declaration.visit_children_with(self);
    }
}
/// One provenance result supplies protected consumer dependencies and native bind
/// producer sites. Producers are used only when a protected consumer requires the
/// compiler; they never turn an unrelated program into a compiler dependency.
#[derive(Clone, Default)]
pub(crate) struct SourceDependencies {
    pub consumers: HashSet<u32>,
    pub bind_producers: HashSet<u32>,
}
pub(super) fn dependencies(program: &mut Program) -> SourceDependencies {
    analyze(program).0
}
/// Return source expression identities whose protected use requires dynamic compilation.
#[cfg(test)]
pub(super) fn sites(program: &mut Program) -> HashSet<u32> {
    analyze(program).0.consumers
}
fn analyze(program: &mut Program) -> (SourceDependencies, usize, usize) {
    fn resolved(program: &mut Program) -> (SourceDependencies, usize, usize) {
        let mut copy = mangler_jsast::deep::clone_program(program);
        let unresolved = Mark::new();
        mangler_jsast::deep::with_flattened_spines(&mut copy, |program| {
            program.visit_mut_with(&mut resolver(unresolved, Mark::new(), false));
            mangler_jsast::Js::repair_resolver_scopes(program);
        });
        let mut provenance = Provenance {
            bindings: HashMap::new(),
            expressions: HashMap::new(),
            operations: Vec::new(),
            sites: Vec::new(),
            producer_calls: Vec::new(),
            projected: false,
            unresolved: SyntaxContext::empty().apply_mark(unresolved),
        };
        copy.visit_with(&mut provenance);
        let (values, evaluations) = provenance.solve();
        let result = provenance
            .sites
            .iter()
            .filter_map(|(span, node)| values[*node].dynamic().then_some(*span))
            .collect();
        let bind_producers = provenance
            .producer_calls
            .iter()
            .filter_map(|(span, node)| {
                let operation = &provenance.operations[*node];
                (matches!(operation.kind, Kind::Call(_))
                    && (values[*node].dynamic() || values[*node].has(SOURCE_ADAPTER))
                    && operation
                        .inputs
                        .first()
                        .is_some_and(|target| values[*target].has(INVOCATION_ADAPTERS)))
                .then_some(*span)
            })
            .collect();
        mangler_jsast::deep::drop_program(copy);
        (
            SourceDependencies {
                consumers: result,
                bind_producers,
            },
            provenance.operations.len(),
            evaluations,
        )
    }
    if GLOBALS.is_set() {
        resolved(program)
    } else {
        mangler_jsast::Js::with_globals(|| resolved(program))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};

    fn needs_compiler(source: &str) -> bool {
        let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
        let dependencies = sites(ast.program_mut());
        struct Entry(Option<swc_core::common::Span>);
        impl Visit for Entry {
            fn visit_fn_decl(&mut self, function: &FnDecl) {
                if function.ident.sym == "pay" {
                    self.0 = Some(function.function.span);
                }
                function.visit_children_with(self);
            }
        }
        let mut entry = Entry(None);
        ast.program().visit_with(&mut entry);
        let span = entry.0.expect("pay source function");
        dependencies
            .iter()
            .any(|site| *site >= span.lo.0 && *site < span.hi.0)
    }

    #[test]
    fn known_dynamic_values_flow_into_selected_source() {
        for source in [
            "function pay(){return (0,eval)('1')}",
            "const run=eval;function pay(){return run('1')}",
            "const make=Function;function pay(){return make('return 1')()}",
            "let a,b;a=Function;b=a;function pay(){return b('return 1')()}",
            "const root=globalThis;const {Function:make}=root;function pay(){return make('return 1')()}",
            "const [make]=[Function];function pay(){return make('return 1')()}",
            "function get(){return Function}const make=get();function pay(){return make('return 1')()}",
            "const get=()=>Function;const make=get();function pay(){return make('return 1')()}",
            "const h=()=>()=>Function;const make=h()();function pay(){return make('return 17')()}",
            "const identity=x=>x;const make=identity(Function);function pay(){return make('return 17')()}",
            "function pay(){return globalThis['eval']('1')}",
            "function pay(){return globalThis?.Function?.('return 1')()}",
            "function pay(){return self['Function']('return 1')()}",
            "function pay(){return window.Function('return 1')()}",
            "function pay(){return Function.call(null,'return 1')()}",
            "function pay(){return Function.apply(null,['return 1'])()}",
            "const make=Function.bind(null);function pay(){return make('return 1')()}",
            "function pay(){return Reflect.apply(Function,null,['return 1'])()}",
            "function pay(){return Reflect.construct(Function,['return 1'])()}",
            "const make=Reflect.get(globalThis,'Function');function pay(){return make('return 1')()}",
            "function pay(){return (()=>{}).constructor('return 1')()}",
            "function pay(){return Object.getPrototypeOf(async()=>{}).constructor('return 1')()}",
            "function pay(){return Object.getPrototypeOf(function*(){}).constructor('yield 1')()}",
            "function pay(){return Object.getPrototypeOf(async function*(){}).constructor('yield 1')()}",
            "function pay(){return [].map.constructor('return 1')()}",
            "function pay(){return ({method(){}}).method.constructor('return 1')()}",
            "function pay(){return ({method:()=>{}}).method.constructor('return 1')()}",
            "function pay(){return [()=>{}][0].constructor('return 1')()}",
            "const box={get make(){return Function}};function pay(){return box.make('return 1')()}",
            "const box={make:Function};function pay(){return box.make('return 1')()}",
            "const box={};box.make=Function;function pay(){return box.make('return 1')()}",
        ] {
            assert!(needs_compiler(source), "missed dependency: {source}");
        }
    }

    #[test]
    fn native_bind_producers_share_the_consumer_provenance_graph() {
        for (source, expected) in [
            (
                "const make=Function.bind(null);function pay(){return make('return 7')()}",
                1,
            ),
            (
                "const make=Function.prototype.call.bind(Function);function pay(){return make(null,'return 7')()}",
                1,
            ),
            (
                "const make=Function.prototype.apply.bind(Function);function pay(){return make(null,['return 7'])()}",
                1,
            ),
            (
                "const make=Function.bind(null).bind(null);function pay(){return make('return 7')()}",
                1,
            ),
            (
                "const first=Function.bind(null);const make=first.bind(null);function pay(){return make('return 7')()}",
                2,
            ),
            (
                "function produce(){return Function.bind(null)}const make=produce();function pay(){return make('return 7')()}",
                1,
            ),
            (
                "const make=Object.getPrototypeOf(async function(){}).constructor.bind(null);function pay(){return make('return 7')()}",
                1,
            ),
            (
                "const make=Function.prototype.bind.call(Function,null);function pay(){return make('return 7')()}",
                1,
            ),
            (
                "const make=Function.prototype.bind.apply(Function,[null]);function pay(){return make('return 7')()}",
                1,
            ),
            (
                "const make=Reflect.apply(Function.prototype.bind,Function,[null]);function pay(){return make('return 7')()}",
                1,
            ),
            (
                "const bind=Function.prototype.call.bind(Function.prototype.bind);const make=bind(Function,null);function pay(){return make('return 7')()}",
                2,
            ),
            (
                "const bind=Function.prototype.apply.bind(Function.prototype.bind);const make=bind(Function,[null]);function pay(){return make('return 7')()}",
                2,
            ),
            (
                "const bind=Reflect.apply.bind(null,Function.prototype.bind);const make=bind(Function,[null]);function pay(){return make('return 7')()}",
                2,
            ),
            (
                "function ordinary(){}const bound=ordinary.bind(null);function pay(){return bound()}",
                0,
            ),
            ("function pay(Function){return Function.bind(null)}", 0),
        ] {
            let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
            let before_consumer = source.find("function pay").unwrap() as u32 + 1;
            assert_eq!(
                dependencies(ast.program_mut())
                    .bind_producers
                    .iter()
                    .filter(|site| **site < before_consumer)
                    .count(),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn optional_bind_calls_share_producer_and_consumer_provenance() {
        for producer in [
            "Function?.bind(null)",
            "Function.bind?.(null)",
            "Function?.bind?.(null)",
            "Function?.bind(null).bind(null)",
            "(Function?.bind)(null)",
            "Function.prototype.bind.call?.(Function,null)",
            "Reflect?.apply?.(Function.prototype.bind,Function,[null])",
            "Function?.[(sideEffect(),'bind')]?.(null)",
        ] {
            let source =
                format!("const make={producer};function pay(){{return make('return 7')()}}");
            let mut ast = Js.parse(&source, &ParseOpts::default()).unwrap();
            let consumer = source.find("function pay").unwrap() as u32 + 1;
            let result = dependencies(ast.program_mut());
            assert!(
                result.bind_producers.iter().any(|site| *site < consumer),
                "{source}"
            );
            assert!(
                result.consumers.iter().any(|site| *site > consumer),
                "{source}"
            );
        }
        for source in [
            "function pay(callback){return callback?.()}",
            "function pay(Function){return Function?.bind?.(null)}",
        ] {
            assert!(!needs_compiler(source), "{source}");
        }
    }

    #[test]
    fn ordinary_calls_and_lexical_shadows_remain_lean() {
        for source in [
            "function pay(x){return Math.round(x*1.07)}",
            "function pay(callback){return callback('ordinary')}",
            "function pay(Function){return Function('ordinary')}",
            "function Function(){return 1}function pay(){return Function()}",
            "function pay(){const Function=()=>1;return Function()}",
            "function pay(globalThis){return globalThis.Function('ordinary')}",
            "function pay(){return Function.prototype.call.call(()=>1,null)}",
            "const call=Function.prototype.call;function pay(){return call.call(()=>1,null)}",
            "const make=Function;function pay(){const make=()=>1;return make()}",
            "function other(){return Function}function pay(){return 1}",
        ] {
            assert!(!needs_compiler(source), "unrelated dependency: {source}");
        }
    }

    #[test]
    fn indirect_dependency_reaches_compiler_usage_without_an_eval_instruction() {
        for (source, expected) in [
            (
                "const make=Function;function pay(){return make('return 1')()}",
                true,
            ),
            ("function pay(value){return Math.round(value*1.07)}", false),
        ] {
            let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
            let sites = sites(ast.program_mut());
            let Program::Script(script) = ast.program() else {
                panic!("script")
            };
            let function = script
                .body
                .iter()
                .find_map(|statement| match statement {
                    Stmt::Decl(Decl::Fn(function)) if function.ident.sym == "pay" => {
                        Some(&function.function)
                    }
                    _ => None,
                })
                .unwrap();
            let compiled = mangler_vm::compile_body_with_opts(
                &function.params,
                function.body.as_ref().unwrap(),
                mangler_vm::CompileOptions {
                    source_compiler_sites: Some(&sites),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(compiled.requires_source_compiler, expected);
            assert!(
                !compiled
                    .code
                    .iter()
                    .any(|instruction| matches!(instruction, mangler_vm::Instr::EvalCall(_)))
            );
            let mut usage = mangler_vm::chunk::InstructionUsage::default();
            usage.include(&compiled);
            assert_eq!(usage.has_eval(), expected);
        }
    }

    #[test]
    fn reverse_alias_chain_uses_bounded_dependency_work() {
        let count = 20_000;
        let mut source = String::new();
        for index in 0..count {
            source.push_str(&format!("var alias{index}=alias{};", index + 1));
        }
        source.push_str(&format!(
            "var alias{count}=Function;function pay(){{return alias0('return 1')}}"
        ));
        let mut ast = Js.parse(&source, &ParseOpts::default()).unwrap();
        let (sites, nodes, evaluations) = analyze(ast.program_mut());
        assert!(!sites.consumers.is_empty());
        assert!(
            evaluations < nodes * 5,
            "{evaluations} evaluations for {nodes} nodes"
        );
    }

    #[test]
    fn wide_bind_arguments_use_bounded_dependency_work() {
        let count = 20_000;
        let mut source = String::new();
        for index in 0..count {
            source.push_str(&format!("var alias{index}=alias{};", index + 1));
        }
        source.push_str(&format!("var alias{count}=Function;function pay(){{return Function.prototype.bind.call(Function,null"));
        for index in 0..count {
            source.push_str(&format!(",alias{index}"));
        }
        source.push_str(")}");
        let mut ast = Js.parse(&source, &ParseOpts::default()).unwrap();
        let (sites, nodes, evaluations) = analyze(ast.program_mut());
        assert!(!sites.bind_producers.is_empty());
        assert!(
            evaluations < nodes * 5,
            "{evaluations} evaluations for {nodes} nodes"
        );
    }

    #[test]
    fn higher_order_factory_depth_uses_bounded_dependency_work() {
        let count = 20_000;
        let mut source = String::from("const factory0=Function;");
        for index in 1..=count {
            source.push_str(&format!("const factory{index}=()=>factory{};", index - 1));
        }
        source.push_str(&format!("function pay(){{return factory{count}()}}"));
        let mut ast = Js.parse(&source, &ParseOpts::default()).unwrap();
        let (sites, nodes, evaluations) = analyze(ast.program_mut());
        let marker = source.rfind("factory").unwrap() as u32 + 1;
        assert!(sites.consumers.contains(&marker));
        assert!(
            evaluations < nodes * 5,
            "{evaluations} evaluations for {nodes} nodes"
        );
    }

    #[test]
    fn twenty_thousand_logical_operands_preserve_dependency_without_recursion() {
        let mut ast = Js
            .parse("function pay(){return Function}", &ParseOpts::default())
            .unwrap();
        let Program::Script(script) = ast.program_mut() else {
            panic!("script")
        };
        let Stmt::Decl(Decl::Fn(function)) = &mut script.body[0] else {
            panic!("function")
        };
        let Stmt::Return(statement) = &mut function.function.body.as_mut().unwrap().stmts[0] else {
            panic!("return")
        };
        let mut expression = statement.arg.take().unwrap();
        let span = expression.span();
        for _ in 0..20_000 {
            expression = Box::new(Expr::Bin(BinExpr {
                span,
                op: BinaryOp::LogicalOr,
                left: expression,
                right: Box::new(Expr::Lit(Lit::Bool(Bool { span, value: false }))),
            }));
        }
        statement.arg = Some(expression);
        let (sites, nodes, evaluations) = analyze(ast.program_mut());
        assert!(sites.consumers.contains(&span.lo.0));
        assert!(
            evaluations < nodes * 5,
            "{evaluations} evaluations for {nodes} nodes"
        );
        let program = std::mem::replace(
            ast.program_mut(),
            Program::Script(Script {
                span: swc_core::common::DUMMY_SP,
                body: Vec::new(),
                shebang: None,
            }),
        );
        mangler_jsast::deep::drop_program(program);
    }
}
