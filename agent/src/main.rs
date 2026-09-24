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
use std::time::{Duration, Instant};

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
///
/// Called exactly once per process, from `load_runtime_config`. The
/// detection retry budget inside `observe_platform` is therefore a
/// per-start budget, not a per-call one.
fn evaluate_platform_admission(
    registry: Option<&PlatformRegistry>,
    config: &mut AgentConfig,
) -> PlatformAdmission {
    let Some(registry) = registry else {
        return PlatformAdmission::detection_failed("installed platform registry is unavailable");
    };
    // Observing is what the registry check gates, so it stays behind it:
    // a host with no registry must not pay the detection retry budget to
    // reach a verdict that is already settled.
    admit_observed_platform(registry, config, observe_platform())
}

/// Turn one settled observation into the held verdict.
///
/// Split from `evaluate_platform_admission` so the verdict can be driven
/// from a scripted observation without a host: what a deploy is refused or
/// admitted on is this function's output, and that is what the retry has
/// to be shown to change.
fn admit_observed_platform(
    registry: &PlatformRegistry,
    config: &mut AgentConfig,
    observation: Result<Observation, PlatformProbeError>,
) -> PlatformAdmission {
    // Already validated at config load, so an unparsable value cannot
    // reach here; `ok()` selects the row floor rather than guessing.
    let operator_posture = config
        .admission_posture
        .as_deref()
        .and_then(|value| value.parse::<AdmissionPosture>().ok());
    let admission = match observation {
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
    // The verdict must be settled before this call. `apply_memory_limit`
    // early-returns on anything that is not `Supported { capability:
    // Some(_) }`, so a verdict that becomes supported AFTER this line runs
    // leaves `device_memory_bytes` as configured — which the shipped
    // config leaves unset, and both memory gates are Option-gated. That
    // admits deploys on L4 and H100 with both memory checks silently
    // disabled. The detection retry is inside `observe_platform` for
    // exactly this reason; any future late re-detect must re-apply the
    // ceiling here or it reintroduces that hole.
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

/// How many observation attempts one start may make, counting the first.
const DETECTION_RETRY_ATTEMPTS: u32 = 6;
/// The delay before the second attempt; every later one doubles it.
const DETECTION_RETRY_FIRST_DELAY: Duration = Duration::from_millis(500);
/// The ceiling on the retry SCHEDULE: no sleep is started that would end
/// past it, and a sleep that would is shortened to whatever is left.
///
/// It is not a ceiling on total elapsed time. The attempt that runs after
/// the last permitted sleep is not itself bounded by this, so an episode
/// ends at up to this budget plus one attempt's work — and an attempt's
/// work has no timeout of its own, because `SystemHostProbe::sources`
/// forks `uname -m` through an untimed `Command::output()`. The metadata
/// connect inside it is capped at 250ms; nothing else is.
///
/// Deliberately short. Exhausting the budget is byte-identical to the
/// behaviour before the retry existed, so being too short costs nothing
/// relative to that; being too long costs real availability, because
/// `load_runtime_config` runs before the state store is opened, before
/// startup recovery, before the previously-active worker is re-warmed and
/// before the control socket is listening — and the CLI does not retry, so
/// `tensorplate status` fails outright for the length of the window.
///
/// RAISING THIS HAS A HARD CEILING ABOVE IT. `packaging/scripts/install.sh`
/// waits `TP_INSTALL_SERVICE_READY_TIMEOUT_SECONDS` (default 30) for the
/// agent socket to appear and then `die`s, and this budget is spent before
/// the socket is created — as is the backend probe that follows it. A
/// budget raised toward 30s turns an install that would have completed
/// degraded, with doctor reporting the detection failure, into a hard
/// install failure with the packages already on disk. Raise the installer
/// default in the same change, or stay well under it.
///
/// IT ALSO LENGTHENS A RESTART CYCLE, WHICH THE UNIT NOW ACCOUNTS FOR.
/// `StartLimitBurst=5` / `StartLimitIntervalSec` rate-limit unit STARTS, so
/// they only bite when five starts land inside the window. A start that
/// pays this budget and then fails after admission — an unopenable state
/// store, a bound socket — takes this budget plus `RestartSec=5` per cycle,
/// so at a 60s window at most three starts fitted and the unit restarted
/// indefinitely instead of settling into `failed` where `systemctl status`
/// can see it. Both units now set `StartLimitIntervalSec=300`, which holds
/// five worst-case cycles; the derivation is in the unit beside the value.
/// RAISING THIS BUDGET EATS INTO THAT MARGIN — five cycles must stay inside
/// the window, so a materially larger budget needs the window raised in the
/// same change.
///
/// This number is a first estimate. Nobody has measured the gap between
/// `network.target` and the first metadata answer on either production
/// cloud row; the recovered, exhausted and stopped lines report exactly
/// that gap, so the fleet's own journals are what should correct it.
const DETECTION_RETRY_BUDGET: Duration = Duration::from_secs(20);

/// Everything one successful observation settles: the platform report, the
/// observed stack, and an accelerator probe failure if the host was
/// readable but its card was not.
type Observation = (PlatformReport, ObservedStack, Option<PlatformProbeError>);

/// Which step of one observation attempt failed.
///
/// The two steps raise the same error VARIANT and mean different things.
/// `sources()` raises `IdentityUnestablished` only from `unusable_record`:
/// the machine-type record is a directory, a symlinked path, or oversized.
/// That is a tamper signal, it is deterministic, and retrying it would let
/// a later live answer overwrite the record without the signal ever being
/// reported. `identify` raises it only from `establish_machine_type`: this
/// host is a Compute Engine instance whose metadata service could not be
/// reached and which has no usable record for this boot. Only the second
/// is retryable, and tagging the step is what tells them apart without
/// matching on prose.
#[derive(Debug)]
enum ObservationFailure {
    Sources(PlatformProbeError),
    Identify(PlatformProbeError),
}

impl ObservationFailure {
    /// The error this step raised, for the log line that reports it.
    fn error(&self) -> &PlatformProbeError {
        match self {
            Self::Sources(err) | Self::Identify(err) => err,
        }
    }

    /// Drop the step tag. Which step failed decides whether to retry; once
    /// the loop is over, what the caller holds is the error itself, so the
    /// existing `platform detection failed:` line is unchanged.
    fn into_error(self) -> PlatformProbeError {
        match self {
            Self::Sources(err) | Self::Identify(err) => err,
        }
    }
}

/// One observation attempt: everything the verdict is computed from,
/// gathered once, tagged with the step that failed if one did.
///
/// Which step raised the failure is what decides whether another attempt
/// could settle it — see [`ObservationFailure`], because the two steps
/// raise the same error variant and mean different things.
///
/// An integrated accelerator is already in the platform report: it is
/// identified from the same sources the host is. A discrete card is not —
/// it is a separate device the vendor tool enumerates — so it is asked for
/// only when the report carries none.
///
/// A HOST probe failure is still fatal — without the host there is nothing
/// to reason about. An ACCELERATOR probe failure is returned alongside the
/// report instead of replacing it: the host report carries the PCI
/// evidence that says whether the failure is a broken driver on a real
/// card, and propagating the error here throws that evidence away before
/// anything can read it. That is what made the usual broken-driver case —
/// an installed `nvidia-smi` exiting non-zero — surface as an untyped
/// detection failure.
fn observe_platform_once(log: &mut impl Write) -> Result<Observation, ObservationFailure> {
    let probe = SystemHostProbe::new();
    let (_sources, mut report) = observe_identity(
        || probe.sources(),
        |sources| identify_and_record(&probe, sources, log),
    )?;
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

/// The head of one observation — gather the host sources, then identify
/// the platform from them — with each step's failure tagged by the step
/// that raised it.
///
/// This exists as its own function because the tagging IS the gate. The
/// two steps raise the same error variant (see [`ObservationFailure`]) and
/// only the identify step's is retryable, so a tag assigned on the wrong
/// side either restores the defect or makes the tamper signal retryable.
/// Inline in `observe_platform_once` that decision is reachable only from
/// a Compute Engine host; here it is reachable from a test with scripted
/// steps. Taking the steps as closures also makes the order structural:
/// `identify` cannot run, and cannot be handed sources it did not get,
/// unless `sources` succeeded first.
fn observe_identity<S, R>(
    sources: impl FnOnce() -> Result<S, PlatformProbeError>,
    identify: impl FnOnce(&S) -> Result<R, PlatformProbeError>,
) -> Result<(S, R), ObservationFailure> {
    let sources = sources().map_err(ObservationFailure::Sources)?;
    let identified = identify(&sources).map_err(ObservationFailure::Identify)?;
    Ok((sources, identified))
}

/// The production wiring of the retry: the real clock, a real sleep, and
/// the shipped budget, saying everything it does on stderr.
fn observe_platform() -> Result<Observation, PlatformProbeError> {
    let mut log = std::io::stderr();
    observe_with_shipped_policy(
        || observe_platform_once(&mut std::io::stderr()),
        std::thread::sleep,
        Instant::now,
        &mut log,
    )
}

/// The retry driven with the SHIPPED attempt count and budget.
///
/// Split from `observe_platform` so a test can drive the numbers this
/// binary actually ships rather than numbers a test chose. Everything left
/// in `observe_platform` is the real clock, the real sleep and stderr; the
/// policy a start runs under is here, where `DETECTION_RETRY_ATTEMPTS`
/// reading 6 and the loop being GIVEN 6 are the same fact.
fn observe_with_shipped_policy<T>(
    attempt: impl FnMut() -> Result<T, ObservationFailure>,
    sleep: impl FnMut(Duration),
    now: impl FnMut() -> Instant,
    log: &mut impl Write,
) -> Result<T, PlatformProbeError> {
    observe_with_retry(
        attempt,
        sleep,
        now,
        DETECTION_RETRY_ATTEMPTS,
        DETECTION_RETRY_BUDGET,
        log,
    )
}

/// Retry `attempt` while it fails retryably, bounded by both `attempts`
/// and `budget`, and say on `log` what it did.
///
/// No I/O, no real clock and no real sleep of its own: `sleep` and `now`
/// are supplied, so the whole policy is exercisable without waiting and
/// without a host. `log` carries only the retry lines; an attempt's own
/// output goes wherever that attempt was given to write it.
///
/// A retry costs one more pass over the host sources — file reads, one
/// `uname -m` fork, a PCI directory walk and the bounded metadata attempt.
/// It does not re-run `nvidia-smi` or the package database, which the
/// caller reaches only after an attempt has already succeeded, and it does
/// not re-emit the `platform identity:` line, which an attempt writes only
/// once it has an identity to report.
///
/// Four lines are written here, and only when the retry does something:
/// `platform detection retry:` per failed attempt, then exactly one of
/// `platform detection recovered:`, `platform detection exhausted:` or
/// `platform detection stopped:` to close the episode. Every episode that
/// wrote a retry line closes with one of those three, so an operator can
/// classify any journal that shows the retry doing anything. With the
/// existing `platform detection failed:` that is five prefixes sharing
/// eighteen characters, so a parser must match the whole prefix including
/// its colon — `platform detection` alone matches all five. The free-text
/// error is last on every line that carries one, so a trailing `(.+)`
/// capture works, as it does for the `platform identity:` line the
/// offline validation stage parses.
fn observe_with_retry<T>(
    mut attempt: impl FnMut() -> Result<T, ObservationFailure>,
    mut sleep: impl FnMut(Duration),
    mut now: impl FnMut() -> Instant,
    attempts: u32,
    budget: Duration,
    log: &mut impl Write,
) -> Result<T, PlatformProbeError> {
    let start = now();
    // The FIRST error, not the last: a late success must not make the
    // failure that preceded it invisible, and one line has to tell the
    // whole episode.
    let mut first_error: Option<String> = None;
    let mut made: u32 = 0;
    loop {
        made += 1;
        let failure = match attempt() {
            Ok(observed) => {
                if let Some(first) = first_error.as_deref() {
                    let _ = writeln!(
                        log,
                        "platform detection recovered: attempt={made} failed_attempts={} \
                         elapsed={} first_error={first}",
                        made - 1,
                        detection_seconds(now().saturating_duration_since(start)),
                    );
                }
                return Ok(observed);
            }
            Err(failure) => failure,
        };
        let elapsed = now().saturating_duration_since(start);
        if !is_retryable(&failure) {
            // Nothing is said when this is the first attempt, which is the
            // path every non-Compute-Engine host takes: its journal stays
            // byte-identical to before the retry existed. Said out loud
            // once retry lines have been written, because those lines
            // promised more attempts and the episode is ending early with
            // its window unspent. Without this, a metadata service that
            // comes up answering badly mid-window produces a journal that
            // matches neither of the other terminal shapes.
            if made > 1 {
                let _ = writeln!(
                    log,
                    "platform detection stopped: attempts={made} elapsed={} budget={} error={}",
                    detection_seconds(elapsed),
                    detection_seconds(budget),
                    failure.error(),
                );
            }
            return Err(failure.into_error());
        }
        // Bounded by the count AND the deadline. The deadline SHORTENS the
        // next sleep to whatever is left rather than abandoning the
        // schedule, which is what keeps the policy monotonic in attempt
        // cost: clamped, a host whose attempts are slow runs at least as
        // long a window and makes at least as many attempts as one whose
        // attempts are fast. Abandoning instead compares a full doubled
        // delay against the remaining budget, so a slow host crosses the
        // threshold a whole delay early and gives up SOONER in absolute
        // terms — backwards, because attempts are slowest exactly when the
        // network stack is the thing that is struggling.
        let next = retry_delay(made + 1, attempts)
            .map(|delay| delay.min(budget.saturating_sub(elapsed)))
            .filter(|delay| !delay.is_zero());
        let Some(delay) = next else {
            let _ = writeln!(
                log,
                "platform detection exhausted: attempts={made} elapsed={} budget={}",
                detection_seconds(elapsed),
                detection_seconds(budget),
            );
            return Err(failure.into_error());
        };
        let _ = writeln!(
            log,
            "platform detection retry: attempt={made}/{attempts} elapsed={} next_in={} error={}",
            detection_seconds(elapsed),
            detection_seconds(delay),
            failure.error(),
        );
        if first_error.is_none() {
            first_error = Some(failure.error().to_string());
        }
        sleep(delay);
    }
}

/// The delay before attempt `next_attempt`, or `None` when the schedule is
/// over because `next_attempt` is past `attempts`.
///
/// Attempt 1 is immediate, so it never has a delay. Every later attempt
/// doubles from [`DETECTION_RETRY_FIRST_DELAY`]: with six attempts that is
/// 0.5s, 1s, 2s, 4s and 8s, putting four of the six attempts inside the
/// first four seconds, where a link that is merely late is most likely to
/// arrive, without a long tail of connects afterwards.
///
/// Explicit sleeps rather than a bare count, because the metadata query's
/// own timeout does not rate-limit the failure path: a host with no route
/// yet fails the connect immediately rather than consuming the budget, so
/// a sleepless loop would spend every attempt in milliseconds. The sleeps
/// are also what bounds this to at most `attempts` connects per process
/// start.
fn retry_delay(next_attempt: u32, attempts: u32) -> Option<Duration> {
    if next_attempt < 2 || next_attempt > attempts {
        return None;
    }
    // Saturating rather than panicking on an implausible attempt count;
    // the budget bounds the loop whatever this returns.
    let factor = 2_u32.checked_pow(next_attempt - 2).unwrap_or(u32::MAX);
    Some(DETECTION_RETRY_FIRST_DELAY.saturating_mul(factor))
}

/// Whether this failure is the one a later attempt could plausibly settle.
///
/// See [`ObservationFailure`]: only the identify step's
/// `IdentityUnestablished` is network-shaped. Every other failure returns
/// on the first attempt, which is what keeps every non-Compute-Engine host
/// byte-identical to before this retry existed.
fn is_retryable(failure: &ObservationFailure) -> bool {
    matches!(
        failure,
        ObservationFailure::Identify(PlatformProbeError::IdentityUnestablished { .. })
    )
}

/// Seconds to one decimal: the duration form the three detection lines
/// use, so a parser reads one shape for every elapsed figure.
fn detection_seconds(duration: Duration) -> String {
    format!("{:.1}s", duration.as_secs_f64())
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
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::{
        admit_observed_platform, detection_failed, identify_and_record, is_retryable,
        observe_identity, observe_with_retry, observe_with_shipped_policy, parse_dpkg_packages,
        parse_homebrew_packages, platform_identity_line, retry_delay, Observation,
        ObservationFailure, DETECTION_RETRY_ATTEMPTS, DETECTION_RETRY_BUDGET,
    };
    use tensorplate_agent::config::AgentConfig;
    use tensorplate_agent::error::AgentError;
    use tensorplate_agent::platform_admission::{ObservedStack, PlatformAdmission};
    use tensorplate_platform::{
        identify_accelerator, identify_platform, AcceleratorSources, HostSources,
        MachineTypeSource, PlatformProbeError, PlatformRegistry, RecordWrite, SystemHostProbe,
    };
    use tensorplate_protocol::install_paths::MACHINE_TYPE_RECORD_PATH;

    /// The L4 row, the Production cloud exemplar every recovery case here
    /// settles on.
    const L4_ROW: &str = "ubuntu2404-x86-l4-g2s8";

    fn repo_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(relative)
    }

    /// One committed host-identity fixture as the sources a start gathers.
    ///
    /// No committed fixture carries `dmi_product_name`, so a case that
    /// needs one sets it itself — which is also the only way to stage a
    /// host whose firmware answers with something that is not Compute
    /// Engine.
    fn host_sources(fixture: &str) -> HostSources {
        let body = std::fs::read_to_string(
            repo_path("test/platform/host_identity").join(format!("{fixture}.json")),
        )
        .expect("the recorded fixture is committed");
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
            dmi_product_name: text("dmi_product_name"),
            gce_machine_type: text("gce_machine_type"),
            machine_type_record: None,
            gce_instance_id: None,
            instance_binding: None,
            // Synthetic boot identity supplements the recorded hardware facts.
            boot_id: Some("12345678-1234-4234-8234-123456789abc".to_string()),
            proc_meminfo: text("proc_meminfo"),
            pci_devices: text("pci_devices"),
        }
    }

    /// The recorded g2-standard-8 L4 host, as a start with the metadata
    /// service reachable gathers it.
    fn l4_live_sources() -> HostSources {
        HostSources {
            dmi_product_name: Some("Google Compute Engine\n".to_string()),
            ..host_sources(L4_ROW)
        }
    }

    #[test]
    fn a_start_with_a_live_answer_records_it_and_an_offline_start_uses_it() {
        let temporary = std::env::temp_dir().canonicalize().expect("temporary root");
        let root = tempfile::tempdir_in(temporary).expect("tempdir");
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

    // ---- detection retry ------------------------------------------------
    //
    // The defect: platform detection ran once per start, so a Compute
    // Engine instance whose metadata service was not up yet held a
    // detection failure for the whole boot and refused every deploy until
    // somebody restarted the agent by hand. What has to be shown is not
    // that a retry loop exists but that the verdict a deploy is gated on
    // changes -- and that it still does NOT change for the failures that
    // are not network-shaped.

    /// The retryable failure: a Compute Engine instance whose metadata
    /// service has not answered yet and which has no record for this boot.
    /// `attempt` is carried in the detail so a case can tell the first
    /// error from the last.
    fn metadata_unreached(attempt: u32) -> PlatformProbeError {
        PlatformProbeError::IdentityUnestablished {
            source_name: MACHINE_TYPE_RECORD_PATH.to_string(),
            detail: format!("no machine type has been recorded on this host (attempt {attempt})"),
        }
    }

    /// A clock that moves only when the code under test sleeps, or when an
    /// attempt says how long it took. Nothing here waits.
    struct FakeClock {
        base: Instant,
        elapsed: Cell<Duration>,
        slept: RefCell<Vec<Duration>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                elapsed: Cell::new(Duration::ZERO),
                slept: RefCell::new(Vec::new()),
            }
        }

        fn now(&self) -> Instant {
            self.base + self.elapsed.get()
        }

        fn advance(&self, by: Duration) {
            self.elapsed.set(self.elapsed.get() + by);
        }

        fn sleep(&self, delay: Duration) {
            self.slept.borrow_mut().push(delay);
            self.advance(delay);
        }

        fn slept(&self) -> Vec<Duration> {
            self.slept.borrow().clone()
        }
    }

    /// Run the retry against an attempt that fails `failures` times with
    /// the retryable failure and then succeeds with `success`, charging
    /// `work` to the clock for every attempt.
    ///
    /// Returns what the loop returned and what it said, so a case asserts
    /// on the outcome and the journal rather than on the loop's internals.
    fn drive_retry<T>(
        clock: &FakeClock,
        failures: u32,
        attempts: u32,
        work: Duration,
        mut success: impl FnMut() -> T,
    ) -> (Result<T, PlatformProbeError>, String) {
        let made = Cell::new(0_u32);
        let mut log = Vec::new();
        let outcome = observe_with_retry(
            || {
                made.set(made.get() + 1);
                clock.advance(work);
                if made.get() > failures {
                    Ok(success())
                } else {
                    Err(ObservationFailure::Identify(metadata_unreached(made.get())))
                }
            },
            |delay| clock.sleep(delay),
            || clock.now(),
            attempts,
            DETECTION_RETRY_BUDGET,
            &mut log,
        );
        (outcome, String::from_utf8(log).expect("utf-8"))
    }

    fn committed_registry() -> PlatformRegistry {
        PlatformRegistry::load(&repo_path("config/platform")).expect("the committed registry loads")
    }

    /// The config as shipped. It sets no `device_memory_bytes`, which is
    /// the case the memory-ceiling guard below is about.
    fn shipped_config() -> AgentConfig {
        let raw = std::fs::read_to_string(repo_path("packaging/conf/agent.json"))
            .expect("the packaged agent config is committed");
        AgentConfig::parse_json(&raw).expect("the packaged agent config parses")
    }

    /// What a successful observation of the L4 row carries: the recorded
    /// host sources identified, plus the recorded `nvidia-smi` answer for
    /// the same row as its accelerator.
    fn l4_observation() -> Observation {
        let mut report = identify_platform(&l4_live_sources()).expect("the L4 fixture identifies");
        let query = std::fs::read_to_string(
            repo_path("test/platform/accelerator").join(format!("{L4_ROW}.txt")),
        )
        .expect("the recorded L4 accelerator fixture is committed");
        let card = identify_accelerator(&AcceleratorSources {
            nvidia_smi_query: Some(query),
        })
        .expect("the accelerator fixture parses")
        .expect("the accelerator fixture reports one card");
        report.accelerator = Some(card.observation());
        (report, ObservedStack::default(), None)
    }

    /// The unreadable failure the metadata service raises while it is
    /// coming up: the connect succeeds and the peer closes or answers with
    /// something that is not a machine type. `machine_type_sources` turns
    /// that into this, inside `probe.sources()`, so it arrives tagged
    /// `Sources` and is not retryable.
    fn metadata_answered_badly() -> PlatformProbeError {
        PlatformProbeError::Unreadable {
            source_name: "GCE metadata service".to_string(),
            detail: "metadata service closed the connection without answering".to_string(),
        }
    }

    #[test]
    fn the_step_that_raised_a_failure_is_the_step_that_gets_tagged() {
        // The tag IS the gate: `Identify(IdentityUnestablished)` retries
        // and `Sources(IdentityUnestablished)` does not, so a tag assigned
        // on the wrong side either restores the defect or makes the tamper
        // signal retryable. In the production wiring that assignment is
        // reachable only from a Compute Engine host; `observe_identity` is
        // where a test can reach it.
        let failure = observe_identity(
            || Err::<u32, _>(metadata_unreached(1)),
            |_: &u32| -> Result<u32, PlatformProbeError> {
                panic!("identify must not run when the sources step failed")
            },
        )
        .expect_err("the sources step failed");
        assert!(
            matches!(failure, ObservationFailure::Sources(_)),
            "{failure:?}"
        );
        assert!(
            !is_retryable(&failure),
            "an unusable machine-type record is the only way the sources \
             step raises this variant. Retrying it would let a later live \
             answer overwrite the record and the tamper signal would never \
             be reported"
        );

        let failure = observe_identity(|| Ok(7_u32), |_| Err::<u32, _>(metadata_unreached(1)))
            .expect_err("the identify step failed");
        assert!(
            matches!(failure, ObservationFailure::Identify(_)),
            "{failure:?}"
        );
        assert!(
            is_retryable(&failure),
            "a Compute Engine instance whose metadata service has not \
             answered yet must retry, or #216 is not fixed at all"
        );

        // The control: the steps run in order and the identified value is
        // what comes back, so this cannot pass on a build where the two
        // steps were transposed.
        let (sources, identified) = observe_identity(|| Ok(7_u32), |sources: &u32| Ok(sources + 1))
            .expect("both steps ran");
        assert_eq!((sources, identified), (7, 8));
    }

    #[test]
    fn a_failed_identification_writes_no_identity_line() {
        // `identity_logged_once` is what makes the retry safe to put here:
        // the offline validation stage requires exactly one `platform
        // identity:` line per invocation, so an attempt that fails must
        // write none. `identify_platform` failing before the line is
        // written is the whole reason that holds, and only this pins it.
        let temporary = std::env::temp_dir().canonicalize().expect("temporary root");
        let root = tempfile::tempdir_in(temporary).expect("tempdir");
        std::fs::create_dir_all(
            root.path()
                .join(MACHINE_TYPE_RECORD_PATH.trim_start_matches('/'))
                .parent()
                .expect("parent"),
        )
        .expect("stage state/");
        let probe = SystemHostProbe::with_root(root.path());

        let unestablished = HostSources {
            gce_machine_type: None,
            machine_type_record: None,
            ..l4_live_sources()
        };
        let mut log = Vec::new();
        let err = identify_and_record(&probe, &unestablished, &mut log)
            .expect_err("a Compute Engine instance with no answer and no record cannot identify");
        assert!(
            matches!(err, PlatformProbeError::IdentityUnestablished { .. }),
            "{err:?}"
        );
        assert!(
            log.is_empty(),
            "a failed attempt must write no `platform identity:` line, or a \
             boot that recovers on attempt 3 emits three of them and the \
             offline stage's identity_logged_once check fails on a real \
             host: {}",
            String::from_utf8_lossy(&log)
        );

        // The control. Without it this would pass on a build that writes
        // no identity line at all.
        let mut log = Vec::new();
        identify_and_record(&probe, &l4_live_sources(), &mut log)
            .expect("the L4 fixture identifies");
        assert_eq!(
            String::from_utf8(log)
                .expect("utf-8")
                .lines()
                .filter(|line| line.starts_with("platform identity: "))
                .count(),
            1,
            "a successful attempt writes exactly one"
        );
    }

    #[test]
    fn only_the_identify_steps_identity_failure_is_retryable() {
        assert!(
            is_retryable(&ObservationFailure::Identify(metadata_unreached(1))),
            "a Compute Engine instance whose metadata service has not \
             answered yet is the whole point of the retry"
        );
        assert!(
            !is_retryable(&ObservationFailure::Sources(metadata_unreached(1))),
            "the same variant from the sources step is an unusable \
             machine-type record -- a tamper signal. Retrying it would let \
             a later live answer overwrite the record and the signal would \
             never be reported at all"
        );
        for other in [
            PlatformProbeError::Unreadable {
                source_name: "GCE metadata service".to_string(),
                detail: "something answered and it was not the metadata service".to_string(),
            },
            PlatformProbeError::Unrecognized {
                source_name: "GCE metadata service".to_string(),
                detail: "not a machine-type resource name".to_string(),
            },
        ] {
            let rendered = other.to_string();
            assert!(
                !is_retryable(&ObservationFailure::Identify(other)),
                "{rendered} is not network-shaped and must settle on the first attempt"
            );
        }
    }

    #[test]
    fn the_retry_schedule_doubles_from_half_a_second_and_then_ends() {
        assert_eq!(retry_delay(1, 6), None, "the first attempt is immediate");
        assert_eq!(retry_delay(2, 6), Some(Duration::from_millis(500)));
        assert_eq!(retry_delay(3, 6), Some(Duration::from_secs(1)));
        assert_eq!(retry_delay(4, 6), Some(Duration::from_secs(2)));
        assert_eq!(retry_delay(5, 6), Some(Duration::from_secs(4)));
        assert_eq!(retry_delay(6, 6), Some(Duration::from_secs(8)));
        assert_eq!(retry_delay(7, 6), None, "the schedule ends with the count");
        assert_eq!(
            retry_delay(2, 1),
            None,
            "one attempt is the no-retry configuration"
        );
        let total: Duration = (2..=DETECTION_RETRY_ATTEMPTS)
            .filter_map(|attempt| retry_delay(attempt, DETECTION_RETRY_ATTEMPTS))
            .sum();
        assert_eq!(total, Duration::from_millis(15_500));
        assert!(
            total < DETECTION_RETRY_BUDGET,
            "the count bounds a healthy-clock episode before the budget does"
        );
    }

    #[test]
    fn a_recovered_observation_reports_the_first_error_not_the_last() {
        let clock = FakeClock::new();
        let (outcome, log) =
            drive_retry(&clock, 2, DETECTION_RETRY_ATTEMPTS, Duration::ZERO, || ());
        assert!(outcome.is_ok(), "the third attempt answered");
        assert_eq!(
            clock.slept(),
            vec![Duration::from_millis(500), Duration::from_secs(1)]
        );
        assert_eq!(
            log,
            format!(
                "platform detection retry: attempt=1/6 elapsed=0.0s next_in=0.5s error={}\n\
                 platform detection retry: attempt=2/6 elapsed=0.5s next_in=1.0s error={}\n\
                 platform detection recovered: attempt=3 failed_attempts=2 elapsed=1.5s \
                 first_error={}\n",
                metadata_unreached(1),
                metadata_unreached(2),
                metadata_unreached(1),
            ),
            "a late success must not make the earlier failure invisible, so \
             the recovered line carries the FIRST error"
        );
    }

    #[test]
    fn a_non_retryable_failure_returns_on_the_first_attempt_saying_nothing_new() {
        let clock = FakeClock::new();
        let mut log = Vec::new();
        let made = Cell::new(0_u32);
        let outcome: Result<(), PlatformProbeError> = observe_with_retry(
            || {
                made.set(made.get() + 1);
                Err(ObservationFailure::Sources(metadata_unreached(made.get())))
            },
            |delay| clock.sleep(delay),
            || clock.now(),
            DETECTION_RETRY_ATTEMPTS,
            DETECTION_RETRY_BUDGET,
            &mut log,
        );
        assert!(outcome.is_err());
        assert_eq!(made.get(), 1, "a tamper signal is not retried");
        assert!(clock.slept().is_empty(), "and costs no delay");
        assert!(
            log.is_empty(),
            "every non-Compute-Engine host takes this path, so its journal \
             must be byte-identical to before the retry existed"
        );
    }

    #[test]
    fn exhaustion_says_how_many_attempts_it_made_and_what_bounded_them() {
        let clock = FakeClock::new();
        let (outcome, log) = drive_retry(
            &clock,
            u32::MAX,
            DETECTION_RETRY_ATTEMPTS,
            Duration::ZERO,
            || (),
        );
        assert!(outcome.is_err());
        assert_eq!(
            clock.slept(),
            vec![
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
            ]
        );
        assert_eq!(
            log.lines()
                .filter(|l| l.starts_with("platform detection retry: "))
                .count(),
            5
        );
        assert!(
            log.ends_with("platform detection exhausted: attempts=6 elapsed=15.5s budget=20.0s\n"),
            "{log}"
        );
    }

    #[test]
    fn a_non_retryable_failure_mid_window_closes_the_episode_out_loud() {
        // The reachable case this is about: attempt 1 cannot reach the
        // metadata service at all, so it retries; by attempt 2 the route
        // is up but the endpoint is not serving yet, so `query_metadata`
        // reports `Answered` and `machine_type_sources` turns it into
        // `Unreadable` inside `probe.sources()` — tagged `Sources`, and
        // deliberately not retryable. The episode ends at attempt 2 of 6
        // with most of its window unspent, and it has to say so: a retry
        // line has already promised more attempts.
        let clock = FakeClock::new();
        let mut log = Vec::new();
        let made = Cell::new(0_u32);
        let outcome: Result<(), PlatformProbeError> = observe_with_retry(
            || {
                made.set(made.get() + 1);
                if made.get() == 1 {
                    Err(ObservationFailure::Identify(metadata_unreached(1)))
                } else {
                    Err(ObservationFailure::Sources(metadata_answered_badly()))
                }
            },
            |delay| clock.sleep(delay),
            || clock.now(),
            DETECTION_RETRY_ATTEMPTS,
            DETECTION_RETRY_BUDGET,
            &mut log,
        );
        assert!(outcome.is_err());
        assert_eq!(made.get(), 2);
        assert_eq!(clock.slept(), vec![Duration::from_millis(500)]);
        assert_eq!(
            String::from_utf8(log).expect("utf-8"),
            format!(
                "platform detection retry: attempt=1/6 elapsed=0.0s next_in=0.5s error={}\n\
                 platform detection stopped: attempts=2 elapsed=0.5s budget=20.0s error={}\n",
                metadata_unreached(1),
                metadata_answered_badly(),
            ),
            "an episode that wrote a retry line must close with one of the \
             three terminal lines, or the journal matches no documented \
             shape and the window it gave up is invisible to the fleet \
             evidence the budget is supposed to be corrected from"
        );
    }

    #[test]
    fn a_slower_host_does_not_get_fewer_retries_over_a_shorter_window() {
        // The deadline shortens the next sleep to what is left rather than
        // abandoning the schedule. Without that, the budget is checked
        // against a full doubled delay, so a host whose attempts are slow
        // crosses the threshold a whole delay early and gives up SOONER in
        // absolute terms than a fast one -- backwards, because attempts are
        // slowest exactly when the network stack is what is struggling.
        let mut episodes = Vec::new();
        for work in [300, 700, 1000, 1500] {
            let clock = FakeClock::new();
            let (outcome, log) = drive_retry(
                &clock,
                u32::MAX,
                DETECTION_RETRY_ATTEMPTS,
                Duration::from_millis(work),
                || (),
            );
            assert!(outcome.is_err());
            let exhausted = log
                .lines()
                .last()
                .expect("an exhausted episode closes out loud")
                .to_string();
            let attempts: u32 = exhausted
                .split_once("attempts=")
                .and_then(|(_, rest)| rest.split_once(' '))
                .expect("the exhausted line names the attempt count")
                .0
                .parse()
                .expect("a number");
            episodes.push((
                work,
                attempts,
                clock.now().saturating_duration_since(clock.base),
            ));
        }
        for window in episodes.windows(2) {
            let (slow_work, slow_attempts, slow_elapsed) = window[1];
            let (fast_work, fast_attempts, fast_elapsed) = window[0];
            assert!(
                slow_attempts >= fast_attempts,
                "{slow_work}ms attempts made {slow_attempts} attempts where \
                 {fast_work}ms made {fast_attempts}: a slower host must not \
                 be given fewer tries"
            );
            assert!(
                slow_elapsed >= fast_elapsed,
                "{slow_work}ms gave up after {slow_elapsed:?} where \
                 {fast_work}ms ran for {fast_elapsed:?}: a slower host must \
                 not abandon its window earlier"
            );
        }
        // Concretely: every one of these spends its whole schedule.
        assert_eq!(
            episodes
                .iter()
                .map(|(_, attempts, _)| *attempts)
                .collect::<Vec<_>>(),
            vec![6, 6, 6, 6]
        );
    }

    #[test]
    fn the_shipped_policy_makes_six_attempts_over_the_shipped_budget() {
        // `observe_platform` is a one-line delegation to this; the numbers
        // a start actually runs under are passed here. Without this the
        // constant can read 6 while the call site passes something else --
        // the defect restored, with fmt, clippy and every other test green.
        let clock = FakeClock::new();
        let mut log = Vec::new();
        let made = Cell::new(0_u32);
        let outcome: Result<(), PlatformProbeError> = observe_with_shipped_policy(
            || {
                made.set(made.get() + 1);
                Err(ObservationFailure::Identify(metadata_unreached(made.get())))
            },
            |delay| clock.sleep(delay),
            || clock.now(),
            &mut log,
        );
        assert!(outcome.is_err());
        assert_eq!(made.get(), DETECTION_RETRY_ATTEMPTS, "six attempts ship");
        assert_eq!(
            clock.slept(),
            vec![
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
            ]
        );
        let log = String::from_utf8(log).expect("utf-8");
        assert!(
            log.ends_with("platform detection exhausted: attempts=6 elapsed=15.5s budget=20.0s\n"),
            "{log}"
        );

        // And the half that matters: the shipped policy recovers from the
        // failure #216 is about rather than settling it on attempt 1.
        let clock = FakeClock::new();
        let mut log = Vec::new();
        let made = Cell::new(0_u32);
        let outcome = observe_with_shipped_policy(
            || {
                made.set(made.get() + 1);
                if made.get() > 2 {
                    Ok(())
                } else {
                    Err(ObservationFailure::Identify(metadata_unreached(made.get())))
                }
            },
            |delay| clock.sleep(delay),
            || clock.now(),
            &mut log,
        );
        assert!(outcome.is_ok(), "the shipped policy retries");
        assert!(
            String::from_utf8(log)
                .expect("utf-8")
                .contains("platform detection recovered: attempt=3 "),
            "the shipped call site must pass more than one attempt"
        );
    }

    #[test]
    fn the_budget_stops_the_loop_before_the_attempt_count_does() {
        // Attempts that each take three seconds: the count would allow six,
        // the deadline does not. Both bounds are reported, which is what
        // makes the journal say which one bit.
        //
        // Note the reported elapsed: 22.5s against a 20.0s budget. The
        // budget bounds the SLEEP SCHEDULE, not total elapsed — the attempt
        // that runs after the last permitted sleep is not itself timed, so
        // an episode ends at up to the budget plus one attempt's work. That
        // is the documented contract, and this is the case that shows it.
        let clock = FakeClock::new();
        let (outcome, log) = drive_retry(
            &clock,
            u32::MAX,
            DETECTION_RETRY_ATTEMPTS,
            Duration::from_secs(3),
            || (),
        );
        assert!(outcome.is_err());
        assert_eq!(clock.slept().len(), 4);
        assert!(
            log.ends_with("platform detection exhausted: attempts=5 elapsed=22.5s budget=20.0s\n"),
            "{log}"
        );
    }

    #[test]
    fn a_metadata_service_that_answers_on_the_third_attempt_admits_deploys() {
        // The assertion that matters is on the ADMISSION, not on the loop:
        // `ensure_supported` is the exact call the coordinator makes before
        // a deploy, and it is the call that refuses today.
        let registry = committed_registry();
        let mut config = shipped_config();
        let clock = FakeClock::new();
        let (observation, _log) = drive_retry(
            &clock,
            2,
            DETECTION_RETRY_ATTEMPTS,
            Duration::ZERO,
            l4_observation,
        );
        let admission = admit_observed_platform(&registry, &mut config, observation);
        assert_eq!(admission.row_id(), Some(L4_ROW));
        assert!(
            matches!(admission, PlatformAdmission::Supported { .. }),
            "{admission:?}"
        );
        admission
            .ensure_supported()
            .expect("a host whose metadata service came up late must admit deploys");
    }

    #[test]
    fn with_a_single_attempt_the_same_scenario_is_refused() {
        // The mutation control, and the half that makes the case above mean
        // anything: with the retry reduced to one attempt, the identical
        // scenario is the defect -- rejected, and refused at the same gate.
        // Without this, the case above would pass on a build where the
        // retry did nothing at all.
        let registry = committed_registry();
        let mut config = shipped_config();
        let clock = FakeClock::new();
        let (observation, log) = drive_retry(&clock, 2, 1, Duration::ZERO, l4_observation);
        assert!(clock.slept().is_empty(), "one attempt sleeps not at all");
        assert!(
            log.ends_with("platform detection exhausted: attempts=1 elapsed=0.0s budget=20.0s\n"),
            "{log}"
        );
        let admission = admit_observed_platform(&registry, &mut config, observation);
        assert!(
            matches!(admission, PlatformAdmission::Rejected { .. }),
            "{admission:?}"
        );
        match admission.ensure_supported() {
            Err(AgentError::PlatformNotAdmissible { detail, .. }) => {
                assert!(
                    detail.contains("no machine type has been recorded"),
                    "{detail}"
                );
            }
            other => panic!("one attempt must reproduce the defect, got {other:?}"),
        }
    }

    #[test]
    fn exhausted_detection_is_still_a_rejection_that_carries_no_frozen_reason() {
        // Exhaustion must land exactly where a single failed detection
        // lands today: a rejection with no row and no typed reason. "I
        // could not look" must not become "your platform is unsupported",
        // and it must not become "deploy anything" either.
        let registry = committed_registry();
        let mut config = shipped_config();
        let clock = FakeClock::new();
        let (observation, _log) = drive_retry(
            &clock,
            u32::MAX,
            DETECTION_RETRY_ATTEMPTS,
            Duration::ZERO,
            l4_observation,
        );
        let admission = admit_observed_platform(&registry, &mut config, observation);
        match &admission {
            PlatformAdmission::Rejected {
                row_id: None,
                reason: None,
                detail,
            } => assert!(
                detail.starts_with("platform detection failed: "),
                "{detail}"
            ),
            other => panic!("exhaustion must stay today's terminal state, got {other:?}"),
        }
        assert!(admission.ensure_supported().is_err());
    }

    #[test]
    fn a_detection_failure_leaves_the_configured_memory_ceiling_alone() {
        // The trap a careless version of this fix falls into.
        // `apply_memory_limit` early-returns on anything that is not
        // `Supported { capability: Some(_) }`, the shipped config sets no
        // `device_memory_bytes`, and both memory gates are Option-gated. A
        // verdict that becomes supported without the ceiling being applied
        // therefore admits deploys with both memory checks disabled.
        let registry = committed_registry();
        let mut config = shipped_config();
        assert_eq!(
            config.device_memory_bytes, None,
            "the shipped config sets no ceiling of its own"
        );
        let clock = FakeClock::new();
        let (observation, _log) = drive_retry(
            &clock,
            u32::MAX,
            DETECTION_RETRY_ATTEMPTS,
            Duration::ZERO,
            l4_observation,
        );
        admit_observed_platform(&registry, &mut config, observation);
        assert_eq!(
            config.device_memory_bytes, None,
            "a detection failure applies no ceiling, so nothing may be \
             admitted on the strength of one"
        );
    }

    #[test]
    fn a_recovered_detection_applies_the_row_ceiling_once() {
        let registry = committed_registry();
        let clock = FakeClock::new();
        let (observation, _log) = drive_retry(
            &clock,
            2,
            DETECTION_RETRY_ATTEMPTS,
            Duration::ZERO,
            l4_observation,
        );
        let mut config = shipped_config();
        let admission = admit_observed_platform(&registry, &mut config, observation);
        let ceiling = admission
            .capability()
            .expect("a supported L4 carries a bounded capability")
            .max_resident_model_memory();
        assert_eq!(
            config.device_memory_bytes,
            Some(ceiling),
            "the row ceiling is applied on the recovery path, not skipped"
        );

        // A settled verdict applies the ceiling exactly once, so the value
        // the retry path produces is the value one application produces --
        // an operator floor stays in force and a larger one is reduced.
        for (configured, expected) in [(ceiling + 1, ceiling), (4096, 4096)] {
            let clock = FakeClock::new();
            let (observation, _log) = drive_retry(
                &clock,
                2,
                DETECTION_RETRY_ATTEMPTS,
                Duration::ZERO,
                l4_observation,
            );
            let mut config = shipped_config();
            config.device_memory_bytes = Some(configured);
            admit_observed_platform(&registry, &mut config, observation);
            assert_eq!(config.device_memory_bytes, Some(expected));
        }
    }

    #[test]
    fn no_non_compute_engine_host_tree_produces_the_retryable_failure() {
        // The gate rests entirely on `Identify(IdentityUnestablished)`
        // being unreachable off Compute Engine. That is true by
        // construction and nothing else pins it, so a change that widened
        // it would make every host here pay the retry budget.
        let trees = [
            ("jetson-orin-nano-8gb-jp62", None),
            ("macos26-m1pro-16gb", None),
            // Real board product names, injected here because no committed
            // host fixture carries `dmi_product_name` at all.
            (
                "ubuntu2404-x86-rtxpro6000we-physical",
                Some("Precision 7960 Tower\n"),
            ),
            ("ubuntu2404-x86-cpu", Some("PowerEdge R760xa\n")),
        ];
        for (fixture, dmi_product_name) in trees {
            let sources = HostSources {
                dmi_product_name: dmi_product_name.map(str::to_string),
                ..host_sources(fixture)
            };
            assert!(
                !matches!(
                    identify_platform(&sources),
                    Err(PlatformProbeError::IdentityUnestablished { .. })
                ),
                "`{fixture}` is not a Compute Engine instance and must never \
                 reach the retryable failure"
            );
        }

        // The control. Without it this would pass on a build where nothing
        // is ever unestablished.
        let gce_without_an_answer = HostSources {
            gce_machine_type: None,
            machine_type_record: None,
            ..l4_live_sources()
        };
        assert!(
            matches!(
                identify_platform(&gce_without_an_answer),
                Err(PlatformProbeError::IdentityUnestablished { .. })
            ),
            "a Compute Engine instance with no live answer and no record is \
             exactly the failure the retry exists for"
        );
    }
}
