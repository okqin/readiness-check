#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, missing_docs, rust_2024_compatibility)]

//! Binary entrypoint for `readiness-check`.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    readiness_check::run().await
}
