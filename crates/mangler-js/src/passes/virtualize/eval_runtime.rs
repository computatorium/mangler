//! Package the shared runtime source compiler only for programs that use eval.
//! The deployed artifact contains its compiler; execution does not fetch code.
use crate::config::FileConfig;
use mangler_core::{Error, Language, Result};
use mangler_jsast::{Js, ParseOpts};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use swc_core::ecma::ast::{Program, Stmt};

const HOST: &str = include_str!("../../../../mangler-eval/host/compiler.js");
static COMPILER: OnceLock<Arc<str>> = OnceLock::new();

fn asset() -> Result<Arc<str>> {
    if let Some(encoded) = COMPILER.get() {
        return Ok(encoded.clone());
    }
    let explicit = std::env::var_os("MANGLER_EVAL_WASM").map(PathBuf::from);
    let candidates = if let Some(path) = explicit {
        vec![path]
    } else {
        let mut paths = Vec::new();
        if let Ok(executable) = std::env::current_exe()
            && let Some(directory) = executable.parent()
        {
            paths.push(directory.join("mangler-eval.wasm"));
        }
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        paths.push(workspace.join("target/eval-runtime/mangler-eval.wasm"));
        paths.push(workspace.join("target/wasm32-unknown-unknown/release/mangler_eval.wasm"));
        paths
    };
    let path = candidates.iter().find(|path| path.is_file()).ok_or_else(|| {
        Error::transform("virtualize", "Runtime source compilation requires mangler-eval.wasm. Build it with scripts/build-eval-runtime.sh, install it beside mangler, or set MANGLER_EVAL_WASM to the asset.")
    })?;
    let bytes = std::fs::read(path).map_err(|error| {
        Error::transform(
            "virtualize",
            format!("Cannot read {}: {error}", path.display()),
        )
    })?;
    if !bytes.starts_with(b"\0asm\x01\0\0\0") {
        return Err(Error::transform(
            "virtualize",
            format!("{} is not a version-1 WebAssembly module", path.display()),
        ));
    }
    let encoded: Arc<str> = crate::passes::strings::encode::base64_encode(&bytes).into();
    let _ = COMPILER.set(encoded.clone());
    Ok(COMPILER.get().cloned().unwrap_or(encoded))
}

pub(super) fn attach(table: &str, cfg: &FileConfig) -> Result<Vec<Stmt>> {
    let compiler = cfg.fresh_name();
    let constructor = cfg.fresh_name();
    let decoded = cfg.fresh_name();
    let bytes = cfg.fresh_name();
    let index = cfg.fresh_name();
    let encoded = asset()?;
    let host = HOST.replace("ManglerEvalCompiler", &constructor);
    let snapshot = crate::runtime_frontend::intrinsic_snapshot_factory();
    let source = format!(
        "var {compiler}=(function(){{const ManglerEvalIntrinsics=({snapshot})();{host}\nvar {decoded}=atob('{encoded}'),{bytes}=new Uint8Array({decoded}.length);for(var {index}=0;{index}<{decoded}.length;{index}++){bytes}[{index}]={decoded}.charCodeAt({index});return new {constructor}({bytes},{table},{table}.runtime).attach(eval);}})();"
    );
    let parsed = Js.parse(&source, &ParseOpts::default()).map_err(|error| {
        Error::transform(
            "virtualize",
            format!("Runtime compiler integration failed to parse: {error}"),
        )
    })?;
    match parsed.into_program() {
        Program::Script(mut script) => {
            for statement in &mut script.body {
                if let Stmt::Decl(swc_core::ecma::ast::Decl::Var(declaration)) = statement {
                    for variable in &mut declaration.decls {
                        if let Some(swc_core::ecma::ast::Expr::Call(call)) =
                            variable.init.as_deref_mut()
                        {
                            call.span = mangler_jsast::span::runtime_span();
                        }
                    }
                }
            }
            Ok(script.body)
        }
        Program::Module(_) => Err(Error::transform(
            "virtualize",
            "Runtime compiler integration must be a script fragment",
        )),
    }
}
