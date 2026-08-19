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
//   - The format_version major matches the runtime's supported major.
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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tensorplate_protocol::bundle::{
    evaluate_compatibility, parse_bundle, BackendCapabilityView, BackendProfile, BundleDescriptor,
    CompatibilityViolation, DeviceContext, ParseError,
};
use tensorplate_protocol::bundle_manifest::BundleManifest;

use crate::backend_detection::{BackendProbeReport, BackendProbeState};
use crate::config::{AgentConfig, BackendCapability};
use crate::error::{AgentError, AgentResult};

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
/// than [`BackendProbeState::Runnable`]. The error carries a short
/// reason string for log output; the CLI doctor surfaces the full
/// structured detail.
pub fn verify_with_probes(
    bundle_path: &Path,
    config: &AgentConfig,
    probes: &BTreeMap<String, BackendProbeReport>,
) -> AgentResult<VerifiedBundle> {
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
    // packaging: refuse the deploy *before* staging if the bundle's
    // declared backend has a non-Runnable probe report. The cache is
    // populated at agent startup so this check is O(1) at deploy time;
    // we never run Python here. Absent backends (no probe entry) are
    // pass-through — they were either not requested at startup or the
    // operator is managing them out-of-band.
    let backend_hint = descriptor.manifest.backend_hint.as_str();
    if let Some(report) = probes.get(backend_hint) {
        if !matches!(report.state, BackendProbeState::Runnable) {
            return Err(AgentError::BackendUnrunnable {
                backend: backend_hint.to_string(),
                reason: format_probe_reason(&report.state),
            });
        }
    }
    Ok(descriptor.into())
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
        )),
        ParseError::Io(e) => AgentError::Io(e),
        ParseError::ManifestMalformed(m) => AgentError::BundleManifest(m),
        ParseError::UnsupportedSchemaVersion { got, expected } => AgentError::BundleManifest(
            format!("unsupported manifest schema_version `{got}` (expected `{expected}`)"),
        ),
        ParseError::UnsupportedFormatVersion {
            got,
            supported_major,
        } => AgentError::BundleManifest(format!(
            "unsupported bundle format_version `{got}` (runtime supports major {supported_major})"
        )),
        ParseError::ManifestSemantics(e) => AgentError::BundleManifest(e.to_string()),
        ParseError::UnsafeArtifactPath { relative_path } => AgentError::BundleManifest(format!(
            "unsafe artifact path `{relative_path}` (absolute, contains `..`, or contains backslash)"
        )),
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
            },
        );
        let err = verify_with_probes(bundle.path(), &cfg, &probes).expect_err("must reject");
        match err {
            AgentError::BackendUnrunnable { backend, reason } => {
                assert_eq!(backend, "python_pytorch");
                assert!(reason.contains("descriptor not installed"));
            }
            other => panic!("expected BackendUnrunnable, got {other:?}"),
        }
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
