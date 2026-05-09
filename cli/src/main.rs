// SPDX-License-Identifier: Apache-2.0

//! `tensorplate` CLI entrypoint placeholder.
//!
//! V01-E01-F03 only ships a runnable skeleton. The `doctor`, `deploy`,
//! `status`, `infer`, `logs`, and `rollback` subcommands land in V01-E11
//! and target the agent's local control API; the CLI never mutates the
//! serving worker directly.

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
        "usage: tensorplate [--version | --help]\n\
         \n\
         V01-E01-F03 scaffolding only. Subcommands (doctor, deploy, status, \n\
         infer, logs, rollback) land in V01-E11."
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {
            print_usage();
            ExitCode::SUCCESS
        }
        [arg] if arg == "--version" || arg == "-V" => {
            print_version();
            ExitCode::SUCCESS
        }
        [arg] if arg == "--help" || arg == "-h" => {
            print_usage();
            ExitCode::SUCCESS
        }
        _ => {
            print_usage();
            ExitCode::from(2)
        }
    }
}
