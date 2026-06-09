#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, missing_docs, rust_2024_compatibility)]

//! Binary-facing entrypoint for the `readiness-check` command.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

mod diagnostics;
mod intake;
mod readiness_loop;

use diagnostics::{render_config_error, render_configuration_valid, render_readiness_run};
use intake::build_readiness_plan;
use readiness_loop::{ReadinessRun, ReadinessStatus};

#[derive(Debug, Parser)]
#[command(name = "readiness-check")]
struct Cli {
    /// YAML configuration file path.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Inline readiness check in `name=url=expected_status` format.
    #[arg(
        long = "check",
        value_name = "name=url=expected_status",
        allow_hyphen_values = true
    )]
    checks: Vec<String>,

    /// Sleep duration between check rounds (default: 3s).
    #[arg(long)]
    interval: Option<String>,

    /// Per-request timeout for checks without a more specific value (default: 10s).
    #[arg(long = "request-timeout")]
    request_timeout: Option<String>,

    /// Total wait budget, or `infinity` (default: infinity).
    #[arg(long = "max-wait")]
    max_wait: Option<String>,

    /// Disable TLS certificate verification globally.
    #[arg(long = "tls-insecure-skip-verify")]
    tls_insecure_skip_verify: bool,

    /// Validate configuration and exit without running checks.
    #[arg(long = "validate-config")]
    validate_config: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CommandExitCode {
    Success,
    NotReady,
    ConfigurationError,
}

impl CommandExitCode {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::NotReady => 1,
            Self::ConfigurationError => 2,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CommandOutcome {
    exit_code: CommandExitCode,
    stdout: String,
    stderr: String,
}

/// Run the `readiness-check` binary workflow.
///
/// This is the crate's public Rust entrypoint for the binary target. The stable
/// contract remains the command line, YAML configuration, diagnostics, and
/// process exit codes.
pub async fn run() -> ExitCode {
    let cli = Cli::parse();
    let outcome = run_cli(&cli).await;
    if write_output(io::stdout(), outcome.stdout.as_bytes()).is_err()
        || write_output(io::stderr(), outcome.stderr.as_bytes()).is_err()
    {
        return ExitCode::from(1);
    }
    ExitCode::from(outcome.exit_code.as_u8())
}

async fn run_cli(cli: &Cli) -> CommandOutcome {
    match build_readiness_plan(cli) {
        Ok(config) if cli.validate_config => CommandOutcome {
            exit_code: CommandExitCode::Success,
            stdout: String::new(),
            stderr: render_configuration_valid(config.configuration_summary()),
        },
        Ok(config) => {
            let run = readiness_loop::run(&config).await;
            readiness_run_outcome(&run)
        }
        Err(error) => CommandOutcome {
            exit_code: CommandExitCode::ConfigurationError,
            stdout: String::new(),
            stderr: render_config_error(&error),
        },
    }
}

fn write_output(mut output: impl Write, bytes: &[u8]) -> io::Result<()> {
    output.write_all(bytes)
}

fn readiness_run_outcome(run: &ReadinessRun) -> CommandOutcome {
    let exit_code = match run.status {
        ReadinessStatus::Ready => CommandExitCode::Success,
        ReadinessStatus::TimedOut
        | ReadinessStatus::Interrupted { .. }
        | ReadinessStatus::RuntimeSetupFailed => CommandExitCode::NotReady,
    };
    CommandOutcome {
        exit_code,
        stdout: String::new(),
        stderr: render_readiness_run(run),
    }
}
