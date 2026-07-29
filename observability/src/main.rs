// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-observability` entrypoint (V01-E10).
//!
//! The binary parses the V01-E10 observability config, constructs the
//! [`tensorplate_observability::Service`] composition root, and runs
//! the heartbeat / safe-state / status snapshot / ROS 2 health pipeline
//! until signaled. The service is `tick`-driven; the main loop wakes on
//! the configured heartbeat interval, drains the listener, and persists
//! the snapshot.
//!
//! The service is intentionally independent of the agent and the
//! serving worker. It receives events through the V01-E02 health and
//! V01-E09 supervision-event schemas, evaluates heartbeats against a
//! monotonic clock, and emits a local safe-state event when the
//! aggregate state transitions to `degraded` / `failed` / `no-heartbeat`.

#![forbid(unsafe_code)]
#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tensorplate_observability::{InputSource, ObservabilityConfig, Service};
use tensorplate_platform::PlatformRegistry;
use tensorplate_protocol::install_paths;

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_version() {
    println!("{NAME} {VERSION}");
    println!("protocol: {}", tensorplate_protocol::version());
}

fn print_usage() {
    eprintln!(
        "usage: {NAME} [--version] [--config <path>] [--config-json <inline>]\n  \
         V01-E10 observability service. Heartbeat checks use a monotonic clock and\n  \
         never depend on the agent loop. ROS 2 health stub is optional and disabled\n  \
         by default; enable it through `ros2_health.enabled` in the config."
    );
}

fn load_config(args: &[String]) -> Result<ObservabilityConfig, String> {
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
        return ObservabilityConfig::parse_json(&raw).map_err(|e| e.to_string());
    }
    if let Some(text) = config_json {
        return ObservabilityConfig::parse_json(&text).map_err(|e| e.to_string());
    }
    // No config flag supplied: run with built-in defaults. Defaults are
    // local-only and never bind a socket, so this is safe for CI and
    // operator-driven `--version` checks.
    Ok(ObservabilityConfig::default())
}

/// Load the installed platform support registry and attach it to the
/// service.
///
/// Best-effort, and deliberately not fatal: observability reporting a
/// device's health is more useful than observability refusing to start
/// because a package it only reads from is missing. An absent registry
/// stays absent rather than becoming an empty one.
fn attach_platform_registry(service: Service) -> Service {
    let directory = match install_paths::platform_registry_dir() {
        Ok(directory) => directory,
        Err(err) => {
            eprintln!("platform registry: unavailable ({err})");
            return service;
        }
    };
    match PlatformRegistry::load(&directory) {
        Ok(registry) => {
            eprintln!(
                "platform registry: rows={} supported={} roadmap_targets={} dir={}",
                registry.rows().count(),
                registry.supported_rows().count(),
                registry.roadmap_targets().count(),
                directory.display()
            );
            service.with_platform_registry(registry)
        }
        Err(err) => {
            eprintln!("platform registry: unavailable ({err})");
            service
        }
    }
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
    let service = match Service::new(cfg) {
        Ok(s) => Arc::new(attach_platform_registry(s)),
        Err(err) => {
            eprintln!("observability service failed to start: {err}");
            return ExitCode::from(3);
        }
    };
    let interval = Duration::from_millis(service.config().heartbeat.expected_interval_ms);
    eprintln!(
        "tensorplate-observability primary_source={} interval={}ms ros2_enabled={}",
        service.primary_source().as_str(),
        service.config().heartbeat.expected_interval_ms,
        service.config().ros2_health.enabled
    );

    // v0.1.0 process model: the supervisor / systemd unit owns process
    // termination. The `stop` flag below lets test harnesses request a
    // clean exit; production termination relies on signal-default
    // behaviour, which is safe because every snapshot write is
    // atomic-replace and every safe-state event lands in a bounded
    // sink before the next tick begins.
    let stop = Arc::new(AtomicBool::new(false));
    while !stop.load(Ordering::Relaxed) {
        // Only explicitly internal deployments self-heartbeat. The
        // default `serving_worker` source must be refreshed by worker
        // events so a wedged or absent worker can transition to
        // no_heartbeat.
        if matches!(service.primary_source(), InputSource::Internal) {
            service.emit_internal_heartbeat();
        }
        let _events = service.tick();
        if let Err(err) = service.flush_snapshot() {
            eprintln!("snapshot flush failed: {err}");
        }
        std::thread::sleep(interval);
    }
    ExitCode::SUCCESS
}
