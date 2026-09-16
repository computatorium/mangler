//! Optional synchronous compiler ABI. No JavaScript parser or bytecode compiler
//! is duplicated in the host: these exports call the same Rust implementation as
//! the command-line tool. This crate is not linked into ordinary protected output.
use mangler_core::Language;
use mangler_jsast::{Js, ParseOpts};
use mangler_vm::source_text::{self, SourceTextMap};
use mangler_vm::{CompileOptions, Compiled, Const, Instr};
use serde_json::{Value, json};
use std::sync::Mutex;
use swc_core::ecma::ast::*;

pub mod constructor;
mod eval_context;

pub const ABI_VERSION: u32 = 1;
static RESULT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Encoding {
    op: Vec<usize>,
    bin: Vec<usize>,
    un: Vec<usize>,
}
impl Encoding {
    fn from_request(request: &Value) -> Result<Self, &'static str> {
        fn permutation(
            request: &Value,
            key: &str,
            count: usize,
        ) -> Result<Vec<usize>, &'static str> {
            let Some(value) = request.get(key) else {
                return Ok((0..count).collect());
            };
            let Some(values) = value.as_array() else {
                return Err("permutation must be an array");
            };
            if values.len() < count {
                return Err("permutation is too short");
            }
            let result = values
                .iter()
                .map(|v| {
                    v.as_u64()
                        .filter(|n| *n <= u32::MAX as u64)
                        .map(|n| n as usize)
                        .ok_or("invalid permutation entry")
                })
                .collect::<Result<Vec<_>, _>>()?;
            let unique = result.iter().collect::<std::collections::HashSet<_>>();
            if unique.len() != result.len() {
                return Err("duplicate permutation entry");
            }
            Ok(result)
        }
        Ok(Self {
            op: permutation(request, "op", mangler_vm::N_OPCODES)?,
            bin: permutation(request, "bin", mangler_vm::N_BIN_OPS)?,
            un: permutation(request, "un", mangler_vm::N_UN_OPS)?,
        })
    }
}

fn constant(value: &Const) -> Value {
    match value {
        Const::Num(n) => json!({"tag":"number","value":n.to_string()}),
        Const::Bool(b) => json!({"tag":"boolean","value":b}),
        Const::Str(s) => json!({"tag":"utf16","value":s.encode_utf16().collect::<Vec<_>>()}),
        Const::Utf16(units) => json!({"tag":"utf16","value":units}),
        Const::BigInt(s) => json!({"tag":"bigint","value":s}),
        Const::RegExp { pattern, flags } => json!({"tag":"regexp","pattern":pattern,"flags":flags}),
        Const::RegExpUtf16 { pattern, flags } => {
            json!({"tag":"regexpUtf16","pattern":pattern,"flags":flags})
        }
        Const::TemplateObject { cooked, raw } => {
            json!({"tag":"template","cooked":cooked.iter().map(|s|s.as_ref().map(|s|s.encode_utf16().collect::<Vec<_>>())).collect::<Vec<_>>(),"raw":raw.iter().map(|s|s.encode_utf16().collect::<Vec<_>>()).collect::<Vec<_>>()})
        }
        Const::TemplateObjectUtf16 { cooked, raw } => {
            json!({"tag":"template","cooked":cooked,"raw":raw})
        }
        Const::Environment(metadata) => {
            use mangler_vm::eval::EnvironmentScope;
            let mut value = json!({"tag":"environment","sourceContext":metadata.source_context as u8,"scopes":metadata.scopes.iter().map(|scope|match scope {
                EnvironmentScope::Variables => json!(0),
                EnvironmentScope::WithObject(slot) => json!({"w":slot}),
                EnvironmentScope::Bindings(bindings) => json!(bindings.iter().map(|b| {
                    let mut values=vec![json!(b.name_const),json!(b.slot),json!(u32::from(b.cell)|(u32::from(b.lexical)<<1)|(u32::from(b.accessor_cell)<<2))];
                    if !b.objects.is_empty(){values.push(json!(b.objects));}
                    values
                }).collect::<Vec<_>>()),
            }).collect::<Vec<_>>()});
            if let Some(context) = &metadata.class_context {
                value["classContext"] = json!({"capsuleSlot":context.capsule_slot,"capsuleCell":context.capsule_cell,"privateNames":context.private_names,"allowSuperProperty":context.allow_super_property,"allowSuperCall":context.allow_super_call,"argumentsForbidden":context.arguments_forbidden});
            }
            value
        }
        Const::NativeFactory(source) => json!({"tag":"nativeFactory","source":source}),
    }
}

fn program(compiled: &Compiled, encoding: &Encoding, strict: bool) -> Value {
    let (code, _) =
        mangler_vm::serialize::serialize(compiled, &encoding.op, &encoding.bin, &encoding.un);
    let mut offset = 0;
    let mut relocations = Vec::new();
    for instruction in &compiled.code {
        if matches!(instruction, Instr::MakeClosure { .. }) {
            relocations.push(offset + 1);
        }
        offset += instruction.size();
    }
    json!({"code":code.into_iter().map(|n|n as u32).collect::<Vec<_>>(),
        "constants":compiled.consts.iter().map(constant).collect::<Vec<_>>(),
        "captures":compiled.captures,"capStart":compiled.cap_start(),"pcount":compiled.pcount,
        "argumentFactory":mangler_vm::table::runtime_argument_factory(compiled,strict),
        "relocations":relocations,
        "children":compiled.children.iter().map(|child|json!({"strict":child.is_strict,"suspension":child.suspension.map_or(0,|kind|kind.code()),"program":program(&child.compiled, encoding, strict || child.is_strict)})).collect::<Vec<_>>()})
}

/// Compile one function expression supplied as a versioned JSON request.
/// Diagnostics are data; unsupported input never becomes an unprotected fallback.
pub fn compile_json(input: &str) -> Value {
    let request: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(e) => return failure("request", &e.to_string()),
    };
    if request["version"].as_u64() != Some(ABI_VERSION.into()) {
        return failure("version", "unsupported compiler ABI version");
    }
    let encoding = match Encoding::from_request(&request) {
        Ok(value) => value,
        Err(error) => return failure("request", error),
    };
    if request["mode"].as_str() == Some("constructor") {
        return compile_constructor(&request, &encoding);
    }
    let units = match request_units(&request, "source", "sourceUnits") {
        Ok(units) => units,
        Err(error) => return failure("request", error),
    };
    let normalized = source_text::normalize(&[units]);
    let source = normalized.fragments[0].as_str();
    let map = &normalized.replacements;
    if request["mode"].as_str() == Some("eval") {
        return compile_eval(source, &request, &encoding, map);
    }
    if request["mode"].as_str() != Some("function") {
        return failure("mode", "expected function or eval compilation mode");
    }
    let wrapped = format!("var __mangler_input=({source});");
    let ast = match Js.parse(&wrapped, &ParseOpts::default()) {
        Ok(ast) => ast,
        Err(error) => return failure("syntax", &error.to_string()),
    };
    let statement = match ast.into_program() {
        Program::Script(mut script) => script.body.remove(0),
        Program::Module(mut module) => match module.body.remove(0) {
            ModuleItem::Stmt(stmt) => stmt,
            _ => return failure("syntax", "expected function expression"),
        },
    };
    let Stmt::Decl(Decl::Var(mut decl)) = statement else {
        return failure("syntax", "expected function expression");
    };
    let mut expression = *decl.decls.remove(0).init.unwrap();
    while let Expr::Paren(paren) = expression {
        expression = *paren.expr;
    }
    let Expr::Fn(function) = expression else {
        return failure("syntax", "expected function expression");
    };
    let length = function
        .function
        .params
        .iter()
        .take_while(|p| !matches!(&p.pat, Pat::Assign(_) | Pat::Rest(_)))
        .count();
    let strict = request["strict"].as_bool().unwrap_or(false);
    let result = compile_function(
        *function.function,
        function.ident.map(|id| id.sym.to_string()),
        length,
        strict,
        &encoding,
        map,
    );
    if needs_preparation(&result) {
        compile_prepared(source, false, strict, &encoding, map)
    } else {
        result
    }
}

fn code_units(value: &Value) -> Result<Vec<u16>, &'static str> {
    value
        .as_array()
        .ok_or("source units must be an array")?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|unit| u16::try_from(unit).ok())
                .ok_or("source code units must be integers between 0 and 65535")
        })
        .collect()
}

fn request_units(
    request: &Value,
    string_key: &str,
    units_key: &str,
) -> Result<Vec<u16>, &'static str> {
    if let Some(value) = request.get(units_key) {
        return code_units(value);
    }
    request[string_key]
        .as_str()
        .map(|source| source.encode_utf16().collect())
        .ok_or("source must be a string or an array of UTF-16 code units")
}

fn compile_constructor(request: &Value, encoding: &Encoding) -> Value {
    let Some(kind) = request["kind"]
        .as_str()
        .and_then(constructor::ConstructorKind::from_name)
    else {
        return failure("request", "unknown dynamic constructor kind");
    };
    let parameter_units = if let Some(parameters) = request.get("parameterUnits") {
        parameters
            .as_array()
            .ok_or("constructor parameters must be an array")
            .and_then(|parameters| {
                parameters
                    .iter()
                    .map(code_units)
                    .collect::<Result<Vec<_>, _>>()
            })
    } else {
        request["parameters"]
            .as_array()
            .ok_or("constructor parameters must be an array")
            .and_then(|parameters| {
                parameters
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .map(|value| value.encode_utf16().collect())
                            .ok_or("constructor parameters must be strings")
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
    };
    let mut fragments = match parameter_units {
        Ok(parameters) => parameters,
        Err(error) => return failure("request", error),
    };
    let body = match request_units(request, "body", "bodyUnits") {
        Ok(body) => body,
        Err(error) => return failure("request", error),
    };
    fragments.push(body);
    let mut normalized = source_text::normalize(&fragments);
    let body = normalized.fragments.pop().unwrap();
    let parameters = normalized.fragments;
    let map = &normalized.replacements;
    let parsed = match constructor::parse(kind, &parameters, &body) {
        Ok(parsed) => parsed,
        Err(error) => return failure("syntax", &error),
    };
    let mut result = compile_function(
        parsed.function,
        None,
        parsed.length as usize,
        parsed.strict,
        encoding,
        map,
    );
    if needs_preparation(&result) {
        result = compile_prepared(&parsed.source, true, parsed.strict, encoding, map);
    }
    if result["ok"] == true {
        result["displayName"] = json!("anonymous");
        result["kind"] = json!(kind.code());
    }
    result
}

fn compile_function(
    function: Function,
    name: Option<String>,
    length: usize,
    inherited_strict: bool,
    encoding: &Encoding,
    source_utf16: &SourceTextMap,
) -> Value {
    let Some(body) = function.body else {
        return failure("syntax", "function requires a body");
    };
    if function.is_async || function.is_generator {
        return failure(
            "compile",
            "suspension lowering requires the program frontend",
        );
    }
    let strict = inherited_strict || mangler_jsast::directives::has_use_strict(&body.stmts);
    let compiled = match mangler_vm::compile_body_with_opts(
        &function.params,
        &body,
        CompileOptions {
            live_captures: true,
            strict,
            source_utf16: Some(source_utf16),
            ..Default::default()
        },
    ) {
        Ok(compiled) => compiled,
        Err(reason) => return failure("compile", reason),
    };
    json!({"version":ABI_VERSION,"ok":true,"strict":strict,"name":name,"length":length,"declaredVars":[],"program":program(&compiled, encoding, strict)})
}

fn needs_preparation(result: &Value) -> bool {
    fn native(program: &Value) -> bool {
        program["constants"].as_array().is_some_and(|constants| {
            constants
                .iter()
                .any(|constant| constant["tag"] == "nativeFactory")
        }) || program["children"]
            .as_array()
            .is_some_and(|children| children.iter().any(|child| native(&child["program"])))
    }
    if result["ok"] == true {
        native(&result["program"])
    } else {
        result["error"]["kind"] == "compile"
    }
}

fn compile_prepared(
    source: &str,
    constructor: bool,
    strict: bool,
    encoding: &Encoding,
    source_utf16: &SourceTextMap,
) -> Value {
    use swc_core::common::DUMMY_SP;
    let prepared = match if constructor {
        mangler_js::runtime_frontend::prepare_constructor_with_source_utf16(source, source_utf16)
    } else {
        mangler_js::runtime_frontend::prepare_function_with_source_utf16(source, source_utf16)
    } {
        Ok(prepared) => prepared,
        Err(error) => return failure("compile", &error.to_string()),
    };
    let block = FunctionBody {
        span: DUMMY_SP,
        stmts: vec![Stmt::Return(ReturnStmt {
            span: DUMMY_SP,
            arg: Some(prepared.initializer),
        })],
    };
    let hidden = prepared.support.names.iter().cloned().collect();
    let compiled = match mangler_vm::compile_body_with_opts(
        &[],
        &block,
        CompileOptions {
            live_captures: true,
            lexical_entry: true,
            lexical_arguments: true,
            strict,
            internal_bindings: Some(&hidden),
            source_utf16: Some(source_utf16),
            ..Default::default()
        },
    ) {
        Ok(compiled) => compiled,
        Err(reason) => return failure("compile", reason),
    };
    json!({"version":ABI_VERSION,"ok":true,"entry":true,"strict":strict,"name":prepared.name,"length":prepared.length,
        "declaredVars":[],"program":program(&compiled,encoding,strict),
        "support":{"factory":prepared.support.factory,"names":prepared.support.names,"tables":prepared.support.tables}})
}

fn compile_eval(
    source: &str,
    request: &Value,
    encoding: &Encoding,
    source_utf16: &SourceTextMap,
) -> Value {
    let result = compile_eval_mode(source, request, encoding, false, source_utf16);
    if needs_preparation(&result) {
        compile_eval_mode(source, request, encoding, true, source_utf16)
    } else {
        result
    }
}

fn compile_eval_mode(
    source: &str,
    request: &Value,
    encoding: &Encoding,
    lower: bool,
    source_utf16: &SourceTextMap,
) -> Value {
    let grammar = match eval_context::Grammar::from_request(request) {
        Ok(grammar) => grammar,
        Err(error) => return failure("request", &error),
    };
    let script = match eval_context::parse(source, request, grammar.as_ref()) {
        Ok(script) => script,
        Err(error) => return failure("syntax", &error),
    };
    let parsed = Program::Script(script);
    if let Err(error) = mangler_jsast::Js::validate_resource_scopes(&parsed) {
        return failure("syntax", &error.to_string());
    }
    let Program::Script(script) = parsed else {
        unreachable!()
    };
    let class_context = grammar.as_ref().map(|grammar| grammar.bind(&script));
    let prepared = mangler_vm::eval::prepare_eval_body(
        script.body,
        request["strict"].as_bool().unwrap_or(false),
        match request["sourceContext"].as_u64() {
            Some(1) => mangler_vm::eval::SourceContext::Script,
            Some(2) => mangler_vm::eval::SourceContext::Module,
            _ => mangler_vm::eval::SourceContext::Function,
        },
    );
    let (prepared, support, class_contexts) = if lower || class_context.is_some() {
        match mangler_js::runtime_frontend::prepare_eval_with_context(
            prepared,
            source_utf16,
            class_context.as_ref(),
        ) {
            Ok(prepared) => (
                prepared.body,
                Some(prepared.support),
                Some(prepared.class_contexts),
            ),
            Err(error) => return failure("compile", &error.to_string()),
        }
    } else {
        (prepared, None, None)
    };
    let hidden = support
        .as_ref()
        .map(|support| support.names.iter().cloned().collect());
    match mangler_vm::eval::compile_prepared_eval_body(
        prepared,
        CompileOptions {
            internal_bindings: hidden.as_ref(),
            source_utf16: Some(source_utf16),
            eval_class_contexts: class_contexts.as_ref(),
            ..Default::default()
        },
    ) {
        Ok(result) => {
            let mut output = json!({"version":ABI_VERSION,"ok":true,"strict":result.strict,"declaredVars":result.declared_vars,"declaredFunctions":result.declared_functions,"program":program(&result.compiled,encoding,result.strict)});
            if let Some(support) = support {
                output["support"] = json!({"factory":support.factory,"names":support.names,"tables":support.tables});
            }
            if let Some(context) = class_context {
                output["classCaptures"] = json!([context.capsule_binding]);
            }
            output
        }
        Err(reason) => failure("compile", reason),
    }
}

fn failure(kind: &str, message: &str) -> Value {
    json!({"version":ABI_VERSION,"ok":false,"error":{"kind":kind,"message":message}})
}

#[unsafe(no_mangle)]
pub extern "C" fn mangler_abi_version() -> u32 {
    ABI_VERSION
}

/// Bits are transported as a signed i64 BigInt by the JavaScript Wasm API.
#[unsafe(no_mangle)]
pub extern "C" fn mangler_compiler_fingerprint() -> u64 {
    mangler_vm::COMPILER_FINGERPRINT
}

#[unsafe(no_mangle)]
pub extern "C" fn mangler_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()) as *mut u8
}

/// # Safety
/// `ptr` and `len` must be a live allocation returned by `mangler_alloc`, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangler_free(ptr: *mut u8, len: usize) {
    unsafe {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
    }
}

/// # Safety
/// The supplied buffer must contain `len` initialized bytes in this instance's
/// linear memory. Calls on an instance are synchronous and must not overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangler_compile(ptr: *const u8, len: usize) -> u32 {
    static PANIC_HOOK: std::sync::Once = std::sync::Once::new();
    PANIC_HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            if let Ok(mut result) = RESULT.lock() {
                *result =
                    serde_json::to_vec(&failure("internal", &info.to_string())).unwrap_or_default();
            }
        }))
    });
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let output = match std::str::from_utf8(bytes) {
        Ok(source) => compile_json(source),
        Err(error) => failure("utf8", &error.to_string()),
    };
    let success = output["ok"].as_bool() == Some(true);
    *RESULT.lock().unwrap() = serde_json::to_vec(&output).unwrap();
    u32::from(success)
}

#[unsafe(no_mangle)]
pub extern "C" fn mangler_result_ptr() -> *const u8 {
    RESULT.lock().unwrap().as_ptr()
}
#[unsafe(no_mangle)]
pub extern "C" fn mangler_result_len() -> usize {
    RESULT.lock().unwrap().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn class_eval_uses_opaque_captures_and_shared_preparation() {
        let context = json!({"privateNames":["x"],"allowSuperProperty":true,"allowSuperCall":true,"argumentsForbidden":false});
        for source in [
            "this.#x",
            "super.value",
            "super()",
            "let __mangler_eval_capsule=1;this.#x",
            "class Inner{read(o){return o.#x}}new Inner().read(this)",
        ] {
            let result = compile_json(&json!({"version":1,"mode":"eval","source":source,"strict":true,"allowNewTarget":true,"classContext":context}).to_string());
            assert_eq!(result["ok"], true, "{source}: {result}");
            assert_eq!(result["classCaptures"].as_array().unwrap().len(), 1);
            if source.starts_with("let ") {
                assert_eq!(result["classCaptures"][0], "__mangler_eval_capsule_");
            }
        }
    }

    #[test]
    fn raw_utf16_source_is_lossless_and_still_obeys_grammar() {
        for source in ["'UNIT'", "(s=>s.raw)`UNIT`", "/UNIT/u"] {
            let units = source.encode_utf16().collect::<Vec<_>>();
            let start = source.find("UNIT").unwrap();
            let mut raw = units[..start].to_vec();
            raw.push(0xd800);
            raw.extend(&units[start + 4..]);
            let result =
                compile_json(&json!({"version":1,"mode":"eval","sourceUnits":raw}).to_string());
            assert_eq!(result["ok"], true, "{result}");
            assert!(result["program"].to_string().contains("55296"), "{result}");
            assert!(
                !result["program"].to_string().contains('\u{e000}'),
                "{result}"
            );
        }
        let invalid =
            compile_json(&json!({"version":1,"mode":"eval","sourceUnits":[0xd800]}).to_string());
        assert_eq!(invalid["error"]["kind"], "syntax", "{invalid}");
        let invalid =
            compile_json(&json!({"version":1,"mode":"eval","sourceUnits":[65536]}).to_string());
        assert_eq!(invalid["error"]["kind"], "request", "{invalid}");
    }

    #[test]
    fn dynamic_constructor_uses_global_name_and_shared_frontend() {
        let ordinary=compile_json(&json!({"version":1,"mode":"constructor","kind":"normal","parameters":[],"body":"return anonymous"}).to_string());
        assert_eq!(ordinary["ok"], true, "{ordinary}");
        assert!(ordinary["name"].is_null());
        assert_eq!(ordinary["displayName"], "anonymous");
        assert!(
            ordinary["program"]["captures"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "anonymous")
        );
        for (kind, body) in [
            ("async", "return await 3"),
            ("generator", "yield 3"),
            ("async-generator", "yield await 3"),
            (
                "normal",
                "class Account{#x=3;value(){return this.#x}}return new Account().value()",
            ),
        ] {
            let result = compile_json(
                &json!({"version":1,"mode":"constructor","kind":kind,"parameters":[],"body":body})
                    .to_string(),
            );
            assert_eq!(result["ok"], true, "{kind}: {result}");
            assert_eq!(result["entry"], true);
            assert!(result["support"]["factory"].is_string());
        }
    }

    #[test]
    fn shares_compiler_and_returns_structured_failures() {
        let result = compile_json(
            &json!({"version":1,"mode":"function","source":"function(a){return a*3+1}"})
                .to_string(),
        );
        assert_eq!(result["ok"], true);
        assert!(result["program"]["code"].as_array().unwrap().len() > 5);
        assert_eq!(compile_json("{}")["error"]["kind"], "version");
        let bad = compile_json(
            &json!({"version":1,"mode":"function","source":"function(){return eval('1')}"})
                .to_string(),
        );
        assert_eq!(bad["ok"], true);
        assert!(
            bad["program"]["code"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_u64() == Some(81))
        );
    }
}
