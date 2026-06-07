use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::NamedTempFile;

fn readiness_check() -> Command {
    Command::cargo_bin("readiness-check").unwrap()
}

fn write_config(contents: &str) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file
}

fn spawn_one_response_server(status: u16, body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let body = body.to_owned();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).unwrap();
        let response = format!(
            "HTTP/1.1 {status} test\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    format!("http://{address}/ready")
}

fn spawn_sequence_server(statuses: Vec<u16>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    thread::spawn(move || {
        for status in statuses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            let response = format!("HTTP/1.1 {status} test\r\nContent-Length: 0\r\n\r\n");
            stream.write_all(response.as_bytes()).unwrap();
        }
    });

    format!("http://{address}/ready")
}

fn spawn_repeating_server(status: u16, max_requests: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    thread::spawn(move || {
        for _ in 0..max_requests {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            let response = format!("HTTP/1.1 {status} test\r\nContent-Length: 0\r\n\r\n");
            stream.write_all(response.as_bytes()).unwrap();
        }
    });

    format!("http://{address}/ready")
}

fn spawn_overlap_probe_server(
    status: u16,
    delay: Duration,
    active_requests: Arc<AtomicUsize>,
    observed_overlap: Arc<AtomicBool>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).unwrap();
        if active_requests.fetch_add(1, Ordering::SeqCst) > 0 {
            observed_overlap.store(true, Ordering::SeqCst);
        }
        thread::sleep(delay);
        active_requests.fetch_sub(1, Ordering::SeqCst);
        let response = format!("HTTP/1.1 {status} test\r\nContent-Length: 0\r\n\r\n");
        stream.write_all(response.as_bytes()).unwrap();
    });

    format!("http://{address}/ready")
}

fn spawn_redirect_server(location: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let location = location.to_owned();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).unwrap();
        let response =
            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n",);
        stream.write_all(response.as_bytes()).unwrap();
    });

    format!("http://{address}/redirect")
}

fn spawn_headers_without_body_server(status: u16, content_length: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).unwrap();
        let response =
            format!("HTTP/1.1 {status} test\r\nContent-Length: {content_length}\r\n\r\n");
        stream.write_all(response.as_bytes()).unwrap();
        thread::sleep(Duration::from_secs(2));
    });

    format!("http://{address}/slow-body")
}

fn spawn_no_status_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).unwrap();
        thread::sleep(Duration::from_secs(2));
    });

    format!("http://{address}/no-status")
}

#[test]
fn test_should_exit_success_when_single_inline_check_returns_expected_status() {
    let url = spawn_one_response_server(200, "ready");
    let mut command = readiness_check();

    command
        .args(["--check", &format!("dep={url}=200")])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ));
}

#[test]
fn test_should_wait_until_inline_dependency_changes_from_not_ready_to_ready() {
    let url = spawn_sequence_server(vec![503, 200]);
    let mut command = readiness_check();

    command
        .args([
            "--check",
            &format!("dep={url}=200"),
            "--interval",
            "100ms",
            "--max-wait",
            "2s",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep expected=200 actual=503\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ))
        .stderr(predicate::str::contains(&url).not());
}

#[test]
fn test_should_timeout_when_dependency_never_becomes_ready() {
    let url = spawn_repeating_server(503, 8);
    let started_at = Instant::now();
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep={url}=200"),
            "--interval",
            "100ms",
            "--request-timeout",
            "1s",
            "--max-wait",
            "350ms",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: timeout waiting for dependencies",
        ))
        .stderr(predicate::str::contains(&url).not());

    assert!(started_at.elapsed() < Duration::from_secs(1));
}

#[test]
fn test_should_cap_effective_request_timeout_by_remaining_max_wait() {
    let url = spawn_no_status_server();
    let started_at = Instant::now();
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep={url}=200"),
            "--request-timeout",
            "5s",
            "--max-wait",
            "300ms",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep expected=200 error=request-timeout\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: timeout waiting for dependencies",
        ))
        .stderr(predicate::str::contains(&url).not());

    assert!(started_at.elapsed() < Duration::from_secs(2));
}

#[test]
fn test_should_retry_with_explicit_infinite_max_wait() {
    let url = spawn_sequence_server(vec![503, 200]);
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep={url}=200"),
            "--interval",
            "100ms",
            "--max-wait",
            "infinity",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: waiting dependencies=1",
        ))
        .stderr(predicate::str::contains("max-wait=infinity"))
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ))
        .stderr(predicate::str::contains(&url).not());
}

#[test]
fn test_should_wait_for_all_yaml_dependencies_to_be_ready_in_same_round() {
    let dep1_url = spawn_sequence_server(vec![503, 200]);
    let dep2_url = spawn_sequence_server(vec![200, 200]);
    let config = write_config(&format!(
        r#"
interval: 100ms
request-timeout: 1s
max-wait: 2s
checks:
  - name: dep1
    url: {dep1_url}
    expected-status: 200
  - name: dep2
    url: {dep2_url}
    expected-status: 200
"#
    ));
    let mut command = readiness_check();

    command
        .args(["--config", config.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep1 expected=200 actual=503\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ))
        .stderr(predicate::str::contains(&dep1_url).not())
        .stderr(predicate::str::contains(&dep2_url).not());
}

#[test]
fn test_should_timeout_when_one_dependency_is_ready_and_one_is_not_ready() {
    let ready_url = spawn_repeating_server(200, 8);
    let not_ready_url = spawn_repeating_server(503, 8);
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("ready={ready_url}=200"),
            "--check",
            &format!("not-ready={not_ready_url}=200"),
            "--interval",
            "100ms",
            "--request-timeout",
            "1s",
            "--max-wait",
            "350ms",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=not-ready expected=200 actual=503\n",
        ))
        .stderr(predicate::str::contains("readiness-check: all dependencies ready").not())
        .stderr(predicate::str::contains(&ready_url).not())
        .stderr(predicate::str::contains(&not_ready_url).not());
}

#[test]
fn test_should_not_latch_ready_state_across_rounds() {
    let dep1_url = spawn_sequence_server(vec![200, 503, 200]);
    let dep2_url = spawn_sequence_server(vec![503, 200, 200]);
    let mut command = readiness_check();

    command
        .args([
            "--check",
            &format!("dep1={dep1_url}=200"),
            "--check",
            &format!("dep2={dep2_url}=200"),
            "--interval",
            "100ms",
            "--request-timeout",
            "1s",
            "--max-wait",
            "2s",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep2 expected=200 actual=503\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep1 expected=200 actual=503\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ))
        .stderr(predicate::str::contains(&dep1_url).not())
        .stderr(predicate::str::contains(&dep2_url).not());
}

#[test]
fn test_should_check_dependencies_concurrently_within_each_round() {
    let active_requests = Arc::new(AtomicUsize::new(0));
    let observed_overlap = Arc::new(AtomicBool::new(false));
    let dep1_url = spawn_overlap_probe_server(
        200,
        Duration::from_millis(300),
        Arc::clone(&active_requests),
        Arc::clone(&observed_overlap),
    );
    let dep2_url = spawn_overlap_probe_server(
        200,
        Duration::from_millis(300),
        Arc::clone(&active_requests),
        Arc::clone(&observed_overlap),
    );
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep1={dep1_url}=200"),
            "--check",
            &format!("dep2={dep2_url}=200"),
            "--request-timeout",
            "2s",
            "--max-wait",
            "2s",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ))
        .stderr(predicate::str::contains(&dep1_url).not())
        .stderr(predicate::str::contains(&dep2_url).not());

    assert!(observed_overlap.load(Ordering::SeqCst));
}

#[test]
fn test_should_exit_not_ready_without_printing_url_when_status_differs() {
    let url = spawn_one_response_server(503, "not ready");
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args(["--check", &format!("dep={url}=200"), "--max-wait", "100ms"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep expected=200 actual=503\n",
        ))
        .stderr(predicate::str::contains(&url).not());
}

#[test]
fn test_should_compare_redirect_status_without_following_location() {
    let url = spawn_redirect_server("http://127.0.0.1:1/followed");
    let mut command = readiness_check();

    command
        .args(["--check", &format!("dep={url}=302")])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ));
}

#[test]
fn test_should_not_wait_for_or_log_response_body() {
    let url = spawn_headers_without_body_server(200, 1_000_000);
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(1));

    command
        .args(["--check", &format!("dep={url}=200")])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("slow-body").not())
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ));
}

#[test]
fn test_should_apply_request_timeout_while_waiting_for_status() {
    let url = spawn_no_status_server();
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep={url}=200"),
            "--request-timeout",
            "50ms",
            "--max-wait",
            "60ms",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep expected=200 error=request-timeout\n",
        ))
        .stderr(predicate::str::contains(&url).not());
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
