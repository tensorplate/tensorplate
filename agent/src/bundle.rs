// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F03 + bundle format: agent deploy-time bundle verifier.
//
// The agent calls into the shared `tensorplate_protocol::bundle` parser
// and compatibility evaluator. The old in-crate verifier has been
// migrated so there is exactly one validation path; per bundle format, the
// agent must not run a parallel set of integrity / compat checks.
//
// The verifier checks (via the shared parser):
//
//   - manifest.json exists, decodes against the v0.1 schema, and
//     validates semantically.
//   - Each artifact file's sha256 matches its declared digest.
//   - manifest_digest (when present) matches the canonical manifest with
//     that field stripped.
//   - format_version is exactly one of the supported formats, 0.1 or 0.2.
//
// Then the agent calls into the shared `evaluate_compatibility` to
// check:
//
//   - runtime_compatibility includes the configured runtime version.
//   - target_hardware.device_family matches (or is `any`).
//   - target_hardware.min_memory_bytes / memory_estimate_bytes do not
//     exceed the configured device_memory_bytes.
//   - backend_hint is in `available_backends`.
//   - capability_requirements are satisfied by the configured backend
//     capability map.
//   - precision profile (when set) is in the backend's
//     supported_precision list.
//
// No backend is selected heuristically; bundles are rejected with typed
// errors when their declared backend is unknown or unavailable.
//
// A format 0.2 manifest is also judged against what the agent knows and
// the manifest cannot: the bundles it has deployed, the platform rows it
// loaded and the runner profiles it probed (`DeployFacts`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tensorplate_protocol::agent_state::AgentState;
use tensorplate_protocol::bundle::{
    evaluate_compatibility, parse_bundle, parse_bundle_with, BackendCapabilityView, BackendProfile,
    BundleDescriptor, CompatibilityViolation, DeviceContext, ParseError, ParseOptions,
    MANIFEST_FILENAME,
};
use tensorplate_protocol::bundle_manifest::{BundleManifest, BundleManifestError};
use tensorplate_protocol::bundle_profile::{
    BundleProfile, BundleProfileError, BundleRuleCode, KnownBase, SupportLevel,
    PROFILE_FORMAT_VERSION,
};
use tensorplate_protocol::resident_set::MemberState;

use tensorplate_platform::{PlatformReason, PlatformRegistry};

use crate::backend_detection::{
    BackendProbeReport, BackendProbeState, ServingState, BACKENDS_WITHOUT_DESCRIPTOR,
};
use crate::config::{AgentConfig, BackendCapability};
use crate::error::{AgentError, AgentResult};
use crate::platform_admission::PlatformAdmission;

/// What the agent knows about its target when it judges a deploy. An
/// absent fact is one this agent does not hold, and the rule that needs it
/// is judged without it: no known base, or no row to compare.
#[derive(Clone, Copy, Debug)]
pub struct DeployFacts<'a> {
    /// Probe reports for the backends that have a descriptor.
    pub probes: &'a BTreeMap<String, BackendProbeReport>,
    pub registry: Option<&'a PlatformRegistry>,
    pub admission: Option<&'a PlatformAdmission>,
    /// The durable state, for the bundles recorded as deployed.
    pub state: Option<&'a AgentState>,
}

/// Verified bundle: a manifest, the bundle's content-addressed digest, and
/// the absolute root path it was verified at.
#[derive(Debug, Clone)]
pub struct VerifiedBundle {
    pub manifest: BundleManifest,
    /// `sha256:<hex>` digest of the canonical manifest (with
    /// `manifest_digest` stripped). Used by the agent as the persisted
    /// `bundle_digest` field.
    pub manifest_digest: String,
    pub root_path: PathBuf,
}

impl VerifiedBundle {
    /// Stable bundle identifier suitable for logs and status: `<name>@<version>`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}@{}", self.manifest.name, self.manifest.version)
    }

    /// Absolute path of the `model` artifact. Returns `None` only when
    /// called on an unvalidated manifest; verified bundles always have a
    /// model artifact.
    #[must_use]
    pub fn model_artifact_path(&self) -> Option<PathBuf> {
        self.manifest
            .model_artifact()
            .map(|a| self.root_path.join(&a.path))
    }
}

impl From<BundleDescriptor> for VerifiedBundle {
    fn from(d: BundleDescriptor) -> Self {
        Self {
            manifest: d.manifest,
            manifest_digest: d.manifest_digest,
            root_path: d.root_path,
        }
    }
}

/// Read the staged bundle manifest and return its model artifact path
/// relative to the bundle root.
///
/// Rollback and startup recovery rebuild worker candidates from durable
/// deployment records. Those records store the staged bundle root, while
/// real workers need the model artifact inside that root.
///
/// # Errors
///
/// Returns [`AgentError::BundleManifest`] when the manifest is missing,
/// malformed, or does not validate.
pub(crate) fn model_artifact_relative_path(bundle_path: &Path) -> AgentResult<String> {
    let descriptor = parse_bundle(bundle_path).map_err(parse_error_to_agent_error)?;
    descriptor
        .model_artifact_relative_path()
        .ok_or_else(|| AgentError::BundleManifest("manifest missing model artifact".into()))
}

/// The runner profile the staged bundle at `bundle_path` selects, if its
/// manifest names one. Reads the manifest only: the artifacts were verified
/// when the bundle was staged.
///
/// # Errors
///
/// Returns [`AgentError::BundleMissing`] when the staged directory is gone,
/// and [`AgentError::BundleManifest`] when the manifest is missing,
/// malformed, or does not validate.
pub(crate) fn staged_runner_profile(bundle_path: &Path) -> AgentResult<Option<String>> {
    let options = ParseOptions {
        verify_artifact_digests: false,
    };
    let descriptor = parse_bundle_with(bundle_path, options).map_err(parse_error_to_agent_error)?;
    Ok(descriptor.manifest.profile.and_then(|p| p.runner_profile))
}

/// Verify the bundle at `bundle_path` against the agent config.
///
/// bundle format migrates the implementation to the shared
/// `tensorplate_protocol::bundle::parse_bundle` + `evaluate_compatibility`
/// pair. The returned `VerifiedBundle` shape is preserved so the
/// coordinator and rollback paths continue to work unchanged.
///
/// # Errors
///
/// Returns typed [`AgentError`] variants for every failure class:
/// `BundleMissing`, `BundleManifest`, `BundleIntegrity`,
/// `UnsupportedRuntimeVersion`, `UnsupportedHardware`,
/// `UnsupportedBackend`, `UnsupportedCapability`, `InsufficientCapacity`.
pub fn verify(bundle_path: &Path, config: &AgentConfig) -> AgentResult<VerifiedBundle> {
    verify_with_probes(bundle_path, config, &BTreeMap::new())
}

/// Variant of [`verify`] that also rejects the deploy when the bundle's
/// declared backend has a non-Runnable [`BackendProbeReport`] in
/// `probes`. The coordinator calls this path with the probe results
/// populated at agent startup (packaging). Tests that do not care
/// about backend probing keep using [`verify`].
///
/// # Errors
///
/// In addition to the variants returned by [`verify`], returns
/// [`AgentError::BackendUnrunnable`] when the bundle's
/// `backend_hint` matches a probe entry whose state is anything other
/// than [`BackendProbeState::Runnable`]. The state is the one for the
/// interpreter the bundle's sidecar would run in: the named runner
/// profile's, or the backend's own when the manifest names none. A
/// profile that is not installed is refused as a missing package. The
/// error carries a short reason string for log output; the CLI doctor
/// surfaces the full structured detail.
pub fn verify_with_probes(
    bundle_path: &Path,
    config: &AgentConfig,
    probes: &BTreeMap<String, BackendProbeReport>,
) -> AgentResult<VerifiedBundle> {
    let facts = DeployFacts {
        probes,
        registry: None,
        admission: None,
        state: None,
    };
    verify_before_staging(bundle_path, config, &facts)
}

/// [`verify_with_probes`] with every fact the agent holds: the one
/// validation path a deploy takes before anything is staged.
///
/// # Errors
///
/// In addition to the variants returned by [`verify_with_probes`], returns
/// [`AgentError::BundleManifest`] carrying the rule code when a format 0.2
/// manifest
///
/// - declares a variant whose base is not a bundle this agent has deployed
///   on a row it holds, or which asks for more support than that row gives
///   (judged before the parser, which then refuses every variant as
///   reserved);
/// - names a `hardware_compatibility` row the registry does not have, or
///   asks for `production` on a row that is not Production;
/// - names a runner profile under a backend that declares none.
///
/// Returns [`AgentError::BundleUnsupported`] carrying the rule code when
/// the installed runner profile does not list the manifest's compute type.
pub fn verify_before_staging(
    bundle_path: &Path,
    config: &AgentConfig,
    facts: &DeployFacts<'_>,
) -> AgentResult<VerifiedBundle> {
    check_declared_lineage(bundle_path, facts)?;
    let probes = facts.probes;
    let descriptor = parse_bundle(bundle_path).map_err(parse_error_to_agent_error)?;
    let device = device_context_from_config(config)?;
    let result = evaluate_compatibility(&descriptor, &device);
    if !result.ok {
        // Surface the first violation as a typed agent error. The full
        // list is preserved through `parse_and_check` when callers
        // need the structured payload (e.g. CLI rendering, transaction
        // failure recording).
        if let Some(v) = result.violations.into_iter().next() {
            return Err(violation_to_agent_error(v));
        }
    }
    check_hardware_rows(&descriptor.manifest, facts.registry)?;
    // packaging: refuse the deploy *before* staging if the bundle's
    // declared backend has a non-Runnable probe report. The cache is
    // populated at agent startup so this check is O(1) at deploy time;
    // we never run Python here. Absent backends (no probe entry) are
    // pass-through — they were either not requested at startup or the
    // operator is managing them out-of-band.
    let backend_hint = descriptor.manifest.backend_hint.as_str();
    let profile = descriptor.manifest.profile.as_ref();
    let runner_profile = profile.and_then(|profile| profile.runner_profile.as_deref());
    if let Some(runner) = runner_profile {
        if BACKENDS_WITHOUT_DESCRIPTOR.contains(&backend_hint) {
            return Err(rule_refusal(
                BundleRuleCode::RunnerSelector,
                "runner_profile",
                format!(
                    "`{runner}` cannot be served by backend `{backend_hint}`, which declares no runner profiles"
                ),
            ));
        }
    }
    if let Some(report) = probes.get(backend_hint) {
        let serving = report.serving_state(runner_profile);
        let reason = match serving {
            ServingState::Probed(BackendProbeState::Runnable) => None,
            ServingState::Probed(state) => Some(format_probe_reason(state)),
            ServingState::RunnerProfileNotInstalled => Some(format!(
                "runner profile `{}` is not installed",
                runner_profile.unwrap_or_default()
            )),
        };
        if let Some(reason) = reason {
            return Err(AgentError::BackendUnrunnable {
                backend: backend_hint.to_string(),
                reason,
                platform_reason: PlatformReason::for_serving_state(serving),
            });
        }
        let installed = report
            .runner_profiles
            .iter()
            .map(|probe| &probe.profile)
            .find(|installed| Some(installed.id.as_str()) == runner_profile);
        if let (Some(installed), Some(compute_type)) =
            (installed, profile.and_then(|profile| profile.compute_type))
        {
            if !installed.compute_types.contains(&compute_type) {
                let refusal = BundleProfileError::Rule {
                    code: BundleRuleCode::RunnerComputeType,
                    field: "compute_type".to_owned(),
                    reason: format!(
                        "the installed runner profile `{}` does not list this compute type",
                        installed.id
                    ),
                };
                return Err(AgentError::BundleUnsupported(
                    BundleManifestError::from(refusal).into(),
                ));
            }
        }
    }
    Ok(descriptor.into())
}

fn rule_refusal(code: BundleRuleCode, field: &str, reason: String) -> AgentError {
    profile_refusal(BundleProfileError::Rule {
        code,
        field: field.to_owned(),
        reason,
    })
}

fn profile_refusal(error: BundleProfileError) -> AgentError {
    AgentError::BundleManifest(BundleManifestError::from(error).into())
}

/// Judge a variant declaration against the bases this agent knows, before
/// the parser refuses every variant as reserved. A manifest this cannot
/// read or decode is left to the parser, which reports why.
fn check_declared_lineage(bundle_path: &Path, facts: &DeployFacts<'_>) -> AgentResult<()> {
    let Ok(raw) = fs::read_to_string(bundle_path.join(MANIFEST_FILENAME)) else {
        return Ok(());
    };
    let format_0_2 = serde_json::from_str::<serde_json::Value>(&raw).is_ok_and(|manifest| {
        manifest.get("format_version").and_then(|v| v.as_str()) == Some(PROFILE_FORMAT_VERSION)
    });
    if !format_0_2 {
        return Ok(());
    }
    let Ok(profile) = BundleProfile::from_manifest_text(&raw) else {
        return Ok(());
    };
    profile
        .check_lineage(&known_bases(facts))
        .map_err(profile_refusal)
}

/// The bundles this agent serves, each at the level of the row the machine
/// holds. Empty on a machine that holds no row. No variant can be deployed,
/// so no base has one recorded.
fn known_bases(facts: &DeployFacts<'_>) -> Vec<KnownBase> {
    let level = facts
        .admission
        .zip(facts.registry)
        .and_then(|(admission, registry)| admission.held_row(registry))
        .and_then(|row| match row.support_level() {
            tensorplate_platform::SupportLevel::Production => Some(SupportLevel::Production),
            tensorplate_platform::SupportLevel::Preview => Some(SupportLevel::Preview),
            tensorplate_platform::SupportLevel::Experimental => Some(SupportLevel::Experimental),
            tensorplate_platform::SupportLevel::Planned => None,
        });
    let (Some(level), Some(state)) = (level, facts.state) else {
        return Vec::new();
    };
    let active = state.active.iter().map(|record| {
        (
            &record.bundle_name,
            &record.bundle_version,
            &record.bundle_digest,
        )
    });
    let members = state
        .resident_set
        .iter()
        .flat_map(|set| &set.members)
        .filter(|member| member.state == MemberState::Serving)
        .map(|member| {
            (
                &member.bundle_name,
                &member.bundle_version,
                &member.bundle_digest,
            )
        });
    let mut bases: Vec<KnownBase> = Vec::new();
    for (name, version, digest) in active.chain(members) {
        let base = KnownBase {
            name: name.clone(),
            version: version.clone(),
            manifest_digest: digest.clone(),
            support_level: level,
            variants: Vec::new(),
        };
        if !bases.contains(&base) {
            bases.push(base);
        }
    }
    bases
}

/// Every row a manifest names must be one the registry has, and a
/// manifest asks for `production` only on rows that are Production.
/// Whether a Production row's evidence covers this bundle is not judged
/// here.
fn check_hardware_rows(
    manifest: &BundleManifest,
    registry: Option<&PlatformRegistry>,
) -> AgentResult<()> {
    let (Some(profile), Some(registry)) = (manifest.profile.as_ref(), registry) else {
        return Ok(());
    };
    let mut rows = Vec::with_capacity(profile.hardware_compatibility.len());
    for id in &profile.hardware_compatibility {
        let Some(row) = registry.row(id) else {
            return Err(rule_refusal(
                BundleRuleCode::HardwareRow,
                "hardware_compatibility",
                format!("`{id}` is not a platform support row of this installation"),
            ));
        };
        rows.push(row);
    }
    if profile.support_level != Some(SupportLevel::Production) {
        return Ok(());
    }
    for row in rows {
        if row.support_level() != tensorplate_platform::SupportLevel::Production {
            return Err(rule_refusal(
                BundleRuleCode::SupportClaim,
                "support_level",
                format!(
                    "asks for `production` on row `{}`, which is {}",
                    row.row_id(),
                    row.support_level().as_str()
                ),
            ));
        }
    }
    Ok(())
}

/// Render a probe state into a short single-line reason suitable for
/// embedding in [`AgentError::BackendUnrunnable`]. The full structured
/// detail is preserved in the [`BackendProbeReport`] passed through
/// to doctor.
fn format_probe_reason(state: &BackendProbeState) -> String {
    match state {
        BackendProbeState::Runnable => "runnable".into(),
        BackendProbeState::DescriptorMissing => "backend descriptor not installed".into(),
        BackendProbeState::DescriptorMalformed { reason } => {
            format!("backend descriptor invalid: {reason}")
        }
        BackendProbeState::RunnerProfilePackageMissing {
            profile, package, ..
        } => {
            format!("runner profile `{profile}` needs package `{package}`, which is not installed")
        }
        BackendProbeState::RuntimeVersionMismatch {
            runtime_version,
            descriptor_min,
        } => {
            format!("tensorplate runtime {runtime_version} below backend minimum {descriptor_min}")
        }
        BackendProbeState::PythonInterpreterMissing { interpreter } => {
            format!("Python interpreter `{interpreter}` missing")
        }
        BackendProbeState::PythonVersionMismatch {
            interpreter,
            observed,
            required,
        } => format!(
            "Python interpreter `{interpreter}` is {observed}, descriptor requires {required}"
        ),
        BackendProbeState::PythonModuleImportFailed { module, .. } => {
            format!("Python module `{module}` failed to import")
        }
        BackendProbeState::PytorchMissing { .. } => "PyTorch is not importable".into(),
        BackendProbeState::PytorchVersionMismatch { observed, required } => {
            format!("PyTorch {observed} below descriptor minimum {required}")
        }
    }
}

/// Run the parser + compatibility evaluator and return the structured
/// [`tensorplate_protocol::bundle::CompatibilityResult`] alongside the
/// descriptor.
///
/// Useful for CLI / status callers that want to render every failing
/// check rather than the first one. `verify()` short-circuits to the
/// agent's typed-error taxonomy; this entry point preserves the full
/// list of violations.
///
/// # Errors
///
/// Returns [`AgentError`] for parser failures only. Compat violations
/// flow through the returned [`CompatibilityResult`] and never raise.
pub fn parse_and_check(
    bundle_path: &Path,
    config: &AgentConfig,
) -> AgentResult<(
    BundleDescriptor,
    tensorplate_protocol::bundle::CompatibilityResult,
)> {
    let descriptor = parse_bundle(bundle_path).map_err(parse_error_to_agent_error)?;
    let device = device_context_from_config(config)?;
    let result = evaluate_compatibility(&descriptor, &device);
    Ok((descriptor, result))
}

/// Capacity gate run separately from `verify` so the deploy transaction
/// can record a `capacity_checked` phase between staging and worker
/// preparation. v0.1.0 reuses the same memory-estimate check as the
/// initial verify; future versions may consult live worker telemetry.
///
/// # Errors
///
/// Returns [`AgentError::InsufficientCapacity`] when the bundle's memory
/// estimate exceeds the configured device memory.
pub fn capacity_check(bundle: &VerifiedBundle, config: &AgentConfig) -> AgentResult<()> {
    if let (Some(estimate), Some(dev_mem)) = (
        bundle.manifest.target_hardware.memory_estimate_bytes,
        config.device_memory_bytes,
    ) {
        if estimate > dev_mem {
            return Err(AgentError::InsufficientCapacity);
        }
    }
    Ok(())
}

/// Public re-export so the coordinator can compare bundle digests
/// without depending on the protocol crate directly.
#[must_use]
pub fn bundle_digests_equal(a: &str, b: &str) -> bool {
    tensorplate_protocol::bundle::digests_equal(a, b)
}

fn parse_error_to_agent_error(err: ParseError) -> AgentError {
    match err {
        ParseError::BundleMissing(p) => AgentError::BundleMissing(p),
        ParseError::ManifestMissing { path } => AgentError::BundleManifest(format!(
            "manifest.json missing at {}",
            path.display()
        ).into()),
        ParseError::Io(e) => AgentError::Io(e),
        ParseError::ManifestMalformed(m) => AgentError::BundleManifest(m.into()),
        ParseError::UnsupportedSchemaVersion { got, expected } => AgentError::BundleManifest(
            format!("unsupported manifest schema_version `{got}` (expected `{expected}`)").into(),
        ),
        error @ ParseError::UnsupportedFormatVersion { .. } => {
            AgentError::BundleManifest(error.to_string().into())
        }
        ParseError::ManifestSemantics(e) => AgentError::BundleManifest(e.into()),
        ParseError::UnsafeArtifactPath { relative_path } => AgentError::BundleManifest(format!(
            "unsafe artifact path `{relative_path}` (absolute, contains `..`, or contains backslash)"
        ).into()),
        ParseError::ArtifactMissing { relative_path } => AgentError::BundleIntegrity {
            path: relative_path,
            reason: "missing on disk".into(),
        },
        ParseError::UnsupportedDigestAlgorithm {
            relative_path,
            algo,
        } => AgentError::BundleIntegrity {
            path: relative_path,
            reason: format!("unsupported digest algorithm `{algo}` (only sha256 in v0.1.0)"),
        },
        ParseError::ArtifactDigestMismatch {
            relative_path,
            declared,
            computed,
        } => AgentError::BundleIntegrity {
            path: relative_path,
            reason: format!("digest mismatch: declared `{declared}` computed `{computed}`"),
        },
        ParseError::ManifestDigestMismatch { declared, computed } => AgentError::BundleIntegrity {
            path: "manifest.json".into(),
            reason: format!(
                "manifest_digest mismatch: declared `{declared}` computed `{computed}`"
            ),
        },
        ParseError::UnsupportedArchiveFormat => AgentError::BundleManifest(
            "packaged `.tpmodel` archives are reserved for a later milestone; provide an unpacked bundle directory".into(),
        ),
    }
}

fn violation_to_agent_error(v: CompatibilityViolation) -> AgentError {
    match v {
        CompatibilityViolation::UnsupportedRuntime { message } => {
            AgentError::UnsupportedRuntimeVersion(message)
        }
        CompatibilityViolation::UnsupportedHardware { message } => {
            AgentError::UnsupportedHardware(message)
        }
        CompatibilityViolation::UnavailableBackend { backend } => {
            AgentError::UnsupportedBackend(backend)
        }
        CompatibilityViolation::UnsupportedCapability {
            backend,
            capability,
        } => AgentError::UnsupportedCapability(capability, backend),
        CompatibilityViolation::UnsupportedPrecision { backend, precision } => {
            AgentError::Unavailable(format!(
                "backend `{backend}` does not publish precision `{precision}`"
            ))
        }
        CompatibilityViolation::InsufficientMemory { .. } => AgentError::InsufficientCapacity,
        CompatibilityViolation::BackendArtifactMismatch {
            backend,
            artifact_kind,
        } => AgentError::Unavailable(format!(
            "backend `{backend}` does not accept artifact kind `{artifact_kind}`"
        )),
    }
}

fn device_context_from_config(config: &AgentConfig) -> AgentResult<DeviceContext> {
    let runtime_version = config
        .runtime_version
        .as_deref()
        .ok_or_else(|| AgentError::Config("config.runtime_version not resolved".into()))?
        .to_string();
    let backends = config
        .available_backends
        .iter()
        .map(|backend| {
            let cap = config.capability_for(backend);
            BackendProfile {
                backend: backend.clone(),
                capabilities: capability_view(&cap),
                supported_precision: cap.supported_precision,
                supported_artifact_kinds: cap.supported_artifact_kinds,
            }
        })
        .collect();
    Ok(DeviceContext {
        runtime_version: Some(runtime_version),
        device_family: Some(config.device_family),
        device_memory_bytes: config.device_memory_bytes,
        backends,
    })
}

fn capability_view(c: &BackendCapability) -> BackendCapabilityView {
    BackendCapabilityView {
        async_: c.async_,
        streaming: c.streaming,
        generation: c.generation,
        kv_cache: c.kv_cache,
        fixed_shape: c.fixed_shape,
        deterministic_latency: c.deterministic_latency,
        control_loop_integration: c.control_loop_integration,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access,
        clippy::needless_borrows_for_generic_args
    )]

    use tensorplate_platform::PlatformReason;

    use crate::backend_detection::{BackendProbeReport, BackendProbeState};

    use super::{verify, AgentConfig, AgentError, BackendCapability};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tensorplate_protocol::bundle_manifest::DeviceFamily;
    use tensorplate_protocol::SCHEMA_VERSION;

    fn write_artifact(dir: &std::path::Path, name: &str, body: &[u8]) -> String {
        use sha2::Digest;
        let path = dir.join(name);
        fs::write(&path, body).expect("write artifact");
        let mut hasher = sha2::Sha256::new();
        hasher.update(body);
        format!("sha256:{}", hex::encode(hasher.finalize()))
    }

    fn write_manifest(dir: &std::path::Path, body: &str) {
        fs::write(dir.join("manifest.json"), body).expect("write manifest");
    }

    fn config(state_dir: PathBuf, staging_dir: PathBuf) -> AgentConfig {
        AgentConfig {
            schema_version: SCHEMA_VERSION.to_string(),
            transport: crate::config::ControlTransport::UnixSocket,
            socket_path: Some(PathBuf::from("/tmp/agent.sock")),
            tcp_bind_host: "127.0.0.1".into(),
            tcp_bind_port: 0,
            state_dir,
            staging_dir,
            available_backends: vec!["mock".into()],
            backend_capabilities: BTreeMap::new(),
            memory_admission: None,
            device_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            device_family: DeviceFamily::Any,
            admission_posture: None,
            worker: Default::default(),
            supervision: None,
            runtime_version: Some("0.1.0".into()),
        }
        .validate()
        .expect("valid")
    }

    fn vision_bundle(dir: &std::path::Path) -> String {
        let model_digest = write_artifact(dir, "model.engine", b"engine-bytes");
        format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"yolov8n","version":"1.0.0","format_version":"0.1","model_class":"vision","backend_hint":"mock","artifacts":[{{"role":"model","path":"model.engine","digest":"{model_digest}"}}]}}"#,
        )
    }

    #[test]
    fn happy_path_verifies() {
        let bundle = TempDir::new().expect("td");
        write_manifest(bundle.path(), &vision_bundle(bundle.path()));
        let td = TempDir::new().expect("td2");
        let cfg = config(td.path().join("state"), td.path().join("staging"));
        let v = verify(bundle.path(), &cfg).expect("verify");
        assert_eq!(v.manifest.name, "yolov8n");
        assert!(v.manifest_digest.starts_with("sha256:"));
    }

    #[test]
    fn rejects_corrupt_artifact() {
        let bundle = TempDir::new().expect("td");
        let manifest = vision_bundle(bundle.path());
        write_manifest(bundle.path(), &manifest);
        // Replace artifact bytes after digest was computed.
        fs::write(bundle.path().join("model.engine"), b"tampered").expect("write");
        let td = TempDir::new().expect("td2");
        let cfg = config(td.path().join("s"), td.path().join("st"));
        let err = verify(bundle.path(), &cfg).expect_err("must reject");
        assert!(matches!(err, AgentError::BundleIntegrity { .. }));
    }

    #[test]
    fn rejects_missing_artifact() {
        let bundle = TempDir::new().expect("td");
        let manifest = vision_bundle(bundle.path());
        write_manifest(bundle.path(), &manifest);
        fs::remove_file(bundle.path().join("model.engine")).expect("rm");
        let td = TempDir::new().expect("td2");
        let cfg = config(td.path().join("s"), td.path().join("st"));
        let err = verify(bundle.path(), &cfg).expect_err("must reject");
        assert!(matches!(err, AgentError::BundleIntegrity { .. }));
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"99.99","name":"m","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"mock","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let cfg = config(td.path().join("s"), td.path().join("st"));
        let err = verify(bundle.path(), &cfg).expect_err("must reject");
        assert!(matches!(err, AgentError::BundleManifest(_)));
    }

    #[test]
    fn rejects_unknown_backend() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"m","version":"1","format_version":"0.1","model_class":"vla","backend_hint":"python_pytorch","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let cfg = config(td.path().join("s"), td.path().join("st"));
        let err = verify(bundle.path(), &cfg).expect_err("must reject");
        assert!(matches!(err, AgentError::UnsupportedBackend(_)));
    }

    #[test]
    fn rejects_unsupported_runtime_range() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"m","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"mock","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}],"runtime_compatibility":{{"min_runtime_version":"99.0.0"}}}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let cfg = config(td.path().join("s"), td.path().join("st"));
        let err = verify(bundle.path(), &cfg).expect_err("must reject");
        assert!(matches!(err, AgentError::UnsupportedRuntimeVersion(_)));
    }

    #[test]
    fn rejects_capacity_overflow() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"m","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"mock","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}],"target_hardware":{{"memory_estimate_bytes":17179869184}}}}"#,
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let cfg = config(td.path().join("s"), td.path().join("st"));
        let err = verify(bundle.path(), &cfg).expect_err("must reject");
        assert!(matches!(err, AgentError::InsufficientCapacity));
    }

    #[test]
    fn rejects_missing_capability() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"m","version":"1","format_version":"0.1","model_class":"language","backend_hint":"mock","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}],"capability_requirements":{{"streaming":true}}}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let cfg = config(td.path().join("s"), td.path().join("st"));
        let err = verify(bundle.path(), &cfg).expect_err("must reject");
        assert!(matches!(err, AgentError::UnsupportedCapability(_, _)));
    }

    #[test]
    fn accepts_published_capability() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"m","version":"1","format_version":"0.1","model_class":"language","backend_hint":"mock","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}],"capability_requirements":{{"streaming":true}}}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let mut cfg = config(td.path().join("s"), td.path().join("st"));
        cfg.backend_capabilities.insert(
            "mock".into(),
            BackendCapability {
                streaming: true,
                ..BackendCapability::default()
            },
        );
        verify(bundle.path(), &cfg).expect("ok");
    }

    #[test]
    fn rejects_unpublished_e13_capabilities() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"m","version":"1","format_version":"0.1","model_class":"vla","backend_hint":"mock","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}],"capability_requirements":{{"deterministic_latency":true,"control_loop_integration":true}}}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let cfg = config(td.path().join("s"), td.path().join("st"));
        let err = verify(bundle.path(), &cfg).expect_err("must reject");
        assert!(matches!(err, AgentError::UnsupportedCapability(_, _)));
    }

    #[test]
    fn rejects_unsupported_precision_from_agent_profile() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"m","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"mock","precision_hint":"fp16","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let mut cfg = config(td.path().join("s"), td.path().join("st"));
        cfg.backend_capabilities.insert(
            "mock".into(),
            BackendCapability {
                supported_precision: vec!["int8".into()],
                ..BackendCapability::default()
            },
        );
        let err = verify(bundle.path(), &cfg).expect_err("must reject");
        assert!(matches!(err, AgentError::Unavailable(_)));
    }

    #[test]
    fn rejects_explicit_artifact_kind_mismatch_from_agent_profile() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"m","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"mock","artifacts":[{{"role":"model","kind":"vitis_xmodel","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let mut cfg = config(td.path().join("s"), td.path().join("st"));
        cfg.backend_capabilities.insert(
            "mock".into(),
            BackendCapability {
                supported_artifact_kinds: vec!["tensorrt_engine".into()],
                ..BackendCapability::default()
            },
        );
        let err = verify(bundle.path(), &cfg).expect_err("must reject");
        assert!(matches!(err, AgentError::Unavailable(_)));
    }

    #[test]
    fn verify_with_probes_rejects_non_runnable_backend() {
        use super::verify_with_probes;
        use crate::backend_detection::{BackendProbeReport, BackendProbeState};
        use std::collections::BTreeMap;

        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"smolvla","version":"1","format_version":"0.1","model_class":"vla","backend_hint":"python_pytorch","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let mut cfg = config(td.path().join("s"), td.path().join("st"));
        cfg.available_backends.push("python_pytorch".into());

        // No probe entry yet -> verify rejects with UnsupportedBackend
        // because backend_capabilities doesn't publish anything for
        // python_pytorch — but verify happens to short-circuit at the
        // capability stage first. Add the probe entry and confirm the
        // BackendUnrunnable path lights up.
        let mut probes = BTreeMap::new();
        probes.insert(
            "python_pytorch".into(),
            BackendProbeReport {
                backend_name: "python_pytorch".into(),
                descriptor_path: PathBuf::from(
                    "/usr/share/tensorplate/backends/python_pytorch/backend.json",
                ),
                state: BackendProbeState::DescriptorMissing,
                install_hint: Some("apt install tensorplate-backend-python-pytorch".into()),
                runner_profiles: Vec::new(),
            },
        );
        let err = verify_with_probes(bundle.path(), &cfg, &probes).expect_err("must reject");
        match err {
            AgentError::BackendUnrunnable {
                backend,
                reason,
                platform_reason,
            } => {
                assert_eq!(backend, "python_pytorch");
                assert!(reason.contains("descriptor not installed"));
                // An absent descriptor is a package that is not
                // installed, and must not read as a runtime that is.
                assert_eq!(platform_reason, Some(PlatformReason::MissingBackendPackage));
            }
            other => panic!("expected BackendUnrunnable, got {other:?}"),
        }
    }

    #[test]
    fn a_runtime_that_will_not_import_is_not_a_missing_package() {
        // The distinction the reason vocabulary exists to keep. Both
        // states refuse the deploy; they send an operator to different
        // places, and collapsing them tells someone whose PyTorch cannot
        // reach its accelerator to reinstall a package they already have.
        use super::verify_with_probes;
        use std::collections::BTreeMap;

        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"smolvla","version":"1","format_version":"0.1","model_class":"vla","backend_hint":"python_pytorch","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let mut cfg = config(td.path().join("s"), td.path().join("st"));
        cfg.available_backends.push("python_pytorch".into());

        let mut probes = BTreeMap::new();
        probes.insert(
            "python_pytorch".into(),
            BackendProbeReport {
                backend_name: "python_pytorch".into(),
                descriptor_path: PathBuf::from("/usr/share/tensorplate/backends/python_pytorch"),
                state: BackendProbeState::PytorchMissing {
                    detail: "No module named 'torch'".into(),
                },
                install_hint: None,
                runner_profiles: Vec::new(),
            },
        );

        let record = verify_with_probes(bundle.path(), &cfg, &probes)
            .expect_err("an unusable runtime must refuse the deploy")
            .to_record();
        // The typed reason must reach the wire, not stop at the crate
        // boundary: `context` is what a CLI and the durable store read.
        assert_eq!(
            record.context.as_deref(),
            Some("accelerator_runtime_unavailable"),
            "an installed-but-unusable runtime is not a missing package"
        );
    }

    #[test]
    fn a_runner_profile_package_that_is_not_installed_is_a_missing_package() {
        use super::verify_with_probes;
        use std::collections::BTreeMap;

        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"smolvla","version":"1","format_version":"0.1","model_class":"vla","backend_hint":"python_pytorch","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let mut cfg = config(td.path().join("s"), td.path().join("st"));
        cfg.available_backends.push("python_pytorch".into());

        let mut probes = BTreeMap::new();
        probes.insert(
            "python_pytorch".into(),
            BackendProbeReport {
                backend_name: "python_pytorch".into(),
                descriptor_path: PathBuf::from("/usr/share/tensorplate/backends/python_pytorch"),
                state: BackendProbeState::RunnerProfilePackageMissing {
                    profile: "kokoro".into(),
                    package: "tensorplate-speech-runtime-cuda".into(),
                    declaration: "runner_profiles.d/kokoro.json".into(),
                },
                install_hint: None,
                runner_profiles: Vec::new(),
            },
        );

        let record = verify_with_probes(bundle.path(), &cfg, &probes)
            .expect_err("a profile with a package missing must refuse the deploy")
            .to_record();
        assert_eq!(
            record.context.as_deref(),
            Some("missing_backend_package"),
            "a package to install is not a runtime that will not run"
        );
        assert!(record.message.contains("tensorplate-speech-runtime-cuda"));
    }

    #[test]
    fn a_bundle_that_names_a_runner_profile_is_decided_by_that_profiles_interpreter() {
        use super::verify_with_probes;
        use crate::backend_detection::{BackendProbeReport, BackendProbeState, RunnerProfileProbe};
        use std::collections::BTreeMap;
        use tensorplate_protocol::backend_descriptor::{ComputeType, RunnerProfile};

        let fixture = |name: &str| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../test/models/bundles")
                .join(name)
        };
        let td = TempDir::new().expect("td");
        let mut cfg = config(td.path().join("s"), td.path().join("st"));
        cfg.available_backends.push("python_pytorch".into());
        cfg.runtime_version = Some("0.3.1".into());
        let probe = |id: &str, state: BackendProbeState| RunnerProfileProbe {
            profile: RunnerProfile {
                id: id.into(),
                interpreter: "/opt/env/bin/python".into(),
                environment_root: "/opt/env".into(),
                library_search_paths: Vec::new(),
                packages: vec!["speech-runtime".into()],
                compute_types: vec![ComputeType::Float16],
            },
            state,
            observed: None,
        };
        // A host with the speech runtime and no PyTorch in the descriptor's
        // own interpreter, and a `kokoro` whose interpreter does not start.
        let report = |state: BackendProbeState| BackendProbeReport {
            backend_name: "python_pytorch".into(),
            descriptor_path: PathBuf::from("backend.json"),
            state,
            install_hint: None,
            runner_profiles: vec![
                probe("faster_whisper", BackendProbeState::Runnable),
                probe(
                    "kokoro",
                    BackendProbeState::PythonInterpreterMissing {
                        interpreter: "/opt/env/bin/python".into(),
                    },
                ),
            ],
        };
        let verdict = |bundle: &str, report: BackendProbeReport| {
            let probes = BTreeMap::from([("python_pytorch".to_string(), report)]);
            verify_with_probes(&fixture(bundle), &cfg, &probes).map(|_| ())
        };
        let refusal = |result: crate::AgentResult<()>| match result {
            Err(AgentError::BackendUnrunnable {
                reason,
                platform_reason,
                ..
            }) => (reason, platform_reason),
            other => panic!("expected BackendUnrunnable, got {other:?}"),
        };
        let no_torch = || BackendProbeState::PytorchMissing {
            detail: "ModuleNotFoundError".into(),
        };

        verdict("v0_2/speech_stt_streaming", report(no_torch())).expect("faster_whisper runs");
        assert_eq!(
            refusal(verdict("v0_2/speech_tts_streaming", report(no_torch()))),
            (
                "Python interpreter `/opt/env/bin/python` missing".to_string(),
                Some(PlatformReason::AcceleratorRuntimeUnavailable)
            )
        );
        // The bundle that names no profile still needs the descriptor's interpreter.
        assert_eq!(
            refusal(verdict("v0_1/x86_fixture_smoke", report(no_torch()))).0,
            "PyTorch is not importable"
        );
        verdict(
            "v0_1/x86_fixture_smoke",
            report(BackendProbeState::Runnable),
        )
        .expect("own");

        // A profile no package declares is a package to install.
        let mut uninstalled = report(BackendProbeState::Runnable);
        uninstalled.runner_profiles.remove(0);
        assert_eq!(
            refusal(verdict("v0_2/speech_stt_streaming", uninstalled)),
            (
                "runner profile `faster_whisper` is not installed".to_string(),
                Some(PlatformReason::MissingBackendPackage)
            )
        );

        // A backend-wide refusal decides a profile's bundle too.
        assert_eq!(
            refusal(verdict(
                "v0_2/speech_stt_streaming",
                report(BackendProbeState::DescriptorMalformed {
                    reason: "bad declaration".into()
                })
            ))
            .1,
            Some(PlatformReason::AcceleratorRuntimeUnavailable)
        );
    }

    #[test]
    fn a_declared_compute_type_must_be_one_the_installed_profile_lists() {
        use super::verify_with_probes;
        use crate::backend_detection::{BackendProbeReport, BackendProbeState, RunnerProfileProbe};
        use std::collections::BTreeMap;
        use tensorplate_protocol::backend_descriptor::{ComputeType, RunnerProfile};

        let td = TempDir::new().expect("td");
        let mut cfg = config(td.path().join("s"), td.path().join("st"));
        cfg.available_backends.push("python_pytorch".into());
        cfg.runtime_version = Some("0.3.1".into());
        // The TTS fixture declares `float32`.
        let verdict = |compute_types: &[ComputeType]| {
            let report = BackendProbeReport {
                backend_name: "python_pytorch".into(),
                descriptor_path: PathBuf::from("backend.json"),
                state: BackendProbeState::Runnable,
                install_hint: None,
                runner_profiles: vec![RunnerProfileProbe {
                    profile: RunnerProfile {
                        id: "kokoro".into(),
                        interpreter: "/opt/env/bin/python".into(),
                        environment_root: "/opt/env".into(),
                        library_search_paths: Vec::new(),
                        packages: vec!["speech-runtime".into()],
                        compute_types: compute_types.to_vec(),
                    },
                    state: BackendProbeState::Runnable,
                    observed: None,
                }],
            };
            let probes = BTreeMap::from([("python_pytorch".to_string(), report)]);
            let bundle = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../test/models/bundles/v0_2/speech_tts_streaming");
            verify_with_probes(&bundle, &cfg, &probes).map(|_| ())
        };

        verdict(&[ComputeType::Float16, ComputeType::Float32]).expect("listed");
        let record = verdict(&[ComputeType::Float16, ComputeType::Int8Float32])
            .expect_err("not listed")
            .to_record();
        assert_eq!(record.context.as_deref(), Some("bundle_r6_compute_type"));
        assert_eq!(record.code, tensorplate_protocol::ErrorCode::Unsupported);
        assert!(!record.recoverable);
        assert!(record.message.contains("`kokoro`"), "{}", record.message);
    }

    #[test]
    fn known_bases_are_the_serving_bundles_at_the_level_of_the_held_row() {
        use super::{known_bases, DeployFacts};
        use crate::platform_admission::PlatformAdmission;
        use std::collections::{BTreeMap, BTreeSet};
        use tensorplate_platform::{AdmissionPosture, PlatformRegistry};
        use tensorplate_protocol::agent_state::decode_agent_state;
        use tensorplate_protocol::bundle_profile::SupportLevel;
        use tensorplate_protocol::resident_set::MemberState;

        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let registry = PlatformRegistry::load(&repo.join("config/platform")).expect("registry");
        let fixture = repo.join("protocol/rust/tests/fixtures/agent_state_0_2_two_member_set.json");
        let mut state =
            decode_agent_state(&fs::read_to_string(fixture).expect("fixture")).expect("state");
        let probes = BTreeMap::new();
        let admitted = |row: &str, validated| PlatformAdmission::Supported {
            row_id: row.into(),
            capability: None,
            installed_packages: BTreeSet::new(),
            posture: AdmissionPosture::TechnicalPrerequisites,
            posture_from: "row",
            validated,
            memory_telemetry: None,
            signal_telemetry: None,
        };
        let names = |admission: Option<&PlatformAdmission>,
                     state: &tensorplate_protocol::agent_state::AgentState| {
            let facts = DeployFacts {
                probes: &probes,
                registry: Some(&registry),
                admission,
                state: Some(state),
            };
            known_bases(&facts)
                .into_iter()
                .map(|base| {
                    assert!(base.variants.is_empty());
                    (base.name, base.version, base.support_level)
                })
                .collect::<Vec<_>>()
        };
        let member =
            |name: &str, version: &str, level| (name.to_owned(), version.to_owned(), level);

        let production = admitted("ubuntu2404-x86-l4-g2s8", true);
        assert_eq!(
            names(Some(&production), &state),
            [
                member("speech-stt-bundle", "1.0.0", SupportLevel::Production),
                member("speech-tts-bundle", "1.0.1", SupportLevel::Production),
            ]
        );
        let preview = admitted("ubuntu2404-x86-l4-g2s24", true);
        assert_eq!(names(Some(&preview), &state)[0].2, SupportLevel::Preview);

        // A planned row, a machine its row's evidence does not cover, a
        // refused machine and a row the registry lost hold no level.
        for admission in [
            admitted("jetson-agx-orin-32gb", true),
            admitted("ubuntu2404-x86-l4-g2s8", false),
            admitted("ubuntu2404-x86-l4-unlisted", true),
            PlatformAdmission::detection_failed("no probe"),
        ] {
            assert!(names(Some(&admission), &state).is_empty(), "{admission:?}");
        }
        assert!(names(None, &state).is_empty());

        let set = state.resident_set.as_mut().expect("set");
        set.members[0].state = MemberState::Quarantined;
        set.members[1].bundle_name = "repeated".into();
        let mut repeat = set.members[1].clone();
        repeat.deployment_id = "speech-tts-again".into();
        set.members.push(repeat);
        assert_eq!(
            names(Some(&production), &state),
            [member("repeated", "1.0.1", SupportLevel::Production)]
        );
    }

    #[test]
    fn verify_with_probes_accepts_runnable_backend() {
        use super::verify_with_probes;
        use crate::backend_detection::{BackendProbeReport, BackendProbeState};
        use std::collections::BTreeMap;

        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"yolov8n","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"mock","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let cfg = config(td.path().join("s"), td.path().join("st"));
        let mut probes = BTreeMap::new();
        // Probing the mock backend never runs in production (main()
        // skips it) but we exercise the happy path here.
        probes.insert(
            "mock".into(),
            BackendProbeReport {
                backend_name: "mock".into(),
                descriptor_path: PathBuf::from("/dev/null"),
                state: BackendProbeState::Runnable,
                install_hint: None,
                runner_profiles: Vec::new(),
            },
        );
        verify_with_probes(bundle.path(), &cfg, &probes).expect("ok");
    }

    #[test]
    fn parse_and_check_reports_full_violation_list() {
        use super::parse_and_check;
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        // unavailable backend + unsupported hardware in one bundle.
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"m","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"vitis_ai","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}],"target_hardware":{{"device_family":"kria"}}}}"#
        );
        write_manifest(bundle.path(), &body);
        let td = TempDir::new().expect("td2");
        let mut cfg = config(td.path().join("s"), td.path().join("st"));
        cfg.device_family = DeviceFamily::JetsonOrin;
        let (_, result) = parse_and_check(bundle.path(), &cfg).expect("parse");
        assert!(!result.ok);
        assert!(result.violations.len() >= 2);
    }
}
