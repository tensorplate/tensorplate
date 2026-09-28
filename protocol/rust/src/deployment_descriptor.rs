// SPDX-License-Identifier: Apache-2.0
//
// Deployment descriptor: what the agent verified for one generation of one
// deployment. Mirrors `protocol/schemas/deployment_descriptor.json`; the
// digests are specified in `docs/bundles/integrity.md`.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent_control::is_valid_deployment_id;
use crate::backend_descriptor::{is_normalized_absolute, ComputeType, RunnerProfile};
use crate::bundle::{artifact_path_is_safe, BundleDescriptor};
use crate::bundle_manifest::{ArtifactKind, ArtifactRole, RECOGNIZED_BACKEND_HINTS};
use crate::bundle_profile::{
    is_sha256_digest, BundleProfile, BundleProfileError, DegradedProfile, MemoryBudgetByDomain,
    PipelineStage, SpeechContract, Warmup, PROFILE_FORMAT_VERSION,
};
use crate::canonical_json::{self, CanonicalJsonError, CANONICAL_JSON_VERSION};
use crate::member_quota::{MemberQuota, MemberQuotaError};
use crate::model_spec::{ModelClass, PrecisionHint};
use crate::resident_set::{AdmissionMode, MAX_STATE_COUNTER};
use crate::serde_shape::{deserialize_some, is_canonical_snake_identifier};
use crate::worker_control::MemberRef;
use crate::{ErrorCode, ProtocolError, SCHEMA_VERSION};

const MAX_RUNTIME_VERSION_BYTES: usize = 64;
const MAX_STAGED_PATH_CHARS: usize = 4_096;

/// Why a deployment descriptor cannot be derived or read.
#[derive(Debug, thiserror::Error)]
pub enum DeploymentDescriptorError {
    #[error("deployment descriptor has no canonical form: {0}")]
    Canonical(#[from] CanonicalJsonError),

    #[error("deployment descriptor is malformed: {0}")]
    Malformed(String),

    #[error("deployment descriptor schema_version `{0}` is not `{SCHEMA_VERSION}`")]
    UnsupportedSchemaVersion(String),

    #[error("deployment descriptor canonical_json_version {0} is not {CANONICAL_JSON_VERSION}")]
    UnsupportedCanonicalJsonVersion(String),

    #[error("deployment descriptor configuration: {0}")]
    Profile(#[from] BundleProfileError),

    #[error("deployment descriptor `{field}`: {reason}")]
    Invalid { field: &'static str, reason: String },

    #[error("runner profile `{0}` is not installed")]
    UnknownRunnerProfile(String),

    #[error("installed runner profile `{profile}` cannot load compute type `{compute_type}`")]
    UnsupportedComputeType {
        profile: String,
        compute_type: String,
    },

    /// Decoding filled a default or read a shape its serialization does
    /// not write, so the text and the value it decodes to hash differently.
    #[error("deployment descriptor is not written in its normalized form")]
    NotNormalized,

    #[error("configuration_digest is `{stated}` but the configuration hashes to `{computed}`")]
    ConfigurationDigestMismatch { stated: String, computed: String },

    #[error("descriptor_digest is `{stated}` but the descriptor hashes to `{computed}`")]
    DescriptorDigestMismatch { stated: String, computed: String },
}

/// Version refusals and an unresolvable runner are `Unsupported`, as the
/// shared decode helper reports them; every other refusal is `ConfigInvalid`.
impl From<DeploymentDescriptorError> for ProtocolError {
    fn from(value: DeploymentDescriptorError) -> Self {
        let code = match value {
            DeploymentDescriptorError::UnsupportedSchemaVersion(_)
            | DeploymentDescriptorError::UnsupportedCanonicalJsonVersion(_)
            | DeploymentDescriptorError::UnknownRunnerProfile(_)
            | DeploymentDescriptorError::UnsupportedComputeType { .. } => ErrorCode::Unsupported,
            _ => ErrorCode::ConfigInvalid,
        };
        ProtocolError::new(code, value.to_string())
    }
}

fn invalid(field: &'static str, reason: impl Into<String>) -> DeploymentDescriptorError {
    DeploymentDescriptorError::Invalid {
        field,
        reason: reason.into(),
    }
}

/// The verified bundle a configuration comes from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorBundle {
    pub name: String,
    pub version: String,
    pub format_version: String,
    /// The canonical manifest digest, which pins every manifest declaration.
    pub bundle_digest: String,
}

/// One manifest artifact; `path` resolves under the descriptor's
/// `staged_path`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorArtifact {
    pub role: ArtifactRole,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub kind: Option<ArtifactKind>,
    pub path: String,
    /// The verified digest, `sha256:` and lowercase hex.
    pub digest: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_some"
    )]
    pub byte_size: Option<u64>,
}

/// The installed runner profile a configuration resolved: its identity and
/// the packages that install it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorRunnerProfile {
    pub id: String,
    pub packages: Vec<String>,
}

/// Where the resolved runner profile is installed on this machine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerEnvironment {
    pub interpreter: String,
    pub environment_root: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub library_search_paths: Vec<String>,
}

/// Portable execution configuration: everything an equivalent restart or
/// redeployment on the same runtime reproduces, and nothing it would not.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeploymentConfiguration {
    pub bundle: DescriptorBundle,
    pub model_class: ModelClass,
    pub backend_hint: String,
    pub precision_hint: PrecisionHint,
    pub runtime_version: String,
    pub artifacts: Vec<DescriptorArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_profile: Option<DescriptorRunnerProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_type: Option<ComputeType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warmup: Option<Warmup>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pipeline_stages: Vec<PipelineStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_budget_by_domain: Option<MemoryBudgetByDomain>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_sessions: Option<u32>,
    /// Only a named degraded profile; a manifest that declares none leaves
    /// this absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speech: Option<SpeechContract>,
    /// The selected session count and quota bytes.
    pub quota: MemberQuota,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceptance_profile_digest: Option<String>,
}

impl DeploymentConfiguration {
    /// `configuration_digest`: the SHA-256 of this configuration's
    /// canonical JSON.
    ///
    /// # Errors
    ///
    /// [`DeploymentDescriptorError::Canonical`] when a value has no
    /// canonical form, such as a byte size beyond 2^53-1.
    pub fn digest(&self) -> Result<String, DeploymentDescriptorError> {
        digest_of(self)
    }

    fn has_format_0_2_fields(&self) -> bool {
        self.runner_profile.is_some()
            || self.compute_type.is_some()
            || self.warmup.is_some()
            || !self.pipeline_stages.is_empty()
            || self.memory_budget_by_domain.is_some()
            || self.max_concurrent_sessions.is_some()
            || self.degraded_profile.is_some()
            || self.speech.is_some()
    }

    fn validate(&self) -> Result<(), DeploymentDescriptorError> {
        let c = self;
        if c.bundle.name.is_empty() || c.bundle.version.is_empty() {
            return Err(invalid(
                "configuration.bundle",
                "name and version are non-empty",
            ));
        }
        if !is_format_version(&c.bundle.format_version) {
            return Err(invalid(
                "configuration.bundle.format_version",
                "must be MAJOR.MINOR",
            ));
        }
        if !is_sha256_digest(&c.bundle.bundle_digest) {
            return Err(invalid(
                "configuration.bundle.bundle_digest",
                "must be `sha256:` and 64 lowercase hex digits",
            ));
        }
        if !RECOGNIZED_BACKEND_HINTS.contains(&c.backend_hint.as_str()) {
            return Err(invalid(
                "configuration.backend_hint",
                format!("`{}` is not a recognized backend", c.backend_hint),
            ));
        }
        if !is_runtime_version(&c.runtime_version) {
            return Err(invalid(
                "configuration.runtime_version",
                format!(
                    "must be 1-{MAX_RUNTIME_VERSION_BYTES} bytes of ASCII letters, digits and `.+~-`, starting with a letter or digit"
                ),
            ));
        }
        validate_artifacts(&c.artifacts)?;
        if c.has_format_0_2_fields() && c.bundle.format_version != PROFILE_FORMAT_VERSION {
            return Err(invalid(
                "configuration",
                format!(
                    "format 0.2 fields on a format `{}` bundle",
                    c.bundle.format_version
                ),
            ));
        }
        if c.speech.is_some() && !matches!(c.model_class, ModelClass::Speech | ModelClass::Custom) {
            return Err(invalid(
                "configuration.speech",
                "a speech contract belongs to a `speech` or `custom` bundle",
            ));
        }
        if let Some(runner) = &c.runner_profile {
            validate_runner_profile(runner)?;
        }
        if let Some(warmup) = &c.warmup {
            if let Some(fixture) = warmup
                .fixtures
                .iter()
                .find(|f| !c.artifacts.iter().any(|a| &a.path == *f))
            {
                return Err(invalid(
                    "configuration.warmup.fixtures",
                    format!("`{fixture}` is not an artifact path"),
                ));
            }
        }
        c.quota
            .validate()
            .map_err(|e: MemberQuotaError| invalid("configuration.quota", e.to_string()))?;
        if let Some(digest) = &c.acceptance_profile_digest {
            if !is_sha256_digest(digest) {
                return Err(invalid(
                    "configuration.acceptance_profile_digest",
                    "must be `sha256:` and 64 lowercase hex digits",
                ));
            }
        }
        Ok(())
    }
}

/// What the agent chose for one generation, beside the verified bundle.
#[derive(Clone, Copy, Debug)]
pub struct DescriptorInputs<'a> {
    pub deployment_id: &'a str,
    pub generation: u64,
    pub admission_mode: AdmissionMode,
    /// Absolute root of this generation's staged copy of the bundle;
    /// artifact paths resolve under it.
    pub staged_path: &'a str,
    pub quota: MemberQuota,
    /// The runtime's own version, as [`crate::version`] reports it.
    pub runtime_version: &'a str,
    /// The installed backend descriptor's runner profiles.
    pub runner_profiles: &'a [RunnerProfile],
    pub acceptance_profile_digest: Option<&'a str>,
}

/// A verified deployment generation: its portable configuration and the
/// instance around it, each with its digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeploymentDescriptor {
    pub configuration: DeploymentConfiguration,
    pub configuration_digest: String,
    pub deployment_id: String,
    pub generation: u64,
    pub admission_mode: AdmissionMode,
    pub staged_path: String,
    pub runner_environment: Option<RunnerEnvironment>,
    pub descriptor_digest: String,
}

/// The serialized document; without `descriptor_digest` it is that
/// digest's input.
#[derive(Serialize)]
struct DescriptorDocument<'a> {
    schema_version: &'static str,
    canonical_json_version: u64,
    configuration: &'a DeploymentConfiguration,
    configuration_digest: &'a str,
    deployment_id: &'a str,
    generation: u64,
    admission_mode: AdmissionMode,
    staged_path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_environment: Option<&'a RunnerEnvironment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    descriptor_digest: Option<&'a str>,
}

impl Serialize for DeploymentDescriptor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.document(Some(&self.descriptor_digest))
            .serialize(serializer)
    }
}

impl DeploymentDescriptor {
    /// Derive the descriptor for one generation of a verified bundle and
    /// compute both digests.
    ///
    /// # Errors
    ///
    /// [`DeploymentDescriptorError`] when the bundle's runner profile is
    /// not installed or cannot load its compute type, or an input breaks a
    /// descriptor rule.
    pub fn derive(
        bundle: &BundleDescriptor,
        inputs: &DescriptorInputs<'_>,
    ) -> Result<Self, DeploymentDescriptorError> {
        let manifest = &bundle.manifest;
        let profile = manifest.profile.as_ref();
        let (runner_profile, runner_environment) =
            match profile.and_then(|p| p.runner_profile.as_deref()) {
                None => (None, None),
                Some(id) => {
                    let installed = inputs
                        .runner_profiles
                        .iter()
                        .find(|p| p.id == id)
                        .ok_or_else(|| {
                            DeploymentDescriptorError::UnknownRunnerProfile(id.to_owned())
                        })?;
                    if let Some(compute_type) = profile.and_then(|p| p.compute_type) {
                        if !installed.compute_types.contains(&compute_type) {
                            return Err(DeploymentDescriptorError::UnsupportedComputeType {
                                profile: id.to_owned(),
                                compute_type: compute_type_name(compute_type),
                            });
                        }
                    }
                    (
                        Some(DescriptorRunnerProfile {
                            id: installed.id.clone(),
                            packages: installed.packages.clone(),
                        }),
                        Some(RunnerEnvironment {
                            interpreter: installed.interpreter.clone(),
                            environment_root: installed.environment_root.clone(),
                            library_search_paths: installed.library_search_paths.clone(),
                        }),
                    )
                }
            };
        let configuration = DeploymentConfiguration {
            bundle: DescriptorBundle {
                name: manifest.name.clone(),
                version: manifest.version.clone(),
                format_version: manifest.format_version.clone(),
                bundle_digest: bundle.manifest_digest.to_ascii_lowercase(),
            },
            model_class: manifest.model_class,
            backend_hint: manifest.backend_hint.clone(),
            precision_hint: manifest.precision_hint,
            runtime_version: inputs.runtime_version.to_owned(),
            artifacts: manifest
                .artifacts
                .iter()
                .map(|a| DescriptorArtifact {
                    role: a.role,
                    kind: a.kind,
                    path: a.path.clone(),
                    digest: a.digest.to_ascii_lowercase(),
                    byte_size: a.byte_size,
                })
                .collect(),
            runner_profile,
            compute_type: profile.and_then(|p| p.compute_type),
            warmup: profile.and_then(|p| p.warmup.clone()),
            pipeline_stages: profile
                .map(|p| p.pipeline_stages.clone())
                .unwrap_or_default(),
            memory_budget_by_domain: profile.and_then(|p| p.memory_budget_by_domain.clone()),
            max_concurrent_sessions: profile.and_then(|p| p.max_concurrent_sessions),
            degraded_profile: match profile.and_then(|p| p.degraded_profile.as_ref()) {
                Some(DegradedProfile::Named(name)) => Some(name.clone()),
                Some(DegradedProfile::Disabled) | None => None,
            },
            speech: profile.and_then(|p| p.speech.clone()),
            quota: inputs.quota,
            acceptance_profile_digest: inputs.acceptance_profile_digest.map(str::to_owned),
        };
        let mut descriptor = Self {
            configuration,
            configuration_digest: String::new(),
            deployment_id: inputs.deployment_id.to_owned(),
            generation: inputs.generation,
            admission_mode: inputs.admission_mode,
            staged_path: inputs.staged_path.to_owned(),
            runner_environment,
            descriptor_digest: String::new(),
        };
        descriptor.validate()?;
        descriptor.configuration_digest = descriptor.configuration.digest()?;
        descriptor.descriptor_digest = descriptor.computed_descriptor_digest()?;
        Ok(descriptor)
    }

    /// Read a descriptor and verify it: canonical form, shape, the format
    /// 0.2 fields through the manifest decoder, the descriptor rules, the
    /// normalized form, then both digests.
    ///
    /// # Errors
    ///
    /// [`DeploymentDescriptorError`] naming the first check that fails.
    pub fn from_json(text: &str) -> Result<Self, DeploymentDescriptorError> {
        let canonical = canonical_json::canonicalize(text)?;
        let value: Value = serde_json::from_str(text)
            .map_err(|e| DeploymentDescriptorError::Malformed(e.to_string()))?;
        match value.get("schema_version") {
            Some(Value::String(version)) if version == SCHEMA_VERSION => {}
            other => {
                return Err(DeploymentDescriptorError::UnsupportedSchemaVersion(
                    other.map_or_else(|| "missing".to_owned(), Value::to_string),
                ))
            }
        }
        match value.get("canonical_json_version") {
            Some(Value::Number(n)) if n.as_u64() == Some(CANONICAL_JSON_VERSION) => {}
            other => {
                return Err(DeploymentDescriptorError::UnsupportedCanonicalJsonVersion(
                    other.map_or_else(|| "missing".to_owned(), Value::to_string),
                ))
            }
        }
        let wire: DescriptorWire = serde_json::from_value(value)
            .map_err(|e| DeploymentDescriptorError::Malformed(e.to_string()))?;
        let descriptor = Self {
            configuration: wire.configuration.decode()?,
            configuration_digest: wire.configuration_digest,
            deployment_id: wire.deployment_id,
            generation: wire.generation,
            admission_mode: wire.admission_mode,
            staged_path: wire.staged_path,
            runner_environment: wire.runner_environment,
            descriptor_digest: wire.descriptor_digest,
        };
        descriptor.validate()?;
        if canonical_json::canonicalize_value(&to_value(&descriptor)?)? != canonical {
            return Err(DeploymentDescriptorError::NotNormalized);
        }
        descriptor.verify_digests()?;
        Ok(descriptor)
    }

    /// The member identity the worker control channel and the sidecar's
    /// job messages carry for this generation.
    #[must_use]
    pub fn member(&self) -> MemberRef {
        MemberRef::new(self.deployment_id.clone(), self.generation)
    }

    /// Check the descriptor rules and that both digests match.
    ///
    /// # Errors
    ///
    /// [`DeploymentDescriptorError`] naming the first check that fails.
    pub fn verify(&self) -> Result<(), DeploymentDescriptorError> {
        self.validate()?;
        self.verify_digests()
    }

    fn document<'a>(&'a self, descriptor_digest: Option<&'a str>) -> DescriptorDocument<'a> {
        DescriptorDocument {
            schema_version: SCHEMA_VERSION,
            canonical_json_version: CANONICAL_JSON_VERSION,
            configuration: &self.configuration,
            configuration_digest: &self.configuration_digest,
            deployment_id: &self.deployment_id,
            generation: self.generation,
            admission_mode: self.admission_mode,
            staged_path: &self.staged_path,
            runner_environment: self.runner_environment.as_ref(),
            descriptor_digest,
        }
    }

    fn computed_descriptor_digest(&self) -> Result<String, DeploymentDescriptorError> {
        digest_of(&self.document(None))
    }

    fn verify_digests(&self) -> Result<(), DeploymentDescriptorError> {
        let computed = self.configuration.digest()?;
        if computed != self.configuration_digest {
            return Err(DeploymentDescriptorError::ConfigurationDigestMismatch {
                stated: self.configuration_digest.clone(),
                computed,
            });
        }
        let computed = self.computed_descriptor_digest()?;
        if computed != self.descriptor_digest {
            return Err(DeploymentDescriptorError::DescriptorDigestMismatch {
                stated: self.descriptor_digest.clone(),
                computed,
            });
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), DeploymentDescriptorError> {
        if !is_valid_deployment_id(&self.deployment_id) {
            return Err(invalid(
                "deployment_id",
                "must be one filesystem-safe path segment of 1-128 bytes",
            ));
        }
        if !(1..=MAX_STATE_COUNTER).contains(&self.generation) {
            return Err(invalid("generation", "must be in [1, 2^53)"));
        }
        if !is_normalized_absolute(&self.staged_path)
            || self.staged_path.chars().count() > MAX_STAGED_PATH_CHARS
        {
            return Err(invalid(
                "staged_path",
                format!(
                    "must be an absolute path with no `.` or `..` segment, of at most {MAX_STAGED_PATH_CHARS} characters"
                ),
            ));
        }
        self.configuration.validate()?;
        if self.configuration.runner_profile.is_some() != self.runner_environment.is_some() {
            return Err(invalid(
                "runner_environment",
                "is present exactly when `configuration.runner_profile` is",
            ));
        }
        if let Some(environment) = &self.runner_environment {
            validate_runner_environment(environment)?;
        }
        Ok(())
    }
}

fn validate_artifacts(artifacts: &[DescriptorArtifact]) -> Result<(), DeploymentDescriptorError> {
    const FIELD: &str = "configuration.artifacts";
    if artifacts
        .iter()
        .filter(|a| a.role == ArtifactRole::Model)
        .count()
        != 1
    {
        return Err(invalid(FIELD, "must hold exactly one `model` artifact"));
    }
    let mut paths = BTreeSet::new();
    for artifact in artifacts {
        if !artifact_path_is_safe(&artifact.path) {
            return Err(invalid(
                FIELD,
                format!(
                    "`{}` is not a relative path inside the bundle",
                    artifact.path
                ),
            ));
        }
        if !paths.insert(artifact.path.as_str()) {
            return Err(invalid(FIELD, format!("lists `{}` twice", artifact.path)));
        }
        if !is_sha256_digest(&artifact.digest) {
            return Err(invalid(
                FIELD,
                format!("`{}` has no `sha256:` lowercase digest", artifact.path),
            ));
        }
    }
    Ok(())
}

fn validate_runner_profile(
    runner: &DescriptorRunnerProfile,
) -> Result<(), DeploymentDescriptorError> {
    if !is_canonical_snake_identifier(&runner.id) {
        return Err(invalid(
            "configuration.runner_profile.id",
            "must be a lower_snake_case identifier",
        ));
    }
    let mut seen = BTreeSet::new();
    if runner.packages.is_empty()
        || runner
            .packages
            .iter()
            .any(|p| p.trim().is_empty() || !seen.insert(p.as_str()))
    {
        return Err(invalid(
            "configuration.runner_profile.packages",
            "must list unique, non-blank package names",
        ));
    }
    Ok(())
}

/// The backend descriptor's own path rules for an installed runner profile.
fn validate_runner_environment(
    environment: &RunnerEnvironment,
) -> Result<(), DeploymentDescriptorError> {
    let root = Path::new(&environment.environment_root);
    let interpreter = Path::new(&environment.interpreter);
    let mut seen = BTreeSet::new();
    let paths_ok = is_normalized_absolute(&environment.environment_root)
        && is_normalized_absolute(&environment.interpreter)
        && interpreter != root
        && interpreter.starts_with(root)
        && environment.library_search_paths.iter().all(|dir| {
            is_normalized_absolute(dir) && Path::new(dir).starts_with(root) && seen.insert(dir)
        });
    if !paths_ok {
        return Err(invalid(
            "runner_environment",
            "paths must be absolute with no `.` or `..` segment, the interpreter and library search paths inside `environment_root`, and library search paths unique",
        ));
    }
    Ok(())
}

fn is_format_version(version: &str) -> bool {
    version.split_once('.').is_some_and(|(major, minor)| {
        [major, minor]
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn is_runtime_version(version: &str) -> bool {
    version.len() <= MAX_RUNTIME_VERSION_BYTES
        && version
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'~' | b'-'))
}

fn compute_type_name(compute_type: ComputeType) -> String {
    serde_json::to_value(compute_type)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn to_value(value: &impl Serialize) -> Result<Value, DeploymentDescriptorError> {
    serde_json::to_value(value).map_err(|e| DeploymentDescriptorError::Malformed(e.to_string()))
}

fn digest_of(value: &impl Serialize) -> Result<String, DeploymentDescriptorError> {
    let canonical = canonical_json::canonicalize_value(&to_value(value)?)?;
    Ok(canonical_json::sha256_digest(&canonical))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DescriptorWire {
    #[serde(rename = "schema_version")]
    _schema_version: String,
    #[serde(rename = "canonical_json_version")]
    _canonical_json_version: u64,
    configuration: ConfigurationWire,
    configuration_digest: String,
    deployment_id: String,
    generation: u64,
    admission_mode: AdmissionMode,
    staged_path: String,
    #[serde(default, deserialize_with = "deserialize_some")]
    runner_environment: Option<RunnerEnvironment>,
    descriptor_digest: String,
}

/// The format 0.2 fields stay raw here: they are decoded by the manifest's
/// own decoder, so a descriptor can hold nothing a manifest could not.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigurationWire {
    bundle: DescriptorBundle,
    model_class: ModelClass,
    backend_hint: String,
    precision_hint: PrecisionHint,
    runtime_version: String,
    artifacts: Vec<DescriptorArtifact>,
    #[serde(default, deserialize_with = "deserialize_some")]
    runner_profile: Option<DescriptorRunnerProfile>,
    #[serde(default, deserialize_with = "deserialize_some")]
    compute_type: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_some")]
    warmup: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pipeline_stages: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_some")]
    memory_budget_by_domain: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_some")]
    max_concurrent_sessions: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_some")]
    degraded_profile: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_some")]
    speech: Option<Value>,
    quota: MemberQuota,
    #[serde(default, deserialize_with = "deserialize_some")]
    acceptance_profile_digest: Option<String>,
}

impl ConfigurationWire {
    fn decode(self) -> Result<DeploymentConfiguration, DeploymentDescriptorError> {
        let mut fields = Map::new();
        if let Some(runner) = &self.runner_profile {
            fields.insert("runner_profile".into(), Value::String(runner.id.clone()));
        }
        for (key, value) in [
            ("compute_type", self.compute_type),
            ("warmup", self.warmup),
            ("pipeline_stages", self.pipeline_stages),
            ("memory_budget_by_domain", self.memory_budget_by_domain),
            ("max_concurrent_sessions", self.max_concurrent_sessions),
            ("degraded_profile", self.degraded_profile),
        ] {
            if let Some(value) = value {
                fields.insert(key.into(), value);
            }
        }
        if let Some(speech) = self.speech {
            let mut blocks = Map::new();
            blocks.insert("speech".into(), speech);
            fields.insert("model_blocks".into(), Value::Object(blocks));
        }
        let profile = BundleProfile::from_manifest_text(&Value::Object(fields).to_string())?;
        Ok(DeploymentConfiguration {
            bundle: self.bundle,
            model_class: self.model_class,
            backend_hint: self.backend_hint,
            precision_hint: self.precision_hint,
            runtime_version: self.runtime_version,
            artifacts: self.artifacts,
            runner_profile: self.runner_profile,
            compute_type: profile.compute_type,
            warmup: profile.warmup,
            pipeline_stages: profile.pipeline_stages,
            memory_budget_by_domain: profile.memory_budget_by_domain,
            max_concurrent_sessions: profile.max_concurrent_sessions,
            degraded_profile: match profile.degraded_profile {
                Some(DegradedProfile::Named(name)) => Some(name),
                Some(DegradedProfile::Disabled) | None => None,
            },
            speech: profile.speech,
            quota: self.quota,
            acceptance_profile_digest: self.acceptance_profile_digest,
        })
    }
}
