#[path = "../build/fingerprint.rs"]
mod fingerprint;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let next = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mangler-fingerprint-{}-{next}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn file(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, source).unwrap();
        path
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn identical_sources_have_identical_ids_in_different_checkout_paths() {
    let left = Workspace::new();
    let right = Workspace::new();
    let a = BTreeSet::from([
        left.file("src/a.rs", "fn a() {}"),
        left.file("Cargo.lock", "v=1"),
    ]);
    let b = BTreeSet::from([
        right.file("Cargo.lock", "v=1"),
        right.file("src/a.rs", "fn a() {}"),
    ]);
    assert_eq!(
        fingerprint::fingerprint(&left.0, &a).unwrap(),
        fingerprint::fingerprint(&right.0, &b).unwrap()
    );
    right.file("src/a.rs", "fn a() { different(); }");
    assert_ne!(
        fingerprint::fingerprint(&left.0, &a).unwrap(),
        fingerprint::fingerprint(&right.0, &b).unwrap()
    );
}

#[test]
fn filenames_and_content_boundaries_are_part_of_the_identity() {
    let workspace = Workspace::new();
    let file = workspace.file("a", "bc");
    let before = fingerprint::fingerprint(&workspace.0, &BTreeSet::from([file.clone()])).unwrap();
    std::fs::remove_file(file).unwrap();
    let file = workspace.file("ab", "c");
    let after = fingerprint::fingerprint(&workspace.0, &BTreeSet::from([file])).unwrap();
    assert_ne!(before, after);
}

#[test]
fn inventory_covers_frontend_lowering_vm_host_and_dependencies() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let inputs = fingerprint::compiler_inputs(root).unwrap();
    for name in [
        "Cargo.lock",
        "Cargo.toml",
        "crates/mangler-vm/src/isa.rs",
        "crates/mangler-vm/src/compile/expr.rs",
        "crates/mangler-js/src/passes/suspension.rs",
        "crates/mangler-jsast/src/lang.rs",
        "crates/mangler-eval/src/lib.rs",
        "crates/mangler-eval/host/compiler.js",
        "vendor/swc_ecma_parser-45.1.1/src/parser/stmt.rs",
        "vendor/swc_ecma_transforms_base-49.0.1/src/resolver/mod.rs",
    ] {
        assert!(
            inputs.contains(&root.join(name)),
            "missing semantic input {name}"
        );
    }
    assert_ne!(fingerprint::fingerprint(root, &inputs).unwrap(), 0);
}
