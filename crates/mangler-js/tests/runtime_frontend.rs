use mangler_core::Language;
use mangler_js::runtime_frontend::{
    PreparedFunction, intrinsic_snapshot_factory, prepare_constructor, prepare_function,
};
use mangler_jsast::{Js, ParseOpts};
use mangler_testkit::cross_engine::{Engine, chrome_path, evaluate_many, node_path};
use swc_core::ecma::ast::*;

#[test]
fn interpreter_assembly_keeps_pooled_helper_captures_in_scope() {
    use mangler_vm::{Instr, InterpreterSpec, VmDiversity};
    let diversity = VmDiversity::baseline(0);
    let specs = [("run", false), ("runStrict", true)].map(|(name, is_strict)| InterpreterSpec {
        name,
        is_strict,
        table: "table",
        rc: "construct",
        sy: "iteratorKey",
        needs_eh: true,
        diversity: &diversity,
        usage: None,
    });
    for batch in [false, true] {
        let mut ast = Js
            .parse(
                "var table=[],construct=Reflect.construct,iteratorKey=Symbol.iterator;",
                &ParseOpts::default(),
            )
            .unwrap();
        let Program::Script(script) = ast.program_mut() else {
            unreachable!()
        };
        script.body.extend(if batch {
            mangler_vm::emit_interpreters(&specs).unwrap()
        } else {
            specs
                .iter()
                .map(|spec| mangler_vm::emit_interpreter(spec).unwrap())
                .collect()
        });
        mangler_js::runtime_frontend::isolate_generated_runtime(
            ast.program_mut(),
            "table",
            "intrinsics",
        )
        .unwrap();
        let support = Js.print(&ast);
        let source = format!(
            "var intrinsics=({})();{support}var results=[];for(var field of ['value','get']){{Object.prototype[field]=field==='value'?19:function(){{return 19}};try{{for(var fn of [run,runStrict]){{var code=[{},0,{}],constants=[];code.d=constants.d=1;results.push(fn(code,constants,[3],[],1,1,undefined,false));}}}}finally{{delete Object.prototype[field]}}}}globalThis.__out=JSON.stringify(results);",
            intrinsic_snapshot_factory(),
            Instr::LoadLocal(0).discriminant(),
            Instr::Ret.discriminant(),
        );
        mangler_testkit::assert_behaviorally_equal("globalThis.__out='[3,3,3,3]'", &source);
    }
}

fn artifact(prepared: &PreparedFunction) -> String {
    artifact_with_intrinsics(prepared, &format!("({})()", intrinsic_snapshot_factory()))
}

fn artifact_with_intrinsics(prepared: &PreparedFunction, intrinsics: &str) -> String {
    let mut ast = Js.parse("var callable=0;", &ParseOpts::default()).unwrap();
    let Program::Script(script) = ast.program_mut() else {
        unreachable!()
    };
    let Stmt::Decl(Decl::Var(declaration)) = &mut script.body[0] else {
        unreachable!()
    };
    declaration.decls[0].init = Some(prepared.initializer.clone());
    let imports = prepared
        .support
        .names
        .iter()
        .map(|name| format!("var {name}=__runtime_support.{name};"))
        .collect::<String>();
    format!(
        "var __runtime_support=({})({intrinsics});{imports}{}",
        prepared.support.factory,
        Js.print(&ast)
    )
}

#[test]
fn callable_metadata_and_namespace_are_explicit() {
    let prepared =
        prepare_function("function pay(x,{y},z=3,...rest){return x+y+z+rest.length}").unwrap();
    assert_eq!(prepared.name.as_deref(), Some("pay"));
    assert_eq!(prepared.length, 2);
    assert!(!prepared.support.tables.is_empty());
    assert!(
        prepared
            .support
            .tables
            .iter()
            .all(|table| prepared.support.names.contains(table))
    );
    assert!(!prepared.support.factory.contains("new WebAssembly.Module"));
    let constructor =
        prepare_constructor("function anonymous(x){return typeof anonymous}").unwrap();
    assert_eq!(constructor.name, None);
    assert_eq!(constructor.length, 1);
    assert!(prepare_function("(function(){},globalThis.leak=1)").is_err());
}

#[test]
fn callable_frontend_is_deterministic() {
    let source = "function pay(x){class Account{#n=x;charge(){return this.#n*78329+34691}}return new Account().charge()}";
    let left = prepare_function(source).unwrap();
    let right = prepare_function(source).unwrap();
    assert_eq!(left.support.factory, right.support.factory);
    assert_eq!(artifact(&left), artifact(&right));
    assert!(!left.support.factory.contains("this.#n*78329+34691"));
}

#[test]
fn shared_frontend_preserves_classes_suspension_captures_and_resources_in_real_engines() {
    let fixtures = [
        (
            "function pay(n){return n*rate}",
            "globalThis.__out=callable(4)",
        ),
        (
            "function pay(){return __mangler_runtime_input}",
            "globalThis.__out=callable()",
        ),
        (
            "function pay(n){class A{#n=n;charge(){return this.#n+1}}return new A().charge()}",
            "globalThis.__out=callable(4)",
        ),
        (
            "function* pay(n){try{yield n;return n+1}finally{globalThis.closed=true}}",
            "let g=callable(4);globalThis.__out=JSON.stringify([g.next(),g.return(9),closed])",
        ),
        (
            "async function pay(a){a=2;return [await a,arguments[0],arguments.callee===pay]}",
            "callable(1).then(v=>globalThis.__out=JSON.stringify(v))",
        ),
        (
            "function pay(){let events=[];{using resource={[Symbol.dispose](){events.push('dispose')}};events.push('body')}return events}",
            "globalThis.__out=JSON.stringify(callable())",
        ),
        (
            "async function pay(){let events=[];{await using resource={[Symbol.asyncDispose]:async function(){events.push('dispose')}};events.push('body')}return events}",
            "callable().then(v=>globalThis.__out=JSON.stringify(v))",
        ),
    ];
    let mut programs = Vec::new();
    for (source, invocation) in fixtures {
        let prefix = "var rate=3,__mangler_runtime_input=7;";
        programs.push(format!("{prefix}var callable=({source});{invocation}"));
        let prepared = prepare_function(source).unwrap_or_else(|error| panic!("{source}: {error}"));
        programs.push(format!("{prefix}{}{invocation}", artifact(&prepared)));
    }
    for engine in [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    {
        let sources: Vec<_> = programs.iter().map(String::as_str).collect();
        let results = evaluate_many(&engine, &sources).unwrap();
        for index in 0..fixtures.len() {
            assert_eq!(
                results[index * 2]["outcome"][0],
                "value",
                "invalid native fixture {}",
                fixtures[index].0
            );
            assert_eq!(
                results[index * 2],
                results[index * 2 + 1],
                "{} changed {}",
                engine.name(),
                fixtures[index].0
            );
        }
    }
}

#[test]
fn eval_frontend_preserves_completion_declarations_and_source_context() {
    use mangler_js::runtime_frontend::prepare_eval;
    use mangler_vm::eval::{SourceContext, compile_prepared_eval_body, prepare_eval_body};
    let fixtures = [
        "var total=4; total+3",
        "var total=4; function charge(n){return n+total} charge(3)",
        "class Account{#n=4;charge(){return this.#n+3}}new Account().charge()",
        "function* values(){yield 3;return 4}var g=values();g.next().value",
        "{using resource={[Symbol.dispose](){}}; 7}",
    ];
    for source in fixtures {
        for strict in [false, true] {
            for context in [
                SourceContext::Function,
                SourceContext::Script,
                SourceContext::Module,
            ] {
                let ast = Js.parse(source, &ParseOpts::default()).unwrap();
                let Program::Script(script) = ast.program() else {
                    unreachable!()
                };
                let prepared = prepare_eval_body(script.body.clone(), strict, context);
                let variables = prepared.declared_vars.clone();
                let functions = prepared.declared_functions.clone();
                let internals = prepared.internal_bindings.clone();
                let lowered =
                    prepare_eval(prepared).unwrap_or_else(|error| panic!("{source}: {error}"));
                assert_eq!(lowered.body.source_context, context);
                assert_eq!(lowered.body.strict, strict);
                assert_eq!(lowered.body.declared_vars, variables);
                assert_eq!(lowered.body.declared_functions, functions);
                assert!(internals.is_subset(&lowered.body.internal_bindings));
                assert!(!lowered.support.tables.is_empty());
                compile_prepared_eval_body(lowered.body, Default::default())
                    .unwrap_or_else(|error| panic!("{source}: {error}"));
            }
        }
    }
}

fn compiled_artifact(prepared: &PreparedFunction) -> String {
    use mangler_vm::{CompileOptions, TableBuilder, VmNames, compile_body_with_opts};
    use swc_core::common::DUMMY_SP;
    let body = FunctionBody {
        span: DUMMY_SP,
        stmts: vec![Stmt::Return(ReturnStmt {
            span: DUMMY_SP,
            arg: Some(prepared.initializer.clone()),
        })],
    };
    let hidden = prepared.support.names.iter().cloned().collect();
    let compiled = compile_body_with_opts(
        &[],
        &body,
        CompileOptions {
            live_captures: true,
            lexical_entry: true,
            lexical_arguments: true,
            internal_bindings: Some(&hidden),
            ..Default::default()
        },
    )
    .unwrap();
    let names = VmNames {
        lean_interp: "__entry_vm".into(),
        eh_interp: "__entry_eh".into(),
        lean_interp_strict: "__entry_strict".into(),
        eh_interp_strict: "__entry_strict_eh".into(),
        table: "__entry_table".into(),
        rc: "__entry_construct".into(),
        sy: "__entry_iterator".into(),
    };
    let mut table = TableBuilder::new(&mut mangler_core::Rng::for_pass(0, "test_entry"));
    let chunk = table.add(compiled);
    let table = table.finish(&names).unwrap();
    let mut ast = Js.parse("", &ParseOpts::default()).unwrap();
    let Program::Script(script) = ast.program_mut() else {
        unreachable!()
    };
    script.body = table.prologue;
    let captures = chunk.captures.iter().map(|name|
        format!("{{__proto__:null,get:()=>{name},set:__value=>{name}=__value,type:()=>typeof {name}}}")
    ).collect::<Vec<_>>().join(",");
    let imports = prepared
        .support
        .names
        .iter()
        .map(|name| format!("var {name}=__runtime_support.{name};"))
        .collect::<String>();
    format!(
        "var __runtime_support=({})(({})());{imports}{}var callable={}({}[{}][0],{}[{}][1],[],[{}],{},{},this,true,[]);",
        prepared.support.factory,
        intrinsic_snapshot_factory(),
        Js.print(&ast),
        names.interp_for(chunk.needs_eh, chunk.is_strict),
        names.table,
        chunk.index,
        names.table,
        chunk.index,
        captures,
        chunk.cap_start,
        chunk.pcount
    )
}

#[test]
fn all_constructor_kinds_execute_the_final_vm_entry_in_real_engines() {
    let fixtures = [
        (
            "function anonymous(n){class A{#n=n;read(){return this.#n+1}}return new A().read()}",
            "globalThis.__out=callable(4)",
        ),
        (
            "async function anonymous(n){return await n+1}",
            "callable(4).then(v=>globalThis.__out=v)",
        ),
        (
            "function* anonymous(n){yield n;return n+1}",
            "let g=callable(4);globalThis.__out=JSON.stringify([g.next(),g.next()])",
        ),
        (
            "async function* anonymous(n){yield await n;return n+1}",
            "(async()=>{let g=callable(4);globalThis.__out=JSON.stringify([await g.next(),await g.next()])})()",
        ),
    ];
    let mut programs = Vec::new();
    for (source, invocation) in fixtures {
        programs.push(format!("var callable=({source});{invocation}"));
        let prepared = prepare_constructor(source).unwrap();
        programs.push(format!("{}{invocation}", compiled_artifact(&prepared)));
    }
    for engine in [
        node_path().map(Engine::Node),
        chrome_path().map(Engine::Chrome),
    ]
    .into_iter()
    .flatten()
    {
        let sources: Vec<_> = programs.iter().map(String::as_str).collect();
        let results = evaluate_many(&engine, &sources).unwrap();
        for (index, fixture) in fixtures.iter().enumerate() {
            assert_eq!(results[index * 2]["outcome"][0], "value");
            assert_eq!(
                results[index * 2],
                results[index * 2 + 1],
                "{}: {}",
                engine.name(),
                fixture.0
            );
        }
    }
}

#[test]
fn runtime_support_uses_startup_intrinsics_and_source_globals_stay_live() {
    let source = "function pay(){class Box{#n=4;read(){return this.#n}}function* values(){yield new Box().read()}return values().next().value+Object.marker+Reflect.marker}";
    let prepared = prepare_function(source).unwrap();
    let setup = format!(
        "const snapshot=({})();const originalObject=Object,originalReflect=Reflect;globalThis.Object={{marker:5}};globalThis.Reflect={{marker:7}};",
        intrinsic_snapshot_factory()
    );
    let native = format!(
        "{setup}try{{const callable=({source});globalThis.__out=callable()}}finally{{globalThis.Object=originalObject;globalThis.Reflect=originalReflect}}"
    );
    let protected = format!(
        "{setup}try{{{}globalThis.__out=callable()}}finally{{globalThis.Object=originalObject;globalThis.Reflect=originalReflect}}",
        artifact_with_intrinsics(&prepared, "snapshot")
    );
    let binding = format!(
        r#"const snapshot=({})();
const savedCall=Function.prototype.call,savedApply=Function.prototype.apply,savedBind=Function.prototype.bind,savedReflectApply=Reflect.apply;
try{{
 Function.prototype.call=Function.prototype.apply=Function.prototype.bind=Reflect.apply=function(){{throw 'late lookup'}};
 const call=snapshot(['Object','prototype','hasOwnProperty','call'],true);
 const apply=snapshot(['Array','prototype','slice','apply'],true);
 const bind=snapshot(['Array','prototype','join','bind'],true);
 const join=bind([1,2],'-');
 Object.prototype.value=9;
 const accessor=snapshot(['Object','prototype','__proto__']);
 globalThis.__out=snapshot(['Function','prototype','call'])===savedCall&&snapshot(['Function','prototype','apply'])===savedApply&&snapshot(['Function','prototype','bind'])===savedBind&&accessor===null&&call({{a:1}},'a')&&apply([1,2,3],[1]).length===2&&join()==='1-2';
}}finally{{delete Object.prototype.value;Function.prototype.call=savedCall;Function.prototype.apply=savedApply;Function.prototype.bind=savedBind;Reflect.apply=savedReflectApply}}"#,
        intrinsic_snapshot_factory()
    );
    let mut engines = vec![Engine::Node(
        node_path().expect("Node is required for runtime support isolation"),
    )];
    if let Some(chrome) = chrome_path() {
        engines.push(Engine::Chrome(chrome));
    }
    for engine in engines {
        let results = evaluate_many(
            &engine,
            &[&native, &protected, &binding, "globalThis.__out=true"],
        )
        .unwrap();
        assert_eq!(
            results[0]["outcome"][0],
            "value",
            "{} native fixture: {}",
            engine.name(),
            results[0]
        );
        assert_eq!(
            results[0],
            results[1],
            "{} snapshot factory: {}",
            engine.name(),
            results[1]
        );
        assert_eq!(
            results[2]["outcome"],
            results[3]["outcome"],
            "{} bound intrinsic: {}",
            engine.name(),
            results[2]
        );
    }
}
