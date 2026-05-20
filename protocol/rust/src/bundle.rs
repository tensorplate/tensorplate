// SPDX-License-Identifier: Apache-2.0
//
// V01-E13-F01 / V01-E13-F02 / V01-E13-F05: shared bundle parser, layout, and
// integrity verifier.
//
// The agent calls into this module before staging a bundle. The parser is
// deliberately runtime-free: it never loads TensorRT, CUDA, PyTorch, or
// Vitis SDKs, and it returns typed `ParseError` values that downstream
// crates map onto their own error taxonomies.
//
// This is the single shared bundle parser/validator. V01-E08's earlier
// agent-side verifier has been migrated to call into this module instead
// of running a second validation path.

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bundle_manifest::{
    ArtifactKind, ArtifactRole, BundleManifest, BundleManifestError, DeviceFamily,
};
use crate::SCHEMA_VERSION;

/// Stable name of the manifest file inside a bundle root.
pub const MANIFEST_FILENAME: &str = "manifest.json";

/// Optional, reserved location for provenance signature blobs declared by
/// the `signature` field. v0.1.0 does not enforce content; this constant
/// documents the path the parser tolerates.
pub const SIGNATURE_FILENAME: &str = "provenance/signature.json";

/// Optional reserved location for SBOM blobs declared by `provenance.sbom`.
pub const SBOM_FILENAME: &str = "provenance/sbom.json";

/// Bundle parser configuration. v0.1.0 only exposes the "verify artifact
/// digests" knob; future versions may add limits (max archive size,
/// streaming hash budget) without changing the parser entrypoint shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseOptions {
    /// When true (default), every artifact's on-disk file is opened and its
    /// declared digest is verified. Tests that work on a synthetic
    /// fixture without checked-in artifact bytes can disable this.
    pub verify_artifact_digests: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            verify_artifact_digests: true,
        }
    }
}

/// One artifact descriptor exposed to verifier and agent code. Path is
/// absolute; relative `path` from the manifest is preserved for logging
/// and error messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactDescriptor {
    pub role: ArtifactRole,
    pub relative_path: String,
    pub absolute_path: PathBuf,
    pub digest: String,
    pub byte_size: Option<u64>,
}

/// Bundle descriptor returned by [`parse_bundle`]. The descriptor is a
/// value object with no SDK or worker dependencies — it can flow through
/// the agent's deploy transaction, be persisted, or be re-validated
/// without touching the host filesystem again.
#[derive(Clone, Debug, PartialEq)]
pub struct BundleDescriptor {
    /// Absolute, canonicalized bundle root.
    pub root_path: PathBuf,
    /// Fully-validated manifest envelope.
    pub manifest: BundleManifest,
    /// sha256:hex digest of the canonical manifest (with `manifest_digest`
    /// stripped). Stable across parser implementations that follow the
    /// canonicalization documented in [`docs/bundles/integrity.md`].
    pub manifest_digest: String,
    /// One descriptor per declared artifact. Order matches the manifest.
    pub artifacts: Vec<ArtifactDescriptor>,
}

impl BundleDescriptor {
    /// `<name>@<version>` identifier suitable for logs and status output.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}@{}", self.manifest.name, self.manifest.version)
    }

    /// Absolute path of the artifact whose role is `model`. Validated
    /// bundles always contain exactly one such artifact.
    #[must_use]
    pub fn model_artifact_path(&self) -> Option<PathBuf> {
        self.artifacts
            .iter()
            .find(|a| a.role == ArtifactRole::Model)
            .map(|a| a.absolute_path.clone())
    }

    /// Relative path of the artifact whose role is `model`.
    #[must_use]
    pub fn model_artifact_relative_path(&self) -> Option<String> {
        self.artifacts
            .iter()
            .find(|a| a.role == ArtifactRole::Model)
            .map(|a| a.relative_path.clone())
    }
}

/// Typed parse errors. Every parse failure path returns one of these
/// variants with bounded context (no full file contents, no unbounded
/// directory listings).
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("bundle path `{0}` does not exist or is not a directory")]
    BundleMissing(PathBuf),

    #[error("manifest `{path}` missing or not a regular file")]
    ManifestMissing { path: PathBuf },

    #[error("manifest file system error: {0}")]
    Io(#[from] std::io::Error),

    #[error("manifest JSON is malformed: {0}")]
    ManifestMalformed(String),

    #[error("manifest schema_version `{got}` is not supported (expected `{expected}`)")]
    UnsupportedSchemaVersion { got: String, expected: &'static str },

    #[error("manifest format_version `{got}` is not supported (runtime accepts major `{supported_major}`)")]
    UnsupportedFormatVersion { got: String, supported_major: u32 },

    #[error("manifest semantic validation failed: {0}")]
    ManifestSemantics(#[from] BundleManifestError),

    #[error("artifact `{relative_path}` declares unsafe path (absolute, contains `..`, or contains `\\`)")]
    UnsafeArtifactPath { relative_path: String },

    #[error("artifact `{relative_path}` is missing on disk under bundle root")]
    ArtifactMissing { relative_path: String },

    #[error("artifact `{relative_path}` digest algorithm `{algo}` is not supported (v0.1.0 only accepts sha256)")]
    UnsupportedDigestAlgorithm { relative_path: String, algo: String },

    #[error(
        "artifact `{relative_path}` digest mismatch: declared `{declared}` computed `{computed}`"
    )]
    ArtifactDigestMismatch {
        relative_path: String,
        declared: String,
        computed: String,
    },

    #[error("manifest `manifest_digest` mismatch: declared `{declared}` computed `{computed}`")]
    ManifestDigestMismatch { declared: String, computed: String },

    #[error("packaged `.tpmodel` archives are reserved for a later milestone; provide an unpacked bundle directory")]
    UnsupportedArchiveFormat,
}

impl ParseError {
    /// Short stable identifier suitable for logs / status output. Maps each
    /// variant to a static slug; downstream crates use the slug when they
    /// project parse errors onto wire-format error codes.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BundleMissing(_) => "bundle_missing",
            Self::ManifestMissing { .. } => "manifest_missing",
            Self::Io(_) => "io",
            Self::ManifestMalformed(_) => "manifest_malformed",
            Self::UnsupportedSchemaVersion { .. } => "schema_version_unsupported",
            Self::UnsupportedFormatVersion { .. } => "format_version_unsupported",
            Self::ManifestSemantics(_) => "manifest_semantics",
            Self::UnsafeArtifactPath { .. } => "artifact_path_unsafe",
            Self::ArtifactMissing { .. } => "artifact_missing",
            Self::UnsupportedDigestAlgorithm { .. } => "digest_algorithm_unsupported",
            Self::ArtifactDigestMismatch { .. } => "artifact_digest_mismatch",
            Self::ManifestDigestMismatch { .. } => "manifest_digest_mismatch",
            Self::UnsupportedArchiveFormat => "archive_format_unsupported",
        }
    }
}

/// Compatibility result returned by [`evaluate_compatibility`]. Compat is
/// the **second** validation pass: it consumes a fully-parsed
/// [`BundleDescriptor`] plus a device context, and answers whether the
/// bundle can be staged on *this* device.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompatibilityResult {
    pub ok: bool,
    pub violations: Vec<CompatibilityViolation>,
}

impl CompatibilityResult {
    #[must_use]
    pub fn ok() -> Self {
        Self {
            ok: true,
            violations: Vec::new(),
        }
    }

    #[must_use]
    pub fn fail(violations: Vec<CompatibilityViolation>) -> Self {
        Self {
            ok: false,
            violations,
        }
    }
}

/// Typed compatibility-violation categories. Compat errors carry the
/// `code` slug used by the agent and CLI to render specific failures
/// without parsing free-form strings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum CompatibilityViolation {
    /// Bundle runtime range does not include the local runtime version.
    UnsupportedRuntime { message: String },
    /// Hardware mismatch (device family / minimum memory).
    UnsupportedHardware { message: String },
    /// Backend declared in the manifest is not available on the device.
    UnavailableBackend { backend: String },
    /// Capability listed in `capability_requirements` is not published.
    UnsupportedCapability { backend: String, capability: String },
    /// Precision profile declared by the bundle is not published by the
    /// configured backend.
    UnsupportedPrecision { backend: String, precision: String },
    /// Memory estimate exceeds the configured device memory.
    InsufficientMemory {
        estimate_bytes: u64,
        available_bytes: u64,
    },
    /// Backend / artifact mismatch (e.g., backend `tensorrt` paired with
    /// an `.xmodel` artifact).
    BackendArtifactMismatch {
        backend: String,
        artifact_kind: String,
    },
}

impl CompatibilityViolation {
    /// Short stable slug suitable for logs and status output.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedRuntime { .. } => "unsupported_runtime",
            Self::UnsupportedHardware { .. } => "unsupported_hardware",
            Self::UnavailableBackend { .. } => "unavailable_backend",
            Self::UnsupportedCapability { .. } => "unsupported_capability",
            Self::UnsupportedPrecision { .. } => "unsupported_precision",
            Self::InsufficientMemory { .. } => "insufficient_memory",
            Self::BackendArtifactMismatch { .. } => "backend_artifact_mismatch",
        }
    }
}

/// Backend capability map slice used by the compatibility evaluator.
/// Mirrors the agent's `BackendCapability` but lives in the protocol crate
/// so the parser can validate without an agent dependency.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct BackendCapabilityView {
    pub async_: bool,
    pub streaming: bool,
    pub generation: bool,
    pub kv_cache: bool,
    pub fixed_shape: bool,
    pub deterministic_latency: bool,
    pub control_loop_integration: bool,
}

/// Backend publication record consumed by [`evaluate_compatibility`].
///
/// The agent assembles one entry per available backend and passes the
/// resulting `BackendProfile` slice into the evaluator. Each profile
/// carries the backend name, its capability flags, and the precision
/// profiles it can run.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BackendProfile {
    pub backend: String,
    pub capabilities: BackendCapabilityView,
    pub supported_precision: Vec<String>,
    /// Optional list of artifact kinds the backend accepts. When empty,
    /// the evaluator skips the backend/artifact-kind cross-check.
    pub supported_artifact_kinds: Vec<String>,
}

/// Device context evaluated against a [`BundleDescriptor`].
///
/// All fields are optional or have safe defaults; the evaluator skips any
/// check whose context value is `None`.
#[derive(Clone, Debug, Default)]
pub struct DeviceContext {
    pub runtime_version: Option<String>,
    pub device_family: Option<DeviceFamily>,
    pub device_memory_bytes: Option<u64>,
    pub backends: Vec<BackendProfile>,
}

impl DeviceContext {
    fn backend_profile(&self, name: &str) -> Option<&BackendProfile> {
        self.backends.iter().find(|b| b.backend == name)
    }
}

// ----- Parser entrypoints --------------------------------------------------

/// Parse a bundle at `bundle_path` and return a fully-validated
/// [`BundleDescriptor`].
///
/// `bundle_path` must point at an existing directory. v0.1.0 rejects
/// archive inputs with [`ParseError::UnsupportedArchiveFormat`]; the
/// archive reader will land in a later milestone behind the same
/// entrypoint.
///
/// # Errors
///
/// See [`ParseError`]. Every failure path returns a typed variant with
/// bounded context (no unbounded paths or contents).
pub fn parse_bundle(bundle_path: &Path) -> Result<BundleDescriptor, ParseError> {
    parse_bundle_with(bundle_path, ParseOptions::default())
}

/// Like [`parse_bundle`] but with a caller-provided
/// [`ParseOptions`]. Test fixtures that ship a manifest without on-disk
/// artifact bytes call this with `verify_artifact_digests: false`.
///
/// # Errors
///
/// See [`ParseError`].
pub fn parse_bundle_with(
    bundle_path: &Path,
    options: ParseOptions,
) -> Result<BundleDescriptor, ParseError> {
    let bundle_path = canonicalize_bundle_path(bundle_path)?;
    reject_archive_path(&bundle_path)?;

    let manifest_path = bundle_path.join(MANIFEST_FILENAME);
    if !manifest_path.is_file() {
        return Err(ParseError::ManifestMissing {
            path: manifest_path,
        });
    }

    let raw = fs::read_to_string(&manifest_path)?;
    let manifest = decode_manifest(&raw)?;
    let canonical_digest = compute_canonical_manifest_digest(&raw)?;

    let supported_major = crate::BUNDLE_FORMAT_VERSION_MAJOR;
    let (major, _minor) = parse_format_version(&manifest.format_version)?;
    if major != supported_major {
        return Err(ParseError::UnsupportedFormatVersion {
            got: manifest.format_version.clone(),
            supported_major,
        });
    }

    let mut artifacts = Vec::with_capacity(manifest.artifacts.len());
    for art in &manifest.artifacts {
        if !artifact_path_is_safe(&art.path) {
            return Err(ParseError::UnsafeArtifactPath {
                relative_path: art.path.clone(),
            });
        }
        let absolute = bundle_path.join(&art.path);
        if !absolute.starts_with(&bundle_path) {
            return Err(ParseError::UnsafeArtifactPath {
                relative_path: art.path.clone(),
            });
        }

        if options.verify_artifact_digests {
            if !absolute.is_file() {
                return Err(ParseError::ArtifactMissing {
                    relative_path: art.path.clone(),
                });
            }
            verify_artifact_digest(&absolute, &art.digest, &art.path)?;
        }
        artifacts.push(ArtifactDescriptor {
            role: art.role,
            relative_path: art.path.clone(),
            absolute_path: absolute,
            digest: art.digest.clone(),
            byte_size: art.byte_size,
        });
    }

    if let Some(declared) = manifest.manifest_digest.as_deref() {
        if !digests_equal(declared, &canonical_digest) {
            return Err(ParseError::ManifestDigestMismatch {
                declared: declared.to_string(),
                computed: canonical_digest,
            });
        }
    }

    Ok(BundleDescriptor {
        root_path: bundle_path,
        manifest,
        manifest_digest: canonical_digest,
        artifacts,
    })
}

// ----- Compatibility evaluator --------------------------------------------

/// Evaluate compatibility of `descriptor` against `device`.
///
/// Returns a [`CompatibilityResult`] whose `violations` vec lists every
/// failing check in deterministic order. The caller is free to short-
/// circuit on the first violation when constructing typed errors, or to
/// surface the full list when rendering CLI / status output.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn evaluate_compatibility(
    descriptor: &BundleDescriptor,
    device: &DeviceContext,
) -> CompatibilityResult {
    let mut violations = Vec::new();
    let m = &descriptor.manifest;

    // Runtime version range.
    if let Some(runtime_version) = device.runtime_version.as_deref() {
        if let Some(min) = m.runtime_compatibility.min_runtime_version.as_deref() {
            if version_cmp(runtime_version, min).is_lt() {
                violations.push(CompatibilityViolation::UnsupportedRuntime {
                    message: format!(
                        "bundle requires runtime >= {min}; device runs {runtime_version}"
                    ),
                });
            }
        }
        if let Some(max) = m.runtime_compatibility.max_runtime_version.as_deref() {
            if version_cmp(runtime_version, max).is_gt() {
                violations.push(CompatibilityViolation::UnsupportedRuntime {
                    message: format!(
                        "bundle requires runtime <= {max}; device runs {runtime_version}"
                    ),
                });
            }
        }
    }

    // Hardware family.
    if let Some(device_family) = device.device_family {
        let bundle_hw = m.target_hardware.device_family;
        if bundle_hw != DeviceFamily::Any
            && device_family != DeviceFamily::Any
            && bundle_hw != device_family
        {
            violations.push(CompatibilityViolation::UnsupportedHardware {
                message: format!(
                    "bundle targets `{}`; device family is `{}`",
                    bundle_hw.as_str(),
                    device_family.as_str()
                ),
            });
        }
    }

    // Memory estimate / minimum memory.
    if let Some(available) = device.device_memory_bytes {
        if let Some(min_mem) = m.target_hardware.min_memory_bytes {
            if min_mem > available {
                violations.push(CompatibilityViolation::InsufficientMemory {
                    estimate_bytes: min_mem,
                    available_bytes: available,
                });
            }
        }
        if let Some(estimate) = m.target_hardware.memory_estimate_bytes {
            if estimate > available {
                violations.push(CompatibilityViolation::InsufficientMemory {
                    estimate_bytes: estimate,
                    available_bytes: available,
                });
            }
        }
    }

    // Backend availability + capability + precision + artifact kind.
    let backend = m.backend_hint.clone();
    if !device.backends.is_empty() {
        match device.backend_profile(&backend) {
            None => {
                violations.push(CompatibilityViolation::UnavailableBackend { backend });
            }
            Some(profile) => {
                let req = &m.capability_requirements;
                let cap = profile.capabilities;
                let pairs: &[(bool, bool, &str)] = &[
                    (req.async_, cap.async_, "async"),
                    (req.streaming, cap.streaming, "streaming"),
                    (req.generation, cap.generation, "generation"),
                    (req.kv_cache, cap.kv_cache, "kv_cache"),
                    (req.fixed_shape, cap.fixed_shape, "fixed_shape"),
                    (
                        req.deterministic_latency,
                        cap.deterministic_latency,
                        "deterministic_latency",
                    ),
                    (
                        req.control_loop_integration,
                        cap.control_loop_integration,
                        "control_loop_integration",
                    ),
                ];
                for (need, have, name) in pairs {
                    if *need && !*have {
                        violations.push(CompatibilityViolation::UnsupportedCapability {
                            backend: profile.backend.clone(),
                            capability: (*name).to_string(),
                        });
                    }
                }
                let precision = m.precision_hint.as_str();
                if precision != "auto"
                    && !profile.supported_precision.is_empty()
                    && !profile
                        .supported_precision
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case(precision))
                {
                    violations.push(CompatibilityViolation::UnsupportedPrecision {
                        backend: profile.backend.clone(),
                        precision: precision.to_string(),
                    });
                }
                if !profile.supported_artifact_kinds.is_empty() {
                    if let Some(art) = m.artifacts.iter().find(|a| a.role == ArtifactRole::Model) {
                        if let Some(kind) = art
                            .kind
                            .map(ArtifactKind::as_str)
                            .or_else(|| artifact_kind_for_path(&art.path))
                        {
                            if !profile
                                .supported_artifact_kinds
                                .iter()
                                .any(|k| k.eq_ignore_ascii_case(kind))
                            {
                                violations.push(CompatibilityViolation::BackendArtifactMismatch {
                                    backend: profile.backend.clone(),
                                    artifact_kind: kind.to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    if violations.is_empty() {
        CompatibilityResult::ok()
    } else {
        CompatibilityResult::fail(violations)
    }
}

/// Heuristic mapping from artifact filename to the artifact-kind slug
/// used by [`BackendProfile::supported_artifact_kinds`]. Returns `None`
/// for unknown extensions; the evaluator skips the cross-check in that
/// case.
#[must_use]
pub fn artifact_kind_for_path(p: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(p)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("engine") => Some("tensorrt_engine"),
        Some("pt" | "pth") => Some("libtorch_state"),
        Some("safetensors" | "py") => Some("python_pytorch_entry"),
        Some("onnx") => Some("onnx"),
        Some("xmodel") => Some("vitis_xmodel"),
        _ => None,
    }
}

// ----- Internal helpers ----------------------------------------------------

fn canonicalize_bundle_path(path: &Path) -> Result<PathBuf, ParseError> {
    match path.canonicalize() {
        Ok(p) if p.is_dir() => Ok(p),
        Ok(_) => Err(ParseError::BundleMissing(path.into())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(ParseError::BundleMissing(path.into()))
        }
        Err(err) => Err(err.into()),
    }
}

fn reject_archive_path(path: &Path) -> Result<(), ParseError> {
    if path.is_file() {
        let lower = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(lower.as_deref(), Some("tpmodel" | "zip" | "tar" | "tar.gz")) {
            return Err(ParseError::UnsupportedArchiveFormat);
        }
    }
    Ok(())
}

fn decode_manifest(raw: &str) -> Result<BundleManifest, ParseError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ParseError::ManifestMalformed(e.to_string()))?;
    let observed = value
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ParseError::ManifestMalformed("manifest missing schema_version".into()))?;
    if observed != SCHEMA_VERSION {
        return Err(ParseError::UnsupportedSchemaVersion {
            got: observed.to_string(),
            expected: SCHEMA_VERSION,
        });
    }
    let manifest: BundleManifest =
        serde_json::from_value(value).map_err(|e| ParseError::ManifestMalformed(e.to_string()))?;
    let manifest = manifest.validate()?;
    Ok(manifest)
}

fn parse_format_version(v: &str) -> Result<(u32, u32), ParseError> {
    let mut parts = v.split('.');
    let major = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| ParseError::UnsupportedFormatVersion {
            got: v.to_string(),
            supported_major: crate::BUNDLE_FORMAT_VERSION_MAJOR,
        })?;
    let minor = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| ParseError::UnsupportedFormatVersion {
            got: v.to_string(),
            supported_major: crate::BUNDLE_FORMAT_VERSION_MAJOR,
        })?;
    if parts.next().is_some() {
        return Err(ParseError::UnsupportedFormatVersion {
            got: v.to_string(),
            supported_major: crate::BUNDLE_FORMAT_VERSION_MAJOR,
        });
    }
    Ok((major, minor))
}

fn artifact_path_is_safe(p: &str) -> bool {
    if p.is_empty() {
        return false;
    }
    if p.starts_with('/') {
        return false;
    }
    if p.contains('\\') {
        return false;
    }
    p.split('/').all(|seg| !matches!(seg, ".." | "." | ""))
}

fn verify_artifact_digest(path: &Path, declared: &str, relative: &str) -> Result<(), ParseError> {
    let Some((algo, _hex)) = declared.split_once(':') else {
        return Err(ParseError::UnsupportedDigestAlgorithm {
            relative_path: relative.to_string(),
            algo: declared.to_string(),
        });
    };
    if !algo.eq_ignore_ascii_case("sha256") {
        return Err(ParseError::UnsupportedDigestAlgorithm {
            relative_path: relative.to_string(),
            algo: algo.to_string(),
        });
    }
    let computed = sha256_hex_streaming(path)?;
    let computed_full = format!("sha256:{computed}");
    if !digests_equal(declared, &computed_full) {
        return Err(ParseError::ArtifactDigestMismatch {
            relative_path: relative.to_string(),
            declared: declared.to_string(),
            computed: computed_full,
        });
    }
    Ok(())
}

fn sha256_hex_streaming(path: &Path) -> Result<String, ParseError> {
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
    Ok(hex::encode(hasher.finalize()))
}

/// Compute the canonical manifest digest given the raw manifest JSON text.
/// The canonicalization strips the `manifest_digest` field from the
/// top-level object and re-serializes through `serde_json::to_vec` so the
/// digest is independent of whitespace.
///
/// # Errors
///
/// Returns [`ParseError::ManifestMalformed`] when the input is not valid
/// JSON.
pub fn compute_canonical_manifest_digest(raw: &str) -> Result<String, ParseError> {
    let mut value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ParseError::ManifestMalformed(e.to_string()))?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("manifest_digest");
    }
    let canonical =
        serde_json::to_vec(&value).map_err(|e| ParseError::ManifestMalformed(e.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

/// Compute sha256 of a file as `sha256:<hex>`. Exposed so the fixture
/// digest helper can reuse the same streaming hash as the parser.
///
/// # Errors
///
/// Returns [`ParseError::Io`] on read failures.
pub fn compute_artifact_digest(path: &Path) -> Result<String, ParseError> {
    let hex = sha256_hex_streaming(path)?;
    Ok(format!("sha256:{hex}"))
}

/// Constant-ish equality on `algo:hex` digest strings, ignoring case in
/// the algorithm and hex segments.
#[must_use]
pub fn digests_equal(a: &str, b: &str) -> bool {
    match (split_digest(a), split_digest(b)) {
        (Some((alg_a, hex_a)), Some((alg_b, hex_b))) => alg_a == alg_b && hex_a == hex_b,
        _ => false,
    }
}

fn split_digest(d: &str) -> Option<(String, String)> {
    let (algo, hex) = d.split_once(':')?;
    Some((algo.to_ascii_lowercase(), hex.to_ascii_lowercase()))
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let pa = parse_loose_version(a);
    let pb = parse_loose_version(b);
    pa.cmp(&pb)
}

fn parse_loose_version(v: &str) -> (u32, u32, u32) {
    let core = v.split('-').next().unwrap_or(v);
    let mut parts = core.split('.').map(|s| s.parse::<u32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    (major, minor, patch)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, body: &str) {
        fs::write(dir.join("manifest.json"), body).expect("write manifest");
    }

    fn write_artifact(dir: &Path, name: &str, body: &[u8]) -> String {
        fs::write(dir.join(name), body).expect("write artifact");
        let mut hasher = Sha256::new();
        hasher.update(body);
        format!("sha256:{}", hex::encode(hasher.finalize()))
    }

    fn vision_manifest(dir: &Path) -> String {
        let model_digest = write_artifact(dir, "model.engine", b"engine-bytes");
        format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"yolov8n","version":"1.0.0","format_version":"0.1","model_class":"vision","backend_hint":"tensorrt","artifacts":[{{"role":"model","path":"model.engine","digest":"{model_digest}"}}]}}"#
        )
    }

    #[test]
    fn parser_returns_descriptor_for_valid_bundle() {
        let bundle = TempDir::new().expect("td");
        let body = vision_manifest(bundle.path());
        write_manifest(bundle.path(), &body);
        let d = parse_bundle(bundle.path()).expect("parse ok");
        assert_eq!(d.manifest.name, "yolov8n");
        assert!(d.manifest_digest.starts_with("sha256:"));
        assert_eq!(d.artifacts.len(), 1);
        assert_eq!(
            d.model_artifact_relative_path().as_deref(),
            Some("model.engine")
        );
    }

    #[test]
    fn rejects_missing_manifest() {
        let bundle = TempDir::new().expect("td");
        let err = parse_bundle(bundle.path()).expect_err("must reject");
        assert!(matches!(err, ParseError::ManifestMissing { .. }));
    }

    #[test]
    fn rejects_unsafe_artifact_path_absolute() {
        let bundle = TempDir::new().expect("td");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"x","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"tensorrt","artifacts":[{{"role":"model","path":"/etc/passwd","digest":"sha256:ab"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let err = parse_bundle(bundle.path()).expect_err("must reject");
        assert!(matches!(err, ParseError::ManifestSemantics(_)));
    }

    #[test]
    fn rejects_unsafe_artifact_path_parent_traversal() {
        let bundle = TempDir::new().expect("td");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"x","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"tensorrt","artifacts":[{{"role":"model","path":"../escape.bin","digest":"sha256:ab"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let err = parse_bundle(bundle.path()).expect_err("must reject");
        assert!(matches!(err, ParseError::ManifestSemantics(_)));
    }

    #[test]
    fn rejects_unsupported_format_version_major() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"x","version":"1","format_version":"99.0","model_class":"vision","backend_hint":"tensorrt","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let err = parse_bundle(bundle.path()).expect_err("must reject");
        assert!(matches!(err, ParseError::UnsupportedFormatVersion { .. }));
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"99.99","name":"x","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"tensorrt","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let err = parse_bundle(bundle.path()).expect_err("must reject");
        assert!(matches!(err, ParseError::UnsupportedSchemaVersion { .. }));
    }

    #[test]
    fn rejects_corrupted_artifact() {
        let bundle = TempDir::new().expect("td");
        let body = vision_manifest(bundle.path());
        write_manifest(bundle.path(), &body);
        fs::write(bundle.path().join("model.engine"), b"tampered").expect("write");
        let err = parse_bundle(bundle.path()).expect_err("must reject");
        assert!(matches!(err, ParseError::ArtifactDigestMismatch { .. }));
    }

    #[test]
    fn rejects_missing_artifact() {
        let bundle = TempDir::new().expect("td");
        let body = vision_manifest(bundle.path());
        write_manifest(bundle.path(), &body);
        fs::remove_file(bundle.path().join("model.engine")).expect("rm");
        let err = parse_bundle(bundle.path()).expect_err("must reject");
        assert!(matches!(err, ParseError::ArtifactMissing { .. }));
    }

    #[test]
    fn rejects_archive_input() {
        let bundle = TempDir::new().expect("td");
        let archive = bundle.path().join("foo.tpmodel");
        fs::write(&archive, b"not a real archive").expect("write");
        let err = parse_bundle(&archive).expect_err("must reject");
        assert!(matches!(
            err,
            ParseError::BundleMissing(_) | ParseError::UnsupportedArchiveFormat
        ));
    }

    #[test]
    fn canonical_digest_is_stable_across_whitespace() {
        let a = r#"{ "schema_version" : "0.1" , "name":"x"}"#;
        let b = r#"{"name":"x","schema_version":"0.1"}"#;
        let da = compute_canonical_manifest_digest(a).expect("a");
        let db = compute_canonical_manifest_digest(b).expect("b");
        assert_eq!(da, db);
    }

    #[test]
    fn compatibility_ok_for_matching_device() {
        let bundle = TempDir::new().expect("td");
        let body = vision_manifest(bundle.path());
        write_manifest(bundle.path(), &body);
        let d = parse_bundle(bundle.path()).expect("parse");

        let device = DeviceContext {
            runtime_version: Some("0.1.0".into()),
            device_family: Some(DeviceFamily::Any),
            device_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            backends: vec![BackendProfile {
                backend: "tensorrt".into(),
                capabilities: BackendCapabilityView::default(),
                supported_precision: vec!["auto".into(), "fp16".into()],
                supported_artifact_kinds: vec!["tensorrt_engine".into()],
            }],
        };
        let r = evaluate_compatibility(&d, &device);
        assert!(r.ok, "expected ok, got {r:?}");
    }

    #[test]
    fn compatibility_rejects_unavailable_backend() {
        let bundle = TempDir::new().expect("td");
        let body = vision_manifest(bundle.path());
        write_manifest(bundle.path(), &body);
        let d = parse_bundle(bundle.path()).expect("parse");
        let device = DeviceContext {
            backends: vec![BackendProfile {
                backend: "libtorch".into(),
                ..BackendProfile::default()
            }],
            ..DeviceContext::default()
        };
        let r = evaluate_compatibility(&d, &device);
        assert!(!r.ok);
        assert!(matches!(
            r.violations[0],
            CompatibilityViolation::UnavailableBackend { .. }
        ));
    }

    #[test]
    fn compatibility_rejects_new_capability_requirements() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"x","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"tensorrt","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}],"capability_requirements":{{"deterministic_latency":true,"control_loop_integration":true}}}}"#
        );
        write_manifest(bundle.path(), &body);
        let d = parse_bundle(bundle.path()).expect("parse");
        let device = DeviceContext {
            backends: vec![BackendProfile {
                backend: "tensorrt".into(),
                capabilities: BackendCapabilityView::default(),
                ..BackendProfile::default()
            }],
            ..DeviceContext::default()
        };
        let r = evaluate_compatibility(&d, &device);
        assert!(!r.ok);
        assert!(r.violations.iter().any(|v| matches!(
            v,
            CompatibilityViolation::UnsupportedCapability { capability, .. }
                if capability == "deterministic_latency"
        )));
        assert!(r.violations.iter().any(|v| matches!(
            v,
            CompatibilityViolation::UnsupportedCapability { capability, .. }
                if capability == "control_loop_integration"
        )));
    }

    #[test]
    fn compatibility_rejects_artifact_kind_mismatch() {
        let bundle = TempDir::new().expect("td");
        let body = vision_manifest(bundle.path());
        write_manifest(bundle.path(), &body);
        let d = parse_bundle(bundle.path()).expect("parse");
        let device = DeviceContext {
            backends: vec![BackendProfile {
                backend: "tensorrt".into(),
                supported_artifact_kinds: vec!["libtorch_state".into()],
                ..BackendProfile::default()
            }],
            ..DeviceContext::default()
        };
        let r = evaluate_compatibility(&d, &device);
        assert!(!r.ok);
        assert!(r
            .violations
            .iter()
            .any(|v| matches!(v, CompatibilityViolation::BackendArtifactMismatch { .. })));
    }

    #[test]
    fn compatibility_prefers_explicit_artifact_kind_over_extension() {
        let bundle = TempDir::new().expect("td");
        let digest = write_artifact(bundle.path(), "model.engine", b"x");
        let body = format!(
            r#"{{"schema_version":"{SCHEMA_VERSION}","name":"x","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"tensorrt","artifacts":[{{"role":"model","kind":"vitis_xmodel","path":"model.engine","digest":"{digest}"}}]}}"#
        );
        write_manifest(bundle.path(), &body);
        let d = parse_bundle(bundle.path()).expect("parse");
        let device = DeviceContext {
            backends: vec![BackendProfile {
                backend: "tensorrt".into(),
                supported_artifact_kinds: vec!["tensorrt_engine".into()],
                ..BackendProfile::default()
            }],
            ..DeviceContext::default()
        };
        let r = evaluate_compatibility(&d, &device);
        assert!(!r.ok);
        assert!(r.violations.iter().any(|v| matches!(
            v,
            CompatibilityViolation::BackendArtifactMismatch { artifact_kind, .. }
                if artifact_kind == "vitis_xmodel"
        )));
    }
}
