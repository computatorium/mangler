use mangler_core::Rng;
use mangler_testkit::eval::{CaptureMode, assert_behaviorally_equal_with};
use mangler_vm::diversity::VmDiversity;
use mangler_vm::table::{TableBuilder, VmNames};
use swc_core::common::sync::Lrc;
use swc_core::common::{FileName, SourceMap};
use swc_core::ecma::ast::*;
use swc_core::ecma::codegen::{Config as CodegenConfig, Emitter, text_writer::JsWriter};
use swc_core::ecma::parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};

/// Parse a function-expression source into `(name, params, body)`. `name` is the
/// function's own identifier (a named function expression like `function fac(){…}`),
/// or `None` for an anonymous one.
fn parse_fn_named(src: &str) -> (Option<String>, Vec<Param>, FunctionBody) {
    let cm: Lrc<SourceMap> = Default::default();
    let wrapped = format!("var __f = ({src});");
    let fm = cm.new_source_file(Lrc::new(FileName::Custom("t.js".into())), wrapped);
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::EsNext,
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let program = parser.parse_program().unwrap();
    let stmt = match program {
        Program::Script(s) => s.body.into_iter().next().unwrap(),
        Program::Module(m) => match m.body.into_iter().next().unwrap() {
            ModuleItem::Stmt(s) => s,
            _ => panic!("stmt"),
        },
    };
    let init = match stmt {
        Stmt::Decl(Decl::Var(v)) => *v.decls.into_iter().next().unwrap().init.unwrap(),
        _ => panic!("var"),
    };
    let fe = match init {
        Expr::Paren(p) => match *p.expr {
            Expr::Fn(fe) => fe,
            _ => panic!("fn"),
        },
        Expr::Fn(fe) => fe,
        _ => panic!("fn"),
    };
    let name = fe.ident.map(|i| i.sym.to_string());
    (name, fe.function.params, fe.function.body.unwrap())
}

/// Render a slice of statements to minified JS source.
fn render(stmts: Vec<Stmt>) -> String {
    let cm: Lrc<SourceMap> = Default::default();
    let mut buf = Vec::new();
    {
        let wr = JsWriter::new(cm.clone(), "", &mut buf, None);
        let mut emitter = Emitter {
            cfg: CodegenConfig::default().with_minify(true),
            cm,
            comments: None,
            wr,
        };
        let program = Program::Script(Script {
            span: swc_core::common::DUMMY_SP,
            body: stmts,
            shebang: None,
        });
        emitter.emit_program(&program).unwrap();
    }
    String::from_utf8(buf).unwrap()
}

fn check(source: &str, expected_invocation: bool) {
    let wrapped = format!("function(){{{source}}}");
    let (_, _, body) = parse_fn_named(&wrapped);
    let eval = mangler_vm::eval::compile_eval_body_with_context(
        body.stmts,
        false,
        mangler_vm::eval::SourceContext::Function,
    )
    .expect("eval source compiles");
    let div = VmDiversity::draw(&mut Rng::for_pass(42, "vm"));
    let mut table = TableBuilder::with_diversity(div);
    let chunk = table.add_strict(eval.compiled, eval.strict);
    let names = VmNames {
        lean_interp: "Vv".into(),
        eh_interp: "Dd".into(),
        lean_interp_strict: "Vs".into(),
        eh_interp_strict: "Ds".into(),
        table: "Tt".into(),
        rc: "rcc".into(),
        sy: "syy".into(),
    };
    let table = table.finish(&names).expect("eval table");
    let declarations = if eval.declared_vars.is_empty() {
        String::new()
    } else {
        format!("var {};", eval.declared_vars.join(","))
    };
    let captures = chunk
        .captures
        .iter()
        .map(|name| format!("{{get:()=>{name},set:value=>{name}=value,type:()=>typeof {name}}}"))
        .collect::<Vec<_>>()
        .join(",");
    let interpreter = names.interp_for(chunk.needs_eh, eval.strict);
    let invoke = if expected_invocation {
        "value=[value(),value()];"
    } else {
        ""
    };
    let source_literal = render(vec![Stmt::Expr(ExprStmt {
        span: swc_core::common::DUMMY_SP,
        expr: Box::new(Expr::Lit(Lit::Str(Str {
            span: swc_core::common::DUMMY_SP,
            value: source.into(),
            raw: None,
        }))),
    })]);
    let source_literal = source_literal.trim_end_matches(';');
    let original = format!(
        "function caller(){{var value=eval({source_literal});{invoke}return value}}globalThis.__out=JSON.stringify(caller(10));"
    );
    let transformed = format!(
        "{};function caller(){{{declarations}var value={interpreter}(Tt[{}][0],Tt[{}][1],[],[{captures}],{},0,this,true);{invoke}return value}}globalThis.__out=JSON.stringify(caller(10));",
        render(table.prologue),
        chunk.index,
        chunk.index,
        chunk.cap_start
    );
    assert_behaviorally_equal_with(&original, &transformed, &CaptureMode::sink());
    if let Some(node) = mangler_testkit::cross_engine::node_path() {
        let values = mangler_testkit::cross_engine::evaluate_many(
            &mangler_testkit::cross_engine::Engine::Node(node),
            &[&original, &transformed],
        )
        .expect("V8 evaluates eval completion");
        assert_eq!(values[0], values[1], "eval completion: {source}");
    }
}

#[test]
fn completion_values_preserve_empty_and_branch_results() {
    for source in [
        "",
        "1;{}",
        "1;var x",
        "1;if(false)2",
        "1;while(false)2",
        "1;for(;false;)2",
        "1;label:{break label}",
        "var i=0;while(i++<3){i}",
        "for(var i=0;i<3;i++){i}",
        "switch(1){case 1:3;break;default:4}",
        "var i=0;outer:while(i++<2){if(i===1)continue outer;i}",
    ] {
        check(source, false);
    }
}

#[test]
fn eval_completion_survives_finally_and_abrupt_control_flow() {
    for source in [
        "try{2}finally{3}",
        "1;try{}finally{}",
        "try{throw 0}catch(e){e+2}finally{9}",
        "label:try{2}finally{3;break label}",
        "var i=0;while(i++<2){try{i}finally{10}}",
    ] {
        check(source, false);
    }
}

#[test]
fn eval_var_references_and_lexical_closures() {
    check("var n=1;var f=()=>++n;f();n", false);
    check("let n=1;()=>++n", true);
    check("'use strict';var n=1;()=>++n", true);
    check("arguments[0]", false);
    check("with({__mangler_eval_completion_0:99}){2}", false);
}

fn js_string(source: &str) -> String {
    render(vec![Stmt::Expr(ExprStmt {
        span: swc_core::common::DUMMY_SP,
        expr: Box::new(Expr::Lit(Lit::Str(Str {
            span: swc_core::common::DUMMY_SP,
            value: source.into(),
            raw: None,
        }))),
    })])
    .trim_end_matches(';')
    .to_string()
}

// The fixture provider isolates environment execution from parser transport: each
// requested source is compiled by the same Rust eval compiler into this table.
// The WASM package exercises the production source compiler transport separately.
fn check_environment(function: &str, eval_sources: &[&str]) {
    check_environment_expected(function, eval_sources, None);
}
fn check_environment_expected(function: &str, eval_sources: &[&str], expected: Option<&str>) {
    let (_, params, body) = parse_fn_named(function);
    let strict = body.stmts.iter().take_while(|stmt| matches!(stmt, Stmt::Expr(e) if matches!(&*e.expr, Expr::Lit(Lit::Str(_))))).any(|stmt| matches!(stmt, Stmt::Expr(e) if matches!(&*e.expr, Expr::Lit(Lit::Str(value)) if value.value == *"use strict")));
    let compiled = mangler_vm::compile::compile_body_with_opts(
        &params,
        &body,
        mangler_vm::compile::CompileOptions {
            live_captures: true,
            strict,
            ..Default::default()
        },
    )
    .unwrap_or_else(|reason| panic!("caller compiles {function}: {reason}"));
    let mut table =
        TableBuilder::with_diversity(VmDiversity::draw(&mut Rng::for_pass(74, "eval-env")));
    let caller = table.add_strict(compiled, strict);
    let mut fixtures = Vec::new();
    for source in eval_sources {
        for strict in [false, true] {
            let (_, _, body) = parse_fn_named(&format!("function(){{{source}}}"));
            let evaluated = mangler_vm::eval::compile_eval_body_with_context(
                body.stmts,
                strict,
                mangler_vm::eval::SourceContext::Function,
            )
            .expect("eval fixture compiles");
            let chunk = table.add_strict(evaluated.compiled, evaluated.strict);
            let vars = evaluated
                .declared_vars
                .iter()
                .map(|name| js_string(name))
                .collect::<Vec<_>>()
                .join(",");
            let caps = chunk
                .captures
                .iter()
                .map(|name| js_string(name))
                .collect::<Vec<_>>()
                .join(",");
            fixtures.push(format!("if(source==={}&&options.strict==={strict})return{{declaredVars:[{vars}],program:{{captures:[{caps}],capStart:{},pcount:{}}},row:Tt[{}]}};",js_string(source),chunk.cap_start,chunk.pcount,chunk.index));
        }
    }
    let names = VmNames {
        lean_interp: "Vv".into(),
        eh_interp: "Dd".into(),
        lean_interp_strict: "Vs".into(),
        eh_interp_strict: "Ds".into(),
        table: "Tt".into(),
        rc: "rcc".into(),
        sy: "syy".into(),
    };
    let prologue = render(table.finish(&names).expect("environment table").prologue);
    let caps = caller
        .captures
        .iter()
        .map(|name| {
            if strict && matches!(name.as_str(), "eval" | "arguments") {
                format!("{{get:()=>{name},type:()=>typeof {name}}}")
            } else {
                format!("{{get:()=>{name},set:value=>{name}=value,type:()=>typeof {name}}}")
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let call = format!(
        "{}(Tt[{}][0],Tt[{}][1],arguments,[{caps}],{},{},this,true)",
        names.interp_for(caller.needs_eh, strict),
        caller.index,
        caller.index,
        caller.cap_start,
        caller.pcount
    );
    let directive = if strict { "'use strict';" } else { "" };
    let factory_call = format!(
        "{}(Tt[{}][0],Tt[{}][1],args,[{caps}],{},{},receiver,true,refs)",
        names.interp_for(caller.needs_eh, strict),
        caller.index,
        caller.index,
        caller.cap_start,
        caller.pcount
    );
    let shell = format!(
        "Tt[{}][5]?Tt[{}][5](function(receiver,args,refs){{return {factory_call}}}):function(){{{directive}return {call}}}",
        caller.index, caller.index
    );
    let original = format!("globalThis.__out=JSON.stringify(({function})(10));");
    let transformed = format!(
        "{prologue};Tt.evalIntrinsic=eval;Tt.eval=function(source,options){{{}throw Error('Unknown eval fixture '+source)}};var caller={shell};globalThis.__out=JSON.stringify(caller(10));",
        fixtures.join("")
    );
    let baseline = expected
        .map(|value| format!("globalThis.__out=JSON.stringify({value});"))
        .unwrap_or_else(|| original.clone());
    assert_behaviorally_equal_with(&baseline, &transformed, &CaptureMode::sink());
    if let Some(node) = mangler_testkit::cross_engine::node_path() {
        let values = mangler_testkit::cross_engine::evaluate_many(
            &mangler_testkit::cross_engine::Engine::Node(node),
            &[&original, &transformed],
        )
        .expect("V8 evaluates environment program");
        assert_eq!(values[0], values[1], "eval environment: {function}");
    }
}

#[test]
fn direct_eval_preserves_live_variable_and_lexical_environments() {
    check_environment(
        "function(a){let hidden=3;return eval('hidden+a')}",
        &["hidden+a"],
    );
    check_environment(
        "function(){var before=()=>late;eval('var late=7');return before()}",
        &["var late=7"],
    );
    check_environment(
        "function(){let hidden=8;return (()=>eval('hidden'))()}",
        &["hidden"],
    );
    check_environment("function(){var n=2;eval('n=5');return n}", &["n=5"]);
    check_environment("function(){let n=2;eval('n=5');return n}", &["n=5"]);
    check_environment(
        "function(){let n=2;try{eval('var n=5')}catch(e){return e.name}}",
        &["var n=5"],
    );
    check_environment(
        "function(){var n=2;return [eval(4),eval(),eval('n')]}",
        &["n"],
    );
    check_environment("function(){var eval=(s)=>s+'!';return eval('test')}", &[]);
    check_environment(
        "function(){var named=1;return (function named(){return eval('typeof named')})()}",
        &["typeof named"],
    );
}

#[test]
fn eval_closures_keep_iteration_and_parameter_environment_identity() {
    check_environment(
        "function(){var fs=[];for(let i=0;i<3;i++)fs.push(()=>eval('i'));return fs.map(f=>f())}",
        &["i"],
    );
    check_environment("function(a=3,f=()=>eval('a')){var a=7;return f()}", &["a"]);
    check_environment(
        "function(){eval('var n=1;var f=()=>++n');return [f(),f(),n]}",
        &["var n=1;var f=()=>++n"],
    );
    check_environment(
        "function(){var n=1;return eval('(()=>{var n=3;return n})()')+n}",
        &["(()=>{var n=3;return n})()"],
    );
    check_environment(
        "function(){'use strict';var n=2;return [eval('var n=5;n'),n]}",
        &["var n=5;n"],
    );
    check_environment("function(){var n=2;with({n:4})return eval('n')}", &["n"]);
}

#[test]
fn eval_deletability_and_lexical_failures_follow_binding_identity() {
    check_environment(
        "function(){eval('var late=3');var before=()=>typeof late;var first=before();var removed=eval('delete late');return [first,removed,before()]}",
        &["var late=3", "delete late"],
    );
    check_environment(
        "function(){var value=1;return [eval('delete value'),value]}",
        &["delete value"],
    );
    check_environment(
        "function(){const value=1;try{eval('value=2')}catch(e){return [e.name,value]}}",
        &["value=2"],
    );
    check_environment(
        "function(){try{return eval('typeof value')}catch(e){return e.name}let value=1}",
        &["typeof value"],
    );
}

#[test]
fn eval_declared_vars_respect_annex_b_lexical_boundaries() {
    for (source, expected) in [
        ("var a;var a;{var b}", vec!["a", "b"]),
        ("let f;{function f(){}}", vec![]),
        ("for(let f=0;f<1;f++){function f(){}}", vec![]),
        ("{function f(){}}", vec!["f"]),
        ("try{}catch({f}){{function f(){}}}", vec![]),
        ("try{}catch([f]){{function f(){}}}", vec![]),
        ("try{}catch(f){{function f(){}}}", vec!["f"]),
        ("function f(){var inside}", vec!["f"]),
    ] {
        let (_, _, body) = parse_fn_named(&format!("function(){{{source}}}"));
        let compiled = mangler_vm::eval::compile_eval_body_with_context(
            body.stmts,
            false,
            mangler_vm::eval::SourceContext::Function,
        )
        .unwrap();
        assert_eq!(compiled.declared_vars, expected, "{source}");
    }
}

#[test]
fn eval_block_functions_keep_distinct_mutable_bindings() {
    check_environment(
        "function(){var initial,current,outer;eval('{function f(){initial=f;f=123;current=f;return 7}}outer=f;f()');return [initial(),current,outer()]}",
        &["{function f(){initial=f;f=123;current=f;return 7}}outer=f;f()"],
    );
    for statement in [
        "try{throw {}}catch({f}){{function f(){}}}",
        "try{throw []}catch([f]){{function f(){}}}",
    ] {
        let code = format!("{statement};try{{f;'bound'}}catch(e){{e.name}}");
        check_environment(&format!("function(){{return eval({code:?})}}"), &[&code]);
    }
}

#[test]
fn sloppy_eval_allows_simple_catch_parameter_var_but_rejects_patterns() {
    check_environment(
        "function(){try{throw 1}catch(x){eval('var x=2')}return typeof x}",
        &["var x=2"],
    );
    check_environment(
        "function(){try{throw 1}catch(x){return eval('var x=2;x')}}",
        &["var x=2;x"],
    );
    check_environment(
        "function(){try{throw {x:1}}catch({x}){try{return eval('var x=2;x')}catch(e){return e.name}}}",
        &["var x=2;x"],
    );
}

#[test]
fn parameter_eval_uses_distinct_parameter_and_body_variable_records() {
    check_environment("function(a=1){eval('var a=3');return a}", &["var a=3"]);
    check_environment(
        "function(a=1){var get=()=>a;eval('var a=3');return get()}",
        &["var a=3"],
    );
    check_environment(
        "function(){try{return (function(a=eval('var a=1')){})()}catch(e){return e.name}}",
        &["var a=1"],
    );
    check_environment(
        "function(){return (function(a=eval('var x=3'),b=()=>x){var x=5;return [b(),x]})()}",
        &["var x=3"],
    );
}

#[test]
fn eval_strict_writes_respect_immutable_function_expression_binding() {
    check_environment(
        "function(){return (function named(){eval('named=1');return typeof named})()}",
        &["named=1"],
    );
    // QuickJS does not enforce strict eval writes to an immutable named
    // function binding; V8 and the transformed VM must throw TypeError.
    check_environment_expected(
        "function(){return (function named(){try{eval('\"use strict\";named=1')}catch(e){return e.name}})()}",
        &["\"use strict\";named=1"],
        Some("\"TypeError\""),
    );
}

#[test]
fn eval_declaration_preflight_distinguishes_functions_from_annex_b_vars() {
    let (_, _, body) = parse_fn_named(
        "function(){var x;function first(){} {function block(){}} function last(){}} ",
    );
    let compiled = mangler_vm::eval::compile_eval_body_with_context(
        body.stmts,
        false,
        mangler_vm::eval::SourceContext::Function,
    )
    .unwrap();
    assert_eq!(compiled.declared_functions, ["first", "last"]);
    assert_eq!(compiled.declared_vars, ["x", "first", "block", "last"]);
}

#[test]
fn lexical_entries_preserve_source_grammar_without_creating_function_var_records() {
    use mangler_vm::{
        compile::{CompileOptions, compile_body_with_opts},
        eval::{EnvironmentScope, SourceContext},
    };
    let (_, params, body) = parse_fn_named("function(){eval('var added=1')}");
    for context in [
        SourceContext::Function,
        SourceContext::Script,
        SourceContext::Module,
    ] {
        let compiled = compile_body_with_opts(
            &params,
            &body,
            CompileOptions {
                lexical_entry: true,
                source_context: context,
                live_captures: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !compiled
                .code
                .iter()
                .any(|op| matches!(op, mangler_vm::isa::Instr::BeginVarEnvironment(_)))
        );
        assert!(compiled.consts.iter().any(|constant|matches!(constant,mangler_vm::chunk::Const::Environment(metadata) if metadata.source_context == context && metadata.scopes.contains(&EnvironmentScope::Variables))));
    }
}

#[test]
fn class_eval_capsule_is_captured_without_becoming_a_source_binding() {
    use mangler_vm::compile::CompileOptions;
    use mangler_vm::eval::{EnvironmentScope, EvalClassContext, EvalClassContexts};
    use swc_core::ecma::visit::{Visit, VisitWith};
    struct EvalPosition(u32);
    impl Visit for EvalPosition {
        fn visit_call_expr(&mut self, call: &CallExpr) {
            if matches!(&call.callee, Callee::Expr(expr) if matches!(&**expr,Expr::Ident(id) if id.sym=="eval"))
            {
                self.0 = call.span.lo.0;
            }
            call.visit_children_with(self);
        }
    }
    let (_, params, body) = parse_fn_named("function(capsule){return ()=>eval('this.#secret')}");
    let mut position = EvalPosition(0);
    body.visit_with(&mut position);
    let contexts = EvalClassContexts::from([(
        position.0,
        EvalClassContext {
            capsule_binding: "capsule".into(),
            private_names: vec!["secret".into()],
            allow_super_property: true,
            allow_super_call: false,
            arguments_forbidden: false,
        },
    )]);
    let compiled = mangler_vm::compile::compile_body_with_opts(
        &params,
        &body,
        CompileOptions {
            eval_class_contexts: Some(&contexts),
            ..Default::default()
        },
    )
    .unwrap();
    let child = &compiled.children[0].compiled;
    assert!(child.captures.iter().any(|name| name == "capsule"));
    let mut found = false;
    for chunk in [&compiled, child] {
        for constant in &chunk.consts {
            let mangler_vm::Const::Environment(environment) = constant else {
                continue;
            };
            if let Some(context) = &environment.class_context {
                assert_eq!(context.private_names, ["secret"]);
                assert!(context.allow_super_property);
                assert!(!context.allow_super_call);
                found = true;
            }
            for scope in &environment.scopes {
                if let EnvironmentScope::Bindings(bindings) = scope {
                    for binding in bindings {
                        assert_ne!(
                            chunk.consts[binding.name_const as usize],
                            mangler_vm::Const::Str("capsule".into())
                        );
                    }
                }
            }
        }
    }
    assert!(
        found,
        "eval snapshot must retain its class grammar and native capsule"
    );
}

#[test]
fn generated_class_capsule_initializes_once_before_parameter_eval() {
    use mangler_vm::compile::CompileOptions;
    use mangler_vm::eval::{EvalClassContext, EvalClassContexts};
    use mangler_vm::{Const, Instr};
    use swc_core::ecma::visit::{Visit, VisitWith};
    struct Calls(Vec<u32>);
    impl Visit for Calls {
        fn visit_call_expr(&mut self, call: &CallExpr) {
            self.0.push(call.span.lo.0);
            call.visit_children_with(self);
        }
    }
    let (_, params, mut body) = parse_fn_named("function(a=eval('1')){const capsule={};return a}");
    let Stmt::Decl(Decl::Var(declaration)) = &mut body.stmts[0] else {
        unreachable!()
    };
    declaration.span = swc_core::common::DUMMY_SP;
    let mut calls = Calls(Vec::new());
    params.visit_with(&mut calls);
    let contexts: EvalClassContexts = calls
        .0
        .into_iter()
        .map(|position| {
            (
                position,
                EvalClassContext {
                    capsule_binding: "capsule".into(),
                    private_names: vec!["secret".into()],
                    allow_super_property: true,
                    allow_super_call: false,
                    arguments_forbidden: false,
                },
            )
        })
        .collect();
    let compiled = mangler_vm::compile::compile_body_with_opts(
        &params,
        &body,
        CompileOptions {
            eval_class_contexts: Some(&contexts),
            ..Default::default()
        },
    )
    .unwrap();
    let slot = compiled
        .consts
        .iter()
        .find_map(|constant| match constant {
            Const::Environment(metadata) => metadata
                .class_context
                .as_ref()
                .map(|context| context.capsule_slot),
            _ => None,
        })
        .expect("parameter eval retains capsule metadata");
    let initialized: Vec<_> = compiled
        .code
        .iter()
        .enumerate()
        .filter(|(_, instruction)| **instruction == Instr::InitLocal(slot))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        initialized.len(),
        1,
        "body must not initialize the capsule again"
    );
    let eval = compiled
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instr::EvalCall(_)))
        .unwrap();
    assert!(
        initialized[0] < eval,
        "capsule must precede parameter defaults"
    );
    assert_eq!(
        compiled
            .code
            .iter()
            .filter(|instruction| **instruction == Instr::BeginLexical(slot * 2 + 1))
            .count(),
        1,
        "body must not reset the capsule descriptor captured by defaults"
    );
}
