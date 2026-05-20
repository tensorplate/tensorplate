// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F01-T01: CLI config loader and schema validation.
//
// The CLI is operable without a config file — the default `local` profile
// targets the agent's well-known socket path. The config exists to (a)
// override that path on machines that move the agent socket, (b) declare
// an explicit remote URL for laptop-to-device workflows, and (c) point
// `tensorplate logs` at the right NDJSON source. Validation runs before
// any agent call so misspelled fields never leak into network requests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CliError, CliResult};

/// CLI config schema version. Independent track from the wire protocol;
/// bumps require an entry in `docs/cli/` and a migration note.
pub const CLI_CONFIG_SCHEMA_VERSION: &str = "0.1";

/// Default profile name used when no config file is present.
pub const DEFAULT_PROFILE_NAME: &str = "local";

/// Default Unix socket path for the local agent. Mirrors V01-E14 packaging.
pub const DEFAULT_LOCAL_AGENT_SOCKET: &str = "/var/run/tensorplate/agent.sock";

/// Default request timeout for agent calls (milliseconds).
pub const DEFAULT_AGENT_TIMEOUT_MS: u64 = 30_000;

/// Device access profile mode. Mirrors `config/schemas/cli.json`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileMode {
    /// Local agent reached via its packaged Unix domain socket.
    Local,
    /// Explicit remote `host:port` agent reached via SSH/VPN/overlay.
    Url,
    /// Reserved for v0.2+. Returns typed `Unsupported` at command execution.
    SshTunnel,
    /// Reserved for v0.2+. Returns typed `Unsupported` at command execution.
    Overlay,
    /// Reserved for v0.2+. Returns typed `Unsupported` at command execution.
    Relay,
}

impl ProfileMode {
    #[must_use]
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Local | Self::Url)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Url => "url",
            Self::SshTunnel => "ssh_tunnel",
            Self::Overlay => "overlay",
            Self::Relay => "relay",
        }
    }
}

/// Single named profile.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileSpec {
    pub mode: ProfileMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Output defaults applied when the user does not pass `--output`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutputDefaults {
    #[serde(default = "default_output_mode")]
    pub mode: String,
    #[serde(default = "default_color")]
    pub color: String,
}

impl Default for OutputDefaults {
    fn default() -> Self {
        Self {
            mode: default_output_mode(),
            color: default_color(),
        }
    }
}

fn default_output_mode() -> String {
    "human".into()
}

fn default_color() -> String {
    "auto".into()
}

/// Optional log-source configuration for `tensorplate logs`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LogSourceConfig {
    #[serde(default = "default_log_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default = "default_tail")]
    pub tail_default: u64,
}

impl Default for LogSourceConfig {
    fn default() -> Self {
        Self {
            kind: default_log_kind(),
            path: None,
            tail_default: default_tail(),
        }
    }
}

fn default_log_kind() -> String {
    "file".into()
}

const fn default_tail() -> u64 {
    100
}

/// Versioned CLI configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    #[serde(default = "default_profile_name")]
    pub default_profile: String,
    #[serde(default)]
    pub output: OutputDefaults,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub log_source: LogSourceConfig,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileSpec>,
}

fn default_schema_version() -> String {
    CLI_CONFIG_SCHEMA_VERSION.to_string()
}

fn default_profile_name() -> String {
    DEFAULT_PROFILE_NAME.to_string()
}

const fn default_timeout_ms() -> u64 {
    DEFAULT_AGENT_TIMEOUT_MS
}

impl Default for CliConfig {
    fn default() -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            DEFAULT_PROFILE_NAME.into(),
            ProfileSpec {
                mode: ProfileMode::Local,
                display_name: Some("Local TensorPlate agent".into()),
                description: Some(
                    "Default profile targeting the packaged Unix domain socket.".into(),
                ),
                socket_path: Some(PathBuf::from(DEFAULT_LOCAL_AGENT_SOCKET)),
                agent_url: None,
                serving_url: None,
                timeout_ms: None,
            },
        );
        Self {
            schema_version: CLI_CONFIG_SCHEMA_VERSION.into(),
            default_profile: DEFAULT_PROFILE_NAME.into(),
            output: OutputDefaults {
                mode: "human".into(),
                color: "auto".into(),
            },
            timeout_ms: DEFAULT_AGENT_TIMEOUT_MS,
            log_source: LogSourceConfig::default(),
            profiles,
        }
    }
}

impl CliConfig {
    /// Validate the config. Returns a fully-resolved config or a typed
    /// [`CliError::Config`] error.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Config`] for unknown schema versions, empty
    /// or duplicated profile fields, missing required values per mode,
    /// and zero timeouts.
    pub fn validate(mut self) -> CliResult<Self> {
        if self.schema_version != CLI_CONFIG_SCHEMA_VERSION {
            return Err(CliError::Config(format!(
                "unsupported cli config schema_version `{}` (expected `{}`)",
                self.schema_version, CLI_CONFIG_SCHEMA_VERSION
            )));
        }
        if self.timeout_ms == 0 {
            return Err(CliError::Config("timeout_ms must be > 0".into()));
        }
        if !matches!(self.output.mode.as_str(), "human" | "json") {
            return Err(CliError::Config(format!(
                "output.mode must be `human` or `json`, got `{}`",
                self.output.mode
            )));
        }
        if !matches!(self.output.color.as_str(), "auto" | "always" | "never") {
            return Err(CliError::Config(format!(
                "output.color must be `auto`, `always`, or `never`, got `{}`",
                self.output.color
            )));
        }
        if self.log_source.tail_default == 0 {
            return Err(CliError::Config(
                "log_source.tail_default must be > 0".into(),
            ));
        }
        if !matches!(self.log_source.kind.as_str(), "file" | "directory") {
            return Err(CliError::Config(format!(
                "log_source.kind must be `file` or `directory`, got `{}`",
                self.log_source.kind
            )));
        }
        // If the user did not declare profiles, seed the well-known local
        // profile so commands still work against the packaged install.
        if self.profiles.is_empty() {
            self.profiles.insert(
                DEFAULT_PROFILE_NAME.into(),
                ProfileSpec {
                    mode: ProfileMode::Local,
                    display_name: None,
                    description: None,
                    socket_path: Some(PathBuf::from(DEFAULT_LOCAL_AGENT_SOCKET)),
                    agent_url: None,
                    serving_url: None,
                    timeout_ms: None,
                },
            );
        }
        for (name, profile) in &self.profiles {
            if name.is_empty() {
                return Err(CliError::Config("profile name must be non-empty".into()));
            }
            validate_profile(name, profile)?;
        }
        if !self.profiles.contains_key(&self.default_profile) {
            return Err(CliError::Config(format!(
                "default_profile `{}` is not declared in `profiles`",
                self.default_profile
            )));
        }
        Ok(self)
    }

    /// Parse a JSON config document and validate it.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Config`] if the document is malformed or
    /// fails [`Self::validate`].
    pub fn parse_json(text: &str) -> CliResult<Self> {
        let cfg: Self = serde_json::from_str(text)
            .map_err(|e| CliError::Config(format!("cli config is not valid JSON: {e}")))?;
        cfg.validate()
    }

    /// Load a config file at `path`, parsing and validating it.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Config`] if the file cannot be read, is
    /// malformed, or fails [`Self::validate`].
    pub fn load(path: &Path) -> CliResult<Self> {
        let body = fs::read_to_string(path).map_err(|e| {
            CliError::Config(format!(
                "failed to read cli config `{}`: {e}",
                path.display()
            ))
        })?;
        Self::parse_json(&body)
    }

    /// Load the config from `--config <path>` if supplied; otherwise
    /// fall back to `$TENSORPLATE_CLI_CONFIG`; otherwise return defaults.
    ///
    /// Defaults intentionally do not search arbitrary system paths: that
    /// would let an attacker plant a config file in a writable dir and
    /// take control of the operator's CLI.
    pub fn load_or_default(explicit: Option<&Path>) -> CliResult<Self> {
        if let Some(p) = explicit {
            return Self::load(p);
        }
        if let Ok(env_path) = std::env::var("TENSORPLATE_CLI_CONFIG") {
            if !env_path.is_empty() {
                return Self::load(Path::new(&env_path));
            }
        }
        Ok(Self::default())
    }

    /// Return the profile spec for `name`.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Config`] when the profile does not exist.
    pub fn profile(&self, name: &str) -> CliResult<&ProfileSpec> {
        self.profiles.get(name).ok_or_else(|| {
            CliError::Config(format!("profile `{name}` is not declared in cli config"))
        })
    }
}

fn validate_profile(name: &str, profile: &ProfileSpec) -> CliResult<()> {
    match profile.mode {
        ProfileMode::Local => {
            // local mode requires a socket path or we default to the well-known one.
            if let Some(p) = profile.socket_path.as_deref() {
                if !p.is_absolute() {
                    return Err(CliError::Config(format!(
                        "profile `{name}`: socket_path `{}` must be absolute",
                        p.display()
                    )));
                }
            }
            if profile.agent_url.is_some() {
                return Err(CliError::Config(format!(
                    "profile `{name}`: `agent_url` is only valid for mode=url"
                )));
            }
        }
        ProfileMode::Url => {
            let Some(url) = profile.agent_url.as_deref() else {
                return Err(CliError::Config(format!(
                    "profile `{name}`: mode=url requires `agent_url`"
                )));
            };
            // We accept `host:port` form. The `tcp://` scheme is reserved
            // for forward compatibility but rejected here so we can be
            // sure the existing transport layer (loopback TCP) is what
            // the user expects.
            if url.contains("://") {
                return Err(CliError::Config(format!(
                    "profile `{name}`: agent_url must be `host:port`, not `{url}`"
                )));
            }
            if !url.contains(':') {
                return Err(CliError::Config(format!(
                    "profile `{name}`: agent_url `{url}` must include a port",
                )));
            }
            if profile.socket_path.is_some() {
                return Err(CliError::Config(format!(
                    "profile `{name}`: `socket_path` is only valid for mode=local"
                )));
            }
        }
        ProfileMode::SshTunnel | ProfileMode::Overlay | ProfileMode::Relay => {
            // Reserved modes: accepted by the schema, rejected at command
            // execution. We still validate the user did not mix incompatible
            // fields so spelling mistakes do not silently disappear.
            if profile.socket_path.is_some() && profile.agent_url.is_some() {
                return Err(CliError::Config(format!(
                    "profile `{name}`: cannot specify both socket_path and agent_url"
                )));
            }
        }
    }
    if let Some(s) = profile.serving_url.as_deref() {
        if !s.starts_with("http://") && !s.starts_with("https://") {
            return Err(CliError::Config(format!(
                "profile `{name}`: serving_url `{s}` must start with `http://` or `https://`"
            )));
        }
    }
    if let Some(t) = profile.timeout_ms {
        if t == 0 {
            return Err(CliError::Config(format!(
                "profile `{name}`: timeout_ms must be > 0"
            )));
        }
    }
    Ok(())
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

    #[test]
    fn default_config_is_valid_and_carries_local_profile() {
        let cfg = CliConfig::default().validate().unwrap();
        assert_eq!(cfg.default_profile, "local");
        let p = cfg.profile("local").unwrap();
        assert_eq!(p.mode, ProfileMode::Local);
        assert!(p.socket_path.as_ref().unwrap().is_absolute());
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let raw = r#"{"schema_version":"99.99"}"#;
        let err = CliConfig::parse_json(raw).unwrap_err();
        assert!(matches!(err, CliError::Config(_)));
    }

    #[test]
    fn rejects_zero_timeout() {
        let raw = format!(r#"{{"schema_version":"{CLI_CONFIG_SCHEMA_VERSION}","timeout_ms":0}}"#);
        let err = CliConfig::parse_json(&raw).unwrap_err();
        assert!(matches!(err, CliError::Config(_)));
    }

    #[test]
    fn local_profile_rejects_relative_socket_path() {
        let raw = format!(
            r#"{{"schema_version":"{CLI_CONFIG_SCHEMA_VERSION}","profiles":{{"local":{{"mode":"local","socket_path":"relative/path"}}}}}}"#
        );
        let err = CliConfig::parse_json(&raw).unwrap_err();
        assert!(matches!(err, CliError::Config(_)));
    }

    #[test]
    fn url_profile_requires_agent_url_with_port() {
        let raw = format!(
            r#"{{"schema_version":"{CLI_CONFIG_SCHEMA_VERSION}","default_profile":"remote","profiles":{{"remote":{{"mode":"url","agent_url":"http://example"}}}}}}"#
        );
        let err = CliConfig::parse_json(&raw).unwrap_err();
        assert!(matches!(err, CliError::Config(_)));
        let raw_ok = format!(
            r#"{{"schema_version":"{CLI_CONFIG_SCHEMA_VERSION}","default_profile":"remote","profiles":{{"remote":{{"mode":"url","agent_url":"127.0.0.1:18000"}}}}}}"#
        );
        let cfg = CliConfig::parse_json(&raw_ok).unwrap();
        assert_eq!(cfg.profile("remote").unwrap().mode, ProfileMode::Url);
    }

    #[test]
    fn default_profile_must_exist() {
        let raw = format!(
            r#"{{"schema_version":"{CLI_CONFIG_SCHEMA_VERSION}","default_profile":"nope","profiles":{{"local":{{"mode":"local"}}}}}}"#
        );
        let err = CliConfig::parse_json(&raw).unwrap_err();
        assert!(matches!(err, CliError::Config(_)));
    }

    #[test]
    fn reserved_profile_mode_validates_but_unsupported_is_signalled_later() {
        let raw = format!(
            r#"{{"schema_version":"{CLI_CONFIG_SCHEMA_VERSION}","default_profile":"jump","profiles":{{"jump":{{"mode":"ssh_tunnel"}}}}}}"#
        );
        let cfg = CliConfig::parse_json(&raw).unwrap();
        assert!(!cfg.profile("jump").unwrap().mode.is_supported());
    }

    #[test]
    fn load_or_default_returns_default_when_unspecified() {
        std::env::remove_var("TENSORPLATE_CLI_CONFIG");
        let cfg = CliConfig::load_or_default(None).unwrap();
        assert_eq!(cfg.default_profile, "local");
    }
}
