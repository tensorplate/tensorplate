// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F02-T01: device access profile resolution.
//
// Profile resolution is a pure projection of the CLI config and the
// global flags. Reserved modes (`ssh-tunnel`, `overlay`, `relay`) parse
// but return a typed `Unsupported` error here so command code never
// reaches the network layer with an unhandled mode.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::{CliConfig, ProfileMode, ProfileSpec, DEFAULT_LOCAL_AGENT_SOCKET};
use crate::error::{CliError, CliResult};

/// Resolved profile + effective transport settings ready to be consumed
/// by [`crate::client::NetAgentClient`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedProfile {
    pub name: String,
    pub mode: ProfileMode,
    pub display_name: Option<String>,
    pub transport: Transport,
    pub serving_url: Option<String>,
    pub timeout: Duration,
}

/// Concrete transport selected for this invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Transport {
    /// Connect to the agent's Unix domain socket at `path`.
    UnixSocket { path: PathBuf },
    /// Connect to the agent's loopback TCP endpoint at `host:port`. Used
    /// for both the `local` profile (when the agent is configured for
    /// TCP) and the `url` profile (laptop-to-device workflows).
    LoopbackTcp { host: String, port: u16 },
}

/// Resolve the active profile from the CLI config + global flags.
///
/// `--agent-url` always wins; otherwise the named profile from
/// `--profile`; otherwise the config's `default_profile`.
///
/// # Errors
///
/// - [`CliError::Config`] when the named profile does not exist.
/// - [`CliError::UnsupportedProfile`] when the profile uses a reserved mode.
/// - [`CliError::Config`] when required fields are missing for the mode.
pub fn resolve(
    cfg: &CliConfig,
    profile_override: Option<&str>,
    agent_url_override: Option<&str>,
) -> CliResult<ResolvedProfile> {
    if let Some(url) = agent_url_override {
        let (host, port) = parse_host_port(url)?;
        return Ok(ResolvedProfile {
            name: "<agent-url>".into(),
            mode: ProfileMode::Url,
            display_name: Some(format!("explicit agent url {url}")),
            transport: Transport::LoopbackTcp { host, port },
            serving_url: None,
            timeout: Duration::from_millis(cfg.timeout_ms),
        });
    }
    let name = profile_override.unwrap_or(&cfg.default_profile).to_string();
    let profile = cfg.profile(&name)?;
    if !profile.mode.is_supported() {
        return Err(CliError::UnsupportedProfile {
            mode: profile.mode.as_str().to_string(),
        });
    }
    let transport = transport_for_profile(&name, profile)?;
    let timeout_ms = profile.timeout_ms.unwrap_or(cfg.timeout_ms);
    Ok(ResolvedProfile {
        name,
        mode: profile.mode,
        display_name: profile.display_name.clone(),
        transport,
        serving_url: profile.serving_url.clone(),
        timeout: Duration::from_millis(timeout_ms),
    })
}

fn transport_for_profile(name: &str, profile: &ProfileSpec) -> CliResult<Transport> {
    match profile.mode {
        ProfileMode::Local => {
            let socket_path = profile
                .socket_path
                .clone()
                .unwrap_or_else(|| PathBuf::from(DEFAULT_LOCAL_AGENT_SOCKET));
            Ok(Transport::UnixSocket { path: socket_path })
        }
        ProfileMode::Url => {
            let agent_url = profile.agent_url.as_deref().ok_or_else(|| {
                CliError::Config(format!("profile `{name}`: mode=url requires `agent_url`"))
            })?;
            let (host, port) = parse_host_port(agent_url)?;
            Ok(Transport::LoopbackTcp { host, port })
        }
        other => Err(CliError::UnsupportedProfile {
            mode: other.as_str().to_string(),
        }),
    }
}

fn parse_host_port(value: &str) -> CliResult<(String, u16)> {
    let (host, port) = value.rsplit_once(':').ok_or_else(|| {
        CliError::Config(format!(
            "agent url `{value}` must be `host:port`, got no `:`"
        ))
    })?;
    if host.is_empty() {
        return Err(CliError::Config(format!(
            "agent url `{value}` has empty host"
        )));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| CliError::Config(format!("agent url `{value}` has non-numeric port")))?;
    if port == 0 {
        return Err(CliError::Config(format!("agent url `{value}` has port 0")));
    }
    Ok((host.to_string(), port))
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
    use crate::config::CliConfig;

    #[test]
    fn explicit_agent_url_wins() {
        let cfg = CliConfig::default().validate().unwrap();
        let r = resolve(&cfg, Some("local"), Some("10.0.0.5:18080")).unwrap();
        assert!(matches!(
            r.transport,
            Transport::LoopbackTcp { ref host, port } if host == "10.0.0.5" && port == 18080
        ));
    }

    #[test]
    fn default_profile_resolves_to_unix_socket() {
        let cfg = CliConfig::default().validate().unwrap();
        let r = resolve(&cfg, None, None).unwrap();
        assert!(matches!(r.transport, Transport::UnixSocket { .. }));
        assert_eq!(r.mode, ProfileMode::Local);
    }

    #[test]
    fn reserved_modes_return_typed_unsupported() {
        let raw = r#"{"schema_version":"0.1","default_profile":"jump","profiles":{"jump":{"mode":"relay"}}}"#;
        let cfg = CliConfig::parse_json(raw).unwrap();
        let err = resolve(&cfg, None, None).unwrap_err();
        assert!(matches!(err, CliError::UnsupportedProfile { ref mode } if mode == "relay"));
    }

    #[test]
    fn parse_host_port_rejects_zero_port() {
        let err = parse_host_port("host:0").unwrap_err();
        assert!(matches!(err, CliError::Config(_)));
    }

    #[test]
    fn parse_host_port_rejects_missing_colon() {
        let err = parse_host_port("hostonly").unwrap_err();
        assert!(matches!(err, CliError::Config(_)));
    }

    #[test]
    fn url_profile_resolves_with_global_timeout() {
        let raw = r#"{"schema_version":"0.1","timeout_ms":12000,"default_profile":"r","profiles":{"r":{"mode":"url","agent_url":"10.0.0.5:18000"}}}"#;
        let cfg = CliConfig::parse_json(raw).unwrap();
        let r = resolve(&cfg, None, None).unwrap();
        assert_eq!(r.timeout, std::time::Duration::from_millis(12_000));
    }
}
