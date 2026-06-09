use std::fmt::Write as _;
use std::time::Duration;

use crate::intake::{ConfigError, ConfigurationSummary};
use crate::readiness_loop::{
    CheckExecutionError, ObservedState, ReadinessEvent, ReadinessRun, ReceivedSignal,
};

pub(crate) fn render_configuration_valid(summary: ConfigurationSummary) -> String {
    format!(
        "readiness-check: configuration valid dependencies={} max-wait={} tls-insecure-skip-verify={}\n",
        summary.dependencies, summary.max_wait, summary.tls_insecure_skip_verify,
    )
}

pub(crate) fn render_config_error(error: &ConfigError) -> String {
    format!(
        "readiness-check: invalid configuration path={} error=\"{}\"\n",
        error.path(),
        error.message(),
    )
}

pub(crate) fn render_readiness_run(run: &ReadinessRun) -> String {
    render_readiness_events(&run.events)
}

fn render_readiness_events(events: &[ReadinessEvent]) -> String {
    let mut stderr = String::new();
    for event in events {
        render_readiness_event(event, &mut stderr);
    }
    stderr
}

fn render_readiness_event(event: &ReadinessEvent, stderr: &mut String) {
    match event {
        ReadinessEvent::WaitingStarted {
            dependencies,
            interval,
            max_wait,
            tls_insecure_skip_verify,
        } => {
            let _ = writeln!(
                stderr,
                "readiness-check: waiting dependencies={} interval={} max-wait={} tls-insecure-skip-verify={}",
                dependencies,
                format_duration(*interval),
                max_wait,
                tls_insecure_skip_verify,
            );
        }
        ReadinessEvent::DependencyNotReady {
            name,
            expected_status,
            state,
        } => render_dependency_not_ready(stderr, name, *expected_status, *state),
        ReadinessEvent::DependencyStateChanged {
            name,
            expected_status,
            state,
            ready,
        } => render_dependency_state_changed(stderr, name, *expected_status, *state, *ready),
        ReadinessEvent::StillWaiting { not_ready, elapsed } => {
            let _ = writeln!(
                stderr,
                "readiness-check: still waiting not-ready={} elapsed={}",
                not_ready,
                format_duration(*elapsed),
            );
        }
        ReadinessEvent::AllReady { elapsed } => {
            let _ = writeln!(
                stderr,
                "readiness-check: all dependencies ready elapsed={}",
                format_duration(*elapsed),
            );
        }
        ReadinessEvent::TimedOut { elapsed } => {
            let _ = writeln!(
                stderr,
                "readiness-check: timeout waiting for dependencies elapsed={}",
                format_duration(*elapsed),
            );
        }
        ReadinessEvent::Interrupted { signal, elapsed } => {
            let _ = writeln!(
                stderr,
                "readiness-check: interrupted signal={} elapsed={}",
                signal_label(*signal),
                format_duration(*elapsed),
            );
        }
        ReadinessEvent::HttpClientSetupFailed { error } => {
            let _ = writeln!(
                stderr,
                "readiness-check: HTTP client setup failed error={}",
                error_category(*error),
            );
        }
        ReadinessEvent::SignalSetupFailed => {
            stderr.push_str("readiness-check: signal setup failed error=signal-unavailable\n");
        }
    }
}

fn render_dependency_not_ready(
    stderr: &mut String,
    name: &str,
    expected_status: u16,
    state: ObservedState,
) {
    match state {
        ObservedState::Ready | ObservedState::Status(_) => {
            let actual = actual_status_or_expected(state, expected_status);
            let _ = writeln!(
                stderr,
                "readiness-check: dependency not ready name={name} expected={expected_status} actual={actual}",
            );
        }
        ObservedState::Error(error) => {
            let _ = writeln!(
                stderr,
                "readiness-check: dependency not ready name={name} expected={expected_status} error={}",
                error_category(error),
            );
        }
    }
}

fn render_dependency_state_changed(
    stderr: &mut String,
    name: &str,
    expected_status: u16,
    state: ObservedState,
    ready: bool,
) {
    match state {
        ObservedState::Ready | ObservedState::Status(_) => {
            let actual = actual_status_or_expected(state, expected_status);
            let _ = writeln!(
                stderr,
                "readiness-check: dependency state changed name={name} expected={expected_status} actual={actual} ready={ready}",
            );
        }
        ObservedState::Error(error) => {
            let _ = writeln!(
                stderr,
                "readiness-check: dependency state changed name={name} expected={expected_status} error={} ready={ready}",
                error_category(error),
            );
        }
    }
}

const fn actual_status_or_expected(state: ObservedState, expected_status: u16) -> u16 {
    match state {
        ObservedState::Status(actual_status) => actual_status,
        ObservedState::Ready | ObservedState::Error(_) => expected_status,
    }
}

fn format_duration(duration: Duration) -> String {
    format!("{}ms", duration.as_millis())
}

const fn error_category(error: CheckExecutionError) -> &'static str {
    match error {
        CheckExecutionError::RequestTimeout => "request-timeout",
        CheckExecutionError::Dns => "dns",
        CheckExecutionError::ConnectionRefused => "connection-refused",
        CheckExecutionError::ConnectionClosed => "connection-closed",
        CheckExecutionError::Tls => "tls",
        CheckExecutionError::HttpProtocol => "http-protocol",
        CheckExecutionError::RequestError => "request-error",
    }
}

const fn signal_label(signal: ReceivedSignal) -> &'static str {
    match signal {
        ReceivedSignal::Sigterm => "SIGTERM",
        ReceivedSignal::Sigint => "SIGINT",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clap::Parser;

    use super::*;
    use crate::Cli;
    use crate::intake::{MaxWait, build_readiness_plan};
    use crate::readiness_loop::ReadinessStatus;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("readiness-check").chain(args.iter().copied()))
    }

    #[test]
    fn test_should_render_runtime_events_with_stable_key_value_order() {
        let run = ReadinessRun {
            status: ReadinessStatus::TimedOut,
            events: vec![
                ReadinessEvent::WaitingStarted {
                    dependencies: 1,
                    interval: Duration::from_secs(3),
                    max_wait: MaxWait::Finite(Duration::from_millis(100)),
                    tls_insecure_skip_verify: false,
                },
                ReadinessEvent::DependencyNotReady {
                    name: "dep".to_owned(),
                    expected_status: 200,
                    state: ObservedState::Status(503),
                },
                ReadinessEvent::StillWaiting {
                    not_ready: 1,
                    elapsed: Duration::from_millis(12),
                },
                ReadinessEvent::TimedOut {
                    elapsed: Duration::from_millis(100),
                },
            ],
        };

        assert_eq!(
            concat!(
                "readiness-check: waiting dependencies=1 interval=3000ms max-wait=100ms tls-insecure-skip-verify=false\n",
                "readiness-check: dependency not ready name=dep expected=200 actual=503\n",
                "readiness-check: still waiting not-ready=1 elapsed=12ms\n",
                "readiness-check: timeout waiting for dependencies elapsed=100ms\n",
            ),
            render_readiness_run(&run)
        );
    }

    #[test]
    fn test_should_render_sanitized_error_categories() {
        let run = ReadinessRun {
            status: ReadinessStatus::RuntimeSetupFailed,
            events: vec![
                ReadinessEvent::DependencyNotReady {
                    name: "self-signed".to_owned(),
                    expected_status: 200,
                    state: ObservedState::Error(CheckExecutionError::Tls),
                },
                ReadinessEvent::DependencyStateChanged {
                    name: "dep".to_owned(),
                    expected_status: 204,
                    state: ObservedState::Error(CheckExecutionError::RequestTimeout),
                    ready: false,
                },
                ReadinessEvent::Interrupted {
                    signal: ReceivedSignal::Sigterm,
                    elapsed: Duration::from_millis(7),
                },
                ReadinessEvent::HttpClientSetupFailed {
                    error: CheckExecutionError::HttpProtocol,
                },
                ReadinessEvent::SignalSetupFailed,
            ],
        };

        assert_eq!(
            concat!(
                "readiness-check: dependency not ready name=self-signed expected=200 error=tls\n",
                "readiness-check: dependency state changed name=dep expected=204 error=request-timeout ready=false\n",
                "readiness-check: interrupted signal=SIGTERM elapsed=7ms\n",
                "readiness-check: HTTP client setup failed error=http-protocol\n",
                "readiness-check: signal setup failed error=signal-unavailable\n",
            ),
            render_readiness_run(&run)
        );
    }

    #[test]
    fn test_should_render_configuration_validation_summary() {
        let plan = build_readiness_plan(&cli(&[
            "--check",
            "dep=http://127.0.0.1:8080/health=200",
            "--max-wait",
            "5s",
            "--tls-insecure-skip-verify",
        ]))
        .unwrap();

        assert_eq!(
            "readiness-check: configuration valid dependencies=1 max-wait=5000ms tls-insecure-skip-verify=true\n",
            render_configuration_valid(plan.configuration_summary())
        );
    }

    #[test]
    fn test_should_render_config_diagnostics_outside_intake() {
        let error = build_readiness_plan(&cli(&["--check", "dep=http://127.0.0.1:8080/health"]))
            .unwrap_err();

        assert_eq!(
            "readiness-check: invalid configuration path=checks[0] error=\"must use name=url=expected_status\"\n",
            render_config_error(&error)
        );
    }
}
