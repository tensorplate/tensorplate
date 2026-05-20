// SPDX-License-Identifier: Apache-2.0

//! `tensorplate-cli` — operator client for one reachable `tensorplate-agent`.
//!
//! The crate exposes the subcommand modules and the [`run`] entry point used
//! by both the `tensorplate` binary and the V01-E11-F08 integration tests.
//! All mutating commands route through the agent control API; the CLI never
//! mutates the serving worker directly.

#![forbid(unsafe_code)]
// Pedantic lints we deliberately accept across the CLI crate. The CLI is a
// thin client over the agent control API: most argument types are dominated
// by `String` / `PathBuf`, so passing them by value through the command
// dispatcher and renderer is intentional and readable. The match arms and
// `default_trait_access` patterns flagged below match the agent crate's
// conventions for transport projections.
#![allow(
    clippy::needless_pass_by_value,
    clippy::default_trait_access,
    clippy::field_reassign_with_default,
    clippy::large_enum_variant,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::trivially_copy_pass_by_ref,
    clippy::too_many_lines,
    clippy::map_unwrap_or,
    clippy::redundant_closure_for_method_calls,
    clippy::needless_borrows_for_generic_args,
    clippy::ptr_arg
)]

pub mod args;
pub mod client;
pub mod commands;
pub mod config;
pub mod error;
pub mod output;
pub mod profile;

use std::io::Write;

pub use args::{GlobalArgs, OutputMode, ParsedArgs, Subcommand};
pub use client::{AgentClient, MockAgentClient, NetAgentClient, ServingClient};
pub use config::{CliConfig, OutputDefaults, ProfileMode, ProfileSpec};
pub use error::{CliError, CliResult, ExitCode};
pub use output::Renderer;
pub use profile::ResolvedProfile;

/// Crate version string compiled from Cargo metadata.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Crate name compiled from Cargo metadata.
#[must_use]
pub fn name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

/// Run the CLI end-to-end against a caller-supplied set of agent clients,
/// stdout writer, and stderr writer. The binary entry point in `main.rs`
/// wires these to the real environment; integration tests swap in mock
/// clients and in-memory buffers.
///
/// # Errors
///
/// Returns the typed [`CliError`] raised by the chosen subcommand. The
/// binary maps it to a process exit code via [`CliError::exit_code`].
pub fn run<O, E, F>(
    parsed: ParsedArgs,
    cfg: CliConfig,
    client_factory: F,
    stdout: &mut O,
    stderr: &mut E,
) -> CliResult<()>
where
    O: Write,
    E: Write,
    F: FnOnce(&ResolvedProfile) -> CliResult<Box<dyn AgentClient>>,
{
    let profile = profile::resolve(
        &cfg,
        parsed.global.profile.as_deref(),
        parsed.global.agent_url.as_deref(),
        parsed.global.timeout_ms,
    )?;
    let renderer = Renderer::new(effective_output_mode(&parsed.global, &cfg));
    match parsed.subcommand {
        Subcommand::Version => commands::version::run(&renderer, stdout),
        Subcommand::Doctor(opts) => {
            let client = client_factory(&profile)?;
            commands::doctor::run(&renderer, &profile, &*client, &opts, stdout, stderr)
        }
        Subcommand::Deploy(opts) => {
            let client = client_factory(&profile)?;
            commands::deploy::run(&renderer, &profile, &*client, &opts, stdout, stderr)
        }
        Subcommand::Rollback(opts) => {
            let client = client_factory(&profile)?;
            commands::rollback::run(&renderer, &profile, &*client, &opts, stdout, stderr)
        }
        Subcommand::Status(opts) => {
            let client = client_factory(&profile)?;
            commands::status::run(&renderer, &profile, &*client, &opts, stdout, stderr)
        }
        Subcommand::Infer(opts) => {
            let client = client_factory(&profile)?;
            commands::infer::run(&renderer, &profile, &*client, &opts, stdout, stderr)
        }
        Subcommand::Logs(opts) => {
            commands::logs::run(&renderer, &profile, &cfg, &opts, stdout, stderr)
        }
    }
}

/// Resolve the output mode for a command invocation. Explicit argv wins;
/// otherwise use the validated CLI config default.
#[must_use]
pub fn effective_output_mode(global: &GlobalArgs, cfg: &CliConfig) -> OutputMode {
    global.output.unwrap_or(match cfg.output.mode.as_str() {
        "json" => OutputMode::Json,
        _ => OutputMode::Human,
    })
}

/// Generate a correlation id stamped on every agent request. Format is
/// `cli-<random uuidv4>` so server-side logs can grep for CLI traffic.
#[must_use]
pub fn new_correlation_id() -> String {
    format!("cli-{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_output_mode_uses_config_default_when_unspecified() {
        let global = GlobalArgs::default();
        let mut cfg = CliConfig::default();
        cfg.output.mode = "json".into();
        assert_eq!(effective_output_mode(&global, &cfg), OutputMode::Json);
    }

    #[test]
    fn effective_output_mode_prefers_argv_override() {
        let global = GlobalArgs {
            output: Some(OutputMode::Human),
            ..GlobalArgs::default()
        };
        let mut cfg = CliConfig::default();
        cfg.output.mode = "json".into();
        assert_eq!(effective_output_mode(&global, &cfg), OutputMode::Human);
    }
}
