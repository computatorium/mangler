//! Stable compatibility identity, independent of checkout path and build time.
//! FNV-1a identifies accidentally mixed artifacts; it is not authentication.
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const SOURCE_CRATES: &[&str] = &[
    "mangler-core",
    "mangler-config",
    "mangler-passgraph",
    "mangler-jsast",
    "mangler-vm",
    "mangler-js",
    "mangler-eval",
];

fn collect(directory: &Path, paths: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, paths)?;
        } else if path.is_file() {
            paths.insert(path);
        }
    }
    Ok(())
}

pub fn compiler_inputs(root: &Path) -> io::Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    for name in ["Cargo.toml", "Cargo.lock", "crates/mangler-vm/build.rs"] {
        paths.insert(root.join(name));
    }
    collect(&root.join("crates/mangler-vm/build"), &mut paths)?;
    if root.join("vendor").is_dir() {
        collect(&root.join("vendor"), &mut paths)?;
    }
    for name in SOURCE_CRATES {
        let directory = root.join("crates").join(name);
        paths.insert(directory.join("Cargo.toml"));
        collect(&directory.join("src"), &mut paths)?;
    }
    paths.insert(root.join("crates/mangler-eval/host/compiler.js"));
    Ok(paths)
}

pub fn fingerprint(root: &Path, paths: &BTreeSet<PathBuf>) -> io::Result<u64> {
    let mut hash = 0xcbf29ce484222325_u64;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    };
    feed(b"mangler-compiler-compatibility-v1\0");
    // Sort normalized relative names, not platform-specific absolute paths.
    let mut inputs = Vec::with_capacity(paths.len());
    for path in paths {
        let relative = path.strip_prefix(root).map_err(io::Error::other)?;
        let name = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        inputs.push((name, path));
    }
    inputs.sort_by(|left, right| left.0.cmp(&right.0));
    for (name, path) in inputs {
        let bytes = fs::read(path)?;
        feed(&(name.len() as u64).to_le_bytes());
        feed(name.as_bytes());
        feed(&(bytes.len() as u64).to_le_bytes());
        feed(&bytes);
    }
    Ok(hash)
}
