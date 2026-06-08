use std::collections::HashSet;
use std::fs;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::Duration;

use config::{Config as ConfigFile, File, FileFormat};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::Cli;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(3);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CHECKS: usize = 64;
const MAX_NAME_BYTES: usize = 64;
const MAX_URL_BYTES: usize = 2048;
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ReadinessPlan {
    pub(crate) checks: Vec<ReadinessCheck>,
    pub(crate) interval: Duration,
    pub(crate) max_wait: MaxWait,
    pub(crate) tls_insecure_skip_verify: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct ReadinessCheck {
    pub(crate) name: CheckName,
    pub(crate) url: Url,
    pub(crate) expected_status: NonZeroU16,
    pub(crate) request_timeout: Duration,
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub(crate) struct CheckName(String);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum MaxWait {
    Infinity,
    Finite(Duration),
}

#[derive(Debug, Error, Eq, PartialEq)]
#[error("invalid configuration path={path} error=\"{message}\"")]
pub(crate) struct ConfigError {
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

    pub(crate) fn render_log(&self) -> String {
        format!(
            "readiness-check: invalid configuration path={} error=\"{}\"\n",
            self.path, self.message,
        )
    }
}

pub(crate) fn build_readiness_plan(cli: &Cli) -> Result<ReadinessPlan, ConfigError> {
    validate_input_mode(cli)?;

    let config_file = match &cli.config {
        Some(path) => Some(load_raw_config(path)?),
        None => None,
    };
    let interval = resolve_interval(cli, config_file.as_ref())?;
    let request_timeout = resolve_request_timeout(cli, config_file.as_ref())?;
    let max_wait = resolve_max_wait(cli, config_file.as_ref())?;
    let checks = match config_file.as_ref() {
        Some(config) => parse_config_checks(&config.checks, request_timeout)?,
        None => parse_inline_checks(&cli.checks, request_timeout)?,
    };
    let config_tls_insecure_skip_verify = config_file
        .as_ref()
        .and_then(|config| config.tls.as_ref())
        .is_some_and(|tls| tls.insecure_skip_verify);

    Ok(ReadinessPlan {
        checks,
        interval,
        max_wait,
        tls_insecure_skip_verify: cli.tls_insecure_skip_verify || config_tls_insecure_skip_verify,
    })
}

fn resolve_interval(cli: &Cli, config_file: Option<&RawConfig>) -> Result<Duration, ConfigError> {
    match cli.interval.as_deref() {
        Some(value) => parse_duration(value, DurationKind::Interval, "interval"),
        None => match config_file.and_then(|config| config.interval.as_deref()) {
            Some(value) => parse_duration(value, DurationKind::Interval, "interval"),
            None => Ok(DEFAULT_INTERVAL),
        },
    }
}

fn resolve_request_timeout(
    cli: &Cli,
    config_file: Option<&RawConfig>,
) -> Result<Duration, ConfigError> {
    match cli.request_timeout.as_deref() {
        Some(value) => parse_duration(value, DurationKind::RequestTimeout, "request-timeout"),
        None => match config_file.and_then(|config| config.request_timeout.as_deref()) {
            Some(value) => parse_duration(value, DurationKind::RequestTimeout, "request-timeout"),
            None => Ok(DEFAULT_REQUEST_TIMEOUT),
        },
    }
}

fn resolve_max_wait(cli: &Cli, config_file: Option<&RawConfig>) -> Result<MaxWait, ConfigError> {
    match cli.max_wait.as_deref() {
        Some(value) => parse_max_wait(value, "max-wait"),
        None => match config_file.and_then(|config| config.max_wait.as_deref()) {
            Some(value) => parse_max_wait(value, "max-wait"),
            None => Ok(MaxWait::Infinity),
        },
    }
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
        return Err(invalid_inline_check(index));
    };
    let Some(last_separator) = value.rfind('=') else {
        return Err(invalid_inline_check(index));
    };
    if first_separator == last_separator {
        return Err(invalid_inline_check(index));
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

fn invalid_inline_check(index: usize) -> ConfigError {
    ConfigError::new(
        format!("checks[{index}]"),
        "must use name=url=expected_status",
    )
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

    pub(crate) fn as_str(&self) -> &str {
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

#[cfg(test)]
mod tests {
    use std::fs::File as StdFile;
    use std::io::Write;

    use clap::Parser;
    use tempfile::NamedTempFile;

    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("readiness-check").chain(args.iter().copied()))
    }

    fn config_file(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file
    }

    #[test]
    fn test_should_build_readiness_plan_from_inline_checks() {
        let cli = cli(&[
            "--check",
            "dep=https://example.com/health?tenant=a&ready=true=204",
            "--interval",
            "500ms",
            "--request-timeout",
            "5s",
            "--max-wait",
            "30d",
            "--tls-insecure-skip-verify",
        ]);

        let plan = build_readiness_plan(&cli).unwrap();

        assert_eq!(1, plan.checks.len());
        assert_eq!("dep", plan.checks[0].name.as_str());
        assert_eq!(
            "https://example.com/health?tenant=a&ready=true",
            plan.checks[0].url.as_str()
        );
        assert_eq!(204, plan.checks[0].expected_status.get());
        assert_eq!(Duration::from_secs(5), plan.checks[0].request_timeout);
        assert_eq!(Duration::from_millis(500), plan.interval);
        assert_eq!(
            MaxWait::Finite(Duration::from_secs(30 * SECONDS_PER_DAY)),
            plan.max_wait
        );
        assert!(plan.tls_insecure_skip_verify);
    }

    #[test]
    fn test_should_apply_config_defaults_and_per_check_request_timeout() {
        let config = config_file(
            r#"
checks:
  - name: dep1
    url: http://127.0.0.1:8080/health
    expected-status: 200
  - name: dep2
    url: https://service.internal/ready
    expected-status: 204
    request-timeout: 30s
"#,
        );
        let cli = cli(&["--config", config.path().to_str().unwrap()]);

        let plan = build_readiness_plan(&cli).unwrap();

        assert_eq!(2, plan.checks.len());
        assert_eq!(DEFAULT_INTERVAL, plan.interval);
        assert_eq!(MaxWait::Infinity, plan.max_wait);
        assert_eq!(DEFAULT_REQUEST_TIMEOUT, plan.checks[0].request_timeout);
        assert_eq!(Duration::from_secs(30), plan.checks[1].request_timeout);
        assert!(!plan.tls_insecure_skip_verify);
    }

    #[test]
    fn test_should_apply_cli_overrides_to_config_file() {
        let config = config_file(
            r#"
interval: 3s
request-timeout: 10s
max-wait: 30d
tls:
  insecure-skip-verify: false
checks:
  - name: dep1
    url: http://127.0.0.1:8080/health
    expected-status: 200
"#,
        );
        let cli = cli(&[
            "--config",
            config.path().to_str().unwrap(),
            "--interval",
            "1s",
            "--request-timeout",
            "2s",
            "--max-wait",
            "5s",
            "--tls-insecure-skip-verify",
        ]);

        let plan = build_readiness_plan(&cli).unwrap();

        assert_eq!(Duration::from_secs(1), plan.interval);
        assert_eq!(Duration::from_secs(2), plan.checks[0].request_timeout);
        assert_eq!(MaxWait::Finite(Duration::from_secs(5)), plan.max_wait);
        assert!(plan.tls_insecure_skip_verify);
    }

    #[test]
    fn test_should_reject_config_and_inline_checks_together() {
        let cli = cli(&[
            "--config",
            "service-a.yaml",
            "--check",
            "dep=http://127.0.0.1:8080/health=200",
        ]);

        let error = build_readiness_plan(&cli).unwrap_err();

        assert_eq!(
            "readiness-check: invalid configuration path=input error=\"--config and --check are mutually exclusive\"\n",
            error.render_log()
        );
    }

    #[test]
    fn test_should_reject_unknown_yaml_fields() {
        let config = config_file(
            r#"
checks:
  - name: dep
    url: http://127.0.0.1:8080/health
    expected-status: 200
    method: GET
"#,
        );
        let cli = cli(&["--config", config.path().to_str().unwrap()]);

        let error = build_readiness_plan(&cli).unwrap_err();

        assert!(error.render_log().contains("path=config"));
        assert!(error.render_log().contains("unknown field"));
    }

    #[test]
    fn test_should_reject_duplicate_check_names_after_adapter_parsing() {
        let config = config_file(
            r#"
checks:
  - name: dep
    url: http://127.0.0.1:8080/health
    expected-status: 200
  - name: dep
    url: http://127.0.0.1:8081/health
    expected-status: 204
"#,
        );
        let cli = cli(&["--config", config.path().to_str().unwrap()]);

        let error = build_readiness_plan(&cli).unwrap_err();

        assert_eq!(
            "readiness-check: invalid configuration path=checks[1].name error=\"must be unique\"\n",
            error.render_log()
        );
    }

    #[test]
    fn test_should_reject_inline_check_with_missing_status_separator() {
        let cli = cli(&["--check", "dep=http://127.0.0.1:8080/health"]);

        let error = build_readiness_plan(&cli).unwrap_err();

        assert_eq!(
            "readiness-check: invalid configuration path=checks[0] error=\"must use name=url=expected_status\"\n",
            error.render_log()
        );
    }

    #[test]
    fn test_should_reject_unreadable_config_path() {
        let cli = cli(&["--config", "/tmp/readiness-check-test-does-not-exist.yaml"]);

        let error = build_readiness_plan(&cli).unwrap_err();

        assert_eq!(
            "readiness-check: invalid configuration path=config error=\"file must exist and be readable\"\n",
            error.render_log()
        );
    }

    #[test]
    fn test_should_reject_config_directory() {
        let cli = cli(&["--config", "."]);

        let error = build_readiness_plan(&cli).unwrap_err();

        assert_eq!(
            "readiness-check: invalid configuration path=config error=\"must be a regular file\"\n",
            error.render_log()
        );
    }

    #[test]
    fn test_should_load_empty_yaml_file_as_invalid_config_shape() {
        let mut file = NamedTempFile::new().unwrap();
        StdFile::create(file.path()).unwrap();
        file.write_all(b"").unwrap();
        let cli = cli(&["--config", file.path().to_str().unwrap()]);

        let error = build_readiness_plan(&cli).unwrap_err();

        assert!(error.render_log().contains("path=config"));
    }
}
