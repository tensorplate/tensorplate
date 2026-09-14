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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use std::collections::{BTreeMap, BTreeSet};

use tensorplate_agent::{
    backend_detection::{probe_backend, BackendProbeReport, ProbeOptions},
    config::AgentConfig,
    coordinator::Coordinator,
    platform_admission::{ObservedStack, PlatformAdmission},
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
    identify_platform, AdmissionPosture, HostSources, MachineTypeSource, NvidiaSmiProbe,
    PlatformProbeError, PlatformRegistry, PlatformReport, RecordWrite, SystemHostProbe,
};
use tensorplate_protocol::install_paths;

const NAME: &str = env!("CARGO_PKG_NAME");
// A release build may carry an identity Cargo does not: a candidate is
// built from the same tree as the release it is a candidate for, so
// CARGO_PKG_VERSION reports `0.2.1` for both and `--version` cannot tell
// them apart. The release build supplies TP_RELEASE_VERSION; everything
// else falls back to the crate version.
const VERSION: &str = match option_env!("TP_RELEASE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

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

fn print_requested_info(args: &[String]) -> Option<ExitCode> {
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        print_version();
        return Some(ExitCode::SUCCESS);
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        return Some(ExitCode::SUCCESS);
    }
    None
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
fn probe_available_backends(
    cfg: &AgentConfig,
    descriptor_dir: &Path,
) -> BTreeMap<String, BackendProbeReport> {
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
        let descriptor_path = descriptor_dir.join(backend).join("backend.json");
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
fn load_platform_registry(directory: &Path) -> Option<PlatformRegistry> {
    match PlatformRegistry::load(directory) {
        Ok(registry) => {
            eprintln!(
                "platform registry: rows={} supported={} roadmap_targets={} dir={}",
                registry.rows().count(),
                registry.supported_rows().count(),
                registry.roadmap_targets().count(),
                directory.display()
            );
            Some(registry)
        }
        Err(err) => {
            eprintln!("platform registry: unavailable ({err})");
            None
        }
    }
}

/// Settle the bundle-independent platform verdict at startup.
///
/// Runs on every platform. It was macOS-only while Apple silicon was the
/// only integrated part wired up; Ubuntu/NVIDIA and Jetson now resolve
/// through the same path, and gating by target would leave the platforms
/// with the most rows ungated.
///
/// A detection failure is recorded as a REJECTION rather than as an absent
/// verdict. "I could not look" must not become "your platform is
/// unsupported" — so the rejection carries no frozen reason — but it must
/// also not become "deploy anything". An absent verdict skips the gate
/// entirely, on exactly the hardware nobody has characterised.
fn evaluate_platform_admission(
    registry: Option<&PlatformRegistry>,
    config: &mut AgentConfig,
) -> PlatformAdmission {
    let Some(registry) = registry else {
        return PlatformAdmission::detection_failed("installed platform registry is unavailable");
    };
    // Already validated at config load, so an unparsable value cannot
    // reach here; `ok()` selects the row floor rather than guessing.
    let operator_posture = config
        .admission_posture
        .as_deref()
        .and_then(|value| value.parse::<AdmissionPosture>().ok());
    let admission = match observe_platform() {
        // The accelerator could not be probed. Classified from the host
        // report rather than reported as a bare detection failure, so a
        // broken driver on a card that IS on the bus is named as one.
        Ok((report, _observed, Some(err))) => {
            PlatformAdmission::accelerator_probe_failed(&report.host, &err)
        }
        Ok((report, observed, None)) => {
            PlatformAdmission::evaluate(registry, &report, &observed, operator_posture)
        }
        Err(err) => detection_failed(&err, &mut std::io::stderr()),
    };
    admission.apply_memory_limit(config);
    // The posture is reported with its provenance, not just its value. An
    // operator who can see which strictness they are running at, and
    // whether it came from the row or from their own config, can pin it --
    // which is what keeps a future change to the default from being a
    // silent behaviour change on upgrade.
    let (posture, posture_from) = admission
        .posture()
        .map_or(("none", "none"), |(p, from)| (p.as_str(), from));
    // Said out loud on every start. A machine admitted without evidence
    // covering it runs exactly like one that has it, so the log line is
    // the only place the difference is visible -- and an operator who
    // cannot see it cannot decide whether they mind.
    let evidence = match admission.validated() {
        Some(true) => "validated",
        Some(false) => "unvalidated (admitted on technical prerequisites)",
        None => "none",
    };
    eprintln!(
        "platform admission: row={} reason={} posture={posture} ({posture_from}) \
         evidence={evidence} max_resident_model_memory={}",
        admission.row_id().unwrap_or("none"),
        admission
            .reason()
            .map_or("none", tensorplate_platform::PlatformReason::as_str),
        admission.capability().map_or(
            0,
            tensorplate_platform::PlatformCapability::max_resident_model_memory
        )
    );
    admission
}

/// Packages the host's native package manager reports as installed.
///
/// The platform rows use Homebrew package names on macOS and Debian package
/// names on the Linux targets. Querying the wrong database is indistinguishable
/// from an empty database and would reject every otherwise valid deployment.
#[cfg(target_os = "macos")]
fn installed_packages() -> BTreeSet<String> {
    let Ok(output) = std::process::Command::new("brew")
        .args(["list", "--formula"])
        .output()
    else {
        return BTreeSet::new();
    };
    if !output.status.success() {
        return BTreeSet::new();
    }
    parse_homebrew_packages(&output.stdout)
}

#[cfg(not(target_os = "macos"))]
fn installed_packages() -> BTreeSet<String> {
    let Ok(output) = std::process::Command::new("dpkg-query")
        .args(["-W", "-f=${binary:Package} ${db:Status-Status}\n"])
        .output()
    else {
        return BTreeSet::new();
    };
    if !output.status.success() {
        return BTreeSet::new();
    }
    parse_dpkg_packages(&output.stdout)
}

#[cfg(any(test, target_os = "macos"))]
fn parse_homebrew_packages(stdout: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(any(test, not(target_os = "macos")))]
fn parse_dpkg_packages(stdout: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let (name, status) = line.split_once(' ')?;
            (status.trim() == "installed").then(|| name.to_string())
        })
        .collect()
}

/// Everything the verdict is computed from, gathered once.
///
/// An integrated accelerator is already in the platform report: it is
/// identified from the same sources the host is. A discrete card is not —
/// it is a separate device the vendor tool enumerates — so it is asked for
/// only when the report carries none.
/// Observe the platform.
///
/// A HOST probe failure is still fatal — without the host there is nothing
/// to reason about. An ACCELERATOR probe failure is returned alongside the
/// report instead of replacing it: the host report carries the PCI
/// evidence that says whether the failure is a broken driver on a real
/// card, and propagating the error here throws that evidence away before
/// anything can read it. That is what made the usual broken-driver case —
/// an installed `nvidia-smi` exiting non-zero — surface as an untyped
/// detection failure.
fn observe_platform(
) -> Result<(PlatformReport, ObservedStack, Option<PlatformProbeError>), PlatformProbeError> {
    let probe = SystemHostProbe::new();
    let sources = probe.sources()?;
    let mut report = identify_and_record(&probe, &sources, &mut std::io::stderr())?;
    let mut accelerator_probe_error = None;
    if report.accelerator.is_none() {
        match NvidiaSmiProbe::new().detect() {
            Ok(Some(card)) => {
                report.accelerator = Some(card.observation());
            }
            Ok(None) => {}
            Err(err) => accelerator_probe_error = Some(err),
        }
    }
    // Rows record no driver components until their first evidence run, so
    // this is empty today by construction rather than by omission.
    let observed = ObservedStack {
        components: BTreeMap::new(),
        installed_packages: installed_packages(),
    };
    Ok((report, observed, accelerator_probe_error))
}

/// Identify the platform from `sources`, refresh the machine-type record,
/// and say both on `log` as the `platform identity:` line.
///
/// The record is refreshed on every start where the metadata service
/// answered, and never from a machine type that was itself read from the
/// record. A failed write is reported, not fatal: this start has its
/// identity.
fn identify_and_record(
    probe: &SystemHostProbe,
    sources: &HostSources,
    log: &mut impl Write,
) -> Result<PlatformReport, PlatformProbeError> {
    let report = identify_platform(sources)?;
    let record = probe.write_machine_type_record(sources);
    // The journal is where this goes; a start does not fail over a log line.
    let _ = writeln!(
        log,
        "{}",
        platform_identity_line(
            report.host.identity.machine_type.as_deref(),
            report.host.exact.machine_type_source,
            &record,
        )
    );
    Ok(report)
}

/// A detection failure's admission, said on `log` first. The admission line
/// carries no detail for an undetected host, so this is where an offline
/// refusal says why.
fn detection_failed(err: &PlatformProbeError, log: &mut impl Write) -> PlatformAdmission {
    let _ = writeln!(log, "platform detection failed: {err}");
    PlatformAdmission::detection_failed(err.to_string())
}

/// The `platform identity:` start-up line: the machine type, where it came
/// from, and what recording it did. The only place outside doctor that says
/// whether an instance's shape came from the metadata service or from the
/// record, so the offline lifecycle stage can require the right one.
fn platform_identity_line(
    machine_type: Option<&str>,
    source: Option<MachineTypeSource>,
    record: &Result<RecordWrite, PlatformProbeError>,
) -> String {
    let record = match record {
        Ok(write) => write.to_string(),
        Err(err) => format!("failed ({err})"),
    };
    format!(
        "platform identity: machine_type={} source={} record={record}",
        machine_type.unwrap_or("none"),
        source.map_or("none", MachineTypeSource::as_str)
    )
}

fn load_runtime_config(
    args: &[String],
) -> Result<
    (
        AgentConfig,
        PathBuf,
        Option<PlatformRegistry>,
        PlatformAdmission,
    ),
    String,
> {
    let mut config = load_config(args)?;
    let backend_descriptor_dir = install_paths::backend_descriptor_dir()?;
    let platform_registry_dir = install_paths::platform_registry_dir()?;
    let registry = load_platform_registry(&platform_registry_dir);
    let admission = evaluate_platform_admission(registry.as_ref(), &mut config);
    Ok((config, backend_descriptor_dir, registry, admission))
}

fn seed_supervisor_from_state(
    supervisor: &WorkerSupervisor,
    store: &StateStore,
) -> Result<(), String> {
    let snapshot = store.snapshot().map_err(|e| e.to_string())?;
    let desired = snapshot.active.as_ref().map(|active| DesiredWorker {
        deployment_id: active.deployment_id.clone(),
        backend: active.backend_hint.clone(),
        // Reconciled from durable state, which records what is deployed and
        // not where it runs. Unpinned for the same reason the coordinator is:
        // nothing allocates devices yet.
        device_index: None,
    });
    supervisor
        .set_desired_active(desired)
        .map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(exit_code) = print_requested_info(&args) {
        return exit_code;
    }
    let (cfg, backend_descriptor_dir, platform_registry, platform_admission) =
        match load_runtime_config(&args) {
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
    let backend_probes = probe_available_backends(&cfg, &backend_descriptor_dir);
    let mut coordinator =
        Coordinator::new(cfg.clone(), store.clone(), worker).with_backend_probes(backend_probes);
    if let Some(registry) = platform_registry {
        coordinator = coordinator.with_platform_registry(registry);
    }
    // Always present: a detection failure is itself a verdict, so there is
    // no path on which the agent runs with no platform gate at all.
    coordinator = coordinator.with_platform_admission(platform_admission);
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
    // phase advances. The `stop` flag lets tests request a clean exit;
    // production termination is still owned by the process manager.
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::{
        detection_failed, identify_and_record, parse_dpkg_packages, parse_homebrew_packages,
        platform_identity_line,
    };
    use tensorplate_agent::error::AgentError;
    use tensorplate_platform::{
        HostSources, MachineTypeSource, PlatformProbeError, RecordWrite, SystemHostProbe,
    };
    use tensorplate_protocol::install_paths::MACHINE_TYPE_RECORD_PATH;

    /// The recorded g2-standard-8 L4 host, as a start with the metadata
    /// service reachable gathers it.
    fn l4_live_sources() -> HostSources {
        let body = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../test/platform/host_identity/ubuntu2404-x86-l4-g2s8.json"),
        )
        .expect("the recorded L4 fixture is committed");
        let fixture: serde_json::Value = serde_json::from_str(&body).expect("fixture parses");
        let text = |key: &str| {
            fixture["sources"]
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        HostSources {
            uname_machine: text("uname_machine"),
            os_release: text("os_release"),
            cpuinfo: text("cpuinfo"),
            nv_tegra_release: text("nv_tegra_release"),
            nvidia_jetpack_version: text("nvidia_jetpack_version"),
            device_tree_model: text("device_tree_model"),
            sw_vers_product_name: text("sw_vers_product_name"),
            sw_vers_product_version: text("sw_vers_product_version"),
            sw_vers_build_version: text("sw_vers_build_version"),
            cpu_brand: text("cpu_brand"),
            hw_memsize: text("hw_memsize"),
            dmi_product_name: Some("Google Compute Engine\n".to_string()),
            gce_machine_type: text("gce_machine_type"),
            machine_type_record: None,
            proc_meminfo: text("proc_meminfo"),
            pci_devices: text("pci_devices"),
        }
    }

    #[test]
    fn a_start_with_a_live_answer_records_it_and_an_offline_start_uses_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let record = root
            .path()
            .join(MACHINE_TYPE_RECORD_PATH.trim_start_matches('/'));
        std::fs::create_dir_all(record.parent().expect("parent")).expect("stage state/");
        let probe = SystemHostProbe::with_root(root.path());
        let live = l4_live_sources();

        let mut log = Vec::new();
        let report = identify_and_record(&probe, &live, &mut log).expect("detects");
        assert_eq!(
            report.host.identity.machine_type.as_deref(),
            Some("g2-standard-8")
        );
        assert_eq!(
            String::from_utf8(log).expect("utf-8"),
            "platform identity: machine_type=g2-standard-8 source=gce_metadata record=written\n"
        );
        let written = std::fs::read(&record).expect("the start recorded the machine type");

        let mut log = Vec::new();
        identify_and_record(&probe, &live, &mut log).expect("detects");
        assert_eq!(
            String::from_utf8(log).expect("utf-8"),
            "platform identity: machine_type=g2-standard-8 source=gce_metadata record=unchanged\n"
        );

        let offline = HostSources {
            gce_machine_type: None,
            machine_type_record: Some(String::from_utf8(written.clone()).expect("utf-8")),
            ..live
        };
        let mut log = Vec::new();
        let report = identify_and_record(&probe, &offline, &mut log).expect("detects offline");
        assert_eq!(
            report.host.identity.machine_type.as_deref(),
            Some("g2-standard-8")
        );
        assert_eq!(
            String::from_utf8(log).expect("utf-8"),
            "platform identity: machine_type=g2-standard-8 source=recorded_gce_metadata \
             record=not_applicable\n"
        );
        assert_eq!(
            std::fs::read(&record).expect("still there"),
            written,
            "an offline start never rewrites the record"
        );
    }

    #[test]
    fn a_detection_failure_is_said_on_its_own_line_and_refuses_deploys() {
        let err = PlatformProbeError::IdentityUnestablished {
            source_name: MACHINE_TYPE_RECORD_PATH.to_string(),
            detail: "no machine type has been recorded on this host".to_string(),
        };
        let mut log = Vec::new();
        let admission = detection_failed(&err, &mut log);
        assert_eq!(
            String::from_utf8(log).expect("utf-8"),
            format!("platform detection failed: {err}\n")
        );
        match admission.ensure_supported() {
            Err(AgentError::PlatformNotAdmissible { detail, .. }) => {
                assert!(
                    detail.contains("no machine type has been recorded"),
                    "{detail}"
                );
            }
            other => panic!("a detection failure must refuse deploys, got {other:?}"),
        }
    }

    #[test]
    fn the_identity_line_names_the_machine_type_its_source_and_the_record_write() {
        assert_eq!(
            platform_identity_line(
                Some("g2-standard-8"),
                Some(MachineTypeSource::GceMetadata),
                &Ok(RecordWrite::Written)
            ),
            "platform identity: machine_type=g2-standard-8 source=gce_metadata record=written"
        );
        assert_eq!(
            platform_identity_line(
                Some("g2-standard-8"),
                Some(MachineTypeSource::GceMetadata),
                &Ok(RecordWrite::Unchanged)
            ),
            "platform identity: machine_type=g2-standard-8 source=gce_metadata record=unchanged"
        );
        assert_eq!(
            platform_identity_line(
                Some("g2-standard-8"),
                Some(MachineTypeSource::RecordedFromMetadata),
                &Ok(RecordWrite::NotApplicable)
            ),
            "platform identity: machine_type=g2-standard-8 source=recorded_gce_metadata \
             record=not_applicable"
        );
        assert_eq!(
            platform_identity_line(None, None, &Ok(RecordWrite::NotApplicable)),
            "platform identity: machine_type=none source=none record=not_applicable"
        );
        assert_eq!(
            platform_identity_line(
                Some("g2-standard-8"),
                Some(MachineTypeSource::GceMetadata),
                &Ok(RecordWrite::FactsUnavailable("MemTotal"))
            ),
            "platform identity: machine_type=g2-standard-8 source=gce_metadata \
             record=not_recorded (unavailable: MemTotal)"
        );
        let failed = platform_identity_line(
            Some("g2-standard-8"),
            Some(MachineTypeSource::GceMetadata),
            &Err(PlatformProbeError::Unreadable {
                source_name: "/var/lib/tensorplate/state/machine-type.json".to_string(),
                detail: "permission denied".to_string(),
            }),
        );
        assert!(
            failed.starts_with(
                "platform identity: machine_type=g2-standard-8 source=gce_metadata record=failed ("
            ) && failed.contains("permission denied"),
            "{failed}"
        );
    }

    #[test]
    fn homebrew_inventory_uses_formula_names() {
        let installed = parse_homebrew_packages(
            b"tensorplate-agent\ntensorplate-backend-python-pytorch\npython@3.14\n",
        );
        assert!(installed.contains("tensorplate-backend-python-pytorch"));
        assert!(!installed.contains(""));
    }

    #[test]
    fn dpkg_inventory_keeps_only_installed_packages() {
        let installed = parse_dpkg_packages(
            b"tensorplate-agent installed\ntensorplate-backend-python-pytorch not-installed\n",
        );
        assert!(installed.contains("tensorplate-agent"));
        assert!(!installed.contains("tensorplate-backend-python-pytorch"));
    }
}
