# Context

## Glossary

### Readiness loop

The runtime Module that repeatedly checks all configured dependencies until one
terminal outcome is reached: all dependencies are ready in the same round,
finite max-wait is exhausted, or the process is interrupted by SIGTERM or
SIGINT.

Its Interface should expose structured lifecycle events and a final status.
Rendering those events into stderr text belongs outside the readiness loop, in a
small CLI-facing renderer.

SIGTERM and SIGINT handling are internal seams of the readiness loop Module. The
production Adapter listens to OS signals, while tests may use a controllable
Adapter. Callers should not need to coordinate interruption while the loop is
sleeping or while checks are in flight.

HTTP execution is also an internal seam of the readiness loop Module. The
production Adapter uses reqwest and remains the only real HTTP Adapter for v1.
Tests may use a controllable Adapter to return statuses, errors, delays, or
blocked in-flight checks without starting TCP servers for every loop lifecycle
case.

Time input is a narrow internal seam of the readiness loop Module. The
production Adapter uses Tokio time, while tests may use a controllable Adapter
to advance time and verify finite max-wait, effective request timeout, and
sleep budgeting without relying on real wall-clock waits.

Readiness loop Module Interface tests should cover lifecycle rules: same-round
readiness, no ready-state latching, finite and infinite max-wait behaviour,
effective request timeout budgeting, sleep budgeting, interruption while checks
are in flight, and event deduplication. CLI end-to-end tests should keep real
edge behaviour: CLI and YAML parsing, configuration precedence, reqwest HTTP
semantics, TLS and connection error classification, stderr rendering without URL
leaks, real SIGTERM and SIGINT smoke coverage, and process exit codes.

The readiness loop Module Interface is crate-private. The stable external
contract remains the CLI behaviour and `run_cli`; loop events, Adapter shapes,
and internal seams should not become public library commitments.

Implementation may move the readiness loop into a single private module file,
such as `src/readiness_loop.rs`. Keep it as one deep Module: do not split the
loop lifecycle, events, state, and internal Adapter seams into many shallow
modules.

### Diagnostics Module

The diagnostics Module is the crate-private CLI-facing renderer for stable
stderr text. It owns the mapping from validated configuration errors and
readiness loop events into sanitized key-value lines.

The diagnostics Module is responsible for elapsed duration formatting, signal
labels, error category names, dependency state text, configuration validation
summaries, and the no-URL logging guarantee. Intake and readiness loop Modules
should expose validated facts and structured events; they should not format
stderr strings or decide diagnostic category names.
