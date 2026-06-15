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
//!    (stdin/files/globs) → [`Engine::process_many`] → write outputs → print
//!    `--verbose` notes + a size report to stderr → exit code.
//!
//! No transform logic lives in the CLI layer; all of it is behind the per-language
//! processors the [`Engine`] dispatches to.

pub mod config;
pub mod engine;
pub mod io;

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
/// fatal top-level problem (bad flags, config, multi-input output misuse, input
/// collection) that aborts before per-file processing.
pub fn run_args<I, T>(argv: I) -> anyhow::Result<bool>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    try_run(Cli::try_parse_from(argv)?)
}

/// The real driver. Owns ONLY plumbing: config resolution, input collection,
/// output-target validation, handing work to the [`Engine`], rendering notes and
/// the size report, and writing outputs. All parallelism/ordering/stats live in
/// [`Engine::process_many`].
fn try_run(cli: Cli) -> anyhow::Result<bool> {
    let config = config::resolve(cli.flags.clone(), cli.config.as_deref())?;
    let lang_override = config.engine.lang;

    if let Some(j) = cli.jobs {
        rayon::ThreadPoolBuilder::new().num_threads(j).build_global().ok();
    }

    let inputs = io::collect_inputs(&cli.inputs, lang_override)?;
    let target = cli.output_target();

    // Writing N>1 inputs to a single output FILE would silently discard all but
    // the last. Require a directory target for multi-input.
    if inputs.len() > 1
        && let OutputTarget::Path(p) = &target
        && !p.is_dir()
    {
        anyhow::bail!("--output must name an existing directory when processing multiple inputs");
    }

    let engine = Engine::new(config);
    let results = engine.process_many(&inputs);

    let mut had_errors = false;
    for (input, result) in inputs.iter().zip(results) {
        match result {
            Ok(output) => {
                if cli.verbose {
                    for note in &output.notes {
                        eprintln!("[note] {}", note.message);
                    }
                    eprintln!(
                        "[size] {}: {} -> {} bytes ({:.2}x)",
                        input.name(),
                        output.stats.input_bytes,
                        output.stats.output_bytes,
                        output.stats.ratio(),
                    );
                }
                if let Err(e) = io::write_output(input, &output.code, &target) {
                    eprintln!("{e}");
                    had_errors = true;
                }
            }
            Err(e) => {
                eprintln!("{}: {e}", input.name());
                had_errors = true;
                if !cli.keep_going {
                    return Ok(true);
                }
            }
        }
    }
    Ok(had_errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_inputs_and_flattened_flags() {
        let cli = Cli::try_parse_from(["mangler", "a.js", "--preset", "max", "--seed", "5"])
            .unwrap();
        assert_eq!(cli.inputs, vec!["a.js"]);
        assert_eq!(cli.flags.seed, Some(5));
    }

    #[test]
    fn in_place_conflicts_with_output() {
        let r = Cli::try_parse_from(["mangler", "a.js", "--in-place", "-o", "out/"]);
        assert!(r.is_err());
    }
}
