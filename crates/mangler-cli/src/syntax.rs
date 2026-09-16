//! Exact grammar-goal syntax checks, separate from the transform engine.
//!
//! Only typed parser rejection emits `phase:parse,error:SyntaxError`. A crash,
//! invalid configuration, or input failure cannot be mistaken for acceptance of
//! a negative syntax test. This mode never creates an Engine or executes source.

use crate::{Cli, io};
use mangler_config::Lang;
use mangler_core::Error;
use mangler_jsast::{Js, ParseGoal, ParseOpts};
use std::io::Write;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SyntaxGoal {
    Script,
    Module,
}

impl From<SyntaxGoal> for ParseGoal {
    fn from(goal: SyntaxGoal) -> Self {
        match goal {
            SyntaxGoal::Script => Self::Script,
            SyntaxGoal::Module => Self::Module,
        }
    }
}

fn report(phase: &str, error: Option<&str>) -> anyhow::Result<bool> {
    let mut output = std::io::stdout().lock();
    match error {
        Some(error) => writeln!(output, "{{\"phase\":\"{phase}\",\"error\":\"{error}\"}}")?,
        None => writeln!(output, "{{\"phase\":\"{phase}\",\"error\":null}}")?,
    }
    Ok(error.is_some())
}

fn failure(phase: &str, category: &str, detail: impl std::fmt::Display) -> anyhow::Result<bool> {
    eprintln!("{detail}");
    report(phase, Some(category))
}

pub(crate) fn run(cli: &Cli, goal: SyntaxGoal) -> anyhow::Result<bool> {
    if cli.inputs.len() != 1 || cli.jobs == Some(0) {
        return failure(
            "config",
            "ConfigError",
            "--check-syntax requires one input and a positive worker count",
        );
    }
    if cli.flags.lang.is_some_and(|language| language != Lang::Js)
        || (cli.inputs[0] == "-" && cli.flags.lang != Some(Lang::Js))
    {
        return failure(
            "config",
            "ConfigError",
            "--check-syntax requires JavaScript; stdin requires --lang js",
        );
    }
    let plan = match io::plan_inputs(&cli.inputs, cli.flags.lang, &io::OutputTarget::Stdout) {
        Ok(plan) if plan.len() == 1 => plan,
        Ok(_) => {
            return failure(
                "config",
                "ConfigError",
                "--check-syntax requires exactly one source file",
            );
        }
        Err(error) => return failure("config", "ConfigError", error),
    };
    let input = match plan[0].read() {
        Ok(input) => input,
        Err(error) => return failure("io", "IOError", error),
    };
    if input.lang != Some(Lang::Js) {
        return failure(
            "config",
            "ConfigError",
            "--check-syntax requires a JavaScript input",
        );
    }
    let options = input.path.as_ref().map_or_else(ParseOpts::default, |path| {
        ParseOpts::from_filename(&path.to_string_lossy())
    });
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Js::with_globals(|| Js.parse_with_goal(&input.source, &options, goal.into()))
    }));
    match outcome {
        Ok(Ok(_)) => report("parse", None),
        Ok(Err(error @ Error::Parse { .. })) => failure("parse", "SyntaxError", error),
        Ok(Err(error @ Error::Io(_))) => failure("io", "IOError", error),
        Ok(Err(error @ Error::Config(_))) => failure("config", "ConfigError", error),
        Ok(Err(error @ Error::Transform { .. })) => failure("transform", "TransformError", error),
        Ok(Err(error @ Error::Verify(_))) => failure("internal", "VerificationError", error),
        Err(_) => report("internal", Some("ParserPanic")),
    }
}
