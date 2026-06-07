#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, missing_docs, rust_2024_compatibility)]

//! Binary entrypoint for `readiness-check`.

use std::io::{self, Write};
use std::process::{ExitCode, Termination};

use clap::Parser;
use readiness_check::{Cli, run_cli};

fn main() -> impl Termination {
    let cli = Cli::parse();
    let outcome = run_cli(&cli);
    if write_output(io::stdout(), outcome.stdout.as_bytes()).is_err()
        || write_output(io::stderr(), outcome.stderr.as_bytes()).is_err()
    {
        return ExitCode::from(1);
    }
    ExitCode::from(outcome.exit_code.as_u8())
}

fn write_output(mut output: impl Write, bytes: &[u8]) -> io::Result<()> {
    output.write_all(bytes)
}
