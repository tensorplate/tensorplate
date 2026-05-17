// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-agent` entrypoint (V01-E08).
//!
//! Parses the agent config (V01-E08-F01), opens the durable state store
//! (V01-E08-F02), computes a startup recovery plan (V01-E08-F07), starts
//! the local control API (V01-E08-F01), and runs until signaled.
//!
//! The agent is the only management-plane entry point for deploy and
//! rollback operations; the CLI (V01-E11) talks to this binary, never to
//! the serving worker directly. The serving worker's data plane is
//! supervised through the `worker::WorkerControl` interface; v0.1.0 can
//! run either the deterministic in-tree mock worker or the process-backed
//! V01-E07 `tensorplate-serving` client selected by config.

#![forbid(unsafe_code)]
#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tensorplate_agent::{
    config::AgentConfig, coordinator::Coordinator, recovery, server::Server, state::StateStore,
    worker,
};

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_version() {
    println!("{NAME} {VERSION}");
    println!("protocol: {}", tensorplate_protocol::version());
}

fn print_usage() {
    eprintln!(
        "usage: {NAME} [--version] [--config <path>] [--config-json <inline>]\n  \
         Local control API speaks the v0.1 schema documented at\n  \
         protocol/schemas/agent_control.json."
    );
}

fn load_config(args: &[String]) -> Result<AgentConfig, String> {
    let mut config_path: Option<PathBuf> = None;
    let mut config_json: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                config_path = iter.next().cloned().map(PathBuf::from);
            }
            "--config-json" => {
                config_json = iter.next().cloned();
            }
            "--version" | "-V" | "--help" | "-h" => {}
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    if let Some(path) = config_path {
        let raw =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        return AgentConfig::parse_json(&raw).map_err(|e| e.to_string());
    }
    if let Some(text) = config_json {
        return AgentConfig::parse_json(&text).map_err(|e| e.to_string());
    }
    Err("--config <path> or --config-json <inline> is required".into())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        print_version();
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return ExitCode::SUCCESS;
    }
    let cfg = match load_config(&args) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("config error: {err}");
            return ExitCode::from(2);
        }
    };
    let store = match StateStore::open(cfg.state_dir.clone()) {
        Ok(s) => Arc::new(s),
        Err(err) => {
            eprintln!("state store error: {err}");
            return ExitCode::from(3);
        }
    };
    let worker = match worker::from_config(&cfg) {
        Ok(w) => w,
        Err(err) => {
            eprintln!("worker control error: {err}");
            return ExitCode::from(4);
        }
    };
    let coordinator = Arc::new(Coordinator::new(cfg.clone(), store, worker));

    // Startup recovery runs before the control socket opens so replayable
    // transactions are resumed and unsafe candidates are quarantined
    // before new mutating requests can arrive.
    match recovery::apply_startup(coordinator.as_ref()) {
        Ok(plan) => {
            eprintln!(
                "startup recovery: {:?} ({})",
                plan.action,
                plan.reason.as_deref().unwrap_or("")
            );
        }
        Err(err) => {
            eprintln!("startup recovery failed: {err}");
            return ExitCode::from(5);
        }
    }

    let mut server = match Server::start(&cfg, coordinator) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("agent listener failed to start: {err}");
            return ExitCode::from(6);
        }
    };
    eprintln!("tensorplate-agent listening on {}", server.address);

    // v0.1.0 agent process model: rely on systemd / supervisor to deliver
    // SIGTERM. Without an installed handler the default action is process
    // termination, which is safe because every durable mutation lands
    // through `StateStore::update`'s atomic-replace path before each
    // phase advances. The `stop` flag below is reserved for the future
    // V01-E09 supervisor integration that does install handlers; today
    // it lets test harnesses ask the binary to exit cleanly.
    let stop = Arc::new(AtomicBool::new(false));
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    server.shutdown();
    ExitCode::SUCCESS
}
