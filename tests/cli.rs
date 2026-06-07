use std::io::Write;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::NamedTempFile;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn readiness_check() -> Command {
    Command::cargo_bin("readiness-check").unwrap()
}

fn write_config(contents: &str) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file
}

#[test]
fn test_should_validate_inline_check_without_running_http_requests() {
    let mut command = readiness_check();

    command
        .args([
            "--check",
            "dep=https://example.com/health?tenant=a&ready=true=200",
            "--validate-config",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: configuration valid dependencies=1 max-wait=infinity tls-insecure-skip-verify=false\n",
        ));
}

#[test]
fn test_should_validate_yaml_config_without_running_http_requests() {
    let config = write_config(
        r#"
interval: 3s
request-timeout: 10s
max-wait: infinity
tls:
  insecure-skip-verify: false
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
    let mut command = readiness_check();

    command
        .args(["--config", config.path().to_str().unwrap(), "--validate-config"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: configuration valid dependencies=2 max-wait=infinity tls-insecure-skip-verify=false\n",
        ));
}

#[test]
fn test_should_reject_unknown_yaml_fields() {
    let config = write_config(
        r#"
checks:
  - name: dep1
    url: http://127.0.0.1:8080/health
    expected-status: 200
    method: GET
"#,
    );
    let mut command = readiness_check();

    command
        .args([
            "--config",
            config.path().to_str().unwrap(),
            "--validate-config",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=config",
        ))
        .stderr(predicate::str::contains("unknown field"));
}

#[test]
fn test_should_apply_cli_global_overrides_to_yaml_config() {
    let config = write_config(
        r#"
max-wait: 30d
tls:
  insecure-skip-verify: false
checks:
  - name: dep1
    url: http://127.0.0.1:8080/health
    expected-status: 200
"#,
    );
    let mut command = readiness_check();

    command
        .args([
            "--config",
            config.path().to_str().unwrap(),
            "--max-wait",
            "5s",
            "--tls-insecure-skip-verify",
            "--validate-config",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: configuration valid dependencies=1 max-wait=5000ms tls-insecure-skip-verify=true\n",
        ));
}

#[test]
fn test_should_let_cli_request_timeout_override_invalid_yaml_global_timeout() {
    let config = write_config(
        r#"
request-timeout: 0s
checks:
  - name: dep1
    url: http://127.0.0.1:8080/health
    expected-status: 200
"#,
    );
    let mut command = readiness_check();

    command
        .args([
            "--config",
            config.path().to_str().unwrap(),
            "--request-timeout",
            "1s",
            "--validate-config",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: configuration valid dependencies=1",
        ));
}

#[test]
fn test_should_validate_per_check_request_timeout_in_yaml_config() {
    let config = write_config(
        r#"
request-timeout: 1s
checks:
  - name: dep1
    url: http://127.0.0.1:8080/health
    expected-status: 200
    request-timeout: 0s
"#,
    );
    let mut command = readiness_check();

    command
        .args(["--config", config.path().to_str().unwrap(), "--validate-config"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=checks[0].request-timeout error=\"duration must be greater than zero\"\n",
        ));
}

#[test]
fn test_should_reject_empty_yaml_checks() {
    let config = write_config(
        r#"
checks: []
"#,
    );
    let mut command = readiness_check();

    command
        .args(["--config", config.path().to_str().unwrap(), "--validate-config"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=checks error=\"must contain 1..64 entries\"\n",
        ));
}

#[test]
fn test_should_reject_yaml_check_missing_expected_status() {
    let config = write_config(
        r#"
checks:
  - name: dep1
    url: http://127.0.0.1:8080/health
"#,
    );
    let mut command = readiness_check();

    command
        .args([
            "--config",
            config.path().to_str().unwrap(),
            "--validate-config",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=config",
        ))
        .stderr(predicate::str::contains("missing configuration field"));
}

#[test]
fn test_should_apply_yaml_config_defaults() {
    let config = write_config(
        r#"
checks:
  - name: dep1
    url: http://127.0.0.1:8080/health
    expected-status: 200
"#,
    );
    let mut command = readiness_check();

    command
        .args(["--config", config.path().to_str().unwrap(), "--validate-config"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: configuration valid dependencies=1 max-wait=infinity tls-insecure-skip-verify=false\n",
        ));
}

#[test]
fn test_should_reject_config_and_inline_checks_together() {
    let mut command = readiness_check();

    command
        .args([
            "--config",
            "service-a.yaml",
            "--check",
            "dep=http://127.0.0.1:8080/health=200",
            "--validate-config",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=input error=\"--config and --check are mutually exclusive\"\n",
        ));
}

#[test]
fn test_should_reject_duplicate_inline_check_names() {
    let mut command = readiness_check();

    command
        .args([
            "--check",
            "dep=http://127.0.0.1:8080/health=200",
            "--check",
            "dep=http://127.0.0.1:8081/health=204",
            "--validate-config",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=checks[1].name error=\"must be unique\"\n",
        ));
}

#[test]
fn test_should_reject_invalid_inline_check_name() {
    let mut command = readiness_check();

    command
        .args(["--check", "-prod=http://127.0.0.1:8080/health=200", "--validate-config"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=checks[0].name error=\"must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$\"\n",
        ));
}

#[test]
fn test_should_reject_url_userinfo() {
    let mut command = readiness_check();

    command
        .args([
            "--check",
            "dep=http://user:pass@example.com/health=200",
            "--validate-config",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=checks[0].url error=\"userinfo is not allowed\"\n",
        ));
}

#[test]
fn test_should_reject_expected_status_outside_http_range() {
    let mut command = readiness_check();

    command
        .args([
            "--check",
            "dep=http://127.0.0.1:8080/health=600",
            "--validate-config",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=checks[0].expected-status error=\"must be between 100 and 599\"\n",
        ));
}

#[test]
fn test_should_reject_infinity_for_request_timeout() {
    let mut command = readiness_check();

    command
        .args([
            "--check",
            "dep=http://127.0.0.1:8080/health=200",
            "--request-timeout",
            "infinity",
            "--validate-config",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=request-timeout error=\"infinity is only allowed for max-wait\"\n",
        ));
}

#[test]
fn test_should_reject_interval_below_range() {
    let mut command = readiness_check();

    command
        .args([
            "--check",
            "dep=http://127.0.0.1:8080/health=200",
            "--interval",
            "99ms",
            "--validate-config",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=interval error=\"duration must be 100ms..=1h\"\n",
        ));
}

#[test]
fn test_should_reject_missing_input_mode() {
    let mut command = readiness_check();

    command
        .arg("--validate-config")
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=input error=\"either --config or --check is required\"\n",
        ));
}

#[test]
fn test_should_reject_more_than_sixty_four_inline_checks() {
    let mut command = readiness_check();
    let mut args = vec!["--validate-config".to_owned()];
    for index in 0..65 {
        args.push("--check".to_owned());
        args.push(format!(
            "dep{index}=http://127.0.0.1:{}/health=200",
            8_000 + index
        ));
    }

    command
        .args(args)
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=checks error=\"must contain 1..64 entries\"\n",
        ));
}

#[test]
fn test_should_reject_non_http_url_scheme() {
    let mut command = readiness_check();

    command
        .args(["--check", "dep=file:///tmp/health=200", "--validate-config"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=checks[0].url error=\"scheme must be http or https\"\n",
        ));
}

#[test]
fn test_should_reject_invalid_duration_syntax() {
    let mut command = readiness_check();

    command
        .args([
            "--check",
            "dep=http://127.0.0.1:8080/health=200",
            "--interval",
            "1m30s",
            "--validate-config",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=interval error=\"duration unit must be one of ms, s, m, h, d\"\n",
        ));
}

#[test]
fn test_should_reject_max_wait_above_range() {
    let mut command = readiness_check();

    command
        .args([
            "--check",
            "dep=http://127.0.0.1:8080/health=200",
            "--max-wait",
            "31d",
            "--validate-config",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: invalid configuration path=max-wait error=\"duration must be 1ms..=30d or infinity\"\n",
        ));
}

#[test]
fn test_should_accept_valid_inline_durations() {
    let mut command = readiness_check();

    command
        .args([
            "--check",
            "dep=http://127.0.0.1:8080/health=200",
            "--interval",
            "500ms",
            "--request-timeout",
            "5m",
            "--max-wait",
            "30d",
            "--validate-config",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: configuration valid dependencies=1",
        ));
}

#[tokio::test]
async fn test_should_exit_success_when_single_http_check_is_ready() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let mut command = readiness_check();

    command
        .args([
            "--check",
            &format!("dep={}/health=200", server.uri()),
            "--request-timeout",
            "1s",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ));
}

#[tokio::test]
async fn test_should_exit_not_ready_when_http_status_does_not_match() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let check = format!("dep={}/health=200", server.uri());
    let mut command = readiness_check();

    command
        .args(["--check", &check, "--request-timeout", "1s"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep expected=200 actual=503\n",
        ))
        .stderr(predicate::str::contains(server.uri()).not());
}

#[tokio::test]
async fn test_should_not_follow_redirects() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/redirect"))
        .respond_with(ResponseTemplate::new(301).insert_header("Location", "/health"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let mut command = readiness_check();

    command
        .args([
            "--check",
            &format!("dep={}/redirect=200", server.uri()),
            "--request-timeout",
            "1s",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep expected=200 actual=301\n",
        ));
}

#[tokio::test]
async fn test_should_apply_request_timeout_to_http_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
        .mount(&server)
        .await;
    let mut command = readiness_check();

    command
        .args([
            "--check",
            &format!("dep={}/slow=200", server.uri()),
            "--request-timeout",
            "1ms",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep expected=200 error=request-timeout\n",
        ))
        .stderr(predicate::str::contains(server.uri()).not());
}

#[tokio::test]
async fn test_should_check_multiple_dependencies_concurrently_in_one_round() {
    let server = MockServer::start().await;
    for endpoint in ["/slow-a", "/slow-b", "/slow-c"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
            .mount(&server)
            .await;
    }
    let mut command = readiness_check();

    let started_at = Instant::now();
    command
        .args([
            "--check",
            &format!("dep-a={}/slow-a=200", server.uri()),
            "--check",
            &format!("dep-b={}/slow-b=200", server.uri()),
            "--check",
            &format!("dep-c={}/slow-c=200", server.uri()),
            "--request-timeout",
            "700ms",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep-a expected=200 error=request-timeout\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep-b expected=200 error=request-timeout\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep-c expected=200 error=request-timeout\n",
        ));

    assert!(
        started_at.elapsed() < Duration::from_millis(1500),
        "checks should run concurrently within one round",
    );
}

#[tokio::test]
async fn test_should_not_match_or_log_response_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/body"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not ready http://secret.local"))
        .mount(&server)
        .await;
    let mut command = readiness_check();

    command
        .args([
            "--check",
            &format!("dep={}/body=200", server.uri()),
            "--request-timeout",
            "1s",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("not ready").not())
        .stderr(predicate::str::contains("secret.local").not())
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ));
}
