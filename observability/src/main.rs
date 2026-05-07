// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-observability` entrypoint placeholder.
//!
//! V01-E01-F03 only ships a runnable skeleton. Heartbeat detection, the
//! ready/degraded/failed/no-heartbeat state machine, and the safe-state
//! event surface land in V01-E10. Heartbeat checks must use a monotonic
//! clock and must not block the serving request path.

#![forbid(unsafe_code)]

use std::process::ExitCode;

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_version() {
    println!("{NAME} {VERSION}");
    println!("protocol: {}", tensorplate_protocol::version());
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {
            print_version();
            ExitCode::SUCCESS
        }
        [arg] if arg == "--version" || arg == "-V" => {
            print_version();
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!(
                "usage: {NAME} [--version]\n  V01-E01-F03 scaffolding only; \
                 health monitor lands in V01-E10."
            );
            ExitCode::from(2)
        }
    }
}
