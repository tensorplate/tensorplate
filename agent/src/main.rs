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

use std::collections::{BTreeMap, BTreeSet};

use tensorplate_agent::{
    backend_detection::{probe_backend, BackendProbeReport, ProbeOptions},
    config::AgentConfig,
    coordinator::Coordinator,
    recovery,
    server::Server,
    state::StateStore,
    supervision::{
        ensure_supervisor_directories, DesiredWorker, HttpReadinessProbe, MonotonicClock,
        RingEventSink, SystemMonotonicClock, SystemWorkerProcess, TickOutcome, WorkerSupervisor,
    },
    worker,
};
use tensorplate_platform::{
    AcceleratorProbe, DetectedPlatform, HostProbe, NvidiaSmiProbe, PlatformRegistry,
    SystemHostProbe,
};

use tensorplate_agent::platform_admission::{evaluate_platform, ObservedStack, PlatformAdmission};
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
/// Settle the bundle-independent platform verdict at startup.
///
/// Detection failing is NOT a rejection: an agent that cannot read its own
/// hardware has no basis to refuse a deploy, and turning "I could not look"
/// into "your platform is unsupported" is the collapse this codebase
/// refuses everywhere else. The verdict is simply absent, and admission
/// stays silent — doctor is what an operator runs to diagnose that.
fn settle_platform_admission(registry: &PlatformRegistry) -> Option<PlatformAdmission> {
    let host = match SystemHostProbe::new().detect_host() {
        Ok(host) => host,
        Err(err) => {
            eprintln!(
                "platform admission: host identity unreadable, deploy admission disabled: {err}"
            );
            return None;
        }
    };
    let accelerator = match NvidiaSmiProbe::new().detect_accelerator() {
        Ok(accelerator) => accelerator,
        Err(err) => {
            eprintln!(
                "platform admission: accelerator identity unreadable, deploy admission disabled: {err}"
            );
            return None;
        }
    };
    let detected = match accelerator {
        Some(accelerator) => DetectedPlatform::with_accelerator(host, accelerator),
        None => DetectedPlatform::host_only(host),
    };
    // The driver/runtime stack and package set are observations the row is
    // compared against. Rows record no components until their first
    // evidence run, so this is empty today by construction rather than by
    // omission.
    let observed = ObservedStack {
        components: BTreeMap::new(),
        installed_packages: installed_packages(),
    };
    let verdict = evaluate_platform(registry, &detected, &observed);
    eprintln!("platform admission: {verdict:?}");
    Some(PlatformAdmission::new(verdict, observed.installed_packages))
}

/// Package names dpkg reports as installed.
///
/// Queried without a name pattern. A `tensorplate*` glob would cover
/// today's rows, which only require our own packages — but a row is free
/// to require a driver or runtime package, and a glob that silently
/// excluded it would report an installed package as missing and refuse a
/// deploy that should have been admitted.
///
/// An unreadable package database yields an empty set. That can only make
/// admission stricter: a missing package is a rejection, never a pass.
fn installed_packages() -> BTreeSet<String> {
    let Ok(output) = std::process::Command::new("dpkg-query")
        .args(["-W", "-f=${binary:Package} ${db:Status-Status}\n"])
        .output()
    else {
        return BTreeSet::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (name, status) = line.split_once(' ')?;
            (status.trim() == "installed").then(|| name.to_string())
        })
        .collect()
}

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
    if let Some(registry) = load_platform_registry() {
        // Settle the machine-level question once. A deploy then compares
        // against this rather than re-probing hardware on every request.
        if let Some(admission) = settle_platform_admission(&registry) {
            coordinator = coordinator.with_platform_admission(admission);
        }
        coordinator = coordinator.with_platform_registry(registry);
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
