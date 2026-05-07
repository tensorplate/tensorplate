// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-agent` entrypoint placeholder.
//!
//! V01-E01-F03 only ships a runnable skeleton so the workspace and
//! packaging story has a real binary target before the desired-state store,
//! deploy transaction, and worker supervision land in V01-E08 / V01-E09.

#![forbid(unsafe_code)]

use std::process::ExitCode;

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_version() {
    println!("{NAME} {VERSION}");
    println!("protocol: {}", tensorplate_protocol::version());
}

fn print_usage() {
    eprintln!(
        "usage: {NAME} [--version]\n  V01-E01-F03 scaffolding only; \
         deploy/transaction logic lands in V01-E08."
    );
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
            print_usage();
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn protocol_link_is_present() {
        assert!(!tensorplate_protocol::version().is_empty());
    }
}
