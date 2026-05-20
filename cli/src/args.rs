// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F01-T02: argument parser for the v0.1.0 `tensorplate` CLI.
//
// We hand-roll a focused parser rather than pulling in a heavy CLI
// framework: the v0.1.0 surface is small, the flag grammar is stable, and
// keeping argument validation here makes it trivial to unit test command
// dispatch without a live binary.

use std::path::PathBuf;

use crate::error::{CliError, CliResult};

/// Output mode requested on the command line. Each subcommand honours
/// this through the shared [`crate::output::Renderer`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputMode {
    Human,
    Json,
}

impl Default for OutputMode {
    fn default() -> Self {
        Self::Human
    }
}

/// Verbosity level. `quiet` suppresses informational stderr noise and is
/// useful when the CLI is invoked from scripts; `verbose` adds bounded
/// diagnostic context (no debug stack traces, ever).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
}

/// Global flags parsed before the subcommand.
#[derive(Clone, Debug, Default)]
pub struct GlobalArgs {
    pub config_path: Option<PathBuf>,
    pub profile: Option<String>,
    pub agent_url: Option<String>,
    pub output: OutputMode,
    pub timeout_ms: Option<u64>,
    pub no_color: bool,
    pub verbosity: Verbosity,
}

/// Subcommand-specific arguments.
#[derive(Clone, Debug)]
pub enum Subcommand {
    Doctor(DoctorArgs),
    Deploy(DeployArgs),
    Rollback(RollbackArgs),
    Status(StatusArgs),
    Infer(InferArgs),
    Logs(LogsArgs),
    Version,
}

#[derive(Clone, Debug, Default)]
pub struct DoctorArgs {
    pub skip_agent: bool,
}

#[derive(Clone, Debug)]
pub struct DeployArgs {
    pub bundle_path: PathBuf,
    pub deployment_id: Option<String>,
    pub expected_digest: Option<String>,
    pub wait: bool,
    pub wait_timeout_ms: u64,
    pub labels: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct RollbackArgs {
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct StatusArgs {
    pub observability_snapshot: Option<PathBuf>,
    pub include_quarantine: bool,
}

#[derive(Clone, Debug)]
pub struct InferArgs {
    pub input_path: Option<PathBuf>,
    pub from_stdin: bool,
    pub serving_url: Option<String>,
    pub timeout_ms: Option<u64>,
    pub output_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct LogsArgs {
    pub component: Option<String>,
    pub level: Option<String>,
    pub since_ms: Option<u64>,
    pub tail: Option<u64>,
    pub follow: bool,
    pub correlation_id: Option<String>,
    pub source_override: Option<PathBuf>,
}

/// Result of parsing argv. Holds both the global flags and the chosen
/// subcommand so command dispatch is a pure projection of this struct.
#[derive(Clone, Debug)]
pub struct ParsedArgs {
    pub global: GlobalArgs,
    pub subcommand: Subcommand,
}

const USAGE: &str = "usage: tensorplate [global flags] <command> [args]

Commands:
  doctor              Run device, runtime, and dependency checks.
  deploy <bundle>     Submit a model bundle to the agent deploy transaction.
  status              Render active deployment, worker, and observability state.
  infer               Send a single inference request to the active deployment.
  logs                Read bounded structured logs.
  rollback            Roll back to the previous active deployment.
  version             Print CLI and protocol versions.

Global flags:
  --config <path>           CLI config file (default: $TENSORPLATE_CLI_CONFIG or none).
  --profile <name>          Named profile from the CLI config.
  --agent-url <host:port>   Override profile and target a loopback agent URL.
  --output <human|json>     Output mode (default: human).
  --timeout-ms <n>          Per-call agent timeout override.
  --no-color                Disable color in human output.
  --quiet / --verbose       Suppress / expand informational stderr.
  -h, --help                Print usage and exit.
  -V, --version             Print CLI version and exit.

For per-command help, run `tensorplate <command> --help`.";

/// Parse `argv` (excluding the program name).
///
/// # Errors
///
/// Returns [`CliError::Usage`] on unknown flags, missing values, or
/// invalid subcommands.
pub fn parse(argv: &[String]) -> CliResult<ParseOutcome> {
    let mut global = GlobalArgs::default();
    let mut i = 0;
    // Phase 1: consume global flags until we either hit a subcommand or
    // exhaust the input.
    while i < argv.len() {
        let arg = &argv[i];
        match arg.as_str() {
            "-h" | "--help" => return Ok(ParseOutcome::Help),
            "-V" | "--version" => return Ok(ParseOutcome::Version),
            "--config" => {
                let value = require_value(argv, &mut i, "--config")?;
                global.config_path = Some(PathBuf::from(value));
            }
            s if s.starts_with("--config=") => {
                global.config_path = Some(PathBuf::from(&s["--config=".len()..]));
                i += 1;
            }
            "--profile" => {
                let value = require_value(argv, &mut i, "--profile")?;
                global.profile = Some(value);
            }
            s if s.starts_with("--profile=") => {
                global.profile = Some(s["--profile=".len()..].to_string());
                i += 1;
            }
            "--agent-url" => {
                let value = require_value(argv, &mut i, "--agent-url")?;
                global.agent_url = Some(value);
            }
            s if s.starts_with("--agent-url=") => {
                global.agent_url = Some(s["--agent-url=".len()..].to_string());
                i += 1;
            }
            "--output" => {
                let value = require_value(argv, &mut i, "--output")?;
                global.output = parse_output(&value)?;
            }
            s if s.starts_with("--output=") => {
                global.output = parse_output(&s["--output=".len()..])?;
                i += 1;
            }
            "--timeout-ms" => {
                let value = require_value(argv, &mut i, "--timeout-ms")?;
                global.timeout_ms = Some(parse_u64(&value, "--timeout-ms")?);
            }
            "--no-color" => {
                global.no_color = true;
                i += 1;
            }
            "--quiet" => {
                global.verbosity = Verbosity::Quiet;
                i += 1;
            }
            "--verbose" => {
                global.verbosity = Verbosity::Verbose;
                i += 1;
            }
            s if s.starts_with("--") => {
                return Err(CliError::Usage(format!("unknown global flag `{s}`")));
            }
            _ => break,
        }
    }
    // Phase 2: subcommand dispatch.
    if i >= argv.len() {
        return Ok(ParseOutcome::Help);
    }
    let cmd = &argv[i];
    let rest = &argv[i + 1..];
    let subcommand = match cmd.as_str() {
        "version" => Subcommand::Version,
        "doctor" => Subcommand::Doctor(parse_doctor(rest)?),
        "deploy" => Subcommand::Deploy(parse_deploy(rest)?),
        "rollback" => Subcommand::Rollback(parse_rollback(rest)?),
        "status" => Subcommand::Status(parse_status(rest)?),
        "infer" => Subcommand::Infer(parse_infer(rest)?),
        "logs" => Subcommand::Logs(parse_logs(rest)?),
        other => return Err(CliError::Usage(format!("unknown command `{other}`"))),
    };
    Ok(ParseOutcome::Run(ParsedArgs { global, subcommand }))
}

/// Outcome of parsing argv. `Help` and `Version` are short-circuit
/// paths the binary handles before any subcommand work.
#[derive(Clone, Debug)]
pub enum ParseOutcome {
    Help,
    Version,
    Run(ParsedArgs),
}

#[must_use]
pub fn usage_text() -> &'static str {
    USAGE
}

fn require_value(argv: &[String], cursor: &mut usize, flag: &str) -> CliResult<String> {
    let Some(value) = argv.get(*cursor + 1) else {
        return Err(CliError::Usage(format!("flag `{flag}` requires a value")));
    };
    *cursor += 2;
    Ok(value.clone())
}

fn parse_output(value: &str) -> CliResult<OutputMode> {
    match value {
        "human" => Ok(OutputMode::Human),
        "json" => Ok(OutputMode::Json),
        other => Err(CliError::Usage(format!(
            "--output must be `human` or `json`, got `{other}`"
        ))),
    }
}

fn parse_u64(value: &str, flag: &str) -> CliResult<u64> {
    value
        .parse::<u64>()
        .map_err(|_| CliError::Usage(format!("{flag} requires a non-negative integer")))
}

fn parse_doctor(rest: &[String]) -> CliResult<DoctorArgs> {
    let mut args = DoctorArgs::default();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--skip-agent" => {
                args.skip_agent = true;
                i += 1;
            }
            "-h" | "--help" => {
                return Err(CliError::Usage(
                    "doctor: --skip-agent | --output <human|json>".into(),
                ));
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `doctor`: {other}"
                )));
            }
        }
    }
    Ok(args)
}

fn parse_deploy(rest: &[String]) -> CliResult<DeployArgs> {
    let mut bundle_path: Option<PathBuf> = None;
    let mut deployment_id: Option<String> = None;
    let mut expected_digest: Option<String> = None;
    let mut wait = true;
    let mut wait_timeout_ms = 120_000;
    let mut labels = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        match a.as_str() {
            "--deployment-id" => deployment_id = Some(require_value(rest, &mut i, a)?),
            "--expected-digest" => expected_digest = Some(require_value(rest, &mut i, a)?),
            "--no-wait" => {
                wait = false;
                i += 1;
            }
            "--wait-timeout-ms" => {
                let v = require_value(rest, &mut i, a)?;
                wait_timeout_ms = parse_u64(&v, "--wait-timeout-ms")?;
            }
            "--label" => {
                let v = require_value(rest, &mut i, a)?;
                let (k, val) = v
                    .split_once('=')
                    .ok_or_else(|| CliError::Usage("--label requires <key>=<value>".into()))?;
                labels.push((k.to_string(), val.to_string()));
            }
            "-h" | "--help" => {
                return Err(CliError::Usage(
                    "deploy <bundle> [--deployment-id <id>] [--expected-digest <algo:hex>] [--no-wait] [--wait-timeout-ms <n>] [--label <k>=<v>]"
                        .into(),
                ));
            }
            s if s.starts_with("--") => {
                return Err(CliError::Usage(format!("unknown flag for `deploy`: {s}")));
            }
            other => {
                if bundle_path.is_some() {
                    return Err(CliError::Usage(format!(
                        "deploy accepts one bundle path, got an extra `{other}`"
                    )));
                }
                bundle_path = Some(PathBuf::from(other));
                i += 1;
            }
        }
    }
    let bundle_path = bundle_path
        .ok_or_else(|| CliError::Usage("deploy requires a <bundle> path argument".into()))?;
    Ok(DeployArgs {
        bundle_path,
        deployment_id,
        expected_digest,
        wait,
        wait_timeout_ms,
        labels,
    })
}

fn parse_rollback(rest: &[String]) -> CliResult<RollbackArgs> {
    let mut args = RollbackArgs::default();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        match a.as_str() {
            "--reason" => args.reason = Some(require_value(rest, &mut i, a)?),
            "-h" | "--help" => {
                return Err(CliError::Usage("rollback [--reason <text>]".into()));
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `rollback`: {other}"
                )))
            }
        }
    }
    Ok(args)
}

fn parse_status(rest: &[String]) -> CliResult<StatusArgs> {
    let mut args = StatusArgs {
        observability_snapshot: None,
        include_quarantine: true,
    };
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        match a.as_str() {
            "--observability-snapshot" => {
                args.observability_snapshot = Some(PathBuf::from(require_value(rest, &mut i, a)?));
            }
            "--no-quarantine" => {
                args.include_quarantine = false;
                i += 1;
            }
            "-h" | "--help" => {
                return Err(CliError::Usage(
                    "status [--observability-snapshot <path>] [--no-quarantine]".into(),
                ));
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `status`: {other}"
                )))
            }
        }
    }
    Ok(args)
}

fn parse_infer(rest: &[String]) -> CliResult<InferArgs> {
    let mut input_path = None;
    let mut from_stdin = false;
    let mut serving_url = None;
    let mut timeout_ms = None;
    let mut output_path = None;
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        match a.as_str() {
            "--input" => input_path = Some(PathBuf::from(require_value(rest, &mut i, a)?)),
            "--stdin" => {
                from_stdin = true;
                i += 1;
            }
            "--serving-url" => serving_url = Some(require_value(rest, &mut i, a)?),
            "--timeout-ms" => {
                timeout_ms = Some(parse_u64(&require_value(rest, &mut i, a)?, "--timeout-ms")?);
            }
            "--output-file" => output_path = Some(PathBuf::from(require_value(rest, &mut i, a)?)),
            "-h" | "--help" => {
                return Err(CliError::Usage(
                    "infer (--input <path> | --stdin) [--serving-url <url>] [--timeout-ms <n>] [--output-file <path>]"
                        .into(),
                ));
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `infer`: {other}"
                )))
            }
        }
    }
    if input_path.is_none() && !from_stdin {
        return Err(CliError::Usage(
            "infer requires either `--input <path>` or `--stdin`".into(),
        ));
    }
    if input_path.is_some() && from_stdin {
        return Err(CliError::Usage(
            "infer accepts either `--input <path>` or `--stdin`, not both".into(),
        ));
    }
    Ok(InferArgs {
        input_path,
        from_stdin,
        serving_url,
        timeout_ms,
        output_path,
    })
}

fn parse_logs(rest: &[String]) -> CliResult<LogsArgs> {
    let mut args = LogsArgs {
        component: None,
        level: None,
        since_ms: None,
        tail: None,
        follow: false,
        correlation_id: None,
        source_override: None,
    };
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        match a.as_str() {
            "--component" => args.component = Some(require_value(rest, &mut i, a)?),
            "--level" => args.level = Some(require_value(rest, &mut i, a)?),
            "--since-ms" => {
                args.since_ms = Some(parse_u64(&require_value(rest, &mut i, a)?, "--since-ms")?);
            }
            "--tail" => args.tail = Some(parse_u64(&require_value(rest, &mut i, a)?, "--tail")?),
            "--follow" => {
                args.follow = true;
                i += 1;
            }
            "--correlation-id" => args.correlation_id = Some(require_value(rest, &mut i, a)?),
            "--source" => {
                args.source_override = Some(PathBuf::from(require_value(rest, &mut i, a)?));
            }
            "-h" | "--help" => {
                return Err(CliError::Usage(
                    "logs [--component <name>] [--level <name>] [--tail <n>] [--since-ms <n>] [--follow] [--correlation-id <id>] [--source <path>]"
                        .into(),
                ));
            }
            other => return Err(CliError::Usage(format!("unknown flag for `logs`: {other}"))),
        }
    }
    Ok(args)
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

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn help_short_circuits() {
        let out = parse(&argv(&["--help"])).unwrap();
        assert!(matches!(out, ParseOutcome::Help));
    }

    #[test]
    fn version_short_circuits() {
        let out = parse(&argv(&["-V"])).unwrap();
        assert!(matches!(out, ParseOutcome::Version));
    }

    #[test]
    fn deploy_requires_bundle_path() {
        let err = parse(&argv(&["deploy"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn deploy_parses_global_and_subcommand_flags() {
        let out = parse(&argv(&[
            "--profile",
            "remote-dev",
            "--output",
            "json",
            "--no-color",
            "deploy",
            "/tmp/bundle",
            "--deployment-id",
            "d-1",
            "--no-wait",
            "--label",
            "env=ci",
        ]))
        .unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        assert_eq!(parsed.global.profile.as_deref(), Some("remote-dev"));
        assert_eq!(parsed.global.output, OutputMode::Json);
        assert!(parsed.global.no_color);
        let Subcommand::Deploy(d) = parsed.subcommand else {
            panic!("expected Deploy");
        };
        assert_eq!(d.bundle_path, std::path::PathBuf::from("/tmp/bundle"));
        assert_eq!(d.deployment_id.as_deref(), Some("d-1"));
        assert!(!d.wait);
        assert_eq!(d.labels, vec![("env".to_string(), "ci".to_string())]);
    }

    #[test]
    fn infer_requires_input_or_stdin() {
        let err = parse(&argv(&["infer"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn infer_rejects_input_and_stdin_together() {
        let err = parse(&argv(&["infer", "--input", "x.json", "--stdin"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn rejects_unknown_global_flag() {
        let err = parse(&argv(&["--frob"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn rejects_unknown_subcommand() {
        let err = parse(&argv(&["wat"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn timeout_ms_must_be_integer() {
        let err = parse(&argv(&["--timeout-ms", "soon"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn logs_parses_filters() {
        let out = parse(&argv(&[
            "logs",
            "--component",
            "agent",
            "--level",
            "warn",
            "--tail",
            "50",
            "--follow",
        ]))
        .unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        let Subcommand::Logs(l) = parsed.subcommand else {
            panic!("expected Logs");
        };
        assert_eq!(l.component.as_deref(), Some("agent"));
        assert_eq!(l.level.as_deref(), Some("warn"));
        assert_eq!(l.tail, Some(50));
        assert!(l.follow);
    }
}
