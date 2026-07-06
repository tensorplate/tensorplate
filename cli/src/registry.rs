// SPDX-License-Identifier: Apache-2.0
//
// Local SSH device registry for the `tensorplate` CLI.
//
// The registry is a small, human-inspectable JSON file that remembers the
// SSH-reachable devices an operator has enrolled, plus which one is the
// default. It resolves its own path independently of the CLI config
// (`$TENSORPLATE_DEVICE_REGISTRY` override, else an XDG default) because the
// CLI config names a single file and has no default directory to sit beside.
//
// The registry stores only access metadata (SSH target, optional port,
// optional run-as user, cached device facts). It never stores SSH keys,
// passwords, or agent secrets. Writes go through a temp-file + rename so a
// crash mid-write leaves the previous registry intact. Readers always see a
// complete file; concurrent writes from separate processes are last-writer-
// wins, matching the single-operator assumption of a plain JSON registry
// (richer multi-writer concurrency would motivate moving off JSON).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CliError, CliResult};

/// Registry schema version. Independent track from the wire protocol and
/// the CLI config; unknown versions are rejected with a typed error.
pub const DEVICE_REGISTRY_SCHEMA_VERSION: &str = "0.1";

/// Environment override naming an explicit registry file path. Mirrors the
/// `$TENSORPLATE_CLI_CONFIG` style used for `cli.json`. Tests and CI redirect
/// the registry through this variable.
pub const DEVICE_REGISTRY_ENV: &str = "TENSORPLATE_DEVICE_REGISTRY";

/// Default remote staging directory for copied bundles. Enrollment records
/// this on every entry so the registry identity carries the import dir the
/// remote deploy path needs; `device add --import-dir` overrides it for
/// installs whose packaged service permissions place it elsewhere.
pub const DEFAULT_REMOTE_IMPORT_DIR: &str = "/var/lib/tensorplate/bundles/import";

/// A single enrolled device.
///
/// The cached fact fields (`last_seen`, `agent_version`, `protocol_version`,
/// `device_family`, `available_backends`) are populated by a later
/// remote-sync path; enrollment leaves them unset.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceEntry {
    /// SSH destination: `user@host` or a bare `~/.ssh/config` host alias.
    pub ssh_target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
    /// Absolute path to the remote `tensorplate` binary. Recorded when the
    /// operator pins it; required to be absolute and root-owned before it is
    /// used for a run-as invocation (enforced by the later remote adapter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_tensorplate: Option<PathBuf>,
    /// Non-interactive run-as user for reaching the device-local agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_run_as: Option<String>,
    /// Remote staging directory for copied bundles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_import_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_backends: Option<Vec<String>>,
}

/// The on-disk device registry document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceRegistry {
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_device: Option<String>,
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceEntry>,
}

fn default_schema_version() -> String {
    DEVICE_REGISTRY_SCHEMA_VERSION.to_string()
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self {
            schema_version: DEVICE_REGISTRY_SCHEMA_VERSION.to_string(),
            default_device: None,
            devices: BTreeMap::new(),
        }
    }
}

impl DeviceRegistry {
    /// Validate structural invariants. A missing registry is valid (it just
    /// means "no devices enrolled"); an existing one must have a known schema
    /// version, well-formed device names and SSH targets, and a default that
    /// points at a real entry.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Config`] for an unknown schema version, malformed
    /// device names, empty or unsafe SSH targets, unsafe run-as users,
    /// relative remote paths, a zero SSH port, or a dangling default device.
    pub fn validate(&self) -> CliResult<()> {
        if self.schema_version != DEVICE_REGISTRY_SCHEMA_VERSION {
            return Err(CliError::Config(format!(
                "unsupported device registry schema_version `{}` (expected `{}`)",
                self.schema_version, DEVICE_REGISTRY_SCHEMA_VERSION
            )));
        }
        for (name, entry) in &self.devices {
            if !is_valid_device_name(name) {
                return Err(CliError::Config(format!(
                    "device name `{name}` must be non-empty and use only letters, digits, `-`, `_`, or `.`"
                )));
            }
            validate_entry(name, entry)?;
        }
        if let Some(default) = &self.default_device {
            if !self.devices.contains_key(default) {
                return Err(CliError::Config(format!(
                    "default_device `{default}` is not an enrolled device"
                )));
            }
        }
        Ok(())
    }

    /// Parse a registry document and validate it.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Config`] if the document is malformed or fails
    /// [`Self::validate`].
    pub fn parse_json(text: &str) -> CliResult<Self> {
        let registry: Self = serde_json::from_str(text)
            .map_err(|e| CliError::Config(format!("device registry is not valid JSON: {e}")))?;
        registry.validate()?;
        Ok(registry)
    }

    /// Load the registry at `path`. A missing or empty file yields an empty
    /// registry so local commands keep working before any device is enrolled.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Config`] if the file exists but cannot be read, is
    /// malformed, or fails validation.
    pub fn load(path: &Path) -> CliResult<Self> {
        match fs::read_to_string(path) {
            Ok(body) if body.trim().is_empty() => Ok(Self::default()),
            Ok(body) => Self::parse_json(&body),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(CliError::Config(format!(
                "failed to read device registry `{}`: {e}",
                path.display()
            ))),
        }
    }

    /// Persist the registry to `path` atomically (temp file + rename), creating
    /// the parent directory if needed.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Io`] if the directory or file cannot be written.
    pub fn save(&self, path: &Path) -> CliResult<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| {
                    CliError::Io(format!(
                        "failed to create device registry directory `{}`: {e}",
                        parent.display()
                    ))
                })?;
            }
        }
        let encoded = serde_json::to_vec_pretty(self)?;
        let tmp = tmp_path(path);
        {
            let mut file: File = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|e| {
                    CliError::Io(format!(
                        "failed to open temp registry `{}`: {e}",
                        tmp.display()
                    ))
                })?;
            file.write_all(&encoded)
                .map_err(|e| CliError::Io(format!("failed to write registry: {e}")))?;
            file.sync_all()
                .map_err(|e| CliError::Io(format!("failed to flush registry: {e}")))?;
        }
        fs::rename(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            CliError::Io(format!(
                "failed to commit device registry `{}`: {e}",
                path.display()
            ))
        })?;
        // Best-effort directory fsync so the rename survives power loss where
        // the OS supports it.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Ok(dir) = File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
        }
        Ok(())
    }

    /// Resolve the registry path: `$TENSORPLATE_DEVICE_REGISTRY` if set, else
    /// `$XDG_CONFIG_HOME/tensorplate/devices.json`, else
    /// `$HOME/.config/tensorplate/devices.json`.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Config`] when neither the override nor a home
    /// directory can be determined.
    pub fn resolve_path() -> CliResult<PathBuf> {
        let env_override = std::env::var(DEVICE_REGISTRY_ENV).ok();
        let xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let home = std::env::var("HOME").ok();
        resolve_path_from(
            non_empty(env_override.as_deref()),
            non_empty(xdg.as_deref()),
            non_empty(home.as_deref()),
        )
    }
}

/// Pure path-resolution logic, kept separate from environment access so the
/// precedence ladder is unit-testable without mutating process env.
fn resolve_path_from(
    env_override: Option<&str>,
    xdg: Option<&str>,
    home: Option<&str>,
) -> CliResult<PathBuf> {
    if let Some(explicit) = env_override {
        return Ok(PathBuf::from(explicit));
    }
    if let Some(dir) = xdg {
        return Ok(PathBuf::from(dir).join("tensorplate").join("devices.json"));
    }
    if let Some(dir) = home {
        return Ok(PathBuf::from(dir)
            .join(".config")
            .join("tensorplate")
            .join("devices.json"));
    }
    Err(CliError::Config(format!(
        "cannot resolve device registry path: set ${DEVICE_REGISTRY_ENV} or $HOME"
    )))
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|s| !s.is_empty())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    PathBuf::from(name)
}

fn validate_entry(name: &str, entry: &DeviceEntry) -> CliResult<()> {
    if !is_valid_ssh_target(&entry.ssh_target) {
        return Err(CliError::Config(format!(
            "device `{name}`: ssh_target `{}` must be a `user@host` or host alias with no whitespace and no leading `-`",
            entry.ssh_target
        )));
    }
    if entry.ssh_port == Some(0) {
        return Err(CliError::Config(format!(
            "device `{name}`: ssh_port must be > 0"
        )));
    }
    if let Some(user) = entry.remote_run_as.as_deref() {
        if !is_valid_username(user) {
            return Err(CliError::Config(format!(
                "device `{name}`: remote_run_as `{user}` must be a plain username (letters, digits, `-`, `_`, `.`)"
            )));
        }
    }
    if let Some(bin) = entry.remote_tensorplate.as_deref() {
        if !bin.is_absolute() {
            return Err(CliError::Config(format!(
                "device `{name}`: remote_tensorplate `{}` must be an absolute path",
                bin.display()
            )));
        }
    }
    if let Some(dir) = entry.remote_import_dir.as_deref() {
        if !dir.is_absolute() {
            return Err(CliError::Config(format!(
                "device `{name}`: remote_import_dir `{}` must be an absolute path",
                dir.display()
            )));
        }
    }
    Ok(())
}

// A leading `-` is rejected everywhere: these values are later passed as
// structured argv entries to `ssh`, `sudo`, and the CLI itself, and a value
// that begins with a dash would be mis-parsed as an option (argv injection)
// even though no shell is involved.

fn is_valid_device_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn is_valid_ssh_target(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with('-')
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | ':'))
}

fn is_valid_username(user: &str) -> bool {
    !user.is_empty()
        && !user.starts_with('-')
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::default_trait_access,
        clippy::needless_pass_by_value,
        clippy::semicolon_if_nothing_returned,
        clippy::field_reassign_with_default,
        clippy::large_enum_variant,
        clippy::no_effect_underscore_binding,
        clippy::redundant_clone,
        clippy::redundant_closure_for_method_calls
    )]

    use super::*;

    fn entry(target: &str) -> DeviceEntry {
        DeviceEntry {
            ssh_target: target.to_string(),
            ..DeviceEntry::default()
        }
    }

    #[test]
    fn default_registry_is_empty_and_valid() {
        let reg = DeviceRegistry::default();
        assert_eq!(reg.schema_version, DEVICE_REGISTRY_SCHEMA_VERSION);
        assert!(reg.default_device.is_none());
        assert!(reg.devices.is_empty());
        reg.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let raw = r#"{"schema_version":"99.0","devices":{}}"#;
        let err = DeviceRegistry::parse_json(raw).unwrap_err();
        assert!(matches!(err, CliError::Config(_)));
    }

    #[test]
    fn parses_documented_shape() {
        let raw = r#"{
            "schema_version":"0.1",
            "default_device":"orin-lab",
            "devices":{
                "orin-lab":{
                    "ssh_target":"reid@orin-lab.local",
                    "remote_run_as":"tensorplate",
                    "remote_import_dir":"/var/lib/tensorplate/bundles/import",
                    "agent_version":"0.1.5",
                    "available_backends":["tensorrt","python_pytorch"]
                }
            }
        }"#;
        let reg = DeviceRegistry::parse_json(raw).unwrap();
        assert_eq!(reg.default_device.as_deref(), Some("orin-lab"));
        let d = reg.devices.get("orin-lab").unwrap();
        assert_eq!(d.ssh_target, "reid@orin-lab.local");
        assert_eq!(d.remote_run_as.as_deref(), Some("tensorplate"));
    }

    #[test]
    fn rejects_bad_device_name() {
        let mut reg = DeviceRegistry::default();
        reg.devices.insert("bad name".into(), entry("host"));
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));
    }

    #[test]
    fn rejects_leading_dash_to_prevent_argv_injection() {
        // A leading dash would be parsed as an option by ssh/sudo/the CLI.
        let mut reg = DeviceRegistry::default();
        reg.devices
            .insert("d".into(), entry("-oProxyCommand=touch pwned"));
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));

        let mut reg = DeviceRegistry::default();
        let mut e = entry("host");
        e.remote_run_as = Some("-u".into());
        reg.devices.insert("d".into(), e);
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));

        let mut reg = DeviceRegistry::default();
        reg.devices.insert("-flag".into(), entry("host"));
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));
    }

    #[test]
    fn rejects_empty_or_unsafe_ssh_target() {
        let mut reg = DeviceRegistry::default();
        reg.devices.insert("d".into(), entry(""));
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));

        let mut reg = DeviceRegistry::default();
        reg.devices.insert("d".into(), entry("host; rm -rf /"));
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));
    }

    #[test]
    fn rejects_zero_port_and_unsafe_run_as() {
        let mut reg = DeviceRegistry::default();
        let mut e = entry("host");
        e.ssh_port = Some(0);
        reg.devices.insert("d".into(), e);
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));

        let mut reg = DeviceRegistry::default();
        let mut e = entry("host");
        e.remote_run_as = Some("root; id".into());
        reg.devices.insert("d".into(), e);
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));
    }

    #[test]
    fn rejects_relative_remote_paths() {
        let mut reg = DeviceRegistry::default();
        let mut e = entry("host");
        e.remote_tensorplate = Some(PathBuf::from("bin/tensorplate"));
        reg.devices.insert("d".into(), e);
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));

        let mut reg = DeviceRegistry::default();
        let mut e = entry("host");
        e.remote_import_dir = Some(PathBuf::from("relative/import"));
        reg.devices.insert("d".into(), e);
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));
    }

    #[test]
    fn rejects_dangling_default_device() {
        let mut reg = DeviceRegistry::default();
        reg.default_device = Some("ghost".into());
        assert!(matches!(reg.validate().unwrap_err(), CliError::Config(_)));
    }

    #[test]
    fn save_then_load_round_trips_and_creates_parent_dirs() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("nested").join("devices.json");
        let mut reg = DeviceRegistry::default();
        reg.devices.insert("orin".into(), entry("reid@orin.local"));
        reg.default_device = Some("orin".into());
        reg.save(&path).unwrap();
        assert!(path.exists());
        let loaded = DeviceRegistry::load(&path).unwrap();
        assert_eq!(loaded, reg);
    }

    #[test]
    fn load_missing_file_returns_empty_registry() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("does-not-exist.json");
        let reg = DeviceRegistry::load(&path).unwrap();
        assert!(reg.devices.is_empty());
        assert!(reg.default_device.is_none());
    }

    #[test]
    fn resolve_path_precedence() {
        assert_eq!(
            resolve_path_from(Some("/explicit/devices.json"), Some("/xdg"), Some("/home")).unwrap(),
            PathBuf::from("/explicit/devices.json")
        );
        assert_eq!(
            resolve_path_from(None, Some("/xdg"), Some("/home")).unwrap(),
            PathBuf::from("/xdg/tensorplate/devices.json")
        );
        assert_eq!(
            resolve_path_from(None, None, Some("/home")).unwrap(),
            PathBuf::from("/home/.config/tensorplate/devices.json")
        );
        assert!(resolve_path_from(None, None, None).is_err());
    }
}
