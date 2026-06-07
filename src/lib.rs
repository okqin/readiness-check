#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, missing_docs, rust_2024_compatibility)]

//! Domain parsing and validation for the `readiness-check` command.

use std::collections::HashSet;
use std::error::Error as _;
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use config::{Config as ConfigFile, File, FileFormat};
use reqwest::{Client, redirect};
use serde::Deserialize;
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio::time;
use url::Url;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(3);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CHECKS: usize = 64;
const MAX_NAME_BYTES: usize = 64;
const MAX_URL_BYTES: usize = 2048;
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;

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

/// Validated effective configuration used by future readiness execution slices.
#[derive(Debug, Eq, PartialEq)]
pub struct EffectiveConfig {
    checks: Vec<ReadinessCheck>,
    interval: Duration,
    request_timeout: Duration,
    max_wait: MaxWait,
    tls_insecure_skip_verify: bool,
}

/// A validated readiness dependency check.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReadinessCheck {
    name: CheckName,
    url: Url,
    expected_status: NonZeroU16,
    request_timeout: Duration,
}

/// Validated check name used in logs and diagnostics.
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub struct CheckName(String);

/// Total wait budget for readiness execution.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MaxWait {
    /// Wait forever.
    Infinity,
    /// Wait for the supplied finite duration.
    Finite(Duration),
}

/// Error with a precise configuration path.
#[derive(Debug, Error, Eq, PartialEq)]
#[error("invalid configuration path={path} error=\"{message}\"")]
pub struct ConfigError {
    path: String,
    message: String,
}

impl ConfigError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }

    fn render_log(&self) -> String {
        format!(
            "readiness-check: invalid configuration path={} error=\"{}\"\n",
            self.path, self.message,
        )
    }
}

/// Handle parsed CLI arguments and return the output contract for the binary.
pub async fn run_cli(cli: &Cli) -> CommandOutcome {
    match build_effective_config(cli) {
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

async fn run_wait_loop(config: &EffectiveConfig) -> CommandOutcome {
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

        let outcomes = execute_round(&client, &config.checks, &round_timeouts).await;
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
        time::sleep(sleep_duration).await;
    }
}

fn round_timeouts(config: &EffectiveConfig, started_at: Instant) -> Option<Vec<Duration>> {
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

fn next_sleep_duration(config: &EffectiveConfig, started_at: Instant) -> Option<Duration> {
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

fn build_http_client(tls_insecure_skip_verify: bool) -> Result<Client, CheckExecutionError> {
    Client::builder()
        .redirect(redirect::Policy::none())
        .danger_accept_invalid_certs(tls_insecure_skip_verify)
        .no_proxy()
        .build()
        .map_err(CheckExecutionError::from)
}

async fn execute_round(
    client: &Client,
    checks: &[ReadinessCheck],
    request_timeouts: &[Duration],
) -> Vec<CheckOutcome> {
    let mut handles: Vec<JoinHandle<CheckOutcome>> = Vec::with_capacity(checks.len());
    for (check, request_timeout) in checks.iter().zip(request_timeouts) {
        let client = client.clone();
        let check = check.clone();
        let request_timeout = *request_timeout;
        handles.push(tokio::spawn(async move {
            execute_check(&client, &check, request_timeout).await
        }));
    }

    let mut outcomes = Vec::with_capacity(handles.len());
    for handle in handles {
        outcomes.push(match handle.await {
            Ok(outcome) => outcome,
            Err(_error) => CheckOutcome::request_error(),
        });
    }
    outcomes
}

async fn execute_check(
    client: &Client,
    check: &ReadinessCheck,
    request_timeout: Duration,
) -> CheckOutcome {
    match time::timeout(request_timeout, client.get(check.url.clone()).send()).await {
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
    if message.contains("certificate") || message.contains("tls") {
        return CheckExecutionError::Tls;
    }
    if message.contains("refused") {
        return CheckExecutionError::ConnectionRefused;
    }
    CheckExecutionError::RequestError
}

fn build_effective_config(cli: &Cli) -> Result<EffectiveConfig, ConfigError> {
    validate_input_mode(cli)?;

    let config_file = match &cli.config {
        Some(path) => Some(load_raw_config(path)?),
        None => None,
    };
    let interval = match cli.interval.as_deref() {
        Some(value) => parse_duration(value, DurationKind::Interval, "interval")?,
        None => match config_file
            .as_ref()
            .and_then(|config| config.interval.as_deref())
        {
            Some(value) => parse_duration(value, DurationKind::Interval, "interval")?,
            None => DEFAULT_INTERVAL,
        },
    };
    let request_timeout = match cli.request_timeout.as_deref() {
        Some(value) => parse_duration(value, DurationKind::RequestTimeout, "request-timeout")?,
        None => match config_file
            .as_ref()
            .and_then(|config| config.request_timeout.as_deref())
        {
            Some(value) => parse_duration(value, DurationKind::RequestTimeout, "request-timeout")?,
            None => DEFAULT_REQUEST_TIMEOUT,
        },
    };
    let max_wait = match cli.max_wait.as_deref() {
        Some(value) => parse_max_wait(value, "max-wait")?,
        None => match config_file
            .as_ref()
            .and_then(|config| config.max_wait.as_deref())
        {
            Some(value) => parse_max_wait(value, "max-wait")?,
            None => MaxWait::Infinity,
        },
    };
    let checks = match config_file.as_ref() {
        Some(config) => parse_config_checks(&config.checks, request_timeout)?,
        None => parse_inline_checks(&cli.checks, request_timeout)?,
    };
    let config_tls_insecure_skip_verify = config_file
        .as_ref()
        .and_then(|config| config.tls.as_ref())
        .is_some_and(|tls| tls.insecure_skip_verify);

    Ok(EffectiveConfig {
        checks,
        interval,
        request_timeout,
        max_wait,
        tls_insecure_skip_verify: cli.tls_insecure_skip_verify || config_tls_insecure_skip_verify,
    })
}

fn validate_input_mode(cli: &Cli) -> Result<(), ConfigError> {
    match (&cli.config, cli.checks.is_empty()) {
        (Some(_), false) => Err(ConfigError::new(
            "input",
            "--config and --check are mutually exclusive",
        )),
        (None, true) => Err(ConfigError::new(
            "input",
            "either --config or --check is required",
        )),
        (Some(_), true) | (None, false) => Ok(()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RawConfig {
    interval: Option<String>,
    request_timeout: Option<String>,
    max_wait: Option<String>,
    tls: Option<RawTls>,
    checks: Vec<RawCheck>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RawTls {
    #[serde(default)]
    insecure_skip_verify: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RawCheck {
    name: String,
    url: String,
    expected_status: String,
    request_timeout: Option<String>,
}

fn load_raw_config(path: &PathBuf) -> Result<RawConfig, ConfigError> {
    let metadata = fs::metadata(path)
        .map_err(|_error| ConfigError::new("config", "file must exist and be readable"))?;
    if !metadata.is_file() {
        return Err(ConfigError::new("config", "must be a regular file"));
    }

    ConfigFile::builder()
        .add_source(File::from(path.clone()).format(FileFormat::Yaml))
        .build()
        .and_then(ConfigFile::try_deserialize::<RawConfig>)
        .map_err(|error| ConfigError::new("config", error.to_string()))
}

fn parse_config_checks(
    values: &[RawCheck],
    global_request_timeout: Duration,
) -> Result<Vec<ReadinessCheck>, ConfigError> {
    if values.is_empty() || values.len() > MAX_CHECKS {
        return Err(ConfigError::new("checks", "must contain 1..64 entries"));
    }

    let mut seen_names = HashSet::with_capacity(values.len());
    let mut checks = Vec::with_capacity(values.len());

    for (index, value) in values.iter().enumerate() {
        let check = parse_config_check(value, index, global_request_timeout)?;
        if !seen_names.insert(check.name.clone()) {
            return Err(ConfigError::new(
                format!("checks[{index}].name"),
                "must be unique",
            ));
        }
        checks.push(check);
    }

    Ok(checks)
}

fn parse_config_check(
    value: &RawCheck,
    index: usize,
    global_request_timeout: Duration,
) -> Result<ReadinessCheck, ConfigError> {
    let request_timeout = match value.request_timeout.as_deref() {
        Some(raw) => parse_duration(
            raw,
            DurationKind::RequestTimeout,
            &format!("checks[{index}].request-timeout"),
        )?,
        None => global_request_timeout,
    };

    Ok(ReadinessCheck {
        name: CheckName::parse(&value.name, format!("checks[{index}].name"))?,
        url: parse_url(&value.url, format!("checks[{index}].url"))?,
        expected_status: parse_expected_status(
            &value.expected_status,
            format!("checks[{index}].expected-status"),
        )?,
        request_timeout,
    })
}

fn parse_inline_checks(
    values: &[String],
    request_timeout: Duration,
) -> Result<Vec<ReadinessCheck>, ConfigError> {
    if values.len() > MAX_CHECKS {
        return Err(ConfigError::new("checks", "must contain 1..64 entries"));
    }

    let mut seen_names = HashSet::with_capacity(values.len());
    let mut checks = Vec::with_capacity(values.len());

    for (index, value) in values.iter().enumerate() {
        let check = parse_inline_check(value, index, request_timeout)?;
        if !seen_names.insert(check.name.clone()) {
            return Err(ConfigError::new(
                format!("checks[{index}].name"),
                "must be unique",
            ));
        }
        checks.push(check);
    }

    Ok(checks)
}

fn parse_inline_check(
    value: &str,
    index: usize,
    request_timeout: Duration,
) -> Result<ReadinessCheck, ConfigError> {
    let Some(first_separator) = value.find('=') else {
        return Err(ConfigError::new(
            format!("checks[{index}]"),
            "must use name=url=expected_status",
        ));
    };
    let Some(last_separator) = value.rfind('=') else {
        return Err(ConfigError::new(
            format!("checks[{index}]"),
            "must use name=url=expected_status",
        ));
    };
    if first_separator == last_separator {
        return Err(ConfigError::new(
            format!("checks[{index}]"),
            "must use name=url=expected_status",
        ));
    }

    let name = CheckName::parse(&value[..first_separator], format!("checks[{index}].name"))?;
    let url = parse_url(
        &value[first_separator + 1..last_separator],
        format!("checks[{index}].url"),
    )?;
    let expected_status = parse_expected_status(
        &value[last_separator + 1..],
        format!("checks[{index}].expected-status"),
    )?;

    Ok(ReadinessCheck {
        name,
        url,
        expected_status,
        request_timeout,
    })
}

impl CheckName {
    fn parse(value: &str, path: String) -> Result<Self, ConfigError> {
        if value.is_empty() {
            return Err(ConfigError::new(path, "must not be empty"));
        }
        if value.len() > MAX_NAME_BYTES {
            return Err(ConfigError::new(path, "must be at most 64 bytes"));
        }

        let mut bytes = value.bytes();
        let Some(first) = bytes.next() else {
            return Err(ConfigError::new(path, "must not be empty"));
        };
        if !first.is_ascii_alphanumeric()
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(ConfigError::new(
                path,
                "must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$",
            ));
        }

        Ok(Self(value.to_owned()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

fn parse_url(value: &str, path: String) -> Result<Url, ConfigError> {
    if value.len() > MAX_URL_BYTES {
        return Err(ConfigError::new(path, "must be at most 2048 bytes"));
    }

    let url = Url::parse(value)
        .map_err(|_error| ConfigError::new(path.clone(), "must be a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::new(path, "scheme must be http or https"));
    }
    if url.host().is_none() {
        return Err(ConfigError::new(path, "host is required"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::new(path, "userinfo is not allowed"));
    }

    Ok(url)
}

fn parse_expected_status(value: &str, path: String) -> Result<NonZeroU16, ConfigError> {
    let status = value
        .parse::<u16>()
        .map_err(|_error| ConfigError::new(path.clone(), "must be between 100 and 599"))?;
    if !(100..=599).contains(&status) {
        return Err(ConfigError::new(path, "must be between 100 and 599"));
    }
    NonZeroU16::new(status).ok_or_else(|| ConfigError::new(path, "must be between 100 and 599"))
}

#[derive(Debug, Clone, Copy)]
enum DurationKind {
    Interval,
    RequestTimeout,
    MaxWait,
}

fn parse_max_wait(value: &str, path: &str) -> Result<MaxWait, ConfigError> {
    if value == "infinity" {
        return Ok(MaxWait::Infinity);
    }
    Ok(MaxWait::Finite(parse_duration(
        value,
        DurationKind::MaxWait,
        path,
    )?))
}

fn parse_duration(value: &str, kind: DurationKind, path: &str) -> Result<Duration, ConfigError> {
    if value == "infinity" {
        return Err(ConfigError::new(
            path,
            "infinity is only allowed for max-wait",
        ));
    }

    let (number, unit) = split_duration(value)
        .ok_or_else(|| ConfigError::new(path, "duration must be a positive integer plus unit"))?;
    let magnitude = number.parse::<u64>().map_err(|_error| {
        ConfigError::new(path, "duration must be a positive integer plus unit")
    })?;
    if magnitude == 0 {
        return Err(ConfigError::new(path, "duration must be greater than zero"));
    }

    let duration = duration_from_parts(magnitude, unit)
        .ok_or_else(|| ConfigError::new(path, "duration unit must be one of ms, s, m, h, d"))?;
    validate_duration_range(duration, kind, path)?;
    Ok(duration)
}

fn split_duration(value: &str) -> Option<(&str, &str)> {
    let unit_start = value.find(|character: char| !character.is_ascii_digit())?;
    if unit_start == 0 {
        return None;
    }
    let (number, unit) = value.split_at(unit_start);
    if number.is_empty() || unit.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((number, unit))
}

fn duration_from_parts(magnitude: u64, unit: &str) -> Option<Duration> {
    match unit {
        "ms" => Some(Duration::from_millis(magnitude)),
        "s" => Some(Duration::from_secs(magnitude)),
        "m" => magnitude
            .checked_mul(SECONDS_PER_MINUTE)
            .map(Duration::from_secs),
        "h" => magnitude
            .checked_mul(SECONDS_PER_HOUR)
            .map(Duration::from_secs),
        "d" => magnitude
            .checked_mul(SECONDS_PER_DAY)
            .map(Duration::from_secs),
        _ => None,
    }
}

fn validate_duration_range(
    duration: Duration,
    kind: DurationKind,
    path: &str,
) -> Result<(), ConfigError> {
    let (minimum, maximum, message) = match kind {
        DurationKind::Interval => (
            Duration::from_millis(100),
            Duration::from_secs(SECONDS_PER_HOUR),
            "duration must be 100ms..=1h",
        ),
        DurationKind::RequestTimeout => (
            Duration::from_millis(1),
            Duration::from_secs(5 * SECONDS_PER_MINUTE),
            "duration must be 1ms..=5m",
        ),
        DurationKind::MaxWait => (
            Duration::from_millis(1),
            Duration::from_secs(30 * SECONDS_PER_DAY),
            "duration must be 1ms..=30d or infinity",
        ),
    };

    if duration < minimum || duration > maximum {
        return Err(ConfigError::new(path, message));
    }
    Ok(())
}

impl std::fmt::Display for MaxWait {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Infinity => formatter.write_str("infinity"),
            Self::Finite(duration) => write!(formatter, "{}ms", duration.as_millis()),
        }
    }
}
