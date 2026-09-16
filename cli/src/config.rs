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
//
// Discovery order is `--config`, then $TENSORPLATE_CLI_CONFIG, then the
// packaged conffile, then the built-in defaults. The packaged step is one
// fixed absolute path, not a search: see [`CliConfig::resolve_from`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CliError, CliResult};

/// CLI config schema version. Independent track from the wire protocol;
/// bumps require an entry in `docs/cli/` and a migration note.
pub const CLI_CONFIG_SCHEMA_VERSION: &str = "0.1";

/// Environment variable naming the config file to load. Set by the
/// Homebrew launcher for its own prefix; operators may set it to point
/// at a per-user config on any channel.
pub const CLI_CONFIG_ENV: &str = "TENSORPLATE_CLI_CONFIG";

/// The one system config path the CLI reads on its own: the conffile the
/// native packages install. Not a search path — a single fixed absolute
/// location that only root can write on an installed host.
pub const SYSTEM_CLI_CONFIG_PATH: &str = tensorplate_protocol::install_paths::CLI_CONFIG_PATH;

/// Default profile name used when no config file is present.
pub const DEFAULT_PROFILE_NAME: &str = "local";

/// Default Unix socket path for the local agent. Mirrors packaging.
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

/// Where the config in effect for this invocation came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigSource {
    /// `--config <path>`.
    Explicit(PathBuf),
    /// The path named by [`CLI_CONFIG_ENV`].
    Environment(PathBuf),
    /// The packaged system config at [`SYSTEM_CLI_CONFIG_PATH`].
    System(PathBuf),
    /// Built-in defaults: no config file was found.
    BuiltIn,
    /// Built-in defaults: the packaged system config exists but this
    /// caller cannot read it.
    SystemUnreadable(PathBuf),
}

/// A validated config plus how it was found. `warning` carries an
/// operator-facing note about a config that was found but not used; the
/// binary prints it to stderr so a config with no effect never stays
/// invisible.
#[derive(Clone, Debug)]
pub struct ResolvedCliConfig {
    pub config: CliConfig,
    pub source: ConfigSource,
    pub warning: Option<String>,
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
        Self::parse_json(&body).map_err(|e| name_config_file(path, e))
    }

    /// Resolve the config for this invocation, in precedence order:
    /// `--config <path>`, then `$TENSORPLATE_CLI_CONFIG`, then the
    /// packaged system config at [`SYSTEM_CLI_CONFIG_PATH`], then the
    /// built-in defaults.
    ///
    /// The system step is a single fixed absolute path, never a search.
    /// On an installed host only root can write it, so reading it cannot
    /// be steered by planting a file in a directory the caller controls
    /// — which is what the loader has always refused to do.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Config`] when a config that was found cannot be
    /// parsed or fails [`Self::validate`], and when `--config` or
    /// `$TENSORPLATE_CLI_CONFIG` names a file that cannot be read.
    pub fn resolve(explicit: Option<&Path>) -> CliResult<ResolvedCliConfig> {
        Self::resolve_from(
            explicit,
            std::env::var_os(CLI_CONFIG_ENV),
            Self::system_config_path(),
        )
    }

    /// The packaged conffile [`Self::resolve`] reads when nothing else is
    /// set. Named rather than inlined so the wiring itself — which file the
    /// installed binary looks for — is a value a test can assert on. Issue
    /// #203 was exactly this wiring being wrong with every unit test green.
    #[must_use]
    pub fn system_config_path() -> &'static Path {
        Path::new(SYSTEM_CLI_CONFIG_PATH)
    }

    /// [`Self::resolve`] with the environment and the system config path
    /// supplied by the caller, so the precedence chain is testable without
    /// a real `/etc`.
    ///
    /// # Errors
    ///
    /// See [`Self::resolve`].
    pub fn resolve_from(
        explicit: Option<&Path>,
        env_value: Option<OsString>,
        system_path: &Path,
    ) -> CliResult<ResolvedCliConfig> {
        if let Some(path) = explicit {
            return Ok(ResolvedCliConfig {
                config: Self::load(path)?,
                source: ConfigSource::Explicit(path.to_path_buf()),
                warning: None,
            });
        }
        if let Some(env_value) = env_value.filter(|value| !value.is_empty()) {
            let path = PathBuf::from(env_value);
            return Ok(ResolvedCliConfig {
                config: Self::load(&path)?,
                source: ConfigSource::Environment(path),
                warning: None,
            });
        }
        match fs::read_to_string(system_path) {
            // A system config that parses is authoritative; one that does
            // not is an install fault. Falling back to the built-in
            // defaults here would point every command at a socket the
            // operator never configured and say nothing about it.
            Ok(body) => Ok(ResolvedCliConfig {
                config: Self::parse_json(&body).map_err(|e| name_config_file(system_path, e))?,
                source: ConfigSource::System(system_path.to_path_buf()),
                warning: None,
            }),
            // No packaged install here: the built-in defaults are the
            // documented behaviour, and saying so on every command would
            // be noise.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ResolvedCliConfig {
                config: Self::default(),
                source: ConfigSource::BuiltIn,
                warning: None,
            }),
            // The file is there but this caller cannot read it — on a
            // native install, /etc/tensorplate is `root:tensorplate 0750`.
            // Commands that do not need the agent (doctor, version) still
            // work on the defaults, so this reports rather than fails, and
            // says why the packaged settings are not in effect.
            Err(e) => Ok(ResolvedCliConfig {
                config: Self::default(),
                warning: Some(format!(
                    "tensorplate: cannot read the packaged cli config `{}`: {e}; using built-in defaults. \
Join the `{}` group (or re-run as root) to use the packaged profile, or pass --config <path>.",
                    system_path.display(),
                    tensorplate_protocol::install_paths::SYSTEM_GROUP,
                )),
                source: ConfigSource::SystemUnreadable(system_path.to_path_buf()),
            }),
        }
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

/// Name the file a config error came from. Parsing and validation work on
/// a document, not a path, so without this the operator is told a config is
/// wrong without being told which one — and with discovery reaching a file
/// they never named, that is no longer answerable from the command line.
fn name_config_file(path: &Path, error: CliError) -> CliError {
    match error {
        CliError::Config(message) => {
            CliError::Config(format!("{message}; from `{}`", path.display()))
        }
        other => other,
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

    /// A config naming `socket` so a test can tell which file was read.
    fn write_config(path: &Path, socket: &str) {
        std::fs::write(
            path,
            format!(
                r#"{{"schema_version":"{CLI_CONFIG_SCHEMA_VERSION}","profiles":{{"local":{{"mode":"local","socket_path":"{socket}"}}}}}}"#
            ),
        )
        .unwrap();
    }

    fn socket_of(resolved: &ResolvedCliConfig) -> PathBuf {
        resolved
            .config
            .profile(&resolved.config.default_profile)
            .unwrap()
            .socket_path
            .clone()
            .unwrap()
    }

    #[test]
    fn resolve_falls_back_to_built_in_defaults_without_any_config() {
        let td = tempfile::tempdir().unwrap();
        let absent = td.path().join("absent").join("cli.json");
        let resolved = CliConfig::resolve_from(None, None, &absent).unwrap();
        assert_eq!(resolved.config.default_profile, "local");
        assert_eq!(resolved.source, ConfigSource::BuiltIn);
        assert!(resolved.warning.is_none());
        assert_eq!(
            socket_of(&resolved),
            PathBuf::from(DEFAULT_LOCAL_AGENT_SOCKET)
        );
    }

    #[test]
    fn resolve_reads_the_packaged_system_config_when_nothing_else_is_set() {
        let td = tempfile::tempdir().unwrap();
        let system = td.path().join("cli.json");
        write_config(&system, "/run/tensorplate/agent.sock");
        let resolved = CliConfig::resolve_from(None, None, &system).unwrap();
        assert_eq!(
            socket_of(&resolved),
            PathBuf::from("/run/tensorplate/agent.sock")
        );
        assert_eq!(resolved.source, ConfigSource::System(system));
        assert!(resolved.warning.is_none());
    }

    #[test]
    fn explicit_and_environment_configs_outrank_the_packaged_one() {
        let td = tempfile::tempdir().unwrap();
        let system = td.path().join("system.json");
        let from_env = td.path().join("env.json");
        let explicit = td.path().join("explicit.json");
        write_config(&system, "/run/system.sock");
        write_config(&from_env, "/run/env.sock");
        write_config(&explicit, "/run/explicit.sock");

        // The Homebrew launcher exports TENSORPLATE_CLI_CONFIG, so the
        // env step must keep winning over the packaged path.
        let by_env =
            CliConfig::resolve_from(None, Some(from_env.clone().into_os_string()), &system)
                .unwrap();
        assert_eq!(socket_of(&by_env), PathBuf::from("/run/env.sock"));
        assert_eq!(by_env.source, ConfigSource::Environment(from_env.clone()));

        let by_flag =
            CliConfig::resolve_from(Some(&explicit), Some(from_env.into_os_string()), &system)
                .unwrap();
        assert_eq!(socket_of(&by_flag), PathBuf::from("/run/explicit.sock"));
        assert_eq!(by_flag.source, ConfigSource::Explicit(explicit));
    }

    #[test]
    fn an_empty_environment_value_falls_through_to_the_packaged_config() {
        let td = tempfile::tempdir().unwrap();
        let system = td.path().join("cli.json");
        write_config(&system, "/run/system.sock");
        let resolved = CliConfig::resolve_from(None, Some(OsString::from("")), &system).unwrap();
        assert_eq!(socket_of(&resolved), PathBuf::from("/run/system.sock"));
    }

    /// Discovery can land on a file the operator never named, so a config
    /// error that does not say which file it came from is unanswerable.
    #[test]
    fn a_config_error_names_the_file_it_came_from() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("cli.json");
        std::fs::write(&path, r#"{"schema_version":"99.99"}"#).unwrap();
        let CliError::Config(message) = CliConfig::load(&path).unwrap_err() else {
            panic!("expected a config error");
        };
        assert!(
            message.contains(&path.display().to_string()),
            "a validation failure must name the file: {message}"
        );

        std::fs::write(&path, "{not json").unwrap();
        let CliError::Config(message) = CliConfig::load(&path).unwrap_err() else {
            panic!("expected a config error");
        };
        assert!(
            message.contains(&path.display().to_string()),
            "a parse failure must name the file: {message}"
        );
    }

    #[test]
    fn a_malformed_packaged_config_is_an_error_not_a_silent_default() {
        let td = tempfile::tempdir().unwrap();
        let system = td.path().join("cli.json");
        std::fs::write(&system, "{not json").unwrap();
        let err = CliConfig::resolve_from(None, None, &system).unwrap_err();
        let CliError::Config(message) = err else {
            panic!("expected a config error");
        };
        assert!(
            message.contains(&system.display().to_string()),
            "error must name the file: {message}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn an_unreadable_packaged_config_warns_and_uses_the_defaults() {
        use std::os::unix::fs::PermissionsExt;

        let td = tempfile::tempdir().unwrap();
        let etc = td.path().join("etc");
        std::fs::create_dir(&etc).unwrap();
        let system = etc.join("cli.json");
        write_config(&system, "/run/system.sock");
        // Mirrors the installed /etc/tensorplate: a caller outside the
        // service group cannot traverse the directory.
        std::fs::set_permissions(&etc, std::fs::Permissions::from_mode(0o000)).unwrap();
        let resolved = CliConfig::resolve_from(None, None, &system);
        std::fs::set_permissions(&etc, std::fs::Permissions::from_mode(0o700)).unwrap();

        let resolved = resolved.unwrap();
        if matches!(resolved.source, ConfigSource::System(_)) {
            // Running as root, which ignores the mode. Nothing to assert.
            return;
        }
        assert_eq!(
            resolved.source,
            ConfigSource::SystemUnreadable(system.clone())
        );
        assert_eq!(
            socket_of(&resolved),
            PathBuf::from(DEFAULT_LOCAL_AGENT_SOCKET)
        );
        let warning = resolved.warning.expect("unreadable config must warn");
        assert!(
            warning.contains(&system.display().to_string()),
            "warning must name the file: {warning}"
        );
        assert!(
            warning.contains("tensorplate` group") || warning.contains("--config"),
            "warning must say how to fix it: {warning}"
        );
    }

    #[test]
    fn the_packaged_config_path_is_the_installed_conffile() {
        assert_eq!(
            SYSTEM_CLI_CONFIG_PATH,
            tensorplate_protocol::install_paths::CLI_CONFIG_PATH
        );
        assert!(Path::new(SYSTEM_CLI_CONFIG_PATH).is_absolute());
    }

    /// The wiring `resolve()` hands to `resolve_from`. Every other test in
    /// this module supplies its own path, so without this one the installed
    /// binary can look for the wrong file — the #203 defect — with the whole
    /// suite green.
    #[test]
    fn resolve_reads_the_installed_conffile_path() {
        assert_eq!(
            CliConfig::system_config_path(),
            Path::new(tensorplate_protocol::install_paths::CLI_CONFIG_PATH)
        );
    }
}
