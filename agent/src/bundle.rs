// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F03: Bundle reader and deploy-time verifier.
//
// The verifier checks:
//
//   - manifest.json exists, decodes against the v0.1 schema, and validates
//     semantically (BundleManifest::validate).
//   - Each artifact file's sha256 matches its declared digest.
//   - manifest_digest (when present) matches the canonical manifest with
//     that field stripped.
//   - The format_version major matches the runtime's supported major.
//   - runtime_compatibility includes the configured runtime version.
//   - target_hardware.device_family matches the configured device family
//     (or is `any`).
//   - target_hardware.min_memory_bytes does not exceed the configured
//     device_memory_bytes (when both are set).
//   - target_hardware.memory_estimate_bytes does not exceed the configured
//     device_memory_bytes (when both are set).
//   - backend_hint is in `available_backends`.
//   - capability_requirements are satisfied by the configured backend
//     capability map.
//
// No backend is selected heuristically; bundles are rejected with typed
// errors when their declared backend is unknown or unavailable.

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use tensorplate_protocol::bundle_manifest::{BundleManifest, CapabilityRequirements, DeviceFamily};
use tensorplate_protocol::SCHEMA_VERSION;

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
    let manifest_path = bundle_path.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(AgentError::BundleManifest(format!(
            "manifest.json missing at {}",
            manifest_path.display()
        )));
    }
    let raw = fs::read_to_string(&manifest_path)?;
    let manifest = decode_manifest(&raw)?;
    manifest
        .model_artifact()
        .map(|artifact| artifact.path.clone())
        .ok_or_else(|| AgentError::BundleManifest("manifest missing model artifact".into()))
}

/// Verify the bundle at `bundle_path` against the agent config.
///
/// # Errors
///
/// Returns typed [`AgentError`] variants for every failure class:
/// `BundleMissing`, `BundleManifest`, `BundleIntegrity`,
/// `UnsupportedRuntimeVersion`, `UnsupportedHardware`,
/// `UnsupportedBackend`, `UnsupportedCapability`, `InsufficientCapacity`.
#[allow(clippy::too_many_lines)]
pub fn verify(bundle_path: &Path, config: &AgentConfig) -> AgentResult<VerifiedBundle> {
    let bundle_path = canonicalize_bundle_path(bundle_path)?;
    let manifest_path = bundle_path.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(AgentError::BundleManifest(format!(
            "manifest.json missing at {}",
            manifest_path.display()
        )));
    }
    let raw = fs::read_to_string(&manifest_path)?;
    let manifest: BundleManifest = decode_manifest(&raw)?;

    // Format version: accept exactly the runtime-supported major; reject
    // bundles compiled against an unknown future version.
    let (major, _minor) = parse_version_pair(&manifest.format_version)
        .map_err(|m| AgentError::BundleManifest(m.to_string()))?;
    let supported_major = tensorplate_protocol::BUNDLE_FORMAT_VERSION_MAJOR;
    if major != supported_major {
        return Err(AgentError::BundleManifest(format!(
            "unsupported bundle format_version `{}` (runtime supports major {})",
            manifest.format_version, supported_major
        )));
    }

    // Compute artifact digests strictly.
    for art in &manifest.artifacts {
        let path = bundle_path.join(&art.path);
        if !path.is_file() {
            return Err(AgentError::BundleIntegrity {
                path: art.path.clone(),
                reason: format!("missing on disk: {}", path.display()),
            });
        }
        verify_artifact_digest(&path, &art.digest, &art.path)?;
    }

    // Optional self-digest check.
    let canonical_digest = compute_canonical_manifest_digest(&raw)?;
    if let Some(declared) = manifest.manifest_digest.as_deref() {
        if !digests_equal(declared, &canonical_digest) {
            return Err(AgentError::BundleIntegrity {
                path: "manifest.json".into(),
                reason: format!(
                    "manifest_digest mismatch: declared `{declared}` computed `{canonical_digest}`"
                ),
            });
        }
    }

    // Runtime-compatibility window.
    let runtime_version = config
        .runtime_version
        .as_deref()
        .ok_or_else(|| AgentError::Config("config.runtime_version not resolved".into()))?;
    if let Some(min) = manifest
        .runtime_compatibility
        .min_runtime_version
        .as_deref()
    {
        if compare_versions(runtime_version, min)? == std::cmp::Ordering::Less {
            return Err(AgentError::UnsupportedRuntimeVersion(format!(
                "bundle requires runtime >= {min}; this device runs {runtime_version}"
            )));
        }
    }
    if let Some(max) = manifest
        .runtime_compatibility
        .max_runtime_version
        .as_deref()
    {
        if compare_versions(runtime_version, max)? == std::cmp::Ordering::Greater {
            return Err(AgentError::UnsupportedRuntimeVersion(format!(
                "bundle requires runtime <= {max}; this device runs {runtime_version}"
            )));
        }
    }

    // Hardware compatibility.
    let bundle_hw = manifest.target_hardware.device_family;
    if bundle_hw != DeviceFamily::Any
        && config.device_family != DeviceFamily::Any
        && bundle_hw != config.device_family
    {
        return Err(AgentError::UnsupportedHardware(format!(
            "bundle targets `{}`; device family is `{}`",
            bundle_hw.as_str(),
            config.device_family.as_str()
        )));
    }
    if let (Some(min_mem), Some(dev_mem)) = (
        manifest.target_hardware.min_memory_bytes,
        config.device_memory_bytes,
    ) {
        if min_mem > dev_mem {
            return Err(AgentError::InsufficientCapacity);
        }
    }
    if let (Some(estimate), Some(dev_mem)) = (
        manifest.target_hardware.memory_estimate_bytes,
        config.device_memory_bytes,
    ) {
        if estimate > dev_mem {
            return Err(AgentError::InsufficientCapacity);
        }
    }

    // Backend availability.
    if !config.backend_is_available(&manifest.backend_hint) {
        return Err(AgentError::UnsupportedBackend(
            manifest.backend_hint.clone(),
        ));
    }

    // Capability requirements vs configured capability map. Missing
    // capabilities are rejected; the agent does not infer them.
    check_capabilities(
        &manifest.backend_hint,
        manifest.capability_requirements,
        config.capability_for(&manifest.backend_hint),
    )?;

    Ok(VerifiedBundle {
        manifest,
        manifest_digest: canonical_digest,
        root_path: bundle_path,
    })
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

fn canonicalize_bundle_path(path: &Path) -> AgentResult<PathBuf> {
    match path.canonicalize() {
        Ok(p) if p.is_dir() => Ok(p),
        Ok(_) => Err(AgentError::BundleMissing(path.into())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(AgentError::BundleMissing(path.into()))
        }
        Err(err) => Err(err.into()),
    }
}

fn decode_manifest(raw: &str) -> AgentResult<BundleManifest> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| AgentError::BundleManifest(e.to_string()))?;
    let observed = value
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AgentError::BundleManifest("manifest missing schema_version".into()))?;
    if observed != SCHEMA_VERSION {
        return Err(AgentError::BundleManifest(format!(
            "unsupported manifest schema_version `{observed}` (expected `{SCHEMA_VERSION}`)"
        )));
    }
    let manifest: BundleManifest =
        serde_json::from_value(value).map_err(|e| AgentError::BundleManifest(e.to_string()))?;
    manifest
        .validate()
        .map_err(|e| AgentError::BundleManifest(e.to_string()))
}

fn parse_version_pair(v: &str) -> Result<(u32, u32), &'static str> {
    let mut parts = v.split('.');
    let major = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or("malformed major")?;
    let minor = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or("malformed minor")?;
    if parts.next().is_some() {
        return Err("format_version must be MAJOR.MINOR");
    }
    Ok((major, minor))
}

fn compare_versions(a: &str, b: &str) -> AgentResult<std::cmp::Ordering> {
    let pa = parse_loose_version(a)?;
    let pb = parse_loose_version(b)?;
    Ok(pa.cmp(&pb))
}

fn parse_loose_version(v: &str) -> AgentResult<(u32, u32, u32)> {
    // Strip a "0.1.0-dev" style suffix.
    let core = v.split('-').next().unwrap_or(v);
    let mut parts = core.split('.').map(str::parse::<u32>);
    let major = parts
        .next()
        .ok_or_else(|| AgentError::Config(format!("missing major in `{v}`")))?
        .map_err(|_| AgentError::Config(format!("malformed major in `{v}`")))?;
    let minor = parts
        .next()
        .ok_or_else(|| AgentError::Config(format!("missing minor in `{v}`")))?
        .map_err(|_| AgentError::Config(format!("malformed minor in `{v}`")))?;
    let patch = parts
        .next()
        .map(|p| p.map_err(|_| AgentError::Config(format!("malformed patch in `{v}`"))))
        .transpose()?
        .unwrap_or(0);
    if parts.next().is_some() {
        return Err(AgentError::Config(format!(
            "too many segments in version `{v}`"
        )));
    }
    Ok((major, minor, patch))
}

fn verify_artifact_digest(path: &Path, declared: &str, rel: &str) -> AgentResult<()> {
    let Some((algo, _expected_hex)) = declared.split_once(':') else {
        return Err(AgentError::BundleIntegrity {
            path: rel.into(),
            reason: format!("malformed digest `{declared}`"),
        });
    };
    if !algo.eq_ignore_ascii_case("sha256") {
        return Err(AgentError::BundleIntegrity {
            path: rel.into(),
            reason: format!("unsupported digest algorithm `{algo}` (only sha256 in v0.1.0)"),
        });
    }
    let computed = sha256_hex(path)?;
    let computed_full = format!("sha256:{computed}");
    if !digests_equal(declared, &computed_full) {
        return Err(AgentError::BundleIntegrity {
            path: rel.into(),
            reason: format!("digest mismatch: declared `{declared}` computed `{computed_full}`"),
        });
    }
    Ok(())
}

fn digests_equal(a: &str, b: &str) -> bool {
    // Constant-time-ish compare avoiding case differences in the hex.
    let na = normalize_digest(a);
    let nb = normalize_digest(b);
    na == nb
}

/// Public re-export for the coordinator's `expected_bundle_digest` check.
#[must_use]
pub fn bundle_digests_equal(a: &str, b: &str) -> bool {
    digests_equal(a, b)
}

fn normalize_digest(d: &str) -> Option<(String, String)> {
    let (algo, hex) = d.split_once(':')?;
    Some((algo.to_ascii_lowercase(), hex.to_ascii_lowercase()))
}

fn sha256_hex(path: &Path) -> AgentResult<String> {
    let f = File::open(path)?;
    let mut reader = BufReader::new(f);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(hex::encode(digest))
}

/// Canonical manifest digest is sha256 of the manifest JSON with the
/// `manifest_digest` field stripped, serialized in a stable form
/// (`serde_json::to_vec` on the parsed value).
fn compute_canonical_manifest_digest(raw: &str) -> AgentResult<String> {
    let mut value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| AgentError::BundleManifest(e.to_string()))?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("manifest_digest");
    }
    let canonical = serde_json::to_vec(&value)?;
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn check_capabilities(
    backend: &str,
    required: CapabilityRequirements,
    published: BackendCapability,
) -> AgentResult<()> {
    let pairs: &[(bool, bool, &str)] = &[
        (required.async_, published.async_, "async"),
        (required.streaming, published.streaming, "streaming"),
        (required.generation, published.generation, "generation"),
        (required.kv_cache, published.kv_cache, "kv_cache"),
        (required.fixed_shape, published.fixed_shape, "fixed_shape"),
    ];
    for (need, have, name) in pairs {
        if *need && !*have {
            return Err(AgentError::UnsupportedCapability(
                (*name).to_string(),
                backend.to_string(),
            ));
        }
    }
    Ok(())
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
}
