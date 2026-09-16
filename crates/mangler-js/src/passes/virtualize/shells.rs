//! Native callable envelopes for lowered suspension bodies. Only the entry thunk
//! lives in the method; the source body and resume callbacks are bytecode.
use super::*;
use mangler_vm::SuspensionKind;

/// Wrap a compiled entry without adding another Promise adoption. Declarations
/// pass no self name and install the result in their enclosing hoisted binding;
/// named expressions retain a private recursive binding.
pub(super) fn callable(
    function: Function,
    self_name: Option<Ident>,
    display_name: &str,
    kind: SuspensionKind,
    cfg: &FileConfig,
) -> (Box<Expr>, Vec<Stmt>) {
    shell(
        EntryValue::Function(function),
        self_name,
        display_name,
        kind,
        cfg,
    )
}

pub(super) fn arrow(
    arrow: ArrowExpr,
    display_name: &str,
    cfg: &FileConfig,
) -> (Box<Expr>, Vec<Stmt>) {
    shell(
        EntryValue::Arrow(arrow),
        None,
        display_name,
        SuspensionKind::Async,
        cfg,
    )
}

enum EntryValue {
    Function(Function),
    Arrow(ArrowExpr),
}
fn shell(
    entry: EntryValue,
    self_name: Option<Ident>,
    display_name: &str,
    kind: SuspensionKind,
    cfg: &FileConfig,
) -> (Box<Expr>, Vec<Stmt>) {
    let set_proto = cfg.fresh_name();
    let get_proto = cfg.fresh_name();
    let define = cfg.fresh_name();
    let descriptor = cfg.fresh_name();
    let proxy = cfg.fresh_name();
    let apply = cfg.fresh_name();
    let aliases = format!(
        "var {set_proto}=Object.setPrototypeOf,{get_proto}=Object.getPrototypeOf,{define}=Object.defineProperty,{descriptor}=Object.getOwnPropertyDescriptor,{proxy}=Proxy,{apply}=Reflect.apply;"
    );
    let Program::Script(alias_program) = Js
        .parse(&aliases, &ParseOpts::default())
        .expect("shell intrinsic aliases parse")
        .into_program()
    else {
        unreachable!()
    };
    let shell = cfg.fresh_name();
    let template = cfg.fresh_name();
    let finish = cfg.fresh_name();
    let iterator = cfg.fresh_name();
    let proto = cfg.fresh_name();
    let template_source = match kind {
        SuspensionKind::Async => "async function(){}",
        SuspensionKind::Generator => "function*(){}",
        SuspensionKind::AsyncGenerator => "async function*(){}",
    };
    let name = "\"__mangler_display_name\"";
    let iterator_support = if kind != SuspensionKind::Async {
        format!(
            "function {finish}({iterator}){{const {proto}={shell}.prototype;Object.setPrototypeOf({iterator},{proto}!==null&&(typeof {proto}==='object'||typeof {proto}==='function')?{proto}:Object.getPrototypeOf({template}.prototype));return {iterator}}}"
        )
    } else {
        String::new()
    };
    let prototype = if kind == SuspensionKind::Async {
        String::new()
    } else {
        format!(
            "Object.defineProperty({shell},'prototype',{{value:{template}.prototype,writable:true}});"
        )
    };
    let local = self_name
        .as_ref()
        .map(|id| format!("const {}={shell};", id.sym))
        .unwrap_or_default();
    let trap = if kind == SuspensionKind::Async {
        format!(
            "{shell}=new {proxy}({shell},{{apply(target,receiver,args){{try{{return {apply}(target,receiver,args)}}catch(error){{return (async()=>{{throw error}})()}}}}}});"
        )
    } else {
        String::new()
    };
    let source = format!(
        "(()=>{{const {template}={template_source};let {shell}=({{__entry(){{}}}}).__entry;{iterator_support}Object.setPrototypeOf({shell},Object.getPrototypeOf({template}));Object.defineProperty({shell},'name',{{value:{name},configurable:true}});{prototype}{trap}{local}return {shell}}})()"
    );
    let mut program = Js
        .parse(&source, &ParseOpts::default())
        .expect("suspension callable shell parses")
        .into_program();
    let mut parameter_arguments = false;
    if let EntryValue::Function(function) = &entry {
        for param in &function.params {
            mangler_jsast::analysis::binding_names(&param.pat, &mut |id| {
                parameter_arguments |= id.sym == "arguments";
            });
        }
    }
    let callee_bridge = if kind == SuspensionKind::Async
        && matches!(&entry, EntryValue::Function(_))
        && !parameter_arguments
    {
        let current = cfg.fresh_name();
        parse_fn_body_stmts(&format!("function _entry(){{var {current}={descriptor}(arguments,'callee');if({current}&&{descriptor}({current},'value')){{{current}.value={shell};{define}(arguments,'callee',{current});}}}}")).expect("callee identity bridge parses")
    } else {
        Vec::new()
    };
    struct Entry {
        value: Option<EntryValue>,
        finish: String,
        iterator_kind: bool,
        callee_bridge: Vec<Stmt>,
    }
    impl VisitMut for Entry {
        fn visit_mut_expr(&mut self, expression: &mut Expr) {
            if matches!(&*expression,Expr::Member(member) if matches!(&member.prop,MemberProp::Ident(id) if id.sym=="__entry"))
                && matches!(self.value, Some(EntryValue::Arrow(_)))
            {
                let Some(EntryValue::Arrow(arrow)) = self.value.take() else {
                    unreachable!()
                };
                *expression = Expr::Arrow(arrow);
            } else {
                expression.visit_mut_children_with(self);
            }
        }
        fn visit_mut_method_prop(&mut self, method: &mut MethodProp) {
            if matches!(&method.key, PropName::Ident(id) if id.sym == "__entry") {
                let mut function = match self.value.take().expect("single entry shell") {
                    EntryValue::Function(function) => function,
                    EntryValue::Arrow(_) => unreachable!("arrow uses expression replacement"),
                };
                if self.iterator_kind
                    && let Some(Stmt::Return(ret)) = function
                        .body
                        .as_mut()
                        .and_then(|body| body.stmts.last_mut())
                    && let Some(value) = ret.arg.take()
                {
                    ret.arg = Some(Box::new(Expr::Call(CallExpr {
                        span: DUMMY_SP,
                        callee: Callee::Expr(Box::new(Expr::Ident(Ident::new_no_ctxt(
                            self.finish.as_str().into(),
                            DUMMY_SP,
                        )))),
                        args: vec![ExprOrSpread {
                            spread: None,
                            expr: value,
                        }],
                        ..Default::default()
                    })));
                }
                if let Some(body) = &mut function.body {
                    let at = body
                        .stmts
                        .iter()
                        .take_while(|stmt| mangler_jsast::directives::is_directive(stmt))
                        .count();
                    body.stmts
                        .splice(at..at, std::mem::take(&mut self.callee_bridge));
                }
                *method.function = function;
            } else {
                method.visit_mut_children_with(self);
            }
        }
    }
    // Isolate only generated references. The installed entry contains source
    // parameter expressions/capture getters whose own Object bindings must stay.
    struct Generated<'a> {
        display_name: &'a str,
        set: &'a str,
        get: &'a str,
        define: &'a str,
    }
    impl VisitMut for Generated<'_> {
        fn visit_mut_str(&mut self, value: &mut Str) {
            if value.value == *"__mangler_display_name" {
                value.value = self.display_name.into();
                value.raw = None;
            }
        }
        fn visit_mut_span(&mut self, span: &mut swc_core::common::Span) {
            *span = DUMMY_SP;
        }
        fn visit_mut_expr(&mut self, e: &mut Expr) {
            if let Expr::Member(m) = e
                && matches!(&*m.obj,Expr::Ident(id) if id.sym=="Object")
                && let MemberProp::Ident(p) = &m.prop
            {
                let name = match p.sym.as_ref() {
                    "setPrototypeOf" => Some(self.set),
                    "getPrototypeOf" => Some(self.get),
                    "defineProperty" => Some(self.define),
                    _ => None,
                };
                if let Some(name) = name {
                    *e = Expr::Ident(Ident::new_no_ctxt(name.into(), DUMMY_SP));
                    return;
                }
            }
            e.visit_mut_children_with(self);
        }
    }
    program.visit_mut_with(&mut Generated {
        display_name,
        set: &set_proto,
        get: &get_proto,
        define: &define,
    });
    program.visit_mut_with(&mut Entry {
        value: Some(entry),
        finish,
        iterator_kind: kind != SuspensionKind::Async,
        callee_bridge,
    });
    let Program::Script(mut script) = program else {
        unreachable!()
    };
    let Stmt::Expr(mut expression) = script.body.remove(0) else {
        unreachable!()
    };
    if let Expr::Call(call) = expression.expr.as_mut() {
        call.span = mangler_jsast::span::runtime_span();
        if let Callee::Expr(callee) = &mut call.callee {
            let mut callee = callee.as_mut();
            while let Expr::Paren(paren) = callee {
                callee = paren.expr.as_mut();
            }
            if let Expr::Arrow(arrow) = callee
                && let ArrowFunctionBody::FunctionBody(body) = arrow.body.as_mut()
            {
                // The entire source body is an installed protected entry thunk.
                // Native async/generator templates below are protocol support.
                body.span = mangler_jsast::span::generated_factory_span();
            }
        }
    }

    (expression.expr, alias_program.body)
}
