// SPDX-License-Identifier: Apache-2.0
//
// V01-E14-F05: backend descriptor parser.
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
// [`crate::install_paths::BACKEND_DESCRIPTOR_DIR`]).
//
// The schema mirrors `protocol/schemas/backend_descriptor.json`. Both
// shall be edited together.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::SCHEMA_VERSION;

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

/// Typed errors raised when parsing a backend descriptor.
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
}

impl BackendDescriptor {
    /// Read and parse the descriptor at `path`. Returns a typed error
    /// distinguishing "file is missing" from "file is malformed" so
    /// callers (doctor probes, deploy compatibility) can surface
    /// actionable findings without ambiguity.
    ///
    /// # Errors
    ///
    /// Returns [`BackendDescriptorError`] for missing, malformed,
    /// version-mismatched, or semantically invalid descriptors.
    pub fn read_from(path: &Path) -> Result<Self, BackendDescriptorError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BackendDescriptorError::Missing {
                    path: path.display().to_string(),
                });
            }
            Err(source) => {
                return Err(BackendDescriptorError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        Self::parse_with_path(&raw, path)
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
    pub fn parse_with_path(text: &str, path_for_diagnostics: &Path) -> Result<Self, BackendDescriptorError> {
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
        let parsed: Self = serde_json::from_value(value).map_err(|source| {
            BackendDescriptorError::Malformed {
                path: path_for_diagnostics.display().to_string(),
                source,
            }
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
        Ok(self)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use std::path::PathBuf;

    fn minimal_json() -> String {
        format!(
            r#"{{
                "schema_version": "{}",
                "backend_name": "python_pytorch",
                "package_name": "tensorplate-backend-python-pytorch",
                "package_version": "0.1.0"
            }}"#,
            SCHEMA_VERSION
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
                "schema_version": "{}",
                "backend_name": "",
                "package_name": "x",
                "package_version": "0.1.0"
            }}"#,
            SCHEMA_VERSION
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
                "schema_version": "{}",
                "backend_name": "python_pytorch",
                "package_name": "x",
                "package_version": "0.1.0",
                "python": {{
                    "interpreter": "python3"
                }}
            }}"#,
            SCHEMA_VERSION
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
                "schema_version": "{}",
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
            SCHEMA_VERSION
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
