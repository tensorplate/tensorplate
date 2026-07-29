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

use std::collections::BTreeMap;

use tensorplate_agent::{
    backend_detection::{probe_backend, BackendProbeReport, ProbeOptions},
    config::AgentConfig,
    coordinator::Coordinator,
    platform_admission::PlatformAdmission,
    recovery,
    server::Server,
    state::StateStore,
    supervision::{
        ensure_supervisor_directories, DesiredWorker, HttpReadinessProbe, MonotonicClock,
        RingEventSink, SystemMonotonicClock, SystemWorkerProcess, TickOutcome, WorkerSupervisor,
    },
    worker,
};
use tensorplate_platform::{PlatformRegistry, SystemHostProbe};
use tensorplate_protocol::install_paths::{BACKEND_DESCRIPTOR_DIR, PLATFORM_REGISTRY_DIR};

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

fn build_supervisor(cfg: &AgentConfig) -> Result<Option<Arc<WorkerSupervisor>>, String> {
    let Some(supervisor_cfg) = cfg.supervision.clone() else {
        return Ok(None);
    };
    ensure_supervisor_directories(&supervisor_cfg).map_err(|e| e.to_string())?;
    let process = Arc::new(SystemWorkerProcess::new(supervisor_cfg.clone()));
    let probe = Arc::new(HttpReadinessProbe::from_config(&supervisor_cfg));
    let clock: Arc<dyn MonotonicClock> = Arc::new(SystemMonotonicClock);
    let sink = Arc::new(RingEventSink::new(&supervisor_cfg.event_sink));
    let supervisor = WorkerSupervisor::new(supervisor_cfg, process, probe, clock)
        .map_err(|e| e.to_string())?
        .with_event_sink(sink);
    Ok(Some(Arc::new(supervisor)))
}

/// Probe every backend listed in `available_backends` for its
/// packaging readiness state. The map is then handed to the
/// coordinator so a `python_pytorch` deploy with no PyTorch (or no
/// descriptor at all) is refused before staging.
///
/// Backends with no descriptor file under
/// [`BACKEND_DESCRIPTOR_DIR`] log a one-line note and produce a
/// `DescriptorMissing` entry — the coordinator turns that into a
/// typed `BackendUnrunnable` deploy error.
///
/// Probing is best-effort: failures here never block agent startup.
/// The agent prefers to come up degraded so the CLI doctor can
/// surface the issue.
fn probe_available_backends(cfg: &AgentConfig) -> BTreeMap<String, BackendProbeReport> {
    let mut out = BTreeMap::new();
    for backend in &cfg.available_backends {
        // The Vitis AI / Kria adapter and the in-tree mock backend
        // have no on-disk descriptor in v0.1.0. Skipping them keeps
        // the probe map's invariant simple: an entry is present iff
        // the agent has a typed opinion about runnability.
        if matches!(
            backend.as_str(),
            "mock" | "vitis_ai" | "tensorrt" | "libtorch"
        ) {
            continue;
        }
        let descriptor_path = std::path::Path::new(BACKEND_DESCRIPTOR_DIR)
            .join(backend)
            .join("backend.json");
        let report = probe_backend(&descriptor_path, &ProbeOptions::default());
        eprintln!(
            "backend probe: backend={} state={:?} descriptor={}",
            report.backend_name,
            report.state,
            report.descriptor_path.display()
        );
        out.insert(backend.clone(), report);
    }
    out
}

/// Load the installed platform support registry once at startup.
///
/// Loading is best-effort in the same sense the backend probe is: the
/// agent prefers to come up without a registry so `tensorplate doctor`
/// can report why, rather than refusing to start and leaving the
/// operator no local tooling. What the agent must never do is treat an
/// absent registry as an empty one — hence `Option`, not a default.
fn load_platform_registry() -> Option<PlatformRegistry> {
    match PlatformRegistry::load_installed() {
        Ok(registry) => {
            eprintln!(
                "platform registry: rows={} supported={} roadmap_targets={} dir={}",
                registry.rows().count(),
                registry.supported_rows().count(),
                registry.roadmap_targets().count(),
                PLATFORM_REGISTRY_DIR
            );
            Some(registry)
        }
        Err(err) => {
            eprintln!("platform registry: unavailable ({err})");
            None
        }
    }
}

fn evaluate_platform_admission(
    registry: Option<&PlatformRegistry>,
    config: &mut AgentConfig,
) -> Option<PlatformAdmission> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let Some(registry) = registry else {
        return Some(PlatformAdmission::detection_failed(
            "installed platform registry is unavailable",
        ));
    };
    let admission = match SystemHostProbe::new().detect_platform() {
        Ok(report) => PlatformAdmission::evaluate(registry, &report),
        Err(err) => PlatformAdmission::detection_failed(err.to_string()),
    };
    admission.apply_memory_limit(config);
    eprintln!(
        "platform admission: row={} reason={} max_resident_model_memory={}",
        admission.row_id().unwrap_or("none"),
        admission
            .reason()
            .map_or("none", tensorplate_platform::PlatformReason::as_str),
        admission.capability().map_or(
            0,
            tensorplate_platform::PlatformCapability::max_resident_model_memory
        )
    );
    Some(admission)
}

fn load_runtime_config(
    args: &[String],
) -> Result<
    (
        AgentConfig,
        Option<PlatformRegistry>,
        Option<PlatformAdmission>,
    ),
    String,
> {
    let mut config = load_config(args)?;
    let registry = load_platform_registry();
    let admission = evaluate_platform_admission(registry.as_ref(), &mut config);
    Ok((config, registry, admission))
}

fn seed_supervisor_from_state(
    supervisor: &WorkerSupervisor,
    store: &StateStore,
) -> Result<(), String> {
    let snapshot = store.snapshot().map_err(|e| e.to_string())?;
    let desired = snapshot.active.as_ref().map(|active| DesiredWorker {
        deployment_id: active.deployment_id.clone(),
        backend: active.backend_hint.clone(),
    });
    supervisor
        .set_desired_active(desired)
        .map_err(|e| e.to_string())
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
    let (cfg, platform_registry, platform_admission) = match load_runtime_config(&args) {
        Ok(runtime) => runtime,
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
    let supervisor = match build_supervisor(&cfg) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("worker supervision error: {err}");
            return ExitCode::from(4);
        }
    };
    let backend_probes = probe_available_backends(&cfg);
    let mut coordinator =
        Coordinator::new(cfg.clone(), store.clone(), worker).with_backend_probes(backend_probes);
    if let Some(registry) = platform_registry {
        coordinator = coordinator.with_platform_registry(registry);
    }
    if let Some(admission) = platform_admission {
        coordinator = coordinator.with_platform_admission(admission);
    }
    if let Some(supervisor) = supervisor.as_ref() {
        coordinator = coordinator.with_supervisor(supervisor.clone());
    }
    let coordinator = Arc::new(coordinator);

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
    if let Some(supervisor) = supervisor.as_ref() {
        if let Err(err) = seed_supervisor_from_state(supervisor, &store) {
            eprintln!("worker supervision recovery failed: {err}");
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
    // phase advances. The `stop` flag below lets test harnesses ask the
    // binary to exit cleanly; production termination is still owned by
    // the process manager in v0.1.0.
    let stop = Arc::new(AtomicBool::new(false));
    while !stop.load(Ordering::Relaxed) {
        if let Some(supervisor) = supervisor.as_ref() {
            match supervisor.tick() {
                Ok(TickOutcome::Continue) => {}
                Ok(TickOutcome::Fault(fault)) => {
                    eprintln!(
                        "worker supervision fault: deployment={:?} class={:?} code={:?} message={}",
                        fault.deployment_id, fault.class, fault.error_code, fault.message
                    );
                }
                Ok(TickOutcome::Terminal(phase)) => {
                    eprintln!("worker supervision terminal state: {phase:?}");
                }
                Err(err) => {
                    eprintln!("worker supervision tick failed: {err}");
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    server.shutdown();
    ExitCode::SUCCESS
}
