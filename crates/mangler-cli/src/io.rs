//! Filesystem planning, bounded input loading, and atomic output replacement.
//!
//! Planning reads metadata only: every destination is checked before any source
//! is transformed or replaced. The CLI loads one worker-sized batch at a time.

use crate::Input;
use anyhow::Context;
use mangler_config::Lang;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// Where to write outputs. Multiple inputs require a directory or in-place mode.
pub enum OutputTarget<'a> {
    Stdout,
    InPlace,
    Path(&'a Path),
}

#[derive(Debug)]
pub(crate) struct Source {
    path: Option<PathBuf>,
    rel: Option<PathBuf>,
    identity: Option<PathBuf>,
    lang: Lang,
    bytes: u64,
}

impl Source {
    fn name(&self) -> String {
        self.path
            .as_ref()
            .map_or_else(|| "<stdin>".into(), |p| p.display().to_string())
    }

    fn read(&self) -> anyhow::Result<Input> {
        let source = match &self.path {
            Some(path) => {
                std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
            }
            None => {
                use std::io::Read;
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .context("read stdin")?;
                text
            }
        };
        Ok(Input {
            source,
            path: self.path.clone(),
            rel: self.rel.clone(),
            lang: Some(self.lang),
        })
    }
}

pub(crate) enum PlannedItem {
    Input {
        source: Source,
        destination: Option<PathBuf>,
    },
    Error(String),
}

impl PlannedItem {
    pub(crate) fn read(&self) -> anyhow::Result<Input> {
        match self {
            Self::Input { source, .. } => source.read(),
            Self::Error(message) => anyhow::bail!("{message}"),
        }
    }

    pub(crate) fn name(&self) -> String {
        match self {
            Self::Input { source, .. } => source.name(),
            Self::Error(message) => message.clone(),
        }
    }

    pub(crate) fn bytes(&self) -> u64 {
        match self {
            Self::Input { source, .. } => source.bytes,
            Self::Error(_) => 0,
        }
    }

    pub(crate) fn write(&self, code: &str) -> anyhow::Result<()> {
        match self {
            Self::Input {
                destination: Some(path),
                ..
            } => atomic_write(path, code),
            Self::Input {
                destination: None, ..
            } => write_stdout(code),
            Self::Error(message) => anyhow::bail!("{message}"),
        }
    }
}

/// Discover and validate the complete destination plan without reading sources.
/// Per-input failures remain in sequence for the driver's `--keep-going` policy;
/// ambiguous destinations are fatal regardless of that policy.
pub(crate) fn plan_inputs(
    args: &[String],
    lang: Option<Lang>,
    target: &OutputTarget<'_>,
) -> anyhow::Result<Vec<PlannedItem>> {
    let excluded = match target {
        OutputTarget::Path(path) => Some(resolve_path(path)?),
        _ => None,
    };
    let sources = discover(args, lang, excluded.as_deref());
    let count = sources.iter().filter(|s| s.is_ok()).count();
    if count > 1 {
        match target {
            OutputTarget::Stdout => anyhow::bail!(
                "multiple inputs require --output DIRECTORY or --in-place; stdout accepts one input"
            ),
            OutputTarget::Path(path) if !path.is_dir() => anyhow::bail!(
                "--output must name an existing directory when processing multiple inputs"
            ),
            _ => {}
        }
    }
    let input_paths: HashSet<_> = sources
        .iter()
        .filter_map(|s| s.as_ref().ok().and_then(|s| s.identity.as_ref()).cloned())
        .collect();
    let mut destinations = DestinationRegistry::default();
    let mut planned = Vec::with_capacity(sources.len());
    for source in sources {
        let source = match source {
            Ok(source) => source,
            Err(message) => {
                planned.push(PlannedItem::Error(message));
                continue;
            }
        };
        let input = Input {
            path: source.path.clone(),
            rel: source.rel.clone(),
            ..Input::default()
        };
        let destination = destination(&input, target)?;
        if let Some(path) = &destination {
            destinations.reserve(path, &source.name())?;
            if input_paths.contains(path) && source.identity.as_ref() != Some(path) {
                anyhow::bail!(
                    "output {} would overwrite a different input",
                    path.display()
                );
            }
        }
        planned.push(PlannedItem::Input {
            source,
            destination,
        });
    }
    Ok(planned)
}

/// Shadow reservations use the destination filesystem itself to identify case
/// and Unicode aliases. No final output is created or replaced during planning.
#[derive(Default)]
struct DestinationRegistry {
    roots: HashMap<PathBuf, tempfile::TempDir>,
    owners: HashMap<PathBuf, String>,
}

impl DestinationRegistry {
    fn reserve(&mut self, destination: &Path, owner: &str) -> anyhow::Result<()> {
        let mut ancestor = destination
            .parent()
            .ok_or_else(|| anyhow::anyhow!("output has no parent"))?;
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| anyhow::anyhow!("output has no existing ancestor"))?;
        }
        if !self.roots.contains_key(ancestor) {
            let scratch = tempfile::Builder::new()
                .prefix(".mangler-plan-")
                .tempdir_in(ancestor)
                .with_context(|| format!("plan outputs in {}", ancestor.display()))?;
            self.roots.insert(ancestor.to_path_buf(), scratch);
        }
        let scratch = self.roots[ancestor]
            .path()
            .join(destination.strip_prefix(ancestor)?);
        std::fs::create_dir_all(scratch.parent().unwrap())
            .with_context(|| format!("conflicting output paths at {}", destination.display()))?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&scratch)
        {
            Ok(_) => {
                self.owners
                    .insert(std::fs::canonicalize(&scratch)?, owner.to_owned());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let previous = std::fs::canonicalize(&scratch)
                    .ok()
                    .and_then(|path| self.owners.get(&path))
                    .map(String::as_str)
                    .unwrap_or("another input");
                anyhow::bail!(
                    "output collision: {previous} and {owner} both write {}",
                    destination.display()
                );
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("plan output {}", destination.display()));
            }
        }
        Ok(())
    }
}

/// Compatibility convenience for embedders that explicitly want all source
/// texts in memory. The CLI uses metadata planning and bounded loading instead.
pub fn collect_inputs(args: &[String], lang_override: Option<Lang>) -> anyhow::Result<Vec<Input>> {
    discover(args, lang_override, None)
        .into_iter()
        .map(|source| source.map_err(anyhow::Error::msg)?.read())
        .collect()
}

fn discover(
    args: &[String],
    lang: Option<Lang>,
    excluded: Option<&Path>,
) -> Vec<Result<Source, String>> {
    let mut collector = Collector {
        lang,
        excluded,
        seen: HashSet::new(),
        stdin_seen: false,
        sources: Vec::new(),
    };
    for arg in args {
        if arg == "-" {
            if collector.stdin_seen {
                continue;
            }
            collector.stdin_seen = true;
            match lang {
                Some(lang) => collector.sources.push(Ok(Source {
                    path: None,
                    rel: None,
                    identity: None,
                    lang,
                    bytes: 0,
                })),
                None => collector.error("--lang is required when reading from stdin"),
            }
            continue;
        }
        let path = Path::new(arg);
        // An actual filename wins over pattern syntax, including bracketed routes.
        if path.is_dir() {
            collector.directory(path, path);
        } else if std::fs::symlink_metadata(path).is_ok() {
            collector.file(path, basename(path), false);
        } else if arg.contains(['*', '?', '[']) {
            match glob::glob(arg) {
                Err(error) => collector.error(format!("bad glob {arg}: {error}")),
                Ok(matches) => {
                    let mut paths = Vec::new();
                    for entry in matches {
                        match entry {
                            Ok(path) => paths.push(path),
                            Err(error) => collector.error(format!("glob {arg}: {error}")),
                        }
                    }
                    paths.sort();
                    if paths.is_empty() {
                        collector.error(format!("input pattern matched no files: {arg}"));
                    }
                    for path in paths {
                        if path.is_dir() {
                            collector.directory(&path, &path);
                        } else {
                            collector.file(&path, basename(&path), true);
                        }
                    }
                }
            }
        } else {
            collector.file(path, basename(path), false);
        }
    }
    if collector.sources.is_empty() {
        collector.error("no supported input files found");
    }
    collector.sources
}

struct Collector<'a> {
    lang: Option<Lang>,
    excluded: Option<&'a Path>,
    seen: HashSet<PathBuf>,
    stdin_seen: bool,
    sources: Vec<Result<Source, String>>,
}

impl Collector<'_> {
    fn error(&mut self, message: impl Into<String>) {
        self.sources.push(Err(message.into()));
    }

    fn excluded(&self, path: &Path) -> bool {
        self.excluded
            .is_some_and(|excluded| resolve_path(path).is_ok_and(|path| path.starts_with(excluded)))
    }

    fn directory(&mut self, dir: &Path, root: &Path) {
        if self.excluded(dir) {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                self.error(format!("read directory {}: {error}", dir.display()));
                return;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(error) => self.error(format!("read directory {}: {error}", dir.display())),
            }
        }
        paths.sort();
        for path in paths {
            if self.excluded(&path) {
                continue;
            }
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.error(format!("inspect {}: {error}", path.display()));
                    continue;
                }
            };
            if metadata.is_dir() {
                self.directory(&path, root);
            } else if !(metadata.file_type().is_symlink() && path.is_dir())
                && detect_lang(&path, self.lang).is_some()
            {
                self.file(
                    &path,
                    path.strip_prefix(root).unwrap_or(&path).to_path_buf(),
                    true,
                );
            }
        }
    }

    fn file(&mut self, path: &Path, rel: PathBuf, discovered: bool) {
        if discovered && self.excluded(path) {
            return;
        }
        let result = (|| -> anyhow::Result<Source> {
            let lang = detect_lang(path, self.lang)
                .ok_or_else(|| anyhow::anyhow!("cannot detect language for {}", path.display()))?;
            let metadata =
                std::fs::metadata(path).with_context(|| format!("inspect {}", path.display()))?;
            anyhow::ensure!(
                metadata.is_file(),
                "input is not a regular file: {}",
                path.display()
            );
            let identity = std::fs::canonicalize(path)
                .with_context(|| format!("resolve {}", path.display()))?;
            Ok(Source {
                path: Some(path.to_path_buf()),
                rel: Some(rel),
                identity: Some(identity),
                lang,
                bytes: metadata.len(),
            })
        })();
        match result {
            Ok(source) => {
                if self.seen.insert(source.identity.as_ref().unwrap().clone()) {
                    self.sources.push(Ok(source));
                }
            }
            Err(error) => self.error(format!("{error:#}")),
        }
    }
}

fn basename(path: &Path) -> PathBuf {
    path.file_name().map(PathBuf::from).unwrap_or_default()
}

fn detect_lang(path: &Path, lang_override: Option<Lang>) -> Option<Lang> {
    lang_override.or_else(|| {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Lang::from_ext)
    })
}

/// Resolve existing ancestors/symlinks even when the output does not yet exist.
fn resolve_path(path: &Path) -> anyhow::Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(canonical) => return Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("resolve {}", path.display())),
    }
    let absolute = std::path::absolute(path)?;
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            component => {
                resolved.push(component.as_os_str());
                match std::fs::canonicalize(&resolved) {
                    Ok(canonical) => resolved = canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("resolve {}", resolved.display()));
                    }
                }
            }
        }
    }
    Ok(resolved)
}

fn destination(input: &Input, target: &OutputTarget<'_>) -> anyhow::Result<Option<PathBuf>> {
    let path = match target {
        OutputTarget::Stdout => return Ok(None),
        OutputTarget::InPlace => input
            .path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--in-place requires file inputs (not stdin)"))?
            .clone(),
        OutputTarget::Path(path) if path.is_dir() => {
            let rel = input.rel.as_ref().ok_or_else(|| {
                anyhow::anyhow!("cannot derive output filename for stdin and a directory target")
            })?;
            anyhow::ensure!(
                !rel.as_os_str().is_empty()
                    && rel.components().all(|c| matches!(c, Component::Normal(_))),
                "output relative path must stay within the output directory"
            );
            path.join(rel)
        }
        OutputTarget::Path(path) => path.to_path_buf(),
    };
    Ok(Some(resolve_path(&path)?))
}

/// Atomically replace one output, following an existing file symlink and keeping
/// existing permissions. Failed writes leave the previous destination intact.
pub fn write_output(input: &Input, output: &str, target: &OutputTarget<'_>) -> anyhow::Result<()> {
    match destination(input, target)? {
        Some(path) => atomic_write(&path, output),
        None => write_stdout(output),
    }
}

fn write_stdout(output: &str) -> anyhow::Result<()> {
    use std::io::Write;
    std::io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .context("write stdout")
}

fn atomic_write(path: &Path, output: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no output parent for {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create output directory {}", parent.display()))?;
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("inspect output {}", path.display()));
        }
    };
    let mut builder = tempfile::Builder::new();
    builder.prefix(".mangler-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut temporary = builder
        .tempfile_in(parent)
        .with_context(|| format!("create temporary output in {}", parent.display()))?;
    temporary
        .write_all(output.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    if let Some(permissions) = permissions {
        temporary
            .as_file()
            .set_permissions(permissions)
            .with_context(|| format!("preserve permissions for {}", path.display()))?;
    }
    temporary
        .persist(path)
        .map_err(|error| anyhow::anyhow!("replace {}: {error}", path.display()))?;
    Ok(())
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
        let out =
            collect_inputs(&[dir.path().to_string_lossy().into_owned()], Some(Lang::Js)).unwrap();
        assert!(
            out.iter()
                .any(|i| i.path.as_ref().is_some_and(|p| p.ends_with("a.js")))
        );
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

    #[test]
    fn discovery_order_is_sorted_and_repeated_paths_are_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["z.js", "a.js", "m.js"] {
            std::fs::write(dir.path().join(file), "console.log(1);").unwrap();
        }
        let inputs = collect_inputs(
            &[
                dir.path().to_string_lossy().into_owned(),
                dir.path().join("a.js").to_string_lossy().into_owned(),
            ],
            None,
        )
        .unwrap();
        let names: Vec<_> = inputs
            .iter()
            .map(|input| input.rel.as_ref().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["a.js", "m.js", "z.js"]);
    }

    #[test]
    #[cfg(unix)]
    fn output_replacement_is_atomic_for_existing_readers() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.js");
        std::fs::write(&output, "previous output").unwrap();
        let mut previous = std::fs::File::open(&output).unwrap();
        write_output(
            &Input::default(),
            "new output",
            &OutputTarget::Path(&output),
        )
        .unwrap();
        let mut old_contents = String::new();
        previous.read_to_string(&mut old_contents).unwrap();
        assert_eq!(old_contents, "previous output");
        assert_eq!(std::fs::read_to_string(output).unwrap(), "new output");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_atomic_replacement_keeps_destination_and_removes_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("occupied");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("sentinel"), "keep").unwrap();
        assert!(atomic_write(&destination, "new output").is_err());
        assert_eq!(
            std::fs::read_to_string(destination.join("sentinel")).unwrap(),
            "keep"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
