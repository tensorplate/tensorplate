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
    /// Target an enrolled registry device by name for this invocation.
    pub device: Option<String>,
    /// Force the local (current-process) path, bypassing any default device.
    pub local: bool,
    pub output: Option<OutputMode>,
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
    Device(DeviceCommand),
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

/// A `device` registry subcommand.
#[derive(Clone, Debug)]
pub enum DeviceCommand {
    Add(DeviceAddArgs),
    List,
    Use(String),
    Sync(Option<String>),
    Prune {
        name: String,
        keep: Option<usize>,
        older_than_secs: Option<u64>,
    },
    Remove(String),
    Rename {
        old: String,
        new: String,
    },
}

#[derive(Clone, Debug)]
pub struct DeviceAddArgs {
    pub name: String,
    pub ssh_target: String,
    pub port: Option<u16>,
    pub run_as: Option<String>,
    pub import_dir: Option<PathBuf>,
    pub use_as_default: bool,
    pub no_verify: bool,
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
  device              Manage the local SSH device registry.
  version             Print CLI and protocol versions.

Global flags:
  --config <path>           CLI config file (default: $TENSORPLATE_CLI_CONFIG or none).
  --profile <name>          Named profile from the CLI config.
  --agent-url <host:port>   Override profile and target a loopback agent URL.
  --device <name>           Route the command to an enrolled registry device over SSH.
  --local                   Force the local path, bypassing any default device.
  --output <human|json>     Output mode (default: config output.mode or human).
  --timeout-ms <n>          Per-call agent timeout override.
  --no-color                Disable color in human output.
  --quiet / --verbose       Suppress / expand informational stderr.
  -h, --help                Print usage and exit.
  -V, --version             Print CLI version and exit.

Global flags may appear before or after the subcommand. For per-command help,
run `tensorplate <command> --help`.";

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
            _ if parse_global_flag(argv, &mut i, &mut global, true)? => {}
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
        "version" => {
            parse_version(rest, &mut global)?;
            Subcommand::Version
        }
        "doctor" => Subcommand::Doctor(parse_doctor(rest, &mut global)?),
        "deploy" => Subcommand::Deploy(parse_deploy(rest, &mut global)?),
        "rollback" => Subcommand::Rollback(parse_rollback(rest, &mut global)?),
        "status" => Subcommand::Status(parse_status(rest, &mut global)?),
        "infer" => Subcommand::Infer(parse_infer(rest, &mut global)?),
        "logs" => Subcommand::Logs(parse_logs(rest, &mut global)?),
        "device" => Subcommand::Device(parse_device(rest, &mut global)?),
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

fn parse_global_flag(
    argv: &[String],
    cursor: &mut usize,
    global: &mut GlobalArgs,
    allow_timeout: bool,
) -> CliResult<bool> {
    let arg = argv[*cursor].as_str();
    match arg {
        "--config" => {
            let value = require_value(argv, cursor, "--config")?;
            global.config_path = Some(PathBuf::from(value));
            Ok(true)
        }
        s if s.starts_with("--config=") => {
            global.config_path = Some(PathBuf::from(&s["--config=".len()..]));
            *cursor += 1;
            Ok(true)
        }
        "--profile" => {
            let value = require_value(argv, cursor, "--profile")?;
            global.profile = Some(value);
            Ok(true)
        }
        s if s.starts_with("--profile=") => {
            global.profile = Some(s["--profile=".len()..].to_string());
            *cursor += 1;
            Ok(true)
        }
        "--agent-url" => {
            let value = require_value(argv, cursor, "--agent-url")?;
            global.agent_url = Some(value);
            Ok(true)
        }
        s if s.starts_with("--agent-url=") => {
            global.agent_url = Some(s["--agent-url=".len()..].to_string());
            *cursor += 1;
            Ok(true)
        }
        "--device" => {
            let value = require_value(argv, cursor, "--device")?;
            global.device = Some(value);
            Ok(true)
        }
        s if s.starts_with("--device=") => {
            global.device = Some(s["--device=".len()..].to_string());
            *cursor += 1;
            Ok(true)
        }
        "--local" => {
            global.local = true;
            *cursor += 1;
            Ok(true)
        }
        "--output" => {
            let value = require_value(argv, cursor, "--output")?;
            global.output = Some(parse_output(&value)?);
            Ok(true)
        }
        s if s.starts_with("--output=") => {
            global.output = Some(parse_output(&s["--output=".len()..])?);
            *cursor += 1;
            Ok(true)
        }
        "--timeout-ms" if allow_timeout => {
            let value = require_value(argv, cursor, "--timeout-ms")?;
            global.timeout_ms = Some(parse_u64(&value, "--timeout-ms")?);
            Ok(true)
        }
        "--no-color" => {
            global.no_color = true;
            *cursor += 1;
            Ok(true)
        }
        "--quiet" => {
            global.verbosity = Verbosity::Quiet;
            *cursor += 1;
            Ok(true)
        }
        "--verbose" => {
            global.verbosity = Verbosity::Verbose;
            *cursor += 1;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn parse_u64(value: &str, flag: &str) -> CliResult<u64> {
    value
        .parse::<u64>()
        .map_err(|_| CliError::Usage(format!("{flag} requires a non-negative integer")))
}

fn parse_doctor(rest: &[String], global: &mut GlobalArgs) -> CliResult<DoctorArgs> {
    let mut args = DoctorArgs::default();
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
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

fn parse_version(rest: &[String], global: &mut GlobalArgs) -> CliResult<()> {
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
        match rest[i].as_str() {
            "-h" | "--help" => return Err(CliError::Usage("version".into())),
            other => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `version`: {other}"
                )))
            }
        }
    }
    Ok(())
}

fn parse_deploy(rest: &[String], global: &mut GlobalArgs) -> CliResult<DeployArgs> {
    let mut bundle_path: Option<PathBuf> = None;
    let mut deployment_id: Option<String> = None;
    let mut expected_digest: Option<String> = None;
    let mut wait = true;
    let mut wait_timeout_ms = 120_000;
    let mut labels = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
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

fn parse_rollback(rest: &[String], global: &mut GlobalArgs) -> CliResult<RollbackArgs> {
    let mut args = RollbackArgs::default();
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
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

fn parse_status(rest: &[String], global: &mut GlobalArgs) -> CliResult<StatusArgs> {
    let mut args = StatusArgs {
        observability_snapshot: None,
        include_quarantine: true,
    };
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
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

fn parse_infer(rest: &[String], global: &mut GlobalArgs) -> CliResult<InferArgs> {
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
            _ if parse_global_flag(rest, &mut i, global, false)? => {}
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

fn parse_logs(rest: &[String], global: &mut GlobalArgs) -> CliResult<LogsArgs> {
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
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
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

const DEVICE_USAGE: &str = "device <subcommand>

Subcommands:
  add <name> --ssh <user@host> [--port <n>] [--run-as <user>] [--import-dir <path>] [--use] [--no-verify]
  list [--output <human|json>]
  use <name>
  sync [<name>]
  prune <name> [--keep <n>] [--older-than <dur>]
  remove <name>
  rename <old> <new>";

fn parse_u16(value: &str, flag: &str) -> CliResult<u16> {
    value
        .parse::<u16>()
        .map_err(|_| CliError::Usage(format!("{flag} requires an integer between 0 and 65535")))
}

fn parse_device(rest: &[String], global: &mut GlobalArgs) -> CliResult<DeviceCommand> {
    let mut i = 0;
    // Global flags may appear before the subcommand (e.g. `device --output json list`).
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
        break;
    }
    let Some(sub) = rest.get(i).cloned() else {
        return Err(CliError::Usage(DEVICE_USAGE.into()));
    };
    let args = &rest[i + 1..];
    match sub.as_str() {
        "add" => parse_device_add(args, global),
        "list" => {
            parse_device_flags_only(args, global, "list")?;
            Ok(DeviceCommand::List)
        }
        "use" => Ok(DeviceCommand::Use(parse_device_one_name(
            args, global, "use",
        )?)),
        "sync" => Ok(DeviceCommand::Sync(parse_device_optional_name(
            args, global, "sync",
        )?)),
        "prune" => parse_device_prune(args, global),
        "remove" => Ok(DeviceCommand::Remove(parse_device_one_name(
            args, global, "remove",
        )?)),
        "rename" => parse_device_rename(args, global),
        "-h" | "--help" => Err(CliError::Usage(DEVICE_USAGE.into())),
        other => Err(CliError::Usage(format!(
            "unknown device subcommand `{other}`\n{DEVICE_USAGE}"
        ))),
    }
}

fn parse_device_add(rest: &[String], global: &mut GlobalArgs) -> CliResult<DeviceCommand> {
    let mut name: Option<String> = None;
    let mut ssh_target: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut run_as: Option<String> = None;
    let mut import_dir: Option<PathBuf> = None;
    let mut use_as_default = false;
    let mut no_verify = false;
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
        let a = &rest[i];
        match a.as_str() {
            "--ssh" => ssh_target = Some(require_value(rest, &mut i, a)?),
            "--port" => port = Some(parse_u16(&require_value(rest, &mut i, a)?, "--port")?),
            "--run-as" => run_as = Some(require_value(rest, &mut i, a)?),
            "--import-dir" => import_dir = Some(PathBuf::from(require_value(rest, &mut i, a)?)),
            "--use" => {
                use_as_default = true;
                i += 1;
            }
            "--no-verify" => {
                no_verify = true;
                i += 1;
            }
            "-h" | "--help" => {
                return Err(CliError::Usage(
                    "device add <name> --ssh <user@host> [--port <n>] [--run-as <user>] [--import-dir <path>] [--use] [--no-verify]"
                        .into(),
                ));
            }
            s if s.starts_with("--") => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `device add`: {s}"
                )));
            }
            other => {
                if name.is_some() {
                    return Err(CliError::Usage(format!(
                        "device add accepts one <name>, got an extra `{other}`"
                    )));
                }
                name = Some(other.to_string());
                i += 1;
            }
        }
    }
    let name =
        name.ok_or_else(|| CliError::Usage("device add requires a <name> argument".into()))?;
    let ssh_target = ssh_target
        .ok_or_else(|| CliError::Usage("device add requires `--ssh <user@host>`".into()))?;
    Ok(DeviceCommand::Add(DeviceAddArgs {
        name,
        ssh_target,
        port,
        run_as,
        import_dir,
        use_as_default,
        no_verify,
    }))
}

/// Parse an optional trailing `<name>` positional (used by `device sync`).
fn parse_device_optional_name(
    rest: &[String],
    global: &mut GlobalArgs,
    sub: &str,
) -> CliResult<Option<String>> {
    let mut name: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
        match rest[i].as_str() {
            "-h" | "--help" => return Err(CliError::Usage(format!("device {sub} [<name>]"))),
            s if s.starts_with("--") => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `device {sub}`: {s}"
                )));
            }
            other => {
                if name.is_some() {
                    return Err(CliError::Usage(format!(
                        "device {sub} accepts at most one <name>, got an extra `{other}`"
                    )));
                }
                name = Some(other.to_string());
                i += 1;
            }
        }
    }
    Ok(name)
}

fn parse_device_one_name(rest: &[String], global: &mut GlobalArgs, sub: &str) -> CliResult<String> {
    let mut name: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
        match rest[i].as_str() {
            "-h" | "--help" => return Err(CliError::Usage(format!("device {sub} <name>"))),
            s if s.starts_with("--") => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `device {sub}`: {s}"
                )));
            }
            other => {
                if name.is_some() {
                    return Err(CliError::Usage(format!(
                        "device {sub} accepts one <name>, got an extra `{other}`"
                    )));
                }
                name = Some(other.to_string());
                i += 1;
            }
        }
    }
    name.ok_or_else(|| CliError::Usage(format!("device {sub} requires a <name> argument")))
}

fn parse_device_prune(rest: &[String], global: &mut GlobalArgs) -> CliResult<DeviceCommand> {
    let mut name: Option<String> = None;
    let mut keep: Option<usize> = None;
    let mut older_than_secs: Option<u64> = None;
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
        let a = &rest[i];
        match a.as_str() {
            "--keep" => {
                let v = require_value(rest, &mut i, a)?;
                keep = Some(v.parse::<usize>().map_err(|_| {
                    CliError::Usage("--keep requires a non-negative integer".into())
                })?);
            }
            "--older-than" => {
                older_than_secs = Some(parse_duration_secs(&require_value(rest, &mut i, a)?)?);
            }
            "-h" | "--help" => {
                return Err(CliError::Usage(
                    "device prune <name> [--keep <n>] [--older-than <dur>]".into(),
                ));
            }
            s if s.starts_with("--") => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `device prune`: {s}"
                )));
            }
            other => {
                if name.is_some() {
                    return Err(CliError::Usage(format!(
                        "device prune accepts one <name>, got an extra `{other}`"
                    )));
                }
                name = Some(other.to_string());
                i += 1;
            }
        }
    }
    let name =
        name.ok_or_else(|| CliError::Usage("device prune requires a <name> argument".into()))?;
    if keep.is_none() && older_than_secs.is_none() {
        return Err(CliError::Usage(
            "device prune requires `--keep <n>` and/or `--older-than <dur>`".into(),
        ));
    }
    Ok(DeviceCommand::Prune {
        name,
        keep,
        older_than_secs,
    })
}

/// Parse a duration like `30s`, `15m`, `24h`, `7d`, or a bare seconds count.
fn parse_duration_secs(value: &str) -> CliResult<u64> {
    let value = value.trim();
    let (num, mult) = if let Some(n) = value.strip_suffix('d') {
        (n, 86_400)
    } else if let Some(n) = value.strip_suffix('h') {
        (n, 3_600)
    } else if let Some(n) = value.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = value.strip_suffix('s') {
        (n, 1)
    } else {
        (value, 1)
    };
    let parsed: u64 = num.parse().map_err(|_| {
        CliError::Usage(format!(
            "--older-than must be a duration like `7d`, `24h`, `30m`, `60s`, got `{value}`"
        ))
    })?;
    parsed
        .checked_mul(mult)
        .ok_or_else(|| CliError::Usage(format!("--older-than duration `{value}` is too large")))
}

fn parse_device_rename(rest: &[String], global: &mut GlobalArgs) -> CliResult<DeviceCommand> {
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
        match rest[i].as_str() {
            "-h" | "--help" => return Err(CliError::Usage("device rename <old> <new>".into())),
            s if s.starts_with("--") => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `device rename`: {s}"
                )));
            }
            other => {
                names.push(other.to_string());
                i += 1;
            }
        }
    }
    if names.len() != 2 {
        return Err(CliError::Usage(
            "device rename requires exactly <old> and <new>".into(),
        ));
    }
    Ok(DeviceCommand::Rename {
        old: names[0].clone(),
        new: names[1].clone(),
    })
}

fn parse_device_flags_only(rest: &[String], global: &mut GlobalArgs, sub: &str) -> CliResult<()> {
    let mut i = 0;
    while i < rest.len() {
        if parse_global_flag(rest, &mut i, global, true)? {
            continue;
        }
        match rest[i].as_str() {
            "-h" | "--help" => {
                return Err(CliError::Usage(format!(
                    "device {sub} [--output <human|json>]"
                )));
            }
            other => {
                return Err(CliError::Usage(format!(
                    "unknown flag for `device {sub}`: {other}"
                )));
            }
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
        assert_eq!(parsed.global.output, Some(OutputMode::Json));
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

    #[test]
    fn output_flag_is_accepted_after_subcommand() {
        let out = parse(&argv(&["doctor", "--skip-agent", "--output", "json"])).unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        assert_eq!(parsed.global.output, Some(OutputMode::Json));
        let Subcommand::Doctor(d) = parsed.subcommand else {
            panic!("expected Doctor");
        };
        assert!(d.skip_agent);
    }

    #[test]
    fn global_timeout_is_accepted_after_subcommand() {
        let out = parse(&argv(&["status", "--timeout-ms", "250"])).unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        assert_eq!(parsed.global.timeout_ms, Some(250));
    }

    #[test]
    fn version_accepts_output_after_subcommand() {
        let out = parse(&argv(&["version", "--output=json"])).unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        assert_eq!(parsed.global.output, Some(OutputMode::Json));
        assert!(matches!(parsed.subcommand, Subcommand::Version));
    }

    #[test]
    fn device_add_parses_name_ssh_and_flags() {
        let out = parse(&argv(&[
            "device",
            "add",
            "orin-lab",
            "--ssh",
            "reid@orin.local",
            "--port",
            "2222",
            "--run-as",
            "tensorplate",
            "--import-dir",
            "/srv/tp/import",
            "--use",
        ]))
        .unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        let Subcommand::Device(DeviceCommand::Add(a)) = parsed.subcommand else {
            panic!("expected Device::Add");
        };
        assert_eq!(a.name, "orin-lab");
        assert_eq!(a.ssh_target, "reid@orin.local");
        assert_eq!(a.port, Some(2222));
        assert_eq!(a.run_as.as_deref(), Some("tensorplate"));
        assert_eq!(
            a.import_dir.as_deref(),
            Some(std::path::Path::new("/srv/tp/import"))
        );
        assert!(a.use_as_default);
    }

    #[test]
    fn device_add_requires_ssh_target() {
        let err = parse(&argv(&["device", "add", "orin"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn device_list_accepts_global_output_flag() {
        let out = parse(&argv(&["device", "list", "--output", "json"])).unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        assert_eq!(parsed.global.output, Some(OutputMode::Json));
        assert!(matches!(
            parsed.subcommand,
            Subcommand::Device(DeviceCommand::List)
        ));
    }

    #[test]
    fn device_rename_requires_two_names() {
        let err = parse(&argv(&["device", "rename", "only-one"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
        let out = parse(&argv(&["device", "rename", "old", "new"])).unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        let Subcommand::Device(DeviceCommand::Rename { old, new }) = parsed.subcommand else {
            panic!("expected Device::Rename");
        };
        assert_eq!(old, "old");
        assert_eq!(new, "new");
    }

    #[test]
    fn device_without_subcommand_is_usage_error() {
        let err = parse(&argv(&["device"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn device_and_local_global_flags_parse() {
        let out = parse(&argv(&["--device", "orin", "status"])).unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        assert_eq!(parsed.global.device.as_deref(), Some("orin"));
        assert!(!parsed.global.local);
        assert!(matches!(parsed.subcommand, Subcommand::Status(_)));

        let out = parse(&argv(&["--local", "status"])).unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        assert!(parsed.global.local);
        assert!(parsed.global.device.is_none());
    }

    #[test]
    fn device_add_no_verify_flag_parses() {
        let out = parse(&argv(&[
            "device",
            "add",
            "orin",
            "--ssh",
            "reid@orin.local",
            "--no-verify",
        ]))
        .unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        let Subcommand::Device(DeviceCommand::Add(a)) = parsed.subcommand else {
            panic!("expected Device::Add");
        };
        assert!(a.no_verify);
    }

    #[test]
    fn device_prune_parses_policies_and_requires_one() {
        let out = parse(&argv(&[
            "device",
            "prune",
            "orin",
            "--keep",
            "3",
            "--older-than",
            "7d",
        ]))
        .unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        let Subcommand::Device(DeviceCommand::Prune {
            name,
            keep,
            older_than_secs,
        }) = parsed.subcommand
        else {
            panic!("expected Device::Prune");
        };
        assert_eq!(name, "orin");
        assert_eq!(keep, Some(3));
        assert_eq!(older_than_secs, Some(7 * 86_400));

        // Requires at least one policy.
        let err = parse(&argv(&["device", "prune", "orin"])).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn duration_parsing_covers_units() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("15m").unwrap(), 900);
        assert_eq!(parse_duration_secs("24h").unwrap(), 86_400);
        assert_eq!(parse_duration_secs("2d").unwrap(), 172_800);
        assert_eq!(parse_duration_secs("90").unwrap(), 90);
        assert!(parse_duration_secs("soon").is_err());
    }

    #[test]
    fn device_sync_parses_optional_name() {
        let out = parse(&argv(&["device", "sync"])).unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        assert!(matches!(
            parsed.subcommand,
            Subcommand::Device(DeviceCommand::Sync(None))
        ));

        let out = parse(&argv(&["device", "sync", "orin"])).unwrap();
        let ParseOutcome::Run(parsed) = out else {
            panic!("expected Run");
        };
        let Subcommand::Device(DeviceCommand::Sync(Some(name))) = parsed.subcommand else {
            panic!("expected Device::Sync(Some)");
        };
        assert_eq!(name, "orin");
    }
}
