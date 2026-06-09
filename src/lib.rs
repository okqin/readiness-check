#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, missing_docs, rust_2024_compatibility)]

//! Domain parsing, validation, and CLI handling for the `readiness-check` command.

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::Parser;

mod intake;
mod readiness_loop;

use intake::build_readiness_plan;
use readiness_loop::{ObservedState, ReadinessEvent, ReadinessRun, ReadinessStatus};

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

    /// Sleep duration between check rounds.
    #[arg(long)]
    interval: Option<String>,

    /// Per-request timeout for checks without a more specific value.
    #[arg(long = "request-timeout")]
    request_timeout: Option<String>,

    /// Total wait budget, or `infinity`.
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
            stderr: format!(
                "readiness-check: configuration valid dependencies={} max-wait={} tls-insecure-skip-verify={}\n",
                config.checks.len(),
                config.max_wait,
                config.tls_insecure_skip_verify,
            ),
        },
        Ok(config) => {
            let run = readiness_loop::run(&config).await;
            readiness_run_outcome(&run)
        }
        Err(error) => CommandOutcome {
            exit_code: ExitCode::ConfigurationError,
            stdout: String::new(),
            stderr: error.render_log(),
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
        stderr: render_readiness_events(&run.events),
    }
}

fn render_readiness_events(events: &[ReadinessEvent]) -> String {
    let mut stderr = String::new();
    for event in events {
        match event {
            ReadinessEvent::WaitingStarted {
                dependencies,
                interval,
                max_wait,
                tls_insecure_skip_verify,
            } => {
                let _ = writeln!(
                    stderr,
                    "readiness-check: waiting dependencies={} interval={}ms max-wait={} tls-insecure-skip-verify={}",
                    dependencies,
                    interval.as_millis(),
                    max_wait,
                    tls_insecure_skip_verify,
                );
            }
            ReadinessEvent::DependencyNotReady {
                name,
                expected_status,
                state,
            } => stderr.push_str(&render_dependency_not_ready(name, *expected_status, *state)),
            ReadinessEvent::DependencyStateChanged {
                name,
                expected_status,
                state,
                ready,
            } => stderr.push_str(&render_dependency_state_changed(
                name,
                *expected_status,
                *state,
                *ready,
            )),
            ReadinessEvent::StillWaiting { not_ready, elapsed } => {
                let _ = writeln!(
                    stderr,
                    "readiness-check: still waiting not-ready={} elapsed={}ms",
                    not_ready,
                    elapsed.as_millis(),
                );
            }
            ReadinessEvent::AllReady { elapsed } => {
                let _ = writeln!(
                    stderr,
                    "readiness-check: all dependencies ready elapsed={}ms",
                    elapsed.as_millis(),
                );
            }
            ReadinessEvent::TimedOut { elapsed } => {
                let _ = writeln!(
                    stderr,
                    "readiness-check: timeout waiting for dependencies elapsed={}ms",
                    elapsed.as_millis(),
                );
            }
            ReadinessEvent::Interrupted { signal, elapsed } => {
                let _ = writeln!(
                    stderr,
                    "readiness-check: interrupted signal={} elapsed={}ms",
                    signal.as_str(),
                    elapsed.as_millis(),
                );
            }
            ReadinessEvent::HttpClientSetupFailed { error } => {
                let _ = writeln!(
                    stderr,
                    "readiness-check: HTTP client setup failed error={}",
                    error.classify(),
                );
            }
            ReadinessEvent::SignalSetupFailed => {
                stderr.push_str("readiness-check: signal setup failed error=signal-unavailable\n");
            }
        }
    }
    stderr
}

fn render_dependency_not_ready(name: &str, expected_status: u16, state: ObservedState) -> String {
    match state {
        ObservedState::Ready | ObservedState::Status(_) => {
            let actual = actual_status_or_expected(state, expected_status);
            format!(
                "readiness-check: dependency not ready name={name} expected={expected_status} actual={actual}\n",
            )
        }
        ObservedState::Error(error) => format!(
            "readiness-check: dependency not ready name={name} expected={expected_status} error={}\n",
            error.classify(),
        ),
    }
}

fn render_dependency_state_changed(
    name: &str,
    expected_status: u16,
    state: ObservedState,
    ready: bool,
) -> String {
    match state {
        ObservedState::Ready | ObservedState::Status(_) => {
            let actual = actual_status_or_expected(state, expected_status);
            format!(
                "readiness-check: dependency state changed name={name} expected={expected_status} actual={actual} ready={ready}\n",
            )
        }
        ObservedState::Error(error) => format!(
            "readiness-check: dependency state changed name={name} expected={expected_status} error={} ready={ready}\n",
            error.classify(),
        ),
    }
}

const fn actual_status_or_expected(state: ObservedState, expected_status: u16) -> u16 {
    match state {
        ObservedState::Status(actual_status) => actual_status,
        ObservedState::Ready | ObservedState::Error(_) => expected_status,
    }
}
