use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use rcgen::generate_simple_self_signed;
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use tempfile::NamedTempFile;

fn readiness_check() -> Command {
    Command::cargo_bin("readiness-check").unwrap()
}

fn example_config_path() -> String {
    format!(
        "{}/examples/service-a.readiness.yaml",
        env!("CARGO_MANIFEST_DIR"),
    )
}

fn write_config(contents: &str) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file
}

mod http_dependencies {
    use super::*;

    #[derive(Debug)]
    struct HttpResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl HttpResponse {
        fn status(status: u16) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: String::new(),
            }
        }

        fn with_body(status: u16, body: &str) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.to_owned(),
            }
        }

        fn redirect(location: &str) -> Self {
            Self {
                status: 302,
                headers: vec![("Location".to_owned(), location.to_owned())],
                body: String::new(),
            }
        }

        fn headers_only(status: u16, content_length: usize) -> String {
            format!("HTTP/1.1 {status} test\r\nContent-Length: {content_length}\r\n\r\n")
        }

        fn serialize(&self) -> String {
            let mut response = format!("HTTP/1.1 {} test\r\n", self.status);
            for (name, value) in &self.headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
            response.push_str(&self.body);
            response
        }
    }

    #[derive(Debug)]
    pub struct HttpDependency {
        url: String,
    }

    impl HttpDependency {
        pub fn fixed_status_with_body(status: u16, body: &str) -> Self {
            Self::single_response("/ready", HttpResponse::with_body(status, body))
        }

        pub fn status_sequence<I>(statuses: I) -> Self
        where
            I: IntoIterator<Item = u16>,
            I::IntoIter: Send + 'static,
        {
            let responses = statuses.into_iter().map(HttpResponse::status);
            Self::response_sequence("/ready", responses)
        }

        pub fn repeating_status(status: u16, max_requests: usize) -> Self {
            Self::observed_repeating_status(status, max_requests, Arc::new(AtomicUsize::new(0)))
        }

        pub fn redirecting_to(location: &str) -> Self {
            Self::single_response("/redirect", HttpResponse::redirect(location))
        }

        pub fn headers_without_body(status: u16, content_length: usize) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();

            thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_request(&mut stream);
                let response = HttpResponse::headers_only(status, content_length);
                stream.write_all(response.as_bytes()).unwrap();
                thread::sleep(Duration::from_secs(2));
            });

            Self {
                url: format!("http://{address}/slow-body"),
            }
        }

        pub fn without_status() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();

            thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_request(&mut stream);
                thread::sleep(Duration::from_secs(2));
            });

            Self {
                url: format!("http://{address}/no-status"),
            }
        }

        pub fn self_signed_tls_status(status: u16, max_requests: usize) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let certified_key = generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
            let certificate = certified_key.cert.der().clone();
            let private_key =
                PrivatePkcs8KeyDer::from(certified_key.signing_key.serialize_der()).into();
            let server_config = Arc::new(
                ServerConfig::builder()
                    .with_no_client_auth()
                    .with_single_cert(vec![certificate], private_key)
                    .unwrap(),
            );

            thread::spawn(move || {
                for _ in 0..max_requests {
                    let (stream, _) = listener.accept().unwrap();
                    let connection = ServerConnection::new(Arc::clone(&server_config)).unwrap();
                    let mut tls_stream = StreamOwned::new(connection, stream);
                    if read_request(&mut tls_stream).is_err() {
                        continue;
                    }
                    let response = HttpResponse::status(status).serialize();
                    let _ = tls_stream.write_all(response.as_bytes());
                }
            });

            Self {
                url: format!("https://{address}/ready"),
            }
        }

        pub fn unavailable_with_secret() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            drop(listener);

            Self {
                url: format!("http://{address}/unavailable?secret=token"),
            }
        }

        pub fn url(&self) -> &str {
            &self.url
        }

        fn single_response(path: &str, response: HttpResponse) -> Self {
            Self::response_sequence(path, [response])
        }

        fn response_sequence<I>(path: &str, responses: I) -> Self
        where
            I: IntoIterator<Item = HttpResponse>,
            I::IntoIter: Send + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let responses = responses.into_iter();

            thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    let _ = read_request(&mut stream);
                    stream.write_all(response.serialize().as_bytes()).unwrap();
                }
            });

            Self {
                url: format!("http://{address}{path}"),
            }
        }

        fn observed_repeating_status(
            status: u16,
            max_requests: usize,
            request_count: Arc<AtomicUsize>,
        ) -> Self {
            let responses =
                std::iter::repeat_with(move || HttpResponse::status(status)).take(max_requests);
            Self::observed_response_sequence("/ready", responses, request_count)
        }

        fn observed_response_sequence<I>(
            path: &str,
            responses: I,
            request_count: Arc<AtomicUsize>,
        ) -> Self
        where
            I: IntoIterator<Item = HttpResponse>,
            I::IntoIter: Send + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let responses = responses.into_iter();

            thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    let _ = read_request(&mut stream);
                    request_count.fetch_add(1, Ordering::SeqCst);
                    stream.write_all(response.serialize().as_bytes()).unwrap();
                }
            });

            Self {
                url: format!("http://{address}{path}"),
            }
        }
    }

    impl std::fmt::Display for HttpDependency {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.url)
        }
    }

    #[derive(Debug)]
    pub struct ObservedHttpDependency {
        dependency: HttpDependency,
        request_count: Arc<AtomicUsize>,
    }

    impl ObservedHttpDependency {
        pub fn repeating_status(status: u16, max_requests: usize) -> Self {
            let request_count = Arc::new(AtomicUsize::new(0));
            let dependency = HttpDependency::observed_repeating_status(
                status,
                max_requests,
                Arc::clone(&request_count),
            );
            Self {
                dependency,
                request_count,
            }
        }

        pub fn wait_for_requests(&self, expected: usize, timeout: Duration) {
            let started_at = Instant::now();
            while self.request_count.load(Ordering::SeqCst) < expected
                && started_at.elapsed() < timeout
            {
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(expected, self.request_count.load(Ordering::SeqCst));
        }

        pub fn url(&self) -> &str {
            self.dependency.url()
        }
    }

    #[derive(Debug)]
    pub struct InFlightHttpDependency {
        dependency: HttpDependency,
        active_requests: Arc<AtomicUsize>,
        observed_overlap: Arc<AtomicBool>,
    }

    impl InFlightHttpDependency {
        pub fn delayed_status(status: u16, delay: Duration) -> Self {
            let active_requests = Arc::new(AtomicUsize::new(0));
            let observed_overlap = Arc::new(AtomicBool::new(false));
            let dependency = spawn_delayed_status_dependency(
                status,
                delay,
                Arc::clone(&active_requests),
                Arc::clone(&observed_overlap),
            );
            Self {
                dependency,
                active_requests,
                observed_overlap,
            }
        }

        pub fn wait_until_in_flight(&self, timeout: Duration) {
            let started_at = Instant::now();
            while self.active_requests.load(Ordering::SeqCst) == 0 && started_at.elapsed() < timeout
            {
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(1, self.active_requests.load(Ordering::SeqCst));
        }

        pub fn observed_overlap(&self) -> bool {
            self.observed_overlap.load(Ordering::SeqCst)
        }

        pub fn url(&self) -> &str {
            self.dependency.url()
        }
    }

    #[derive(Debug)]
    pub struct ConcurrentRoundProbe {
        active_requests: Arc<AtomicUsize>,
        observed_overlap: Arc<AtomicBool>,
    }

    impl ConcurrentRoundProbe {
        pub fn new() -> Self {
            Self {
                active_requests: Arc::new(AtomicUsize::new(0)),
                observed_overlap: Arc::new(AtomicBool::new(false)),
            }
        }

        pub fn delayed_ready_dependency(&self, delay: Duration) -> HttpDependency {
            spawn_delayed_status_dependency(
                200,
                delay,
                Arc::clone(&self.active_requests),
                Arc::clone(&self.observed_overlap),
            )
        }

        pub fn observed_overlap(&self) -> bool {
            self.observed_overlap.load(Ordering::SeqCst)
        }
    }

    fn spawn_delayed_status_dependency(
        status: u16,
        delay: Duration,
        active_requests: Arc<AtomicUsize>,
        observed_overlap: Arc<AtomicBool>,
    ) -> HttpDependency {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            if active_requests.fetch_add(1, Ordering::SeqCst) > 0 {
                observed_overlap.store(true, Ordering::SeqCst);
            }
            thread::sleep(delay);
            active_requests.fetch_sub(1, Ordering::SeqCst);
            let response = HttpResponse::status(status).serialize();
            stream.write_all(response.as_bytes()).unwrap();
        });

        HttpDependency {
            url: format!("http://{address}/ready"),
        }
    }

    fn read_request(stream: &mut impl Read) -> std::io::Result<usize> {
        let mut request = [0_u8; 1024];
        stream.read(&mut request)
    }
}

use http_dependencies::ObservedHttpDependency;
use http_dependencies::{ConcurrentRoundProbe, HttpDependency, InFlightHttpDependency};

#[test]
fn test_should_show_defaults_in_cli_help() {
    let mut command = readiness_check();

    command
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("default: 3s"))
        .stdout(predicate::str::contains("default: 10s"))
        .stdout(predicate::str::contains("default: infinity"))
        .stdout(predicate::str::contains("default: false").not());
}

#[cfg(unix)]
fn send_signal(process_id: u32, signal: &str) {
    let status = StdCommand::new("kill")
        .args([signal, &process_id.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(unix)]
fn assert_waiting_process_interrupted(signal: &str, signal_name: &str) {
    let dependency = ObservedHttpDependency::repeating_status(503, 16);
    let mut command = StdCommand::new(assert_cmd::cargo::cargo_bin("readiness-check"));
    command
        .args([
            "--check",
            &format!("dep={}=200", dependency.url()),
            "--interval",
            "1s",
            "--max-wait",
            "infinity",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let started_at = Instant::now();
    let child = command.spawn().unwrap();
    dependency.wait_for_requests(1, Duration::from_secs(1));
    send_signal(child.id(), signal);

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(Some(1), output.status.code());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains(&format!(
        "readiness-check: interrupted signal={signal_name} elapsed="
    )));
    assert!(!stderr.contains(dependency.url()));
    assert!(started_at.elapsed() < Duration::from_secs(2));
}

#[test]
#[cfg(unix)]
fn test_should_interrupt_waiting_process_on_sigterm_without_leaking_url() {
    assert_waiting_process_interrupted("-TERM", "SIGTERM");
}

#[test]
#[cfg(unix)]
fn test_should_interrupt_waiting_process_on_sigint_without_leaking_url() {
    assert_waiting_process_interrupted("-INT", "SIGINT");
}

#[test]
#[cfg(unix)]
fn test_should_interrupt_in_flight_check_on_sigterm_without_waiting_for_request_timeout() {
    let dependency = InFlightHttpDependency::delayed_status(503, Duration::from_secs(4));
    let mut command = StdCommand::new(assert_cmd::cargo::cargo_bin("readiness-check"));
    command
        .args([
            "--check",
            &format!("dep={}=200", dependency.url()),
            "--request-timeout",
            "5s",
            "--max-wait",
            "infinity",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let started_at = Instant::now();
    let child = command.spawn().unwrap();
    dependency.wait_until_in_flight(Duration::from_secs(1));
    send_signal(child.id(), "-TERM");

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(Some(1), output.status.code());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("readiness-check: interrupted signal=SIGTERM elapsed="));
    assert!(!stderr.contains(dependency.url()));
    assert!(started_at.elapsed() < Duration::from_secs(2));
    assert!(!dependency.observed_overlap());
}

#[test]
fn test_should_exit_success_when_single_inline_check_returns_expected_status() {
    let dependency = HttpDependency::fixed_status_with_body(200, "ready");
    let mut command = readiness_check();

    command
        .args(["--check", &format!("dep={dependency}=200")])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ));
}

#[test]
fn test_should_wait_until_inline_dependency_changes_from_not_ready_to_ready() {
    let dependency = HttpDependency::status_sequence([503, 200]);
    let mut command = readiness_check();

    command
        .args([
            "--check",
            &format!("dep={dependency}=200"),
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
        .stderr(predicate::str::contains(dependency.url()).not());
}

#[test]
fn test_should_timeout_when_dependency_never_becomes_ready() {
    let dependency = HttpDependency::repeating_status(503, 8);
    let started_at = Instant::now();
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep={dependency}=200"),
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
        .stderr(predicate::str::contains(dependency.url()).not());

    assert!(started_at.elapsed() < Duration::from_secs(1));
}

#[test]
fn test_should_cap_effective_request_timeout_by_remaining_max_wait() {
    let dependency = HttpDependency::without_status();
    let started_at = Instant::now();
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep={dependency}=200"),
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
        .stderr(predicate::str::contains(dependency.url()).not());

    assert!(started_at.elapsed() < Duration::from_secs(2));
}

#[test]
fn test_should_retry_with_explicit_infinite_max_wait() {
    let dependency = HttpDependency::status_sequence([503, 200]);
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep={dependency}=200"),
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
        .stderr(predicate::str::contains(dependency.url()).not());
}

#[test]
fn test_should_wait_for_all_yaml_dependencies_to_be_ready_in_same_round() {
    let dep1 = HttpDependency::status_sequence([503, 200]);
    let dep2 = HttpDependency::status_sequence([200, 200]);
    let config = write_config(&format!(
        r#"
interval: 100ms
request-timeout: 1s
max-wait: 2s
checks:
  - name: dep1
    url: {dep1}
    expected-status: 200
  - name: dep2
    url: {dep2}
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
        .stderr(predicate::str::contains(dep1.url()).not())
        .stderr(predicate::str::contains(dep2.url()).not());
}

#[test]
fn test_should_timeout_when_one_dependency_is_ready_and_one_is_not_ready() {
    let ready = HttpDependency::repeating_status(200, 8);
    let not_ready = HttpDependency::repeating_status(503, 8);
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("ready={ready}=200"),
            "--check",
            &format!("not-ready={not_ready}=200"),
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
        .stderr(predicate::str::contains(ready.url()).not())
        .stderr(predicate::str::contains(not_ready.url()).not());
}

#[test]
fn test_should_not_latch_ready_state_across_rounds() {
    let dep1 = HttpDependency::status_sequence([200, 503, 200]);
    let dep2 = HttpDependency::status_sequence([503, 200, 200]);
    let mut command = readiness_check();

    command
        .args([
            "--check",
            &format!("dep1={dep1}=200"),
            "--check",
            &format!("dep2={dep2}=200"),
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
        .stderr(predicate::str::contains(dep1.url()).not())
        .stderr(predicate::str::contains(dep2.url()).not());
}

#[test]
fn test_should_check_dependencies_concurrently_within_each_round() {
    let probe = ConcurrentRoundProbe::new();
    let dep1 = probe.delayed_ready_dependency(Duration::from_millis(300));
    let dep2 = probe.delayed_ready_dependency(Duration::from_millis(300));
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep1={dep1}=200"),
            "--check",
            &format!("dep2={dep2}=200"),
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
        .stderr(predicate::str::contains(dep1.url()).not())
        .stderr(predicate::str::contains(dep2.url()).not());

    assert!(probe.observed_overlap());
}

#[test]
fn test_should_exit_not_ready_without_printing_url_when_status_differs() {
    let dependency = HttpDependency::fixed_status_with_body(503, "not ready");
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep={dependency}=200"),
            "--max-wait",
            "100ms",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep expected=200 actual=503\n",
        ))
        .stderr(predicate::str::contains(dependency.url()).not());
}

#[test]
fn test_should_compare_redirect_status_without_following_location() {
    let dependency = HttpDependency::redirecting_to("http://127.0.0.1:1/followed");
    let mut command = readiness_check();

    command
        .args(["--check", &format!("dep={dependency}=302")])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ));
}

#[test]
fn test_should_not_wait_for_or_log_response_body() {
    let dependency = HttpDependency::headers_without_body(200, 1_000_000);
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(1));

    command
        .args(["--check", &format!("dep={dependency}=200")])
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
    let dependency = HttpDependency::without_status();
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep={dependency}=200"),
            "--request-timeout",
            "50ms",
            "--max-wait",
            "500ms",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=dep expected=200 error=request-timeout\n",
        ))
        .stderr(predicate::str::contains(dependency.url()).not());
}

#[test]
fn test_should_log_connection_refused_without_aborting_the_round_or_printing_url() {
    let refused = HttpDependency::unavailable_with_secret();
    let ready = HttpDependency::repeating_status(200, 8);
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("refused={refused}=200"),
            "--check",
            &format!("ready={ready}=200"),
            "--interval",
            "100ms",
            "--request-timeout",
            "100ms",
            "--max-wait",
            "250ms",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=refused expected=200 error=connection-refused\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: timeout waiting for dependencies",
        ))
        .stderr(predicate::str::contains(refused.url()).not())
        .stderr(predicate::str::contains(ready.url()).not())
        .stderr(predicate::str::contains("secret=token").not());
}

#[test]
fn test_should_report_self_signed_https_failure_as_tls_without_printing_url() {
    let dependency = HttpDependency::self_signed_tls_status(200, 1);
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("self-signed={dependency}=200"),
            "--request-timeout",
            "1s",
            "--max-wait",
            "100ms",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: waiting dependencies=1 interval=3000ms max-wait=100ms tls-insecure-skip-verify=false\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: dependency not ready name=self-signed expected=200 error=tls\n",
        ))
        .stderr(predicate::str::contains(dependency.url()).not())
        .stderr(predicate::str::contains("certificate").not())
        .stderr(predicate::str::contains("rustls").not());
}

#[test]
fn test_should_accept_self_signed_https_when_cli_enables_insecure_tls() {
    let dependency = HttpDependency::self_signed_tls_status(200, 1);
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("self-signed={dependency}=200"),
            "--tls-insecure-skip-verify",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: waiting dependencies=1 interval=3000ms max-wait=infinity tls-insecure-skip-verify=true\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ))
        .stderr(predicate::str::contains(dependency.url()).not());
}

#[test]
fn test_should_apply_yaml_insecure_tls_to_all_https_checks() {
    let dep1 = HttpDependency::self_signed_tls_status(200, 1);
    let dep2 = HttpDependency::self_signed_tls_status(204, 1);
    let config = write_config(&format!(
        r#"
tls:
  insecure-skip-verify: true
checks:
  - name: dep1
    url: {dep1}
    expected-status: 200
  - name: dep2
    url: {dep2}
    expected-status: 204
"#
    ));
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args(["--config", config.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: waiting dependencies=2 interval=3000ms max-wait=infinity tls-insecure-skip-verify=true\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ))
        .stderr(predicate::str::contains(dep1.url()).not())
        .stderr(predicate::str::contains(dep2.url()).not());
}

#[test]
fn test_should_log_first_not_ready_state_changes_and_waiting_summary() {
    let dependency = HttpDependency::status_sequence([503, 502, 200]);
    let mut command = readiness_check();
    command.timeout(Duration::from_secs(2));

    command
        .args([
            "--check",
            &format!("dep={dependency}=200"),
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
            "readiness-check: dependency not ready name=dep expected=200 actual=503\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: dependency state changed name=dep expected=200 actual=502 ready=false\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: dependency state changed name=dep expected=200 actual=200 ready=true\n",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: still waiting not-ready=1",
        ))
        .stderr(predicate::str::contains(
            "readiness-check: all dependencies ready",
        ))
        .stderr(predicate::str::contains(dependency.url()).not());
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
fn test_should_validate_shipped_example_readiness_yaml() {
    let mut command = readiness_check();

    command
        .args(["--config", &example_config_path(), "--validate-config"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "readiness-check: configuration valid dependencies=2 max-wait=600000ms tls-insecure-skip-verify=true\n",
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
