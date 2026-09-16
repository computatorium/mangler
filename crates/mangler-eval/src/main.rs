//! Emit the shared interpreter ahead of time; the Wasm host never evaluates JS
//! source strings or constructs functions dynamically.
use mangler_core::Language;
use mangler_jsast::{Js, ParseOpts};
use mangler_vm::{InterpreterSpec, N_BIN_OPS, N_OPCODES, N_UN_OPS, VmDiversity};
use swc_core::common::{SourceMap, sync::Lrc};
use swc_core::ecma::ast::Program;
use swc_core::ecma::codegen::{Config, Emitter, text_writer::JsWriter};

fn main() {
    let diversity = VmDiversity {
        perm: (0..N_OPCODES).collect(),
        bin_perm: (0..N_BIN_OPS).collect(),
        un_perm: (0..N_UN_OPS).collect(),
        code_key: 1,
        skeleton_variant: 0,
        handler_seed: 0,
        dispatch_seed: 0,
        mba_seed: 0,
    };
    let specs: Vec<_> = [("manglerEvalRun", false), ("manglerEvalRunStrict", true)]
        .into_iter()
        .map(|(name, strict)| InterpreterSpec {
            name,
            table: "manglerEvalTable",
            rc: "manglerEvalConstruct",
            sy: "manglerEvalIterator",
            needs_eh: true,
            is_strict: strict,
            diversity: &diversity,
            usage: None,
        })
        .collect();
    let body = mangler_vm::emit_interpreters(&specs).unwrap();
    let host = std::env::args()
        .nth(1)
        .is_some_and(|argument| argument == "--host");
    let mut program = if host {
        Js.parse(include_str!("../host/compiler.js"), &ParseOpts::default())
            .unwrap()
            .into_program()
    } else {
        let prelude = format!(
            "var manglerEvalCompilerFingerprint={}n,manglerEvalTable=[],manglerEvalConstruct=Reflect.construct,manglerEvalIterator=Symbol.iterator;",
            mangler_vm::COMPILER_FINGERPRINT as i64
        );
        let mut program = Js
            .parse(&prelude, &ParseOpts::default())
            .unwrap()
            .into_program();
        let Program::Script(script) = &mut program else {
            unreachable!()
        };
        script.body.extend(body);
        program
    };
    mangler_js::runtime_frontend::isolate_generated_runtime(
        &mut program,
        "manglerEvalTable",
        "ManglerEvalIntrinsics",
    )
    .unwrap();
    let cm: Lrc<SourceMap> = Default::default();
    let mut bytes = Vec::new();
    Emitter {
        cfg: Config::default().with_minify(true),
        cm: cm.clone(),
        comments: None,
        wr: JsWriter::new(cm, "", &mut bytes, None),
    }
    .emit_program(&program)
    .unwrap();
    if !host {
        println!(
            "const ManglerEvalIntrinsics=({})();",
            mangler_js::runtime_frontend::intrinsic_snapshot_factory()
        );
    }
    println!("{}", String::from_utf8(bytes).unwrap());
}
