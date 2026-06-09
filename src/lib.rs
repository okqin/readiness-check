#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, missing_docs, rust_2024_compatibility)]

//! Domain parsing, validation, and CLI handling for the `readiness-check` command.

use std::path::PathBuf;

use clap::Parser;

mod diagnostics;
mod intake;
mod readiness_loop;

use diagnostics::{render_config_error, render_configuration_valid, render_readiness_run};
use intake::build_readiness_plan;
use readiness_loop::{ReadinessRun, ReadinessStatus};

/// Parsed command line arguments.
#[derive(Debug, Parser)]
#[command(name = "readiness-check")]
pub struct Cli {
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

/// Process exit code categories used by the CLI.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ExitCode {
    /// All dependencies are ready, or configuration validation succeeded.
    Success,
    /// Readiness did not complete.
    NotReady,
    /// The command line or configuration is invalid.
    ConfigurationError,
}

impl ExitCode {
    /// Return the numeric process exit code.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::NotReady => 1,
            Self::ConfigurationError => 2,
        }
    }
}

/// Complete result of handling a CLI invocation.
#[derive(Debug, Eq, PartialEq)]
pub struct CommandOutcome {
    /// Exit category for the process.
    pub exit_code: ExitCode,
    /// Stable text that should be written to stdout.
    pub stdout: String,
    /// Stable text that should be written to stderr.
    pub stderr: String,
}

/// Handle parsed CLI arguments and return the output contract for the binary.
pub async fn run_cli(cli: &Cli) -> CommandOutcome {
    match build_readiness_plan(cli) {
        Ok(config) if cli.validate_config => CommandOutcome {
            exit_code: ExitCode::Success,
            stdout: String::new(),
            stderr: render_configuration_valid(&config),
        },
        Ok(config) => {
            let run = readiness_loop::run(&config).await;
            readiness_run_outcome(&run)
        }
        Err(error) => CommandOutcome {
            exit_code: ExitCode::ConfigurationError,
            stdout: String::new(),
            stderr: render_config_error(&error),
        },
    }
}

fn readiness_run_outcome(run: &ReadinessRun) -> CommandOutcome {
    let exit_code = match run.status {
        ReadinessStatus::Ready => ExitCode::Success,
        ReadinessStatus::TimedOut
        | ReadinessStatus::Interrupted { .. }
        | ReadinessStatus::RuntimeSetupFailed => ExitCode::NotReady,
    };
    CommandOutcome {
        exit_code,
        stdout: String::new(),
        stderr: render_readiness_run(run),
    }
}
