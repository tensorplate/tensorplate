// SPDX-License-Identifier: Apache-2.0
//
// packaging: backend descriptor parser.
//
// A backend descriptor is the metadata file installed by an
// out-of-tree backend package. `tensorplate doctor` and the agent
// read it to detect backend presence, compatible TensorPlate runtime
// range, the Python interpreter (when applicable), the PyTorch
// requirement (when applicable), and the sidecar entrypoint — all
// without executing user model code.
//
// The on-disk location is
// `/usr/share/tensorplate/backends/<backend_name>/backend.json` (see
// [`crate::install_paths::BACKEND_DESCRIPTOR_DIR`]). A package that installs
// a runner profile without owning that file declares the profile in a file
// of its own under `runner_profiles.d/` beside it; [`BackendDescriptor::read_from`]
// merges those declarations into `runner_profiles`, so every reader of the
// descriptor sees the profiles that are installed and no others.
//
// The schema mirrors `protocol/schemas/backend_descriptor.json`. Both
// shall be edited together.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::package_inventory::{DpkgInventory, PackageInventory};
use crate::serde_shape::{
    deserialize_map_only, deserialize_some, deserialize_vec_map_only, is_canonical_snake_identifier,
};
use crate::SCHEMA_VERSION;

/// Directory beside a backend's `backend.json` holding one runner profile
/// declaration per installed profile package.
pub const RUNNER_PROFILE_DECLARATION_DIR: &str = "runner_profiles.d";

/// Parsed backend descriptor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackendDescriptor {
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    pub backend_name: String,
    pub package_name: String,
    pub package_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tensorplate_runtime_range: Option<RuntimeRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<PythonRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pytorch: Option<PytorchRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<EntrypointSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<SidecarSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<BackendCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
    /// Installed runner profiles: the interpreter environment each profile's
    /// sidecar runs in, apart from `python`'s. Profiles may share one
    /// environment. Empty when none is installed; `python` then describes
    /// the only interpreter the backend uses. [`Self::read_from`] appends
    /// the profiles declared under [`RUNNER_PROFILE_DECLARATION_DIR`].
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_vec_map_only"
    )]
    pub runner_profiles: Vec<RunnerProfile>,
}

fn default_schema_version() -> String {
    SCHEMA_VERSION.to_string()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeRange {
    pub min: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_exclusive: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PythonRequirements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_versions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_module: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PytorchRequirements {
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default = "default_torch_module")]
    pub import_module: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_devices: Vec<String>,
}

const fn default_true() -> bool {
    true
}

fn default_torch_module() -> String {
    "torch".to_string()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrypointKind {
    ConsoleScript,
    Module,
    Binary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EntrypointSpec {
    pub kind: EntrypointKind,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarTransport {
    UnixSocket,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarLifecycle {
    PerSession,
    PerRequest,
    Shared,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SidecarSpec {
    pub transport: SidecarTransport,
    pub lifecycle: SidecarLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervised_by: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct BackendCapabilities {
    #[serde(default, rename = "async")]
    pub async_: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub generation: bool,
    #[serde(default)]
    pub kv_cache: bool,
    #[serde(default)]
    pub fixed_shape: bool,
    #[serde(default)]
    pub deterministic_latency: bool,
    #[serde(default)]
    pub control_loop_integration: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_precision: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_artifact_kinds: Vec<String>,
}

/// Compute types a runner profile can load a model with. `auto` is
/// deliberately absent: a profile states what it supports, and selection
/// never substitutes one silently.
///
/// `try_from` pins decoding to the plain string form, as for
/// [`crate::platform_memory_profile::PlatformMemoryProfileName`]: the
/// derived `Deserialize` would also accept the map form
/// (`{"float16": null}`), which the schema refuses.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum ComputeType {
    Float32,
    Float16,
    Bfloat16,
    Int8,
    Int8Float32,
    Int8Float16,
    Int8Bfloat16,
    Int16,
}

impl TryFrom<String> for ComputeType {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "float32" => Ok(Self::Float32),
            "float16" => Ok(Self::Float16),
            "bfloat16" => Ok(Self::Bfloat16),
            "int8" => Ok(Self::Int8),
            "int8_float32" => Ok(Self::Int8Float32),
            "int8_float16" => Ok(Self::Int8Float16),
            "int8_bfloat16" => Ok(Self::Int8Bfloat16),
            "int16" => Ok(Self::Int16),
            other => Err(format!("unknown compute type `{other}`")),
        }
    }
}

/// One installed runner profile: the environment its sidecar runs in and
/// the packages that install it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerProfile {
    /// Profile identity a deployment selects, lower_snake_case.
    pub id: String,
    /// Absolute interpreter path inside `environment_root`.
    pub interpreter: String,
    /// Absolute root of the profile's installed environment.
    pub environment_root: String,
    /// Absolute directories inside `environment_root` for the sidecar's
    /// shared-library search path, for libraries loaded by name at run time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub library_search_paths: Vec<String>,
    /// OS packages that install this profile.
    pub packages: Vec<String>,
    /// Compute types this installed profile can load a model with.
    pub compute_types: Vec<ComputeType>,
}

/// One runner profile declared by the package that installs it: a file
/// under [`RUNNER_PROFILE_DECLARATION_DIR`]. Mirrors the schema's
/// `definitions.runner_profile_declaration`. It does not implement
/// `Deserialize`: [`Self::parse_with_path`] is the only way to one, so no
/// loader skips its checks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunnerProfileDeclaration {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub schema_version: String,
    /// `backend_name` of the descriptor the profile belongs to.
    pub backend_name: String,
    pub runner_profile: RunnerProfile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerProfileDeclarationWire {
    #[serde(rename = "$schema", default, deserialize_with = "deserialize_some")]
    schema: Option<String>,
    schema_version: String,
    backend_name: String,
    #[serde(deserialize_with = "deserialize_map_only")]
    runner_profile: RunnerProfile,
}

/// Typed errors raised when reading a backend descriptor or one of its
/// runner profile declarations. `path` names the file at fault.
#[derive(Debug, thiserror::Error)]
pub enum BackendDescriptorError {
    #[error("backend descriptor file `{path}` does not exist")]
    Missing { path: String },

    #[error("backend descriptor file `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("backend descriptor `{path}`: invalid JSON ({source})")]
    Malformed {
        path: String,
        #[source]
        source: serde_json::Error,
    },

    #[error(
        "backend descriptor `{path}`: unsupported schema_version `{got}` (expected `{expected}`)"
    )]
    UnsupportedSchemaVersion {
        path: String,
        got: String,
        expected: &'static str,
    },

    #[error("backend descriptor `{path}`: {message}")]
    Invalid { path: String, message: String },

    #[error(
        "backend descriptor `{second}`: runner profile `{id}` is already declared by `{first}`"
    )]
    DuplicateRunnerProfile {
        id: String,
        first: String,
        second: String,
    },

    #[error("backend descriptor `{path}`: runner profile `{profile}` names package `{package}`, which is not installed")]
    PackageNotInstalled {
        path: String,
        profile: String,
        package: String,
    },

    #[error("backend descriptor `{path}`: cannot tell whether the packages of its runner profiles are installed: {detail}")]
    PackageInventoryUnavailable { path: String, detail: String },
}

impl BackendDescriptor {
    /// Read the descriptor at `path` as installed: parsed, with the runner
    /// profile declarations beside it merged in and every package a runner
    /// profile names checked against dpkg. Returns a typed error
    /// distinguishing "file is missing" from "file is malformed" so
    /// callers (doctor probes, deploy compatibility) can surface
    /// actionable findings without ambiguity.
    ///
    /// # Errors
    ///
    /// Returns [`BackendDescriptorError`] for a missing, malformed,
    /// version-mismatched, or semantically invalid descriptor, and for
    /// each refusal [`Self::read_with_inventory`] lists.
    pub fn read_from(path: &Path) -> Result<Self, BackendDescriptorError> {
        Self::read_with_inventory(path, &DpkgInventory::default())
    }

    /// [`Self::read_from`] with the installed packages taken from
    /// `inventory`, which is asked only when a runner profile exists.
    ///
    /// Declarations are the `*.json` files of
    /// [`RUNNER_PROFILE_DECLARATION_DIR`] beside `path`, merged in file
    /// name order after the descriptor's own profiles. Other entries of
    /// that directory are not declarations and are not read; a missing
    /// directory declares nothing.
    ///
    /// # Errors
    ///
    /// The whole descriptor is refused, never a single profile: when a
    /// declaration cannot be read, is malformed or names another backend;
    /// when two sources declare one profile id; when a profile names a
    /// package that is not installed; and when `inventory` cannot answer.
    pub fn read_with_inventory(
        path: &Path,
        inventory: &dyn PackageInventory,
    ) -> Result<Self, BackendDescriptorError> {
        let mut descriptor = Self::parse_with_path(&read_text(path, true)?, path)?;
        let mut sources: BTreeMap<String, PathBuf> = descriptor
            .runner_profiles
            .iter()
            .map(|profile| (profile.id.clone(), path.to_path_buf()))
            .collect();
        let directory = path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(RUNNER_PROFILE_DECLARATION_DIR);
        for file in declaration_files(&directory)? {
            let declaration =
                RunnerProfileDeclaration::parse_with_path(&read_text(&file, false)?, &file)?;
            if declaration.backend_name != descriptor.backend_name {
                return Err(BackendDescriptorError::Invalid {
                    path: file.display().to_string(),
                    message: format!(
                        "declares a runner profile of backend `{}` beside the descriptor of `{}`",
                        declaration.backend_name, descriptor.backend_name
                    ),
                });
            }
            let id = declaration.runner_profile.id.clone();
            if let Some(first) = sources.get(&id) {
                return Err(BackendDescriptorError::DuplicateRunnerProfile {
                    id,
                    first: first.display().to_string(),
                    second: file.display().to_string(),
                });
            }
            sources.insert(id, file);
            descriptor.runner_profiles.push(declaration.runner_profile);
        }
        descriptor.require_installed_packages(path, &sources, inventory)?;
        Ok(descriptor)
    }

    fn require_installed_packages(
        &self,
        path: &Path,
        sources: &BTreeMap<String, PathBuf>,
        inventory: &dyn PackageInventory,
    ) -> Result<(), BackendDescriptorError> {
        let names: BTreeSet<&str> = self
            .runner_profiles
            .iter()
            .flat_map(|profile| profile.packages.iter().map(String::as_str))
            .collect();
        if names.is_empty() {
            return Ok(());
        }
        let installed = inventory.installed(&names).map_err(|detail| {
            BackendDescriptorError::PackageInventoryUnavailable {
                path: path.display().to_string(),
                detail,
            }
        })?;
        for profile in &self.runner_profiles {
            if let Some(package) = profile.packages.iter().find(|p| !installed.contains(*p)) {
                return Err(BackendDescriptorError::PackageNotInstalled {
                    path: sources
                        .get(&profile.id)
                        .map_or(path, PathBuf::as_path)
                        .display()
                        .to_string(),
                    profile: profile.id.clone(),
                    package: package.clone(),
                });
            }
        }
        Ok(())
    }

    /// Parse a JSON descriptor from a string. `path_for_diagnostics`
    /// is used to enrich error messages — pass the canonical install
    /// location even when reading from a test fixture so the operator
    /// sees the path they would inspect.
    ///
    /// # Errors
    ///
    /// Returns [`BackendDescriptorError`] when the JSON is malformed,
    /// the schema version is unsupported, or required fields are
    /// missing.
    pub fn parse_with_path(
        text: &str,
        path_for_diagnostics: &Path,
    ) -> Result<Self, BackendDescriptorError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|source| BackendDescriptorError::Malformed {
                path: path_for_diagnostics.display().to_string(),
                source,
            })?;
        let observed = value
            .get("schema_version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(SCHEMA_VERSION);
        if observed != SCHEMA_VERSION {
            return Err(BackendDescriptorError::UnsupportedSchemaVersion {
                path: path_for_diagnostics.display().to_string(),
                got: observed.to_string(),
                expected: SCHEMA_VERSION,
            });
        }
        let parsed: Self =
            serde_json::from_value(value).map_err(|source| BackendDescriptorError::Malformed {
                path: path_for_diagnostics.display().to_string(),
                source,
            })?;
        parsed.validate(path_for_diagnostics)
    }

    fn validate(self, path: &Path) -> Result<Self, BackendDescriptorError> {
        let invalid = |msg: &str| BackendDescriptorError::Invalid {
            path: path.display().to_string(),
            message: msg.into(),
        };
        if self.backend_name.trim().is_empty() {
            return Err(invalid("`backend_name` must be non-empty"));
        }
        if self.package_name.trim().is_empty() {
            return Err(invalid("`package_name` must be non-empty"));
        }
        if self.package_version.trim().is_empty() {
            return Err(invalid("`package_version` must be non-empty"));
        }
        if let Some(range) = self.tensorplate_runtime_range.as_ref() {
            if range.min.trim().is_empty() {
                return Err(invalid("`tensorplate_runtime_range.min` must be non-empty"));
            }
        }
        if let Some(py) = self.python.as_ref() {
            if let Some(interpreter) = py.interpreter.as_deref() {
                if !Path::new(interpreter).is_absolute() {
                    return Err(invalid("`python.interpreter` must be an absolute path"));
                }
            }
        }
        let mut ids = BTreeSet::new();
        for profile in &self.runner_profiles {
            if let Err(message) = profile.check() {
                return Err(invalid(&message));
            }
            if !ids.insert(profile.id.as_str()) {
                return Err(invalid(&format!(
                    "runner profile `{}` is declared more than once",
                    profile.id
                )));
            }
        }
        Ok(self)
    }

    /// The installed runner profile with this id, if the descriptor
    /// declares one.
    #[must_use]
    pub fn runner_profile(&self, id: &str) -> Option<&RunnerProfile> {
        self.runner_profiles.iter().find(|p| p.id == id)
    }

    /// Convenience: does this descriptor declare a Python sidecar?
    #[must_use]
    pub fn is_python_sidecar(&self) -> bool {
        self.python.is_some()
            && self
                .sidecar
                .as_ref()
                .is_some_and(|s| matches!(s.transport, SidecarTransport::UnixSocket))
    }

    /// Convenience: does this descriptor declare PyTorch as a required
    /// runtime dependency? Returns `false` when the `pytorch` section
    /// is missing or `required: false`.
    #[must_use]
    pub fn requires_pytorch(&self) -> bool {
        self.pytorch.as_ref().is_some_and(|p| p.required)
    }
}

impl RunnerProfileDeclaration {
    /// Parse one declaration. `path_for_diagnostics` names the file in
    /// errors, as for [`BackendDescriptor::parse_with_path`].
    ///
    /// # Errors
    ///
    /// Returns [`BackendDescriptorError`] when the JSON is malformed, the
    /// schema version is missing or unsupported, or the profile breaks a
    /// runner profile rule.
    pub fn parse_with_path(
        text: &str,
        path_for_diagnostics: &Path,
    ) -> Result<Self, BackendDescriptorError> {
        let path = || path_for_diagnostics.display().to_string();
        let malformed = |source| BackendDescriptorError::Malformed {
            path: path(),
            source,
        };
        let value: serde_json::Value = serde_json::from_str(text).map_err(malformed)?;
        if let Some(observed) = value.get("schema_version").and_then(|v| v.as_str()) {
            if observed != SCHEMA_VERSION {
                return Err(BackendDescriptorError::UnsupportedSchemaVersion {
                    path: path(),
                    got: observed.to_string(),
                    expected: SCHEMA_VERSION,
                });
            }
        }
        let wire: RunnerProfileDeclarationWire = deserialize_map_only(value).map_err(malformed)?;
        let parsed = Self {
            schema: wire.schema,
            schema_version: wire.schema_version,
            backend_name: wire.backend_name,
            runner_profile: wire.runner_profile,
        };
        let invalid = |message: String| BackendDescriptorError::Invalid {
            path: path(),
            message,
        };
        if parsed.backend_name.trim().is_empty() {
            return Err(invalid("`backend_name` must be non-empty".into()));
        }
        parsed.runner_profile.check().map_err(invalid)?;
        Ok(parsed)
    }
}

/// The text of `path`. A descriptor that is absent is `Missing`; a
/// declaration that vanished after it was listed is an I/O refusal.
fn read_text(path: &Path, absent_is_missing: bool) -> Result<String, BackendDescriptorError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if absent_is_missing && e.kind() == std::io::ErrorKind::NotFound => {
            Err(BackendDescriptorError::Missing {
                path: path.display().to_string(),
            })
        }
        Err(source) => Err(BackendDescriptorError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// The `*.json` entries of `directory` in file name order; none when the
/// directory does not exist.
fn declaration_files(directory: &Path) -> Result<Vec<PathBuf>, BackendDescriptorError> {
    let io = |source| BackendDescriptorError::Io {
        path: directory.display().to_string(),
        source,
    };
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io(source)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let file = entry.map_err(io)?.path();
        if file
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            files.push(file);
        }
    }
    files.sort();
    Ok(files)
}

impl RunnerProfile {
    /// The rules the schema cannot state: paths carry no `.` or `..`
    /// segment, the interpreter and every library search path sit inside
    /// the environment root, no path list repeats an entry, and no package
    /// name is blank.
    fn check(&self) -> Result<(), String> {
        let id = &self.id;
        if !is_canonical_snake_identifier(id) {
            return Err(format!("runner profile id `{id}` must be lower_snake_case"));
        }
        if !is_normalized_absolute(&self.environment_root) {
            return Err(format!(
                "runner profile `{id}`: `environment_root` must be an absolute path with no `.` or `..` components"
            ));
        }
        let root = Path::new(&self.environment_root);
        if !is_normalized_absolute(&self.interpreter) {
            return Err(format!(
                "runner profile `{id}`: `interpreter` must be an absolute path with no `.` or `..` components"
            ));
        }
        let interpreter = Path::new(&self.interpreter);
        if interpreter == root || !interpreter.starts_with(root) {
            return Err(format!(
                "runner profile `{id}`: `interpreter` must sit inside `environment_root`"
            ));
        }
        let mut seen = BTreeSet::new();
        for dir in &self.library_search_paths {
            if !is_normalized_absolute(dir) {
                return Err(format!(
                    "runner profile `{id}`: each `library_search_paths` entry must be an absolute path with no `.` or `..` components"
                ));
            }
            if !Path::new(dir).starts_with(root) {
                return Err(format!(
                    "runner profile `{id}`: each `library_search_paths` entry must sit inside `environment_root`"
                ));
            }
            if !seen.insert(Path::new(dir)) {
                return Err(format!(
                    "runner profile `{id}`: `library_search_paths` repeats `{dir}`"
                ));
            }
        }
        if self.packages.is_empty() || self.packages.iter().any(|p| p.trim().is_empty()) {
            return Err(format!(
                "runner profile `{id}` must name at least one non-empty package"
            ));
        }
        if self.packages.iter().collect::<BTreeSet<_>>().len() != self.packages.len() {
            return Err(format!(
                "runner profile `{id}`: `packages` repeats an entry"
            ));
        }
        if self.compute_types.is_empty() {
            return Err(format!(
                "runner profile `{id}` must declare at least one compute type"
            ));
        }
        if self.compute_types.iter().collect::<BTreeSet<_>>().len() != self.compute_types.len() {
            return Err(format!(
                "runner profile `{id}`: `compute_types` repeats an entry"
            ));
        }
        Ok(())
    }
}

/// Absolute, with no `.` or `..` segment, which would let a path name
/// somewhere its text does not show. Checked on the text: `Path::components`
/// drops an interior `.` silently.
pub(crate) fn is_normalized_absolute(path: &str) -> bool {
    Path::new(path).is_absolute()
        && path
            .split('/')
            .all(|segment| segment != "." && segment != "..")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use std::path::PathBuf;

    fn minimal_json() -> String {
        format!(
            r#"{{
                "schema_version": "{SCHEMA_VERSION}",
                "backend_name": "python_pytorch",
                "package_name": "tensorplate-backend-python-pytorch",
                "package_version": "0.1.0"
            }}"#,
        )
    }

    fn fake_path() -> PathBuf {
        PathBuf::from("/usr/share/tensorplate/backends/python_pytorch/backend.json")
    }

    #[test]
    fn parses_minimal_descriptor() {
        let d = BackendDescriptor::parse_with_path(&minimal_json(), &fake_path()).expect("parses");
        assert_eq!(d.backend_name, "python_pytorch");
        assert!(!d.is_python_sidecar());
        assert!(!d.requires_pytorch());
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let raw = r#"{
            "schema_version": "99.99",
            "backend_name": "python_pytorch",
            "package_name": "x",
            "package_version": "0.1.0"
        }"#;
        let err = BackendDescriptor::parse_with_path(raw, &fake_path()).unwrap_err();
        assert!(matches!(
            err,
            BackendDescriptorError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn rejects_empty_required_fields() {
        let raw = format!(
            r#"{{
                "schema_version": "{SCHEMA_VERSION}",
                "backend_name": "",
                "package_name": "x",
                "package_version": "0.1.0"
            }}"#,
        );
        let err = BackendDescriptor::parse_with_path(&raw, &fake_path()).unwrap_err();
        match err {
            BackendDescriptorError::Invalid { message, .. } => {
                assert!(message.contains("backend_name"));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_relative_interpreter() {
        let raw = format!(
            r#"{{
                "schema_version": "{SCHEMA_VERSION}",
                "backend_name": "python_pytorch",
                "package_name": "x",
                "package_version": "0.1.0",
                "python": {{
                    "interpreter": "python3"
                }}
            }}"#,
        );
        let err = BackendDescriptor::parse_with_path(&raw, &fake_path()).unwrap_err();
        match err {
            BackendDescriptorError::Invalid { message, .. } => {
                assert!(message.contains("python.interpreter"));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn reports_missing_file() {
        let p = PathBuf::from("/nonexistent/tensorplate/backend.json");
        let err = BackendDescriptor::read_from(&p).unwrap_err();
        assert!(matches!(err, BackendDescriptorError::Missing { .. }));
    }

    #[test]
    fn full_python_pytorch_descriptor_round_trips() {
        let json = format!(
            r#"{{
                "schema_version": "{SCHEMA_VERSION}",
                "backend_name": "python_pytorch",
                "package_name": "tensorplate-backend-python-pytorch",
                "package_version": "0.1.0",
                "tensorplate_runtime_range": {{
                    "min": "0.1.0",
                    "max_exclusive": "0.2.0"
                }},
                "python": {{
                    "interpreter": "/usr/bin/python3",
                    "minimum_version": "3.10",
                    "supported_versions": ["3.10", "3.11", "3.12"],
                    "import_module": "tensorplate_pytorch_backend"
                }},
                "pytorch": {{
                    "required": true,
                    "import_module": "torch",
                    "minimum_version": "2.1",
                    "supported_devices": ["cpu", "cuda"]
                }},
                "entrypoint": {{
                    "kind": "console_script",
                    "command": "tensorplate-backend-python-pytorch"
                }},
                "sidecar": {{
                    "transport": "unix_socket",
                    "lifecycle": "per_session",
                    "supervised_by": "tensorplate-serving"
                }}
            }}"#,
        );
        let d = BackendDescriptor::parse_with_path(&json, &fake_path()).expect("parses");
        assert!(d.is_python_sidecar());
        assert!(d.requires_pytorch());
        let py = d.python.unwrap();
        assert_eq!(py.interpreter.as_deref(), Some("/usr/bin/python3"));
    }

    #[test]
    fn shipped_python_pytorch_descriptor_parses() {
        // Smoke: ship-config in repo parses identically.
        let repo_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("packaging")
            .join("backend-metadata")
            .join("python_pytorch.json");
        let raw = std::fs::read_to_string(&repo_path).expect("read repo descriptor");
        let d = BackendDescriptor::parse_with_path(&raw, &repo_path).expect("parses");
        assert_eq!(d.backend_name, "python_pytorch");
        assert!(d.requires_pytorch());
    }
}
