//! `mangler-cli` — the [`Engine`] façade plus the thin clap CLI.
//!
//! This crate is two things layered cleanly:
//!
//! 1. A **library** ([`Engine`], [`Input`], [`Output`], [`Stats`]) usable from a
//!    web app or another binary. The engine owns the language-dispatch seam, the
//!    HTML embed-handler wiring, and the batch driver (rayon parallelism +
//!    result ordering + per-unit stats). It returns structured results and
//!    **never prints or exits** — that is the caller's job.
//! 2. A **thin CLI** ([`run`]) that does only argv/io plumbing around the engine:
//!    argv → [`mangler_config::ResolvedConfig`] → [`Engine`] → read inputs
//!    (stdin/files/globs) → validate destinations → bounded load/transform/write → print
//!    `--verbose` notes + a size report to stderr → exit code.
//!
//! No transform logic lives in the CLI layer; all of it is behind the per-language
//! processors the [`Engine`] dispatches to.

pub mod config;
pub mod engine;
pub mod io;
pub mod syntax;

pub use engine::{Engine, Input, Output, Stats};
pub use mangler_config::{ConfigFlags, ResolvedConfig};

use clap::Parser;
use io::OutputTarget;
use std::path::PathBuf;
use std::process::ExitCode;

/// Parsed command-line arguments: the CLI-shaped plumbing args plus the flattened
/// [`ConfigFlags`] (the single obfuscation-flag declaration shared with the TOML
/// surface).
#[derive(Parser, Debug)]
#[command(
    name = "mangler",
    about = "Mangle HTML/CSS/JS into unreadable, functionally identical output",
    after_help = "EXAMPLES:
  # Read JS from stdin, write mangled output to stdout
  cat app.js | mangler - --lang js --preset high

  # Mangle all JS/CSS files in src/ into dist/ with a fixed seed
  mangler src/ -o dist/ --preset max --seed 42

  # Mangle all CSS in-place (overwrite source files)
  mangler \"assets/**/*.css\" --in-place

EXIT CODES:
  0  All inputs processed successfully.
  1  One or more inputs failed (parse error, I/O error, or --verify failure).
     With --keep-going, failed inputs are skipped and the exit code reflects
     whether any failures occurred."
)]
pub struct Cli {
    /// Files, directories, glob patterns, or '-' for stdin. Glob patterns must be
    /// quoted in the shell (e.g. 'src/**/*.js'). Directories are recursed.
    #[arg(required = true)]
    pub inputs: Vec<String>,

    /// Write output to this path. For multiple inputs it must be an existing
    /// directory; defaults to stdout for a single input unless --in-place.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Overwrite each input file with its mangled output. Conflicts with --output.
    #[arg(long, conflicts_with = "output")]
    pub in_place: bool,

    /// Load defaults from a TOML config file. Explicit CLI flags override it.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Number of parallel worker threads. Defaults to logical CPU count; 1
    /// disables parallelism.
    #[arg(short, long)]
    pub jobs: Option<usize>,

    /// Print non-error notes and a per-file size report to stderr.
    #[arg(short, long)]
    pub verbose: bool,

    /// Continue after a per-file error instead of aborting. The exit code is
    /// still non-zero if any input failed.
    #[arg(long)]
    pub keep_going: bool,

    /// Check one JS input with the exact Script or Module grammar and emit a
    /// JSON diagnostic. No transforms or source execution are performed.
    #[arg(long, value_enum, conflicts_with_all = ["output", "in_place", "config", "keep_going"])]
    pub check_syntax: Option<syntax::SyntaxGoal>,

    /// The shared obfuscation flags (also settable via --config TOML).
    #[command(flatten)]
    pub flags: ConfigFlags,
}

impl Cli {
    /// The destination for outputs from the mutually-exclusive
    /// `--in-place`/`--output` flags (stdout when neither is set).
    fn output_target(&self) -> OutputTarget<'_> {
        if self.in_place {
            OutputTarget::InPlace
        } else if let Some(p) = &self.output {
            OutputTarget::Path(p.as_path())
        } else {
            OutputTarget::Stdout
        }
    }
}

/// Program entry point the binary calls. Parses argv, runs the engine, and maps
/// the outcome to an exit code. This is the ONLY place that prints (to stderr)
/// and chooses an exit code; the [`Engine`] does neither.
pub fn run() -> ExitCode {
    match try_run(Cli::parse()) {
        Ok(false) => ExitCode::SUCCESS,
        Ok(true) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// Run with explicit argv — the testable entry point (no `std::process::exit`).
///
/// Returns `Ok(had_errors)` so callers can choose an exit code, or `Err` for a
/// fatal top-level problem (bad flags, config, or ambiguous output destinations)
/// that aborts before per-file processing.
pub fn run_args<I, T>(argv: I) -> anyhow::Result<bool>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    try_run(Cli::try_parse_from(argv)?)
}

/// The real driver. Owns ONLY plumbing: config resolution, input collection,
/// output-target validation, handing work to the [`Engine`], rendering notes and
/// the size report, and writing outputs. Inputs and outputs are held only for a
/// worker-sized batch; the engine owns the single parallel scheduling mechanism.
fn try_run(cli: Cli) -> anyhow::Result<bool> {
    if let Some(goal) = cli.check_syntax {
        return syntax::run(&cli, goal);
    }
    let config = config::resolve(cli.flags.clone(), cli.config.as_deref())?;
    if cli.jobs == Some(0) {
        anyhow::bail!("--jobs must be at least 1");
    }
    let plan = io::plan_inputs(&cli.inputs, config.engine.lang, &cli.output_target())?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cli.jobs.unwrap_or(0))
        .build()?;
    let workers = pool.current_num_threads();
    let engine = Engine::new(config);
    let mut had_errors = false;
    let mut start = 0;
    while start < plan.len() {
        let end = batch_end(&plan, start, workers);
        let batch = &plan[start..end];
        let results = pool.install(|| engine.process_loaded(batch, io::PlannedItem::read));
        for (item, result) in batch.iter().zip(results) {
            match result {
                Ok(output) => {
                    if cli.verbose {
                        for note in &output.notes {
                            eprintln!("[note] {}: {}", item.name(), note.message);
                        }
                        eprintln!(
                            "[size] {}: {} -> {} bytes ({:.2}x)",
                            item.name(),
                            output.stats.input_bytes,
                            output.stats.output_bytes,
                            output.stats.ratio(),
                        );
                    }
                    if let Err(error) = item.write(&output.code) {
                        eprintln!("{error:#}");
                        had_errors = true;
                        if !cli.keep_going {
                            return Ok(true);
                        }
                    }
                }
                Err(error) => {
                    eprintln!("{error:#}");
                    had_errors = true;
                    if !cli.keep_going {
                        return Ok(true);
                    }
                }
            }
        }
        start = end;
    }
    Ok(had_errors)
}

/// At most one input per worker, with a 16 MiB source-size budget. A single
/// larger file gets its own batch; metadata estimates do not cap file size.
fn batch_end(plan: &[io::PlannedItem], start: usize, workers: usize) -> usize {
    const SOURCE_BUDGET: u64 = 16 * 1024 * 1024;
    let mut end = start;
    let mut bytes = 0u64;
    while end < plan.len() && end - start < workers {
        let next = plan[end].bytes();
        if end > start && bytes.saturating_add(next) > SOURCE_BUDGET {
            break;
        }
        bytes = bytes.saturating_add(next);
        end += 1;
        if bytes >= SOURCE_BUDGET {
            break;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_inputs_and_flattened_flags() {
        let cli =
            Cli::try_parse_from(["mangler", "a.js", "--preset", "max", "--seed", "5"]).unwrap();
        assert_eq!(cli.inputs, vec!["a.js"]);
        assert_eq!(cli.flags.seed, Some(5));
    }

    #[test]
    fn in_place_conflicts_with_output() {
        let r = Cli::try_parse_from(["mangler", "a.js", "--in-place", "-o", "out/"]);
        assert!(r.is_err());
    }

    #[test]
    fn batch_limits_follow_workers_and_large_sources_run_alone() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("a.js", 1),
            ("b.js", 1),
            ("c.js", 20 * 1024 * 1024),
            ("d.js", 1),
        ] {
            std::fs::File::create(dir.path().join(name))
                .unwrap()
                .set_len(bytes)
                .unwrap();
        }
        let plan = io::plan_inputs(
            &[dir.path().to_string_lossy().into_owned()],
            None,
            &OutputTarget::InPlace,
        )
        .unwrap();
        assert_eq!(batch_end(&plan, 0, 1), 1);
        assert_eq!(batch_end(&plan, 0, 8), 2);
        assert_eq!(batch_end(&plan, 2, 8), 3);
        assert_eq!(batch_end(&plan, 3, 8), 4);
    }
}
