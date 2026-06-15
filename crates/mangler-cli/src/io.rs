//! Input collection and output writing — the file-system ends of the CLI.
//!
//! This is pure plumbing: it never touches transform logic. [`collect_inputs`]
//! turns raw CLI args (explicit files, directories, glob patterns, or `-` for
//! stdin) into a flat list of [`Input`]s the [`crate::Engine`] consumes;
//! [`write_output`] places each [`crate::Output`] at one of the three
//! [`OutputTarget`]s (stdout, in-place, or a path/dir).
//!
//! Directory args are walked recursively without following symlinks (so a
//! self-referential link can't loop), and each file records its path RELATIVE to
//! the directory root so a directory output target can mirror the input tree
//! instead of collapsing everything onto basenames.

use crate::Input;
use mangler_config::Lang;
use std::path::{Path, PathBuf};

/// Expand CLI input args (files, dirs, glob patterns, or "-") into [`Input`]s.
///
/// `lang_override` forces a language when extension detection is unwanted; it is
/// required for stdin (`-`), which has no extension to detect.
pub fn collect_inputs(args: &[String], lang_override: Option<Lang>) -> anyhow::Result<Vec<Input>> {
    let mut out = Vec::new();
    for arg in args {
        if arg == "-" {
            let src = read_stdin()?;
            let lang = lang_override
                .ok_or_else(|| anyhow::anyhow!("--lang is required when reading from stdin"))?;
            out.push(Input::stdin(src).with_lang(lang));
            continue;
        }
        let path = Path::new(arg);
        if path.is_dir() {
            collect_dir(path, path, lang_override, &mut out)?;
        } else if arg.contains('*') || arg.contains('?') || arg.contains('[') {
            for entry in glob::glob(arg).map_err(|e| anyhow::anyhow!("bad glob: {e}"))? {
                let p = entry.map_err(|e| anyhow::anyhow!("glob: {e}"))?;
                let rel = basename_rel(&p)?;
                push_file(&p, rel, lang_override, &mut out)?;
            }
        } else {
            let rel = basename_rel(path)?;
            push_file(path, rel, lang_override, &mut out)?;
        }
    }
    Ok(out)
}

/// `root` is the original directory arg; each collected file's `rel` is its path
/// relative to `root`, so the output tree mirrors the input tree.
fn collect_dir(
    dir: &Path,
    root: &Path,
    lang_override: Option<Lang>,
    out: &mut Vec<Input>,
) -> anyhow::Result<()> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| anyhow::anyhow!("read dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| anyhow::anyhow!("dir entry: {e}"))?;
        let p = entry.path();
        // Classify WITHOUT following symlinks, so a directory symlink to an
        // ancestor cannot cause infinite recursion.
        let ft = std::fs::symlink_metadata(&p).map(|m| m.file_type());
        let is_real_dir = ft.as_ref().is_ok_and(|f| f.is_dir());
        let is_symlink_to_dir = ft.as_ref().is_ok_and(|f| f.is_symlink()) && p.is_dir();
        if is_real_dir {
            collect_dir(&p, root, lang_override, out)?;
        } else if !is_symlink_to_dir && detect_lang(&p, lang_override).is_some() {
            let rel = p
                .strip_prefix(root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| PathBuf::from(p.file_name().unwrap_or(p.as_os_str())));
            push_file(&p, rel, lang_override, out)?;
        }
    }
    Ok(())
}

/// The basename as a relative path — used for explicitly-named files and glob
/// matches (no tree to mirror, so output is `<dir>/<basename>`).
fn basename_rel(path: &Path) -> anyhow::Result<PathBuf> {
    path.file_name()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot derive a filename for {}", path.display()))
}

fn push_file(
    path: &Path,
    rel: PathBuf,
    lang_override: Option<Lang>,
    out: &mut Vec<Input>,
) -> anyhow::Result<()> {
    let lang = detect_lang(path, lang_override)
        .ok_or_else(|| anyhow::anyhow!("cannot detect language for {}", path.display()))?;
    let source = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    out.push(
        Input::new(source)
            .with_path(path.to_path_buf())
            .with_lang(lang)
            .with_rel(rel),
    );
    Ok(())
}

fn detect_lang(path: &Path, lang_override: Option<Lang>) -> Option<Lang> {
    lang_override.or_else(|| path.extension().and_then(|e| e.to_str()).and_then(Lang::from_ext))
}

fn read_stdin() -> anyhow::Result<String> {
    use std::io::Read;
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .map_err(|e| anyhow::anyhow!("stdin: {e}"))?;
    Ok(s)
}

/// Where to write a single input's output.
///
/// * `Stdout`  — the default for a single input (e.g. when piping).
/// * `InPlace` — overwrite the source file (`--in-place`); errors on stdin input.
/// * `Path`    — a `--output` target; a directory mirrors each input's relative
///   path under it, a plain path is written to directly (single input only).
pub enum OutputTarget<'a> {
    /// Write to standard output.
    Stdout,
    /// Overwrite each source file with its output.
    InPlace,
    /// Write to this path (a directory mirrors `Input::rel`).
    Path(&'a Path),
}

/// Write one input's mangled `output` to the chosen [`OutputTarget`].
///
/// For a directory `Path` target the destination is `target/<input.rel>` and any
/// missing parent directories are created; in-place writes back over
/// `input.path` (and fails if the input was stdin).
pub fn write_output(input: &Input, output: &str, target: &OutputTarget) -> anyhow::Result<()> {
    match target {
        OutputTarget::Stdout => {
            use std::io::Write;
            std::io::stdout()
                .write_all(output.as_bytes())
                .map_err(|e| anyhow::anyhow!("stdout: {e}"))
        }
        OutputTarget::InPlace => {
            let path = input
                .path
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("--in-place requires file inputs (not stdin)"))?;
            std::fs::write(path, output)
                .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))
        }
        OutputTarget::Path(dir_or_file) => write_to_path(input, output, dir_or_file),
    }
}

fn write_to_path(input: &Input, output: &str, target: &Path) -> anyhow::Result<()> {
    let dest = if target.is_dir() {
        let rel = input.rel.as_deref().ok_or_else(|| {
            anyhow::anyhow!("cannot derive output path for directory target (stdin input?)")
        })?;
        target.join(rel)
    } else {
        target.to_path_buf()
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&dest, output).map_err(|e| anyhow::anyhow!("write {}: {e}", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_files_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.css");
        std::fs::write(&f, ".a{color:red}").unwrap();
        let inputs = collect_inputs(&[f.to_string_lossy().into_owned()], None).unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].lang, Some(Lang::Css));
    }

    #[test]
    fn unknown_extension_without_override_errors() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "x").unwrap();
        assert!(collect_inputs(&[f.to_string_lossy().into_owned()], None).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn directory_symlink_loop_does_not_overflow() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.js"), "var x=1;").unwrap();
        let _ = symlink(dir.path(), dir.path().join("loop"));
        let mut out = Vec::new();
        collect_dir(dir.path(), dir.path(), Some(Lang::Js), &mut out).unwrap();
        assert!(out.iter().any(|i| i.path.as_ref().is_some_and(|p| p.ends_with("a.js"))));
    }

    #[test]
    fn dir_inputs_mirror_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::create_dir_all(dir.path().join("b")).unwrap();
        std::fs::write(dir.path().join("a/x.js"), "var x=1;").unwrap();
        std::fs::write(dir.path().join("b/x.js"), "var y=2;").unwrap();
        let inputs =
            collect_inputs(&[dir.path().to_string_lossy().into_owned()], Some(Lang::Js)).unwrap();
        let mut rels: Vec<_> = inputs
            .iter()
            .map(|i| i.rel.as_ref().unwrap().to_string_lossy().replace('\\', "/"))
            .collect();
        rels.sort();
        assert_eq!(rels, vec!["a/x.js".to_string(), "b/x.js".to_string()]);
    }

    #[test]
    fn explicit_file_rel_is_basename() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("only.js");
        std::fs::write(&f, "var z=3;").unwrap();
        let inputs = collect_inputs(&[f.to_string_lossy().into_owned()], None).unwrap();
        assert_eq!(inputs[0].rel.as_ref().unwrap().to_string_lossy(), "only.js");
    }
}
