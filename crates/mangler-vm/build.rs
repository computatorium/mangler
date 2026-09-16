#[path = "build/fingerprint.rs"]
mod fingerprint;

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.parent().unwrap().parent().unwrap();
    let inputs = fingerprint::compiler_inputs(root).expect("enumerate compiler fingerprint inputs");
    for path in &inputs {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    // Directory watches also invalidate the fingerprint when a source is added.
    for name in fingerprint::SOURCE_CRATES {
        println!(
            "cargo:rerun-if-changed={}/crates/{name}/src",
            root.display()
        );
    }
    println!("cargo:rerun-if-changed={}/build", manifest.display());
    println!("cargo:rerun-if-changed={}/vendor", root.display());
    let value = fingerprint::fingerprint(root, &inputs).expect("read compiler fingerprint inputs");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(
        output.join("compiler_fingerprint.rs"),
        format!("/// Source-derived host/Wasm compatibility identity; not an authenticity signature.\npub const COMPILER_FINGERPRINT: u64 = 0x{value:016x};\n"),
    )
    .expect("write compiler fingerprint");
}
