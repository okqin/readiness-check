use std::error::Error as _;
use std::future::Future;
use std::io::{self, ErrorKind};
use std::pin::Pin;
use std::time::{Duration, Instant};

use reqwest::{Client, redirect};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::task::JoinSet;
use tokio::time;

use crate::intake::{MaxWait, ReadinessCheck, ReadinessPlan};

pub(crate) async fn run(config: &ReadinessPlan) -> ReadinessRun {
    let client = match build_http_client(config.tls_insecure_skip_verify) {
        Ok(client) => client,
        Err(error) => {
            return ReadinessRun {
                status: ReadinessStatus::RuntimeSetupFailed,
                events: vec![ReadinessEvent::HttpClientSetupFailed { error }],
            };
        }
    };

    let mut termination_signals = match TerminationSignals::new() {
        Ok(signals) => signals,
        Err(_error) => {
            return ReadinessRun {
                status: ReadinessStatus::RuntimeSetupFailed,
                events: vec![ReadinessEvent::SignalSetupFailed],
            };
        }
    };

    let clock = TokioClock::new();
    let executor = ReqwestCheckExecutor { client };
    run_with_adapters(config, executor, &mut termination_signals, &clock).await
}

async fn run_with_adapters<E, I, C>(
    config: &ReadinessPlan,
    executor: E,
    interrupts: &mut I,
    clock: &C,
) -> ReadinessRun
where
    E: CheckExecutor,
    I: InterruptReceiver,
    C: Clock,
{
    let mut events = vec![ReadinessEvent::WaitingStarted {
        dependencies: config.checks.len(),
        interval: config.interval,
        max_wait: config.max_wait,
        tls_insecure_skip_verify: config.tls_insecure_skip_verify,
    }];
    let mut readiness_state = ReadinessState::new(config.checks.len());

    loop {
        let Some(round_timeouts) = round_timeouts(config, clock) else {
            events.push(ReadinessEvent::TimedOut {
                elapsed: clock.elapsed(),
            });
            return ReadinessRun {
                status: ReadinessStatus::TimedOut,
                events,
            };
        };

        let outcomes = match execute_round(
            executor.clone(),
            &config.checks,
            &round_timeouts,
            interrupts,
        )
        .await
        {
            RoundResult::Completed(outcomes) => outcomes,
            RoundResult::Interrupted(received_signal) => {
                let elapsed = clock.elapsed();
                events.push(ReadinessEvent::Interrupted {
                    signal: received_signal,
                    elapsed,
                });
                return ReadinessRun {
                    status: ReadinessStatus::Interrupted {
                        signal: received_signal,
                    },
                    events,
                };
            }
        };

        match record_round_events(config, outcomes, &mut readiness_state, &mut events) {
            RoundProgress::AllReady => {
                events.push(ReadinessEvent::AllReady {
                    elapsed: clock.elapsed(),
                });
                return ReadinessRun {
                    status: ReadinessStatus::Ready,
                    events,
                };
            }
            RoundProgress::Waiting { not_ready } => {
                events.push(ReadinessEvent::StillWaiting {
                    not_ready,
                    elapsed: clock.elapsed(),
                });
            }
        }

        let Some(sleep_duration) = next_sleep_duration(config, clock) else {
            events.push(ReadinessEvent::TimedOut {
                elapsed: clock.elapsed(),
            });
            return ReadinessRun {
                status: ReadinessStatus::TimedOut,
                events,
            };
        };

        tokio::select! {
            received_signal = interrupts.recv() => {
                let elapsed = clock.elapsed();
                events.push(ReadinessEvent::Interrupted {
                    signal: received_signal,
                    elapsed,
                });
                return ReadinessRun {
                    status: ReadinessStatus::Interrupted {
                        signal: received_signal,
                    },
                    events,
                };
            }
            () = clock.sleep(sleep_duration) => {}
        }
    }
}

fn record_round_events(
    config: &ReadinessPlan,
    outcomes: Vec<CheckOutcome>,
    state: &mut ReadinessState,
    events: &mut Vec<ReadinessEvent>,
) -> RoundProgress {
    let mut not_ready_count = 0_usize;

    for (index, (check, outcome)) in config.checks.iter().zip(outcomes).enumerate() {
        let current_state = outcome.observed_state();
        let previous_state = state.observed_states[index];
        if !outcome.ready {
            not_ready_count += 1;
            record_not_ready_event(
                check,
                outcome.ready,
                current_state,
                previous_state,
                &mut state.reported_not_ready[index],
                events,
            );
        } else if previous_state.is_some_and(|previous| previous != current_state) {
            events.push(ReadinessEvent::DependencyStateChanged {
                name: check.name.as_str().to_owned(),
                expected_status: check.expected_status.get(),
                state: current_state,
                ready: outcome.ready,
            });
        }
        state.observed_states[index] = Some(current_state);
    }

    if not_ready_count == 0 {
        RoundProgress::AllReady
    } else {
        RoundProgress::Waiting {
            not_ready: not_ready_count,
        }
    }
}

fn record_not_ready_event(
    check: &ReadinessCheck,
    ready: bool,
    current_state: ObservedState,
    previous_state: Option<ObservedState>,
    reported_not_ready: &mut bool,
    events: &mut Vec<ReadinessEvent>,
) {
    match previous_state {
        _ if !*reported_not_ready => {
            events.push(ReadinessEvent::DependencyNotReady {
                name: check.name.as_str().to_owned(),
                expected_status: check.expected_status.get(),
                state: current_state,
            });
            *reported_not_ready = true;
        }
        Some(previous) if previous != current_state => {
            events.push(ReadinessEvent::DependencyStateChanged {
                name: check.name.as_str().to_owned(),
                expected_status: check.expected_status.get(),
                state: current_state,
                ready,
            });
        }
        Some(_) | None => {}
    }
}

#[derive(Debug)]
struct ReadinessState {
    observed_states: Vec<Option<ObservedState>>,
    reported_not_ready: Vec<bool>,
}

impl ReadinessState {
    fn new(check_count: usize) -> Self {
        Self {
            observed_states: vec![None; check_count],
            reported_not_ready: vec![false; check_count],
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RoundProgress {
    AllReady,
    Waiting { not_ready: usize },
}

fn round_timeouts(config: &ReadinessPlan, clock: &impl Clock) -> Option<Vec<Duration>> {
    match config.max_wait {
        MaxWait::Infinity => Some(
            config
                .checks
                .iter()
                .map(|check| check.request_timeout)
                .collect(),
        ),
        MaxWait::Finite(max_wait) => {
            let remaining = remaining_wait(clock, max_wait)?;
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

fn next_sleep_duration(config: &ReadinessPlan, clock: &impl Clock) -> Option<Duration> {
    match config.max_wait {
        MaxWait::Infinity => Some(config.interval),
        MaxWait::Finite(max_wait) => {
            let remaining = remaining_wait(clock, max_wait)?;
            Some(config.interval.min(remaining))
        }
    }
}

fn remaining_wait(clock: &impl Clock, max_wait: Duration) -> Option<Duration> {
    let elapsed = clock.elapsed();
    if elapsed >= max_wait {
        return None;
    }
    max_wait.checked_sub(elapsed)
}

fn build_http_client(tls_insecure_skip_verify: bool) -> Result<Client, CheckExecutionError> {
    Client::builder()
        .redirect(redirect::Policy::none())
        .tls_danger_accept_invalid_certs(tls_insecure_skip_verify)
        .no_proxy()
        .build()
        .map_err(CheckExecutionError::from)
}

async fn execute_round<E, I>(
    executor: E,
    checks: &[ReadinessCheck],
    request_timeouts: &[Duration],
    interrupts: &mut I,
) -> RoundResult
where
    E: CheckExecutor,
    I: InterruptReceiver,
{
    let mut tasks: JoinSet<(usize, CheckOutcome)> = JoinSet::new();
    for (index, (check, request_timeout)) in checks.iter().zip(request_timeouts).enumerate() {
        let executor = executor.clone();
        let check = check.clone();
        let request_timeout = *request_timeout;
        tasks.spawn(async move { (index, executor.execute(check, request_timeout).await) });
    }

    let mut outcomes = Vec::with_capacity(checks.len());
    outcomes.resize_with(checks.len(), || None);
    let mut remaining_tasks = checks.len();

    while remaining_tasks > 0 {
        tokio::select! {
            received_signal = interrupts.recv() => {
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

trait CheckExecutor: Clone + Send + Sync + 'static {
    fn execute(
        &self,
        check: ReadinessCheck,
        request_timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = CheckOutcome> + Send + 'static>>;
}

#[derive(Debug, Clone)]
struct ReqwestCheckExecutor {
    client: Client,
}

impl CheckExecutor for ReqwestCheckExecutor {
    fn execute(
        &self,
        check: ReadinessCheck,
        request_timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = CheckOutcome> + Send + 'static>> {
        let client = self.client.clone();
        Box::pin(async move { execute_check(&client, &check, request_timeout).await })
    }
}

trait InterruptReceiver {
    fn recv(&mut self) -> Pin<Box<dyn Future<Output = ReceivedSignal> + Send + '_>>;
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
}

impl InterruptReceiver for TerminationSignals {
    fn recv(&mut self) -> Pin<Box<dyn Future<Output = ReceivedSignal> + Send + '_>> {
        Box::pin(async {
            tokio::select! {
                _ = self.sigterm.recv() => ReceivedSignal::Sigterm,
                _ = self.sigint.recv() => ReceivedSignal::Sigint,
            }
        })
    }
}

trait Clock: Sync {
    fn elapsed(&self) -> Duration;

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

#[derive(Debug)]
struct TokioClock {
    started_at: Instant,
}

impl TokioClock {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(time::sleep(duration))
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
pub(crate) struct ReadinessRun {
    pub(crate) status: ReadinessStatus,
    pub(crate) events: Vec<ReadinessEvent>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ReadinessStatus {
    Ready,
    TimedOut,
    Interrupted { signal: ReceivedSignal },
    RuntimeSetupFailed,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum ReadinessEvent {
    WaitingStarted {
        dependencies: usize,
        interval: Duration,
        max_wait: MaxWait,
        tls_insecure_skip_verify: bool,
    },
    DependencyNotReady {
        name: String,
        expected_status: u16,
        state: ObservedState,
    },
    DependencyStateChanged {
        name: String,
        expected_status: u16,
        state: ObservedState,
        ready: bool,
    },
    StillWaiting {
        not_ready: usize,
        elapsed: Duration,
    },
    AllReady {
        elapsed: Duration,
    },
    TimedOut {
        elapsed: Duration,
    },
    Interrupted {
        signal: ReceivedSignal,
        elapsed: Duration,
    },
    HttpClientSetupFailed {
        error: CheckExecutionError,
    },
    SignalSetupFailed,
}

#[derive(Debug)]
struct CheckOutcome {
    ready: bool,
    actual_status: Option<u16>,
    error: Option<CheckExecutionError>,
}

impl CheckOutcome {
    #[cfg(test)]
    const fn ready(actual_status: u16) -> Self {
        Self {
            ready: true,
            actual_status: Some(actual_status),
            error: None,
        }
    }

    #[cfg(test)]
    const fn status(actual_status: u16) -> Self {
        Self {
            ready: false,
            actual_status: Some(actual_status),
            error: None,
        }
    }

    const fn request_error() -> Self {
        Self {
            ready: false,
            actual_status: None,
            error: Some(CheckExecutionError::RequestError),
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
pub(crate) enum ObservedState {
    Ready,
    Status(u16),
    Error(CheckExecutionError),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum CheckExecutionError {
    RequestTimeout,
    Dns,
    ConnectionRefused,
    ConnectionClosed,
    Tls,
    HttpProtocol,
    RequestError,
}

impl CheckExecutionError {
    pub(crate) const fn classify(self) -> &'static str {
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ReceivedSignal {
    Sigterm,
    Sigint,
}

impl ReceivedSignal {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Sigterm => "SIGTERM",
            Self::Sigint => "SIGINT",
        }
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use clap::Parser;

    use super::*;
    use crate::{Cli, intake::build_readiness_plan};

    fn plan(args: &[&str]) -> ReadinessPlan {
        let cli = Cli::parse_from(std::iter::once("readiness-check").chain(args.iter().copied()));
        build_readiness_plan(&cli).unwrap()
    }

    #[derive(Debug, Clone)]
    struct ScriptedExecutor {
        outcomes: Arc<Mutex<HashMap<String, VecDeque<FakeOutcome>>>>,
        request_timeouts: Arc<Mutex<Vec<Duration>>>,
    }

    impl ScriptedExecutor {
        fn new(outcomes: HashMap<String, VecDeque<FakeOutcome>>) -> Self {
            Self {
                outcomes: Arc::new(Mutex::new(outcomes)),
                request_timeouts: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl CheckExecutor for ScriptedExecutor {
        fn execute(
            &self,
            check: ReadinessCheck,
            request_timeout: Duration,
        ) -> Pin<Box<dyn Future<Output = CheckOutcome> + Send + 'static>> {
            self.request_timeouts.lock().unwrap().push(request_timeout);
            let step = self
                .outcomes
                .lock()
                .unwrap()
                .get_mut(check.name.as_str())
                .and_then(VecDeque::pop_front)
                .unwrap();

            match step {
                FakeOutcome::Outcome(outcome) => Box::pin(async move { outcome }),
                FakeOutcome::Pending => Box::pin(std::future::pending()),
            }
        }
    }

    #[derive(Debug)]
    enum FakeOutcome {
        Outcome(CheckOutcome),
        Pending,
    }

    #[derive(Debug)]
    struct NeverInterrupted;

    impl InterruptReceiver for NeverInterrupted {
        fn recv(&mut self) -> Pin<Box<dyn Future<Output = ReceivedSignal> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }

    #[derive(Debug)]
    struct ImmediateInterrupt {
        signal: ReceivedSignal,
    }

    impl InterruptReceiver for ImmediateInterrupt {
        fn recv(&mut self) -> Pin<Box<dyn Future<Output = ReceivedSignal> + Send + '_>> {
            let signal = self.signal;
            Box::pin(async move { signal })
        }
    }

    #[derive(Debug, Default)]
    struct FakeClock {
        elapsed: Mutex<Duration>,
        sleeps: Mutex<Vec<Duration>>,
    }

    impl FakeClock {
        fn sleeps(&self) -> Vec<Duration> {
            self.sleeps.lock().unwrap().clone()
        }
    }

    impl Clock for FakeClock {
        fn elapsed(&self) -> Duration {
            *self.elapsed.lock().unwrap()
        }

        fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                self.sleeps.lock().unwrap().push(duration);
                *self.elapsed.lock().unwrap() += duration;
            })
        }
    }

    fn scripted_outcomes(
        values: &[(&str, Vec<FakeOutcome>)],
    ) -> HashMap<String, VecDeque<FakeOutcome>> {
        values
            .iter()
            .map(|(name, outcomes)| {
                (
                    (*name).to_owned(),
                    outcomes.iter().cloned().collect::<VecDeque<_>>(),
                )
            })
            .collect()
    }

    impl Clone for FakeOutcome {
        fn clone(&self) -> Self {
            match self {
                Self::Outcome(outcome) => Self::Outcome(CheckOutcome {
                    ready: outcome.ready,
                    actual_status: outcome.actual_status,
                    error: outcome.error,
                }),
                Self::Pending => Self::Pending,
            }
        }
    }

    #[tokio::test]
    async fn test_should_require_all_checks_ready_in_same_round() {
        let config = plan(&[
            "--check",
            "dep1=http://127.0.0.1:8080/ready=200",
            "--check",
            "dep2=http://127.0.0.1:8081/ready=200",
            "--interval",
            "100ms",
            "--request-timeout",
            "1s",
            "--max-wait",
            "2s",
        ]);
        let executor = ScriptedExecutor::new(scripted_outcomes(&[
            (
                "dep1",
                vec![
                    FakeOutcome::Outcome(CheckOutcome::ready(200)),
                    FakeOutcome::Outcome(CheckOutcome::status(503)),
                    FakeOutcome::Outcome(CheckOutcome::ready(200)),
                ],
            ),
            (
                "dep2",
                vec![
                    FakeOutcome::Outcome(CheckOutcome::status(503)),
                    FakeOutcome::Outcome(CheckOutcome::ready(200)),
                    FakeOutcome::Outcome(CheckOutcome::ready(200)),
                ],
            ),
        ]));
        let mut interrupts = NeverInterrupted;
        let clock = FakeClock::default();

        let run = run_with_adapters(&config, executor, &mut interrupts, &clock).await;

        assert_eq!(ReadinessStatus::Ready, run.status);
        assert!(run.events.contains(&ReadinessEvent::DependencyNotReady {
            name: "dep2".to_owned(),
            expected_status: 200,
            state: ObservedState::Status(503),
        }));
        assert!(run.events.contains(&ReadinessEvent::DependencyNotReady {
            name: "dep1".to_owned(),
            expected_status: 200,
            state: ObservedState::Status(503),
        }));
        assert!(run.events.contains(&ReadinessEvent::AllReady {
            elapsed: Duration::from_millis(200),
        }));
    }

    #[tokio::test]
    async fn test_should_cap_request_timeout_and_sleep_by_remaining_max_wait() {
        let config = plan(&[
            "--check",
            "dep=http://127.0.0.1:8080/ready=200",
            "--interval",
            "250ms",
            "--request-timeout",
            "5s",
            "--max-wait",
            "300ms",
        ]);
        let executor = ScriptedExecutor::new(scripted_outcomes(&[(
            "dep",
            vec![
                FakeOutcome::Outcome(CheckOutcome::status(503)),
                FakeOutcome::Outcome(CheckOutcome::status(503)),
            ],
        )]));
        let request_timeouts = Arc::clone(&executor.request_timeouts);
        let mut interrupts = NeverInterrupted;
        let clock = FakeClock::default();

        let run = run_with_adapters(&config, executor, &mut interrupts, &clock).await;

        assert_eq!(ReadinessStatus::TimedOut, run.status);
        assert_eq!(
            vec![Duration::from_millis(300), Duration::from_millis(50)],
            request_timeouts.lock().unwrap().clone()
        );
        assert_eq!(
            vec![Duration::from_millis(250), Duration::from_millis(50)],
            clock.sleeps()
        );
    }

    #[tokio::test]
    async fn test_should_interrupt_in_flight_checks() {
        let config = plan(&[
            "--check",
            "dep=http://127.0.0.1:8080/ready=200",
            "--request-timeout",
            "5s",
            "--max-wait",
            "infinity",
        ]);
        let executor =
            ScriptedExecutor::new(scripted_outcomes(&[("dep", vec![FakeOutcome::Pending])]));
        let mut interrupts = ImmediateInterrupt {
            signal: ReceivedSignal::Sigterm,
        };
        let clock = FakeClock::default();

        let run = run_with_adapters(&config, executor, &mut interrupts, &clock).await;

        assert_eq!(
            ReadinessStatus::Interrupted {
                signal: ReceivedSignal::Sigterm,
            },
            run.status
        );
        assert!(run.events.contains(&ReadinessEvent::Interrupted {
            signal: ReceivedSignal::Sigterm,
            elapsed: Duration::ZERO,
        }));
    }
}
