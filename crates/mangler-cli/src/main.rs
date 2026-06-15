//! `mangler` — the only binary. A thin shell over [`mangler_cli::run`]: it owns
//! no transform logic and no plumbing of its own; the [`mangler_cli::Engine`]
//! and CLI driver do all the work.

use std::process::ExitCode;

fn main() -> ExitCode {
    mangler_cli::run()
}
