#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, missing_docs, rust_2024_compatibility)]

//! Domain parsing and validation for the `readiness-check` command.

use std::error::Error as _;
use std::fmt::Write as _;
use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use reqwest::{Client, redirect};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::task::JoinSet;
use tokio::time;

mod intake;

use intake::{MaxWait, ReadinessCheck, ReadinessPlan, build_readiness_plan};

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
        Ok(config) => run_wait_loop(&config).await,
        Err(error) => CommandOutcome {
            exit_code: ExitCode::ConfigurationError,
            stdout: String::new(),
            stderr: error.render_log(),
        },
    }
}

async fn run_wait_loop(config: &ReadinessPlan) -> CommandOutcome {
    let started_at = Instant::now();
    let client = match build_http_client(config.tls_insecure_skip_verify) {
        Ok(client) => client,
        Err(error) => {
            return CommandOutcome {
                exit_code: ExitCode::NotReady,
                stdout: String::new(),
                stderr: format!(
                    "readiness-check: HTTP client setup failed error={}\n",
                    error.classify(),
                ),
            };
        }
    };

    let mut termination_signals = match TerminationSignals::new() {
        Ok(signals) => signals,
        Err(_error) => return signal_setup_failed_outcome(),
    };

    let mut stderr = String::new();
    let _ = writeln!(
        stderr,
        "readiness-check: waiting dependencies={} interval={}ms max-wait={} tls-insecure-skip-verify={}",
        config.checks.len(),
        config.interval.as_millis(),
        config.max_wait,
        config.tls_insecure_skip_verify,
    );

    let mut observed_states = vec![None; config.checks.len()];
    let mut reported_not_ready = vec![false; config.checks.len()];

    loop {
        let Some(round_timeouts) = round_timeouts(config, started_at) else {
            return timeout_outcome(started_at, stderr);
        };

        let outcomes = match execute_round(
            &client,
            &config.checks,
            &round_timeouts,
            &mut termination_signals,
        )
        .await
        {
            RoundResult::Completed(outcomes) => outcomes,
            RoundResult::Interrupted(received_signal) => {
                return interrupted_outcome(received_signal, started_at, stderr);
            }
        };
        let mut all_ready = true;
        let mut not_ready_count = 0_usize;

        for (index, (check, outcome)) in config.checks.iter().zip(outcomes).enumerate() {
            let current_state = outcome.observed_state();
            let previous_state = observed_states[index];
            if !outcome.ready {
                all_ready = false;
                not_ready_count += 1;
                match previous_state {
                    _ if !reported_not_ready[index] => {
                        stderr.push_str(&outcome.render_not_ready(check));
                        reported_not_ready[index] = true;
                    }
                    Some(previous) if previous != current_state => {
                        stderr.push_str(&outcome.render_state_changed(check));
                    }
                    Some(_) | None => {}
                }
            } else if previous_state.is_some_and(|previous| previous != current_state) {
                stderr.push_str(&outcome.render_state_changed(check));
            }
            observed_states[index] = Some(current_state);
        }

        if all_ready {
            let _ = writeln!(
                stderr,
                "readiness-check: all dependencies ready elapsed={}ms",
                started_at.elapsed().as_millis(),
            );
            return CommandOutcome {
                exit_code: ExitCode::Success,
                stdout: String::new(),
                stderr,
            };
        }

        let _ = writeln!(
            stderr,
            "readiness-check: still waiting not-ready={} elapsed={}ms",
            not_ready_count,
            started_at.elapsed().as_millis(),
        );

        let Some(sleep_duration) = next_sleep_duration(config, started_at) else {
            return timeout_outcome(started_at, stderr);
        };
        tokio::select! {
            received_signal = termination_signals.recv() => {
                return interrupted_outcome(received_signal, started_at, stderr);
            }
            () = time::sleep(sleep_duration) => {}
        }
    }
}

fn round_timeouts(config: &ReadinessPlan, started_at: Instant) -> Option<Vec<Duration>> {
    match config.max_wait {
        MaxWait::Infinity => Some(
            config
                .checks
                .iter()
                .map(|check| check.request_timeout)
                .collect(),
        ),
        MaxWait::Finite(max_wait) => {
            let remaining = remaining_wait(started_at, max_wait)?;
            Some(
                config
                    .checks
                    .iter()
                    .map(|check| check.request_timeout.min(remaining))
                    .collect(),
            )
        }
    }
}

fn next_sleep_duration(config: &ReadinessPlan, started_at: Instant) -> Option<Duration> {
    match config.max_wait {
        MaxWait::Infinity => Some(config.interval),
        MaxWait::Finite(max_wait) => {
            let remaining = remaining_wait(started_at, max_wait)?;
            Some(config.interval.min(remaining))
        }
    }
}

fn remaining_wait(started_at: Instant, max_wait: Duration) -> Option<Duration> {
    let elapsed = started_at.elapsed();
    if elapsed >= max_wait {
        return None;
    }
    max_wait.checked_sub(elapsed)
}

fn timeout_outcome(started_at: Instant, mut stderr: String) -> CommandOutcome {
    let _ = writeln!(
        stderr,
        "readiness-check: timeout waiting for dependencies elapsed={}ms",
        started_at.elapsed().as_millis(),
    );
    CommandOutcome {
        exit_code: ExitCode::NotReady,
        stdout: String::new(),
        stderr,
    }
}

fn signal_setup_failed_outcome() -> CommandOutcome {
    CommandOutcome {
        exit_code: ExitCode::NotReady,
        stdout: String::new(),
        stderr: "readiness-check: signal setup failed error=signal-unavailable\n".to_owned(),
    }
}

fn interrupted_outcome(
    received_signal: ReceivedSignal,
    started_at: Instant,
    mut stderr: String,
) -> CommandOutcome {
    let _ = writeln!(
        stderr,
        "readiness-check: interrupted signal={} elapsed={}ms",
        received_signal.as_str(),
        started_at.elapsed().as_millis(),
    );
    CommandOutcome {
        exit_code: ExitCode::NotReady,
        stdout: String::new(),
        stderr,
    }
}

fn build_http_client(tls_insecure_skip_verify: bool) -> Result<Client, CheckExecutionError> {
    Client::builder()
        .redirect(redirect::Policy::none())
        .tls_danger_accept_invalid_certs(tls_insecure_skip_verify)
        .no_proxy()
        .build()
        .map_err(CheckExecutionError::from)
}

async fn execute_round(
    client: &Client,
    checks: &[ReadinessCheck],
    request_timeouts: &[Duration],
    termination_signals: &mut TerminationSignals,
) -> RoundResult {
    let mut tasks: JoinSet<(usize, CheckOutcome)> = JoinSet::new();
    for (index, (check, request_timeout)) in checks.iter().zip(request_timeouts).enumerate() {
        let client = client.clone();
        let check = check.clone();
        let request_timeout = *request_timeout;
        tasks.spawn(async move { (index, execute_check(&client, &check, request_timeout).await) });
    }

    let mut outcomes = Vec::with_capacity(checks.len());
    outcomes.resize_with(checks.len(), || None);
    let mut remaining_tasks = checks.len();

    while remaining_tasks > 0 {
        tokio::select! {
            received_signal = termination_signals.recv() => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                return RoundResult::Interrupted(received_signal);
            }
            joined = tasks.join_next() => {
                remaining_tasks -= 1;
                match joined {
                    Some(Ok((index, outcome))) => {
                        if let Some(slot) = outcomes.get_mut(index) {
                            *slot = Some(outcome);
                        }
                    }
                    Some(Err(_error)) => {}
                    None => {}
                }
            }
        }
    }

    RoundResult::Completed(
        outcomes
            .into_iter()
            .map(|outcome| outcome.unwrap_or_else(CheckOutcome::request_error))
            .collect(),
    )
}

#[derive(Debug)]
enum RoundResult {
    Completed(Vec<CheckOutcome>),
    Interrupted(ReceivedSignal),
}

#[derive(Debug)]
struct TerminationSignals {
    sigterm: Signal,
    sigint: Signal,
}

impl TerminationSignals {
    fn new() -> io::Result<Self> {
        Ok(Self {
            sigterm: signal(SignalKind::terminate())?,
            sigint: signal(SignalKind::interrupt())?,
        })
    }

    async fn recv(&mut self) -> ReceivedSignal {
        tokio::select! {
            _ = self.sigterm.recv() => ReceivedSignal::Sigterm,
            _ = self.sigint.recv() => ReceivedSignal::Sigint,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ReceivedSignal {
    Sigterm,
    Sigint,
}

impl ReceivedSignal {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Sigterm => "SIGTERM",
            Self::Sigint => "SIGINT",
        }
    }
}

async fn execute_check(
    client: &Client,
    check: &ReadinessCheck,
    request_timeout: Duration,
) -> CheckOutcome {
    let request = client
        .get(check.url.clone())
        .timeout(request_timeout)
        .send();
    match time::timeout(request_timeout, request).await {
        Ok(Ok(response)) => {
            let actual_status = response.status().as_u16();
            CheckOutcome {
                ready: actual_status == check.expected_status.get(),
                actual_status: Some(actual_status),
                error: None,
            }
        }
        Ok(Err(error)) => CheckOutcome {
            ready: false,
            actual_status: None,
            error: Some(CheckExecutionError::from(error)),
        },
        Err(_elapsed) => CheckOutcome {
            ready: false,
            actual_status: None,
            error: Some(CheckExecutionError::RequestTimeout),
        },
    }
}

#[derive(Debug)]
struct CheckOutcome {
    ready: bool,
    actual_status: Option<u16>,
    error: Option<CheckExecutionError>,
}

impl CheckOutcome {
    const fn request_error() -> Self {
        Self {
            ready: false,
            actual_status: None,
            error: Some(CheckExecutionError::RequestError),
        }
    }

    fn render_not_ready(&self, check: &ReadinessCheck) -> String {
        let name = check.name.as_str();
        let expected = check.expected_status.get();
        match (self.actual_status, &self.error) {
            (Some(actual), None | Some(_)) => format!(
                "readiness-check: dependency not ready name={name} expected={expected} actual={actual}\n",
            ),
            (None, Some(error)) => format!(
                "readiness-check: dependency not ready name={name} expected={expected} error={}\n",
                error.classify(),
            ),
            (None, None) => format!(
                "readiness-check: dependency not ready name={name} expected={expected} error=request-error\n",
            ),
        }
    }

    fn render_state_changed(&self, check: &ReadinessCheck) -> String {
        let name = check.name.as_str();
        let expected = check.expected_status.get();
        let ready = self.ready;
        match (self.actual_status, &self.error) {
            (Some(actual), None | Some(_)) => format!(
                "readiness-check: dependency state changed name={name} expected={expected} actual={actual} ready={ready}\n",
            ),
            (None, Some(error)) => format!(
                "readiness-check: dependency state changed name={name} expected={expected} error={} ready={ready}\n",
                error.classify(),
            ),
            (None, None) => format!(
                "readiness-check: dependency state changed name={name} expected={expected} error=request-error ready={ready}\n",
            ),
        }
    }

    const fn observed_state(&self) -> ObservedState {
        if self.ready {
            return ObservedState::Ready;
        }
        match (self.actual_status, self.error) {
            (Some(actual_status), None | Some(_)) => ObservedState::Status(actual_status),
            (None, Some(error)) => ObservedState::Error(error),
            (None, None) => ObservedState::Error(CheckExecutionError::RequestError),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ObservedState {
    Ready,
    Status(u16),
    Error(CheckExecutionError),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CheckExecutionError {
    RequestTimeout,
    Dns,
    ConnectionRefused,
    ConnectionClosed,
    Tls,
    HttpProtocol,
    RequestError,
}

impl CheckExecutionError {
    const fn classify(self) -> &'static str {
        match self {
            Self::RequestTimeout => "request-timeout",
            Self::Dns => "dns",
            Self::ConnectionRefused => "connection-refused",
            Self::ConnectionClosed => "connection-closed",
            Self::Tls => "tls",
            Self::HttpProtocol => "http-protocol",
            Self::RequestError => "request-error",
        }
    }
}

impl From<reqwest::Error> for CheckExecutionError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            return Self::RequestTimeout;
        }
        if is_tls_error(&error) {
            return Self::Tls;
        }
        if error.is_connect() {
            return classify_connect_error(&error);
        }
        if error.is_body() {
            return Self::ConnectionClosed;
        }
        if error.is_request() {
            return Self::HttpProtocol;
        }
        Self::RequestError
    }
}

fn is_tls_error(error: &reqwest::Error) -> bool {
    if message_indicates_tls(&error.to_string()) {
        return true;
    }

    let mut source = error.source();
    while let Some(error) = source {
        if message_indicates_tls(&error.to_string()) {
            return true;
        }
        source = error.source();
    }

    false
}

fn message_indicates_tls(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("certificate")
        || message.contains("cert")
        || message.contains("tls")
        || message.contains("webpki")
        || message.contains("rustls")
}

fn classify_connect_error(error: &reqwest::Error) -> CheckExecutionError {
    let mut source = error.source();
    while let Some(error) = source {
        if let Some(io_error) = error.downcast_ref::<std::io::Error>() {
            match io_error.kind() {
                ErrorKind::ConnectionRefused => return CheckExecutionError::ConnectionRefused,
                ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
                    return CheckExecutionError::ConnectionClosed;
                }
                _ => {}
            }
        }
        source = error.source();
    }

    let message = error.to_string().to_ascii_lowercase();
    if message.contains("dns") || message.contains("resolve") {
        return CheckExecutionError::Dns;
    }
    if message.contains("refused") {
        return CheckExecutionError::ConnectionRefused;
    }
    CheckExecutionError::RequestError
}
