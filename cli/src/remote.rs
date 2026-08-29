// SPDX-License-Identifier: Apache-2.0
//
// SSH remote command adapter: run normal `tensorplate` commands against an
// enrolled registry device over plain OpenSSH.
//
// The adapter is orthogonal to the profile/transport layer: it does not route
// through a `ProfileMode`, it shells out to `ssh` and re-invokes the *remote*
// `tensorplate` CLI, which resolves its own local profile on the device. Every
// remote invocation forces `--local` so the remote CLI never recursively
// routes to its own default device.
//
// Remote commands are built with structured arguments and each token is
// POSIX-single-quoted before being handed to `ssh` — `ssh` runs the remote
// command through the remote shell, so quoting is the injection boundary.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};
use tensorplate_protocol::is_valid_deployment_id;

use crate::args::{DeployArgs, InferArgs, OutputMode, Subcommand};
use crate::error::{CliError, CliResult, ExitCode};
use crate::output::{Renderer, CLI_OUTPUT_SCHEMA_VERSION};
use crate::registry::{DeviceEntry, DeviceRegistry};
use crate::GlobalArgs;

/// Captured result of a remote invocation.
pub struct RemoteOutput {
    /// Remote process exit code (`ssh` yields 255 for its own failures).
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Injectable SSH runner. Production uses [`OpensshRunner`]; tests inject a
/// mock so no real SSH is required.
pub trait SshRunner {
    /// Run `[sudo -n -u <run-as> --] <remote tensorplate> --local <args...>`
    /// on `entry` over SSH, optionally piping `stdin` to the remote process.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Transport`] when `ssh` cannot be launched or does
    /// not complete.
    fn run(
        &self,
        entry: &DeviceEntry,
        args: &[String],
        stdin: Option<&[u8]>,
    ) -> CliResult<RemoteOutput>;

    /// Run an arbitrary, already-tokenized remote command over SSH (e.g. the
    /// `stat` used to vet a run-as binary). Each token is quoted individually.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Transport`] when `ssh` cannot be launched or does
    /// not complete.
    fn run_raw(
        &self,
        entry: &DeviceEntry,
        command: &[String],
        stdin: Option<&[u8]>,
    ) -> CliResult<RemoteOutput>;
}

/// Real runner that shells out to the system `ssh`.
pub struct OpensshRunner;

impl SshRunner for OpensshRunner {
    fn run(
        &self,
        entry: &DeviceEntry,
        args: &[String],
        stdin: Option<&[u8]>,
    ) -> CliResult<RemoteOutput> {
        spawn_ssh(entry, &tensorplate_command_string(entry, args), stdin)
    }

    fn run_raw(
        &self,
        entry: &DeviceEntry,
        command: &[String],
        stdin: Option<&[u8]>,
    ) -> CliResult<RemoteOutput> {
        spawn_ssh(entry, &quote_join(command), stdin)
    }
}

fn spawn_ssh(
    entry: &DeviceEntry,
    remote_command: &str,
    stdin: Option<&[u8]>,
) -> CliResult<RemoteOutput> {
    let mut ssh_args = Vec::new();
    if let Some(port) = entry.ssh_port {
        ssh_args.push("-p".to_string());
        ssh_args.push(port.to_string());
    }
    ssh_args.push(entry.ssh_target.clone());
    ssh_args.push(remote_command.to_string());

    let mut cmd = Command::new("ssh");
    cmd.args(&ssh_args);
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| CliError::Transport {
        message: format!("failed to launch ssh: {e}"),
        hint: Some("is OpenSSH installed and on PATH?".into()),
    })?;
    if let Some(bytes) = stdin {
        if let Some(mut sink) = child.stdin.take() {
            sink.write_all(bytes).map_err(|e| CliError::Transport {
                message: format!("failed to write ssh stdin: {e}"),
                hint: None,
            })?;
        }
    }
    let out = child.wait_with_output().map_err(|e| CliError::Transport {
        message: format!("ssh did not complete: {e}"),
        hint: None,
    })?;
    Ok(RemoteOutput {
        status: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Build the quoted remote command for a `tensorplate` invocation:
/// `[sudo -n -u <run-as> --] <bin> --local <args...>`. `--local` is always
/// forced so the remote CLI resolves its own config. A configured run-as user
/// goes through structured, non-interactive `sudo` arguments — no shell string
/// is interpolated.
fn tensorplate_command_string(entry: &DeviceEntry, args: &[String]) -> String {
    let mut tokens: Vec<String> = Vec::new();
    if let Some(user) = entry.remote_run_as.as_deref() {
        tokens.extend(
            ["sudo", "-n", "-u", user, "--"]
                .iter()
                .map(|s| (*s).to_string()),
        );
    }
    let bin = tensorplate_binary_for_execution(entry);
    tokens.push(bin);
    tokens.push("--local".to_string());
    tokens.extend(args.iter().cloned());
    quote_join(&tokens)
}

fn tensorplate_binary_for_execution(entry: &DeviceEntry) -> String {
    entry.remote_tensorplate.as_deref().map_or_else(
        || {
            if entry.remote_run_as.is_some() {
                DEFAULT_REMOTE_TENSORPLATE.to_string()
            } else {
                "tensorplate".to_string()
            }
        },
        |p| p.to_string_lossy().into_owned(),
    )
}

fn quote_join(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|t| shell_quote(t))
        .collect::<Vec<_>>()
        .join(" ")
}

/// POSIX single-quote a token so the remote shell treats it as one literal
/// argument. Embedded single quotes are escaped as `'\''`.
fn shell_quote(token: &str) -> String {
    let mut out = String::with_capacity(token.len() + 2);
    out.push('\'');
    for c in token.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Where a command should run.
pub enum Route {
    /// Current-process (local socket / URL) path.
    Local,
    /// An enrolled device reached over SSH.
    Device { name: String, entry: DeviceEntry },
}

/// Resolve the routing decision from the global flags and the registry.
///
/// Precedence (highest first): `--local`, `--device <name>`,
/// `--profile`/`--agent-url`, the registry's `default_device`, else local.
///
/// # Errors
///
/// Returns [`CliError`] when `--device` names an unenrolled device or the
/// registry cannot be read for an explicit device request. A missing or
/// malformed registry never blocks the local path.
pub fn resolve_route(global: &GlobalArgs) -> CliResult<Route> {
    if global.local {
        return Ok(Route::Local);
    }
    if let Some(name) = &global.device {
        let path = DeviceRegistry::resolve_path()?;
        let registry = DeviceRegistry::load(&path)?;
        return device_route(&registry, name);
    }
    if global.profile.is_some() || global.agent_url.is_some() {
        return Ok(Route::Local);
    }
    // Consult the default device. A missing or unreadable registry must not
    // break local commands, so only a cleanly-loaded default routes remotely.
    if let Ok(path) = DeviceRegistry::resolve_path() {
        if let Ok(registry) = DeviceRegistry::load(&path) {
            if let Some(default) = registry.default_device.clone() {
                return device_route(&registry, &default);
            }
        }
    }
    Ok(Route::Local)
}

fn device_route(registry: &DeviceRegistry, name: &str) -> CliResult<Route> {
    let entry = registry.devices.get(name).cloned().ok_or_else(|| {
        CliError::Usage(format!(
            "device `{name}` is not enrolled; run `tensorplate device add {name} --ssh <user@host>`"
        ))
    })?;
    Ok(Route::Device {
        name: name.to_string(),
        entry,
    })
}

/// Shared context for a device-routed invocation.
struct RemoteCtx<'a> {
    runner: &'a dyn SshRunner,
    entry: &'a DeviceEntry,
    device_name: &'a str,
    timeout_ms: Option<u64>,
    renderer: &'a Renderer,
}

/// Local options that affect how the remote command is invoked and rendered.
pub struct RouteOptions<'a> {
    pub renderer: &'a Renderer,
    pub timeout_ms: Option<u64>,
}

/// Route an operational subcommand to `entry` over SSH.
///
/// # Errors
///
/// Returns a typed [`CliError`] for unroutable commands, SSH transport
/// failures, malformed or version-incompatible remote output, and mirrors the
/// remote command's exit code via [`CliError::RemoteExit`].
pub fn route<O: Write, E: Write>(
    runner: &dyn SshRunner,
    entry: &DeviceEntry,
    device_name: &str,
    subcommand: &Subcommand,
    options: RouteOptions<'_>,
    stdout: &mut O,
    stderr: &mut E,
) -> CliResult<()> {
    let ctx = RemoteCtx {
        runner,
        entry,
        device_name,
        timeout_ms: options.timeout_ms,
        renderer: options.renderer,
    };
    // Human runs get a one-line banner; JSON runs carry the device in the
    // envelope instead.
    options.renderer.info(
        stderr,
        &format!("→ device `{device_name}` ({})", entry.ssh_target),
    )?;
    match subcommand {
        Subcommand::Infer(opts) => run_remote_infer(&ctx, opts, stdout, stderr),
        other => {
            let args = build_remote_args(other)?;
            run_forwarded(&ctx, &args, None, stdout, stderr)
        }
    }
}

/// Build the remote `tensorplate` argv (subcommand + flags) for the commands
/// that forward without local file handling. Path flags are Jetson-local.
fn build_remote_args(subcommand: &Subcommand) -> CliResult<Vec<String>> {
    match subcommand {
        Subcommand::Status(a) => {
            let mut v = vec!["status".to_string()];
            if !a.include_quarantine {
                v.push("--no-quarantine".to_string());
            }
            if let Some(p) = &a.observability_snapshot {
                v.push("--observability-snapshot".to_string());
                v.push(p.to_string_lossy().into_owned());
            }
            Ok(v)
        }
        Subcommand::Rollback(a) => {
            let mut v = vec!["rollback".to_string()];
            if let Some(r) = &a.reason {
                v.push("--reason".to_string());
                v.push(r.clone());
            }
            Ok(v)
        }
        Subcommand::Logs(a) => {
            if a.follow {
                return Err(CliError::Usage(
                    "`logs --follow` is not supported over `--device` yet; omit `--follow`".into(),
                ));
            }
            let mut v = vec!["logs".to_string()];
            if let Some(c) = &a.component {
                v.push("--component".to_string());
                v.push(c.clone());
            }
            if let Some(l) = &a.level {
                v.push("--level".to_string());
                v.push(l.clone());
            }
            if let Some(s) = a.since_ms {
                v.push("--since-ms".to_string());
                v.push(s.to_string());
            }
            if let Some(t) = a.tail {
                v.push("--tail".to_string());
                v.push(t.to_string());
            }
            if let Some(id) = &a.correlation_id {
                v.push("--correlation-id".to_string());
                v.push(id.clone());
            }
            if let Some(src) = &a.source_override {
                v.push("--source".to_string());
                v.push(src.to_string_lossy().into_owned());
            }
            Ok(v)
        }
        Subcommand::Doctor(a) => {
            let mut v = vec!["doctor".to_string()];
            if a.skip_agent {
                v.push("--skip-agent".to_string());
            }
            Ok(v)
        }
        Subcommand::Version => Ok(vec!["version".to_string()]),
        Subcommand::Deploy(_) => Err(CliError::Internal(
            "deploy must route through route_deploy (bundle staging)".into(),
        )),
        Subcommand::Infer(_) => Err(CliError::Internal(
            "infer must route through run_remote_infer".into(),
        )),
        Subcommand::Device(_) => Err(CliError::Internal(
            "device subcommands are local-only and not remotely routable".into(),
        )),
    }
}

/// Run a remote command and forward its output in the local output mode:
/// human output is streamed verbatim; JSON output is parsed, version-checked,
/// and re-emitted with an additive top-level `device` field.
fn run_forwarded<O: Write, E: Write>(
    ctx: &RemoteCtx<'_>,
    args: &[String],
    stdin: Option<&[u8]>,
    stdout: &mut O,
    stderr: &mut E,
) -> CliResult<()> {
    let mode = ctx.renderer.mode();
    let mut remote_args = args.to_vec();
    append_timeout_arg(&mut remote_args, ctx.timeout_ms);
    remote_args.push("--output".to_string());
    remote_args.push(output_flag(mode).to_string());
    let out = ctx.runner.run(ctx.entry, &remote_args, stdin)?;
    ssh_transport_guard(&out)?;
    match mode {
        OutputMode::Human => {
            if !out.stderr.is_empty() {
                write!(stderr, "{}", out.stderr)?;
            }
            stdout.write_all(&out.stdout)?;
        }
        OutputMode::Json => {
            if out.status == 0 {
                write_json_envelope(ctx, &out.stdout, stdout)?;
            } else {
                write_json_error_envelope(ctx, &out, stderr)?;
            }
        }
    }
    mirror_exit(out.status)
}

/// Route `infer`: read the input locally, pipe it to the remote over stdin,
/// and write any `--output-file` locally.
fn run_remote_infer<O: Write, E: Write>(
    ctx: &RemoteCtx<'_>,
    opts: &InferArgs,
    stdout: &mut O,
    stderr: &mut E,
) -> CliResult<()> {
    let input = read_infer_input(opts)?;
    let mut args = vec!["infer".to_string(), "--stdin".to_string()];
    if let Some(url) = &opts.serving_url {
        args.push("--serving-url".to_string());
        args.push(url.clone());
    }
    if let Some(t) = opts.timeout_ms.or(ctx.timeout_ms) {
        args.push("--timeout-ms".to_string());
        args.push(t.to_string());
    }

    if let Some(out_path) = &opts.output_path {
        // A local output file needs the structured response, so force JSON and
        // render it locally in the originally requested output mode.
        args.push("--output".to_string());
        args.push("json".to_string());
        let out = ctx.runner.run(ctx.entry, &args, Some(&input))?;
        ssh_transport_guard(&out)?;
        if out.status != 0 {
            write_json_error_envelope(ctx, &out, stderr)?;
            return mirror_exit(out.status);
        }
        let envelope = parse_and_check_envelope(&out.stdout)?;
        let result = infer_result_payload(&envelope)?;
        std::fs::write(out_path, serde_json::to_vec_pretty(result)?).map_err(|e| {
            CliError::Io(format!(
                "failed to write --output-file `{}`: {e}",
                out_path.display()
            ))
        })?;
        write_infer_output_file_success(ctx, envelope, stdout)?;
        mirror_exit(out.status)
    } else {
        run_forwarded(ctx, &args, Some(&input), stdout, stderr)
    }
}

fn read_infer_input(opts: &InferArgs) -> CliResult<Vec<u8>> {
    let bytes = if let Some(path) = &opts.input_path {
        std::fs::read(path).map_err(|e| {
            CliError::Usage(format!("failed to read --input `{}`: {e}", path.display()))
        })?
    } else {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| CliError::Io(format!("failed to read stdin: {e}")))?;
        buf
    };
    serde_json::from_slice::<Value>(&bytes)
        .map_err(|e| CliError::Usage(format!("infer input is not valid JSON: {e}")))?;
    Ok(bytes)
}

fn output_flag(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::Human => "human",
        OutputMode::Json => "json",
    }
}

fn append_timeout_arg(args: &mut Vec<String>, timeout_ms: Option<u64>) {
    if let Some(timeout_ms) = timeout_ms {
        args.push("--timeout-ms".to_string());
        args.push(timeout_ms.to_string());
    }
}

fn ssh_transport_guard(out: &RemoteOutput) -> CliResult<()> {
    if out.status == 255 {
        return Err(CliError::Transport {
            message: format!("ssh failed: {}", out.stderr.trim()),
            hint: Some("check the SSH target, host-key trust, and network reachability".into()),
        });
    }
    Ok(())
}

fn write_json_envelope<O: Write>(
    ctx: &RemoteCtx<'_>,
    bytes: &[u8],
    stdout: &mut O,
) -> CliResult<()> {
    let envelope = parse_and_check_envelope(bytes)?;
    let envelope = with_device_field(envelope, ctx.device_name, ctx.entry);
    writeln!(stdout, "{}", serde_json::to_string_pretty(&envelope)?)?;
    Ok(())
}

fn write_json_error_envelope<E: Write>(
    ctx: &RemoteCtx<'_>,
    out: &RemoteOutput,
    stderr: &mut E,
) -> CliResult<()> {
    let bytes = if out.stderr.trim().is_empty() {
        out.stdout.as_slice()
    } else {
        out.stderr.as_bytes()
    };
    let envelope = parse_and_check_envelope(bytes)?;
    let envelope = with_device_field(envelope, ctx.device_name, ctx.entry);
    writeln!(stderr, "{}", serde_json::to_string_pretty(&envelope)?)?;
    Ok(())
}

fn write_infer_output_file_success<O: Write>(
    ctx: &RemoteCtx<'_>,
    envelope: Value,
    stdout: &mut O,
) -> CliResult<()> {
    match ctx.renderer.mode() {
        OutputMode::Human => {
            let human = render_remote_infer_human(&envelope)?;
            writeln!(stdout, "{human}")?;
        }
        OutputMode::Json => {
            let envelope = with_device_field(envelope, ctx.device_name, ctx.entry);
            writeln!(stdout, "{}", serde_json::to_string_pretty(&envelope)?)?;
        }
    }
    Ok(())
}

fn infer_result_payload(envelope: &Value) -> CliResult<&Value> {
    envelope
        .get("payload")
        .and_then(|payload| payload.get("result"))
        .ok_or_else(|| CliError::Transport {
            message: "remote infer JSON payload is missing `payload.result`".into(),
            hint: Some("upgrade the device's tensorplate to a matching version".into()),
        })
}

fn render_remote_infer_human(envelope: &Value) -> CliResult<String> {
    let payload = envelope.get("payload").ok_or_else(|| CliError::Transport {
        message: "remote infer JSON payload is missing `payload`".into(),
        hint: Some("upgrade the device's tensorplate to a matching version".into()),
    })?;
    let endpoint = payload
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("<remote>");
    let result = infer_result_payload(envelope)?;
    let status = result
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let request_id = result
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let mut out = format!("infer: endpoint={endpoint} status={status} request_id={request_id}\n");
    if let Some(outputs) = result.get("outputs").and_then(Value::as_array) {
        out.push_str(&format!("  outputs: {} tensor(s)\n", outputs.len()));
        for output in outputs.iter().take(8) {
            if let Some(name) = output.get("name").and_then(Value::as_str) {
                let shape = output
                    .get("tensor")
                    .and_then(|tensor| tensor.get("shape"))
                    .map(Value::to_string)
                    .unwrap_or_default();
                out.push_str(&format!("    - {name} shape={shape}\n"));
            }
        }
    }
    Ok(out)
}

fn parse_and_check_envelope(stdout: &[u8]) -> CliResult<Value> {
    let envelope: Value = serde_json::from_slice(stdout).map_err(|e| CliError::Transport {
        message: format!("remote returned output the CLI could not parse as JSON: {e}"),
        hint: Some("the remote tensorplate may be too old or wrote non-JSON to stdout".into()),
    })?;
    match envelope.get("schema_version").and_then(Value::as_str) {
        Some(v) if v == CLI_OUTPUT_SCHEMA_VERSION => Ok(envelope),
        Some(v) => Err(CliError::Transport {
            message: format!(
                "incompatible remote CLI output schema: got {v}, expected {CLI_OUTPUT_SCHEMA_VERSION}"
            ),
            hint: Some("upgrade the device's tensorplate to a matching version".into()),
        }),
        None => Err(CliError::Transport {
            message: "remote CLI output is missing `schema_version`".into(),
            hint: Some("the remote tensorplate may be too old".into()),
        }),
    }
}

fn with_device_field(mut envelope: Value, name: &str, entry: &DeviceEntry) -> Value {
    if let Value::Object(map) = &mut envelope {
        map.insert(
            "device".to_string(),
            json!({
                "name": name,
                "ssh_target": entry.ssh_target,
                "family": entry.device_family,
            }),
        );
    }
    envelope
}

fn mirror_exit(status: i32) -> CliResult<()> {
    if status == 0 {
        Ok(())
    } else {
        let code = u8::try_from(status).unwrap_or(1);
        Err(CliError::RemoteExit {
            code: ExitCode::from_u8(code),
        })
    }
}

/// Packaged default path to the remote `tensorplate` binary.
pub const DEFAULT_REMOTE_TENSORPLATE: &str = "/usr/bin/tensorplate";

/// Refreshable device facts fetched by `device sync`.
pub struct DeviceFacts {
    pub agent_version: Option<String>,
    pub protocol_version: Option<String>,
}

/// Reachability preflight for `device add`: run
/// `tensorplate --local status --output json` on the device (through the
/// configured run-as mode) and confirm a well-formed envelope comes back.
///
/// # Errors
///
/// Returns a typed [`CliError`] with an actionable hint when SSH fails or the
/// device-local agent cannot be reached.
pub fn preflight_reachable(runner: &dyn SshRunner, entry: &DeviceEntry) -> CliResult<()> {
    let out = runner.run(
        entry,
        &["status".into(), "--output".into(), "json".into()],
        None,
    )?;
    ssh_transport_guard(&out)?;
    if out.status != 0 {
        if looks_like_old_remote(&out) {
            return Err(old_remote_error());
        }
        return Err(CliError::Unavailable {
            message: format!(
                "could not reach the device-local agent on `{}` (remote exit {})",
                entry.ssh_target, out.status
            ),
            hint: Some(reachability_hint(entry)),
        });
    }
    parse_and_check_envelope(&out.stdout)?;
    Ok(())
}

/// Detect a pre-0.1.5 remote CLI, which rejects the `--local` flag it does not
/// know. The remote-invocation contract (`--local`, envelope-JSON `version`)
/// only exists in >= 0.1.5, so an older device cannot be routed to.
fn looks_like_old_remote(out: &RemoteOutput) -> bool {
    if out.status != 2 {
        return false;
    }
    let stderr = out.stderr.to_ascii_lowercase();
    stderr.contains("--local")
        || (stderr.contains("local")
            && (stderr.contains("unknown")
                || stderr.contains("unexpected")
                || stderr.contains("unrecognized")
                || stderr.contains("usage")))
}

fn old_remote_error() -> CliError {
    CliError::Unavailable {
        message: "the remote tensorplate is too old for `--device` routing".into(),
        hint: Some(
            "upgrade the device's tensorplate to >= 0.1.5; the `--local` remote-invocation contract does not exist in older releases"
                .into(),
        ),
    }
}

fn reachability_hint(entry: &DeviceEntry) -> String {
    if entry.remote_run_as.is_some() {
        "the non-interactive sudoers rule for the tensorplate binary may be missing; add a NOPASSWD entry, or SSH as a user that can reach the agent socket".to_string()
    } else {
        "SSH as a user that can reach the agent socket, configure `--run-as <user>` with a non-interactive sudoers rule, or make the agent socket group-accessible".to_string()
    }
}

/// Verify a run-as device's remote `tensorplate` binary is safe to invoke under
/// sudo: absolute, owned by root, and not group/other-writable. Resolves the
/// binary from the entry (or the packaged default) and `stat`s it over SSH.
///
/// # Errors
///
/// Returns [`CliError::Usage`] when the binary is relative, cannot be stat'd,
/// is not root-owned, or is group/other-writable.
pub fn verify_run_as_binary(runner: &dyn SshRunner, entry: &DeviceEntry) -> CliResult<()> {
    let bin = tensorplate_binary_for_execution(entry);
    if !std::path::Path::new(&bin).is_absolute() {
        return Err(CliError::Usage(format!(
            "run-as requires an absolute remote tensorplate path, got `{bin}`"
        )));
    }
    let out = runner.run_raw(
        entry,
        &["stat".into(), "-c".into(), "%u %a".into(), bin.clone()],
        None,
    )?;
    if out.status != 0 {
        return Err(CliError::Usage(format!(
            "could not stat the remote tensorplate binary `{bin}`: {}",
            out.stderr.trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (uid, mode) = parse_stat_owner_mode(text.trim()).ok_or_else(|| {
        CliError::Usage(format!(
            "unexpected stat output for `{bin}`: {}",
            text.trim()
        ))
    })?;
    if uid != 0 {
        return Err(CliError::Usage(format!(
            "run-as binary `{bin}` must be owned by root (uid 0), got uid {uid}"
        )));
    }
    if mode & 0o022 != 0 {
        return Err(CliError::Usage(format!(
            "run-as binary `{bin}` must not be group/other-writable (mode {mode:o})"
        )));
    }
    Ok(())
}

fn parse_stat_owner_mode(s: &str) -> Option<(u32, u32)> {
    let mut it = s.split_whitespace();
    let uid = it.next()?.parse::<u32>().ok()?;
    let mode = u32::from_str_radix(it.next()?, 8).ok()?;
    Some((uid, mode))
}

/// Fetch refreshable facts for `device sync` from the remote
/// `version --output json`.
///
/// # Errors
///
/// Returns a typed [`CliError`] when the device is unreachable or the version
/// envelope is malformed or version-incompatible.
pub fn fetch_version_facts(runner: &dyn SshRunner, entry: &DeviceEntry) -> CliResult<DeviceFacts> {
    let out = runner.run(
        entry,
        &["version".into(), "--output".into(), "json".into()],
        None,
    )?;
    ssh_transport_guard(&out)?;
    if out.status != 0 {
        if looks_like_old_remote(&out) {
            return Err(old_remote_error());
        }
        return Err(CliError::Unavailable {
            message: format!(
                "remote `version` failed on `{}` (exit {})",
                entry.ssh_target, out.status
            ),
            hint: Some(reachability_hint(entry)),
        });
    }
    let envelope = parse_and_check_envelope(&out.stdout)?;
    let payload = envelope.get("payload");
    Ok(DeviceFacts {
        agent_version: payload
            .and_then(|p| p.get("cli"))
            .and_then(Value::as_str)
            .map(String::from),
        protocol_version: payload
            .and_then(|p| p.get("protocol"))
            .and_then(Value::as_str)
            .map(String::from),
    })
}

/// Current UTC time as an RFC3339 `YYYY-MM-DDThh:mm:ssZ` string.
#[must_use]
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_epoch_rfc3339(secs)
}

fn format_epoch_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

// Howard Hinnant's days-from-civil inverse (proleptic Gregorian, UTC).
fn civil_from_days(z0: i64) -> (i64, u32, u32) {
    let z = z0 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_shifted = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_shifted + 2) / 5 + 1) as u32;
    let month = (if month_shifted < 10 {
        month_shifted + 3
    } else {
        month_shifted - 9
    }) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

// ── Deploy staging ─────────────────────────────────────────────────────────

/// Injectable copier that stages a local bundle onto the device. Production
/// uses [`RsyncScpCopier`]; tests inject a mock so no real transfer is needed.
pub trait BundleCopier {
    /// Copy the contents of `local_bundle` into `remote_dest` on `entry`.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Transport`] when the copy tool cannot be launched
    /// or the transfer fails.
    fn copy_dir(
        &self,
        local_bundle: &Path,
        entry: &DeviceEntry,
        remote_dest: &str,
    ) -> CliResult<()>;
}

/// Real copier: prefers `rsync`, falls back to `scp`.
pub struct RsyncScpCopier;

impl BundleCopier for RsyncScpCopier {
    fn copy_dir(
        &self,
        local_bundle: &Path,
        entry: &DeviceEntry,
        remote_dest: &str,
    ) -> CliResult<()> {
        if command_available("rsync") {
            rsync_copy(local_bundle, entry, remote_dest)
        } else {
            scp_copy(local_bundle, entry, remote_dest)
        }
    }
}

fn command_available(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn rsync_copy(local: &Path, entry: &DeviceEntry, remote_dest: &str) -> CliResult<()> {
    // A trailing slash on both sides copies the bundle's contents into
    // remote_dest, creating remote_dest (rsync makes the final path component).
    let mut src = local.as_os_str().to_os_string();
    src.push("/");
    let dest = format!("{}:{remote_dest}/", entry.ssh_target);
    let mut cmd = Command::new("rsync");
    cmd.arg("-a");
    if let Some(port) = entry.ssh_port {
        cmd.arg("-e").arg(format!("ssh -p {port}"));
    }
    cmd.arg(&src).arg(&dest);
    run_copy(cmd, "rsync")
}

fn scp_copy(local: &Path, entry: &DeviceEntry, remote_dest: &str) -> CliResult<()> {
    // scp copies the local directory to remote_dest (which must not yet exist),
    // so the parent import dir is created beforehand but remote_dest is not.
    let dest = format!("{}:{remote_dest}", entry.ssh_target);
    let mut cmd = Command::new("scp");
    cmd.arg("-r");
    if let Some(port) = entry.ssh_port {
        cmd.arg("-P").arg(port.to_string());
    }
    cmd.arg(local).arg(&dest);
    run_copy(cmd, "scp")
}

fn run_copy(mut cmd: Command, tool: &str) -> CliResult<()> {
    let out = cmd.output().map_err(|e| CliError::Transport {
        message: format!("failed to launch {tool}: {e}"),
        hint: Some(format!("is {tool} installed and on PATH?")),
    })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(CliError::Transport {
            message: format!(
                "{tool} failed to stage the bundle: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            hint: Some(
                "confirm the remote import dir exists and is group-writable by the SSH user".into(),
            ),
        })
    }
}

/// Route `deploy` to a device: validate the local bundle, copy it to a staged
/// import path, then run the remote deploy transaction against that path.
///
/// # Errors
///
/// Returns a typed [`CliError`] for an invalid bundle, an unsafe deployment id,
/// a failed remote mkdir or copy, or a failed remote deploy (exit mirrored).
#[allow(clippy::too_many_arguments)]
pub fn route_deploy<O: Write, E: Write>(
    runner: &dyn SshRunner,
    copier: &dyn BundleCopier,
    entry: &DeviceEntry,
    device_name: &str,
    opts: &DeployArgs,
    options: RouteOptions<'_>,
    stdout: &mut O,
    stderr: &mut E,
) -> CliResult<()> {
    crate::commands::deploy::validate_local_bundle(&opts.bundle_path)?;
    let deployment_id = match &opts.deployment_id {
        Some(id) => {
            if !is_valid_deployment_id(id) {
                return Err(CliError::Usage(format!(
                    "deployment id `{id}` must be 1 to 128 bytes and contain only ASCII letters, digits, `-`, `_`, or `.`; `.` and `..` are reserved"
                )));
            }
            id.clone()
        }
        None => generate_deployment_id(),
    };
    let import_dir = import_dir_for(entry);
    let remote_dest = format!("{import_dir}/{deployment_id}");
    options.renderer.info(
        stderr,
        &format!("→ device `{device_name}`: staging bundle to {remote_dest}"),
    )?;
    // Ensure the import dir exists (one-time setup should already have; this is
    // a safety net). The bundle's own subdir is created by the copy.
    let mk = runner.run_raw(
        entry,
        &["mkdir".into(), "-p".into(), import_dir.clone()],
        None,
    )?;
    ssh_transport_guard(&mk)?;
    if mk.status != 0 {
        return Err(CliError::Unavailable {
            message: format!(
                "could not prepare the remote import dir `{import_dir}` (exit {})",
                mk.status
            ),
            hint: Some(
                "create it as group-writable by the SSH user (see docs/cli/device.md)".into(),
            ),
        });
    }
    copier.copy_dir(&opts.bundle_path, entry, &remote_dest)?;

    let mut args = vec![
        "deploy".to_string(),
        remote_dest,
        "--deployment-id".to_string(),
        deployment_id,
    ];
    if let Some(d) = &opts.expected_digest {
        args.push("--expected-digest".to_string());
        args.push(d.clone());
    }
    if !opts.wait {
        args.push("--no-wait".to_string());
    }
    args.push("--wait-timeout-ms".to_string());
    args.push(opts.wait_timeout_ms.to_string());
    for (k, v) in &opts.labels {
        args.push("--label".to_string());
        args.push(format!("{k}={v}"));
    }
    let ctx = RemoteCtx {
        runner,
        entry,
        device_name,
        timeout_ms: options.timeout_ms,
        renderer: options.renderer,
    };
    run_forwarded(&ctx, &args, None, stdout, stderr)
}

fn generate_deployment_id() -> String {
    format!("deploy-{}", uuid::Uuid::new_v4())
}

fn import_dir_for(entry: &DeviceEntry) -> String {
    entry.remote_import_dir.as_deref().map_or_else(
        || crate::registry::DEFAULT_REMOTE_IMPORT_DIR.to_string(),
        |p| p.to_string_lossy().into_owned(),
    )
}

/// Whether an existing import name can be joined beneath the import root.
///
/// New deployment IDs follow the stricter bounded protocol policy. Cleanup
/// also has to recognize safe names created by older clients, or those legacy
/// directories can never be reclaimed.
fn is_safe_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// ── Import pruning ─────────────────────────────────────────────────────────

/// Result of a `device prune`.
pub struct PruneReport {
    pub deleted: Vec<String>,
    pub kept: Vec<String>,
}

/// Reclaim remote import storage. Keeps the `keep` most-recent imports and/or
/// those newer than `older_than_secs`, always keeps protected active/in-flight
/// imports, and never deletes an import whose subdir name is not a safe segment.
///
/// # Errors
///
/// Returns a typed [`CliError`] when the device is unreachable, listing fails,
/// or a deletion fails.
pub fn prune_imports(
    runner: &dyn SshRunner,
    entry: &DeviceEntry,
    keep: Option<usize>,
    older_than_secs: Option<u64>,
) -> CliResult<PruneReport> {
    let import_dir = import_dir_for(entry);
    let protected = protected_import_names(runner, entry, &import_dir)?;
    let now = device_now(runner, entry)?;
    let mut imports = list_import_dirs(runner, entry, &import_dir)?;
    imports.sort_by(|a, b| b.0.cmp(&a.0)); // newest first

    let cutoff = older_than_secs.map(|o| now.saturating_sub(o));
    let mut kept = Vec::new();
    let mut deleted = Vec::new();
    for (index, (mtime, name)) in imports.iter().enumerate() {
        let is_protected = protected.contains(name);
        let within_keep = keep.map_or(false, |k| index < k);
        let newer_than_cutoff = cutoff.map_or(false, |c| *mtime >= c);
        if is_protected || within_keep || newer_than_cutoff || !is_safe_path_segment(name) {
            kept.push(name.clone());
            continue;
        }
        let rm = runner.run_raw(
            entry,
            &["rm".into(), "-rf".into(), format!("{import_dir}/{name}")],
            None,
        )?;
        ssh_transport_guard(&rm)?;
        if rm.status != 0 {
            return Err(CliError::Unavailable {
                message: format!("failed to remove import `{name}` (exit {})", rm.status),
                hint: None,
            });
        }
        deleted.push(name.clone());
    }
    Ok(PruneReport { deleted, kept })
}

fn protected_import_names(
    runner: &dyn SshRunner,
    entry: &DeviceEntry,
    import_dir: &str,
) -> CliResult<std::collections::BTreeSet<String>> {
    let out = runner.run(
        entry,
        &["status".into(), "--output".into(), "json".into()],
        None,
    )?;
    ssh_transport_guard(&out)?;
    if out.status != 0 {
        return Err(CliError::Unavailable {
            message: format!(
                "could not read device status before pruning (exit {})",
                out.status
            ),
            hint: Some(reachability_hint(entry)),
        });
    }
    let envelope = parse_and_check_envelope(&out.stdout)?;
    let agent = envelope.get("payload").and_then(|p| p.get("agent"));
    let mut protected = std::collections::BTreeSet::new();
    for key in ["active", "in_flight_transaction"] {
        if let Some(id) = agent
            .and_then(|a| a.get(key))
            .and_then(|record| record.get("deployment_id"))
            .and_then(Value::as_str)
            .filter(|id| is_safe_path_segment(id))
        {
            protected.insert(id.to_string());
        }
    }
    if let Some(name) = agent
        .and_then(|a| a.get("in_flight_transaction"))
        .and_then(|tx| tx.get("bundle_path"))
        .and_then(Value::as_str)
        .and_then(|path| import_name_from_path(import_dir, path))
        .filter(|name| is_safe_path_segment(name))
    {
        protected.insert(name);
    }
    Ok(protected)
}

fn import_name_from_path(import_dir: &str, path: &str) -> Option<String> {
    let dir = import_dir.trim_end_matches('/');
    let rest = path.strip_prefix(dir)?.strip_prefix('/')?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(rest.to_string())
}

fn device_now(runner: &dyn SshRunner, entry: &DeviceEntry) -> CliResult<u64> {
    let out = runner.run_raw(entry, &["date".into(), "+%s".into()], None)?;
    ssh_transport_guard(&out)?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|_| CliError::Transport {
            message: "could not read the device clock".into(),
            hint: None,
        })
}

fn list_import_dirs(
    runner: &dyn SshRunner,
    entry: &DeviceEntry,
    import_dir: &str,
) -> CliResult<Vec<(u64, String)>> {
    let out = runner.run_raw(
        entry,
        &[
            "find".into(),
            import_dir.to_string(),
            "-mindepth".into(),
            "1".into(),
            "-maxdepth".into(),
            "1".into(),
            "-type".into(),
            "d".into(),
            "-printf".into(),
            "%T@ %f\\n".into(),
        ],
        None,
    )?;
    ssh_transport_guard(&out)?;
    if out.status != 0 {
        let detail = out.stderr.trim();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return Err(CliError::Unavailable {
            message: format!(
                "could not list remote import dir `{import_dir}` (find exited {}){suffix}",
                out.status
            ),
            hint: Some(
                "confirm the remote import dir exists and is readable by the SSH user".into(),
            ),
        });
    }
    Ok(parse_find_dirs(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_find_dirs(text: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((mtime_raw, name)) = line.split_once(' ') else {
            continue;
        };
        // `%T@` is `epoch.fraction`; take the whole-seconds part.
        let secs = mtime_raw.split('.').next().unwrap_or(mtime_raw);
        if let Ok(mtime) = secs.parse::<u64>() {
            out.push((mtime, name.to_string()));
        }
    }
    out
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
        clippy::redundant_closure_for_method_calls,
        clippy::type_complexity
    )]

    use std::cell::RefCell;

    use super::*;
    use crate::args::{DoctorArgs, InferArgs, LogsArgs, RollbackArgs, StatusArgs};

    fn entry(target: &str) -> DeviceEntry {
        DeviceEntry {
            ssh_target: target.to_string(),
            ..DeviceEntry::default()
        }
    }

    fn forward(
        runner: &MockRunner,
        target: &str,
        args: &[&str],
        mode: OutputMode,
        out: &mut Vec<u8>,
        err: &mut Vec<u8>,
    ) -> CliResult<()> {
        forward_with_timeout(runner, target, args, mode, None, out, err)
    }

    fn forward_with_timeout(
        runner: &MockRunner,
        target: &str,
        args: &[&str],
        mode: OutputMode,
        timeout_ms: Option<u64>,
        out: &mut Vec<u8>,
        err: &mut Vec<u8>,
    ) -> CliResult<()> {
        let e = entry(target);
        let r = Renderer::new(mode);
        let ctx = RemoteCtx {
            runner,
            entry: &e,
            device_name: "orin",
            timeout_ms,
            renderer: &r,
        };
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        run_forwarded(&ctx, &owned, None, out, err)
    }

    fn infer(
        runner: &MockRunner,
        opts: &InferArgs,
        mode: OutputMode,
        out: &mut Vec<u8>,
        err: &mut Vec<u8>,
    ) -> CliResult<()> {
        infer_with_timeout(runner, opts, mode, None, out, err)
    }

    fn infer_with_timeout(
        runner: &MockRunner,
        opts: &InferArgs,
        mode: OutputMode,
        timeout_ms: Option<u64>,
        out: &mut Vec<u8>,
        err: &mut Vec<u8>,
    ) -> CliResult<()> {
        let e = entry("host");
        let r = Renderer::new(mode);
        let ctx = RemoteCtx {
            runner,
            entry: &e,
            device_name: "orin",
            timeout_ms,
            renderer: &r,
        };
        run_remote_infer(&ctx, opts, out, err)
    }

    struct MockRunner {
        calls: RefCell<Vec<(Vec<String>, Option<Vec<u8>>)>>,
        raw_calls: RefCell<Vec<Vec<String>>>,
        status: i32,
        stdout: Vec<u8>,
        stderr: String,
    }

    impl MockRunner {
        fn new(status: i32, stdout: &str) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                raw_calls: RefCell::new(Vec::new()),
                status,
                stdout: stdout.as_bytes().to_vec(),
                stderr: String::new(),
            }
        }

        fn last_args(&self) -> Vec<String> {
            self.calls.borrow().last().unwrap().0.clone()
        }

        fn last_stdin(&self) -> Option<Vec<u8>> {
            self.calls.borrow().last().unwrap().1.clone()
        }

        fn last_raw(&self) -> Vec<String> {
            self.raw_calls.borrow().last().unwrap().clone()
        }
    }

    impl SshRunner for MockRunner {
        fn run(
            &self,
            _entry: &DeviceEntry,
            args: &[String],
            stdin: Option<&[u8]>,
        ) -> CliResult<RemoteOutput> {
            self.calls
                .borrow_mut()
                .push((args.to_vec(), stdin.map(<[u8]>::to_vec)));
            Ok(RemoteOutput {
                status: self.status,
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
            })
        }

        fn run_raw(
            &self,
            _entry: &DeviceEntry,
            command: &[String],
            _stdin: Option<&[u8]>,
        ) -> CliResult<RemoteOutput> {
            self.raw_calls.borrow_mut().push(command.to_vec());
            Ok(RemoteOutput {
                status: self.status,
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
            })
        }
    }

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b;c"), "'a b;c'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn tensorplate_command_forces_local() {
        let e = entry("reid@orin.local");
        let s =
            tensorplate_command_string(&e, &["status".into(), "--output".into(), "json".into()]);
        assert!(s.contains("'--local'"));
        assert!(s.contains("'tensorplate'"));
        assert!(s.contains("'status'"));
        // No run-as user configured, so no sudo wrapping.
        assert!(!s.contains("sudo"));
    }

    #[test]
    fn tensorplate_command_wraps_run_as_with_structured_sudo() {
        let mut e = entry("host");
        e.remote_run_as = Some("tensorplate".into());
        let s = tensorplate_command_string(&e, &["status".into()]);
        assert!(s.starts_with(
            "'sudo' '-n' '-u' 'tensorplate' '--' '/usr/bin/tensorplate' '--local' 'status'"
        ));
    }

    #[test]
    fn remote_command_uses_configured_binary_path() {
        let mut e = entry("host");
        e.remote_tensorplate = Some(std::path::PathBuf::from("/usr/bin/tensorplate"));
        let s = tensorplate_command_string(&e, &["version".into()]);
        assert!(s.starts_with("'/usr/bin/tensorplate' '--local' 'version'"));
    }

    #[test]
    fn build_remote_args_reconstructs_flags() {
        let status = Subcommand::Status(StatusArgs {
            observability_snapshot: Some(std::path::PathBuf::from("/var/obs.json")),
            include_quarantine: false,
        });
        assert_eq!(
            build_remote_args(&status).unwrap(),
            vec![
                "status",
                "--no-quarantine",
                "--observability-snapshot",
                "/var/obs.json"
            ]
        );

        let rollback = Subcommand::Rollback(RollbackArgs {
            reason: Some("bad weights".into()),
        });
        assert_eq!(
            build_remote_args(&rollback).unwrap(),
            vec!["rollback", "--reason", "bad weights"]
        );

        let doctor = Subcommand::Doctor(DoctorArgs {
            skip_agent: true,
            record: None,
        });
        assert_eq!(
            build_remote_args(&doctor).unwrap(),
            vec!["doctor", "--skip-agent"]
        );

        assert_eq!(
            build_remote_args(&Subcommand::Version).unwrap(),
            vec!["version"]
        );
    }

    #[test]
    fn logs_follow_is_rejected_over_device() {
        let logs = Subcommand::Logs(LogsArgs {
            component: None,
            level: None,
            since_ms: None,
            tail: None,
            follow: true,
            correlation_id: None,
            source_override: None,
        });
        assert!(matches!(build_remote_args(&logs), Err(CliError::Usage(_))));
    }

    struct MockCopier {
        calls: RefCell<Vec<(std::path::PathBuf, String)>>,
    }

    impl BundleCopier for MockCopier {
        fn copy_dir(
            &self,
            local_bundle: &Path,
            _entry: &DeviceEntry,
            remote_dest: &str,
        ) -> CliResult<()> {
            self.calls
                .borrow_mut()
                .push((local_bundle.to_path_buf(), remote_dest.to_string()));
            Ok(())
        }
    }

    /// Runner that returns queued responses in call order (across run/run_raw).
    struct ScriptedRunner {
        responses: RefCell<std::collections::VecDeque<RemoteOutput>>,
        raw_args: RefCell<Vec<Vec<String>>>,
    }

    impl ScriptedRunner {
        fn new(responses: Vec<RemoteOutput>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                raw_args: RefCell::new(Vec::new()),
            }
        }

        fn out(status: i32, stdout: &str) -> RemoteOutput {
            RemoteOutput {
                status,
                stdout: stdout.as_bytes().to_vec(),
                stderr: String::new(),
            }
        }

        fn err(status: i32, stderr: &str) -> RemoteOutput {
            RemoteOutput {
                status,
                stdout: Vec::new(),
                stderr: stderr.to_string(),
            }
        }
    }

    impl SshRunner for ScriptedRunner {
        fn run(
            &self,
            _entry: &DeviceEntry,
            _args: &[String],
            _stdin: Option<&[u8]>,
        ) -> CliResult<RemoteOutput> {
            Ok(self
                .responses
                .borrow_mut()
                .pop_front()
                .expect("scripted response"))
        }

        fn run_raw(
            &self,
            _entry: &DeviceEntry,
            command: &[String],
            _stdin: Option<&[u8]>,
        ) -> CliResult<RemoteOutput> {
            self.raw_args.borrow_mut().push(command.to_vec());
            Ok(self
                .responses
                .borrow_mut()
                .pop_front()
                .expect("scripted response"))
        }
    }

    #[test]
    fn deploy_build_args_is_internal_error() {
        // Deploy routes through route_deploy, not build_remote_args.
        let deploy = Subcommand::Deploy(crate::args::DeployArgs {
            bundle_path: std::path::PathBuf::from("/tmp/b"),
            deployment_id: None,
            expected_digest: None,
            wait: true,
            wait_timeout_ms: 1000,
            labels: vec![],
        });
        assert!(matches!(
            build_remote_args(&deploy),
            Err(CliError::Internal(_))
        ));
    }

    #[test]
    fn route_deploy_stages_bundle_then_runs_remote_deploy() {
        let td = tempfile::TempDir::new().unwrap();
        let bundle = td.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("manifest.json"), b"{}").unwrap();
        let opts = crate::args::DeployArgs {
            bundle_path: bundle.clone(),
            deployment_id: Some("yolo-1".into()),
            expected_digest: None,
            wait: false,
            wait_timeout_ms: 1000,
            labels: vec![],
        };
        let runner = MockRunner::new(
            0,
            r#"{"schema_version":"0.1","command":"deploy","status":"ok","payload":{}}"#,
        );
        let copier = MockCopier {
            calls: RefCell::new(Vec::new()),
        };
        let mut e = entry("reid@orin.local");
        e.remote_import_dir = Some(std::path::PathBuf::from(
            "/var/lib/tensorplate/bundles/import",
        ));
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        route_deploy(
            &runner,
            &copier,
            &e,
            "orin",
            &opts,
            RouteOptions {
                renderer: &r,
                timeout_ms: None,
            },
            &mut out,
            &mut err,
        )
        .unwrap();
        let (local, dest) = copier.calls.borrow().last().unwrap().clone();
        assert_eq!(local, bundle);
        assert_eq!(dest, "/var/lib/tensorplate/bundles/import/yolo-1");
        // mkdir -p targeted the import dir.
        assert!(runner
            .last_raw()
            .contains(&"/var/lib/tensorplate/bundles/import".to_string()));
        // The remote deploy ran against the staged path with the deployment id.
        let args = runner.last_args();
        assert_eq!(args[0], "deploy");
        assert_eq!(args[1], "/var/lib/tensorplate/bundles/import/yolo-1");
        assert!(args.windows(2).any(|w| w == ["--deployment-id", "yolo-1"]));
        assert!(args.contains(&"--no-wait".to_string()));
    }

    #[test]
    fn route_deploy_rejects_unsafe_deployment_id() {
        let td = tempfile::TempDir::new().unwrap();
        let bundle = td.path().join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("manifest.json"), b"{}").unwrap();
        let opts = crate::args::DeployArgs {
            bundle_path: bundle,
            deployment_id: Some("../../etc".into()),
            expected_digest: None,
            wait: true,
            wait_timeout_ms: 1000,
            labels: vec![],
        };
        let runner = MockRunner::new(0, "");
        let copier = MockCopier {
            calls: RefCell::new(Vec::new()),
        };
        let r = Renderer::new(OutputMode::Human);
        let err = route_deploy(
            &runner,
            &copier,
            &entry("host"),
            "orin",
            &opts,
            RouteOptions {
                renderer: &r,
                timeout_ms: None,
            },
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
        assert!(copier.calls.borrow().is_empty());
    }

    #[test]
    fn prune_keeps_newest_and_active_deletes_the_rest() {
        // status → active deploy-a (oldest); date; find (unsorted); two rm.
        let responses = vec![
            ScriptedRunner::out(
                0,
                r#"{"schema_version":"0.1","command":"status","status":"ok","payload":{"agent":{"active":{"deployment_id":"deploy-a"}}}}"#,
            ),
            ScriptedRunner::out(0, "1000000\n"),
            ScriptedRunner::out(
                0,
                "999000 deploy-a\n999900 deploy-b\n999990 deploy-c\n999999 deploy-d\n",
            ),
            ScriptedRunner::out(0, ""),
            ScriptedRunner::out(0, ""),
        ];
        let runner = ScriptedRunner::new(responses);
        // keep=1 keeps deploy-d (newest); deploy-a is active (kept despite oldest);
        // deploy-c and deploy-b are deleted.
        let report = prune_imports(&runner, &entry("host"), Some(1), None).unwrap();
        assert_eq!(report.deleted, vec!["deploy-c", "deploy-b"]);
        assert!(report.kept.contains(&"deploy-d".to_string()));
        assert!(report.kept.contains(&"deploy-a".to_string()));
        // Deletions used rm -rf under the import dir.
        assert!(runner.raw_args.borrow().iter().any(|c| c[0] == "rm"));
    }

    #[test]
    fn prune_keeps_in_flight_import() {
        let responses = vec![
            ScriptedRunner::out(
                0,
                r#"{"schema_version":"0.1","command":"status","status":"ok","payload":{"agent":{"active":{"deployment_id":"deploy-a"},"in_flight_transaction":{"deployment_id":"deploy-b","bundle_path":"/var/lib/tensorplate/bundles/import/deploy-b"}}}}"#,
            ),
            ScriptedRunner::out(0, "1000000\n"),
            ScriptedRunner::out(0, "999000 deploy-a\n999100 deploy-b\n999200 deploy-c\n"),
            ScriptedRunner::out(0, ""),
        ];
        let runner = ScriptedRunner::new(responses);

        let report = prune_imports(&runner, &entry("host"), None, Some(1)).unwrap();

        assert_eq!(report.deleted, vec!["deploy-c"]);
        assert!(report.kept.contains(&"deploy-a".to_string()));
        assert!(report.kept.contains(&"deploy-b".to_string()));
        let raw_args = runner.raw_args.borrow();
        let rm_args: Vec<_> = raw_args
            .iter()
            .filter(|c| c.first().map(String::as_str) == Some("rm"))
            .collect();
        assert_eq!(rm_args.len(), 1);
        assert_eq!(
            rm_args[0][2],
            "/var/lib/tensorplate/bundles/import/deploy-c"
        );
    }

    #[test]
    fn prune_protects_active_and_reclaims_stale_safe_legacy_names() {
        let active = "a".repeat(tensorplate_protocol::MAX_DEPLOYMENT_ID_BYTES + 1);
        let stale = "b".repeat(tensorplate_protocol::MAX_DEPLOYMENT_ID_BYTES + 1);
        for legacy in [&active, &stale] {
            assert!(is_safe_path_segment(legacy));
            assert!(!is_valid_deployment_id(legacy));
        }
        let status = serde_json::json!({
            "schema_version": "0.1",
            "command": "status",
            "status": "ok",
            "payload": {"agent": {"active": {"deployment_id": active}}},
        })
        .to_string();
        let listed = format!("999000 {active}\n999100 {stale}\n");
        let runner = ScriptedRunner::new(vec![
            ScriptedRunner::out(0, &status),
            ScriptedRunner::out(0, "1000000\n"),
            ScriptedRunner::out(0, &listed),
            ScriptedRunner::out(0, ""),
        ]);

        let report = prune_imports(&runner, &entry("host"), None, Some(1)).unwrap();

        assert_eq!(report.deleted, vec![stale.clone()]);
        assert_eq!(report.kept, vec![active]);
        let expected = format!("/var/lib/tensorplate/bundles/import/{stale}");
        assert!(runner.raw_args.borrow().iter().any(|args| {
            args.first().map(String::as_str) == Some("rm")
                && args.get(2).map(String::as_str) == Some(expected.as_str())
        }));
    }

    #[test]
    fn list_import_dirs_fails_when_find_fails() {
        let runner = ScriptedRunner::new(vec![ScriptedRunner::err(1, "find: Permission denied\n")]);

        let err = list_import_dirs(
            &runner,
            &entry("host"),
            "/var/lib/tensorplate/bundles/import",
        )
        .unwrap_err();

        match err {
            CliError::Unavailable { message, .. } => {
                assert!(message.contains("could not list remote import dir"));
                assert!(message.contains("find exited 1"));
                assert!(message.contains("Permission denied"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn parse_find_dirs_parses_mtime_and_name() {
        let parsed = parse_find_dirs("1699999999.12345 deploy-a\n1700000000.9 deploy-b\n\n");
        assert_eq!(
            parsed,
            vec![
                (1_699_999_999, "deploy-a".to_string()),
                (1_700_000_000, "deploy-b".to_string())
            ]
        );
    }

    #[test]
    fn deployment_id_policy_rejects_traversal() {
        assert!(is_valid_deployment_id("deploy-abc_1.2"));
        assert!(!is_valid_deployment_id(".."));
        assert!(!is_valid_deployment_id("."));
        assert!(!is_valid_deployment_id("a/b"));
        assert!(!is_valid_deployment_id(""));
    }

    #[test]
    fn route_allows_run_as_devices() {
        let runner = MockRunner::new(0, "");
        let mut e = entry("host");
        e.remote_run_as = Some("tensorplate".into());
        let r = Renderer::new(OutputMode::Human);
        let mut out = Vec::new();
        let mut err = Vec::new();
        route(
            &runner,
            &e,
            "orin",
            &Subcommand::Version,
            RouteOptions {
                renderer: &r,
                timeout_ms: None,
            },
            &mut out,
            &mut err,
        )
        .unwrap();
        // The command was routed to the device; run-as sudo wrapping is applied
        // by the runner (see tensorplate_command_wraps_run_as_with_structured_sudo).
        assert!(!runner.calls.borrow().is_empty());
    }

    #[test]
    fn json_route_injects_device_and_mirrors_success() {
        let runner = MockRunner::new(
            0,
            r#"{"schema_version":"0.1","command":"status","status":"ok","payload":{"agent_state":"ready"}}"#,
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        forward(
            &runner,
            "reid@orin.local",
            &["status"],
            OutputMode::Json,
            &mut out,
            &mut err,
        )
        .unwrap();
        // The remote was asked for JSON.
        assert!(runner.last_args().contains(&"json".to_string()));
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["device"]["name"], "orin");
        assert_eq!(parsed["device"]["ssh_target"], "reid@orin.local");
        assert_eq!(parsed["payload"]["agent_state"], "ready");
    }

    #[test]
    fn human_route_forwards_stdout_verbatim() {
        let runner = MockRunner::new(0, "agent: ready\n");
        let mut out = Vec::new();
        let mut err = Vec::new();
        forward(
            &runner,
            "host",
            &["status"],
            OutputMode::Human,
            &mut out,
            &mut err,
        )
        .unwrap();
        assert!(runner.last_args().contains(&"human".to_string()));
        assert_eq!(String::from_utf8(out).unwrap(), "agent: ready\n");
    }

    #[test]
    fn global_timeout_is_forwarded_to_remote_commands() {
        let runner = MockRunner::new(0, "agent: ready\n");
        let mut out = Vec::new();
        let mut err = Vec::new();
        forward_with_timeout(
            &runner,
            "host",
            &["status"],
            OutputMode::Human,
            Some(250),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(
            runner.last_args(),
            vec!["status", "--timeout-ms", "250", "--output", "human"]
        );
    }

    #[test]
    fn nonzero_remote_exit_is_mirrored() {
        let runner = MockRunner::new(6, "error: nothing to roll back\n");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let e = forward(
            &runner,
            "host",
            &["rollback"],
            OutputMode::Human,
            &mut out,
            &mut err,
        )
        .unwrap_err();
        assert!(matches!(e, CliError::RemoteExit { code } if code == ExitCode::Unavailable));
        assert!(e.already_reported());
    }

    #[test]
    fn json_nonzero_remote_error_is_forwarded_from_stderr() {
        let mut runner = MockRunner::new(6, "");
        runner.stderr = r#"{"schema_version":"0.1","command":"rollback","status":"unavailable","error":{"code":"unsupported","message":"no previous active deployment"}}"#.into();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let e = forward(
            &runner,
            "reid@orin.local",
            &["rollback"],
            OutputMode::Json,
            &mut out,
            &mut err,
        )
        .unwrap_err();
        assert!(matches!(e, CliError::RemoteExit { code } if code == ExitCode::Unavailable));
        assert!(out.is_empty());
        let parsed: Value = serde_json::from_slice(&err).unwrap();
        assert_eq!(parsed["device"]["name"], "orin");
        assert_eq!(parsed["device"]["ssh_target"], "reid@orin.local");
        assert_eq!(parsed["error"]["message"], "no previous active deployment");
    }

    #[test]
    fn ssh_255_is_transport_error() {
        let mut runner = MockRunner::new(255, "");
        runner.stderr = "ssh: connect to host: Connection refused".into();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let e = forward(
            &runner,
            "host",
            &["status"],
            OutputMode::Json,
            &mut out,
            &mut err,
        )
        .unwrap_err();
        assert!(matches!(e, CliError::Transport { .. }));
    }

    #[test]
    fn bad_remote_schema_version_fails_closed() {
        let bad = br#"{"schema_version":"9.9","command":"status","status":"ok"}"#;
        assert!(matches!(
            parse_and_check_envelope(bad),
            Err(CliError::Transport { .. })
        ));
        assert!(matches!(
            parse_and_check_envelope(b"not json"),
            Err(CliError::Transport { .. })
        ));
    }

    #[test]
    fn infer_reads_input_and_pipes_over_stdin() {
        let td = tempfile::TempDir::new().unwrap();
        let input = td.path().join("in.json");
        std::fs::write(&input, br#"{"inputs":[]}"#).unwrap();
        let opts = InferArgs {
            input_path: Some(input),
            from_stdin: false,
            serving_url: Some("http://127.0.0.1:9000/infer".into()),
            timeout_ms: Some(1000),
            output_path: None,
        };
        let runner = MockRunner::new(
            0,
            r#"{"schema_version":"0.1","command":"infer","status":"ok","payload":{"ok":true}}"#,
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        infer(&runner, &opts, OutputMode::Json, &mut out, &mut err).unwrap();
        let args = runner.last_args();
        assert!(args.contains(&"--stdin".to_string()));
        assert!(args.contains(&"--serving-url".to_string()));
        assert_eq!(runner.last_stdin().unwrap(), br#"{"inputs":[]}"#);
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["device"]["name"], "orin");
    }

    #[test]
    fn infer_uses_global_timeout_when_command_timeout_is_absent() {
        let td = tempfile::TempDir::new().unwrap();
        let input = td.path().join("in.json");
        std::fs::write(&input, br#"{"inputs":[]}"#).unwrap();
        let opts = InferArgs {
            input_path: Some(input),
            from_stdin: false,
            serving_url: None,
            timeout_ms: None,
            output_path: None,
        };
        let runner = MockRunner::new(
            0,
            r#"{"schema_version":"0.1","command":"infer","status":"ok","payload":{"ok":true}}"#,
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        infer_with_timeout(
            &runner,
            &opts,
            OutputMode::Json,
            Some(250),
            &mut out,
            &mut err,
        )
        .unwrap();
        let args = runner.last_args();
        assert!(args.windows(2).any(|w| w == ["--timeout-ms", "250"]));
    }

    #[test]
    fn infer_output_file_is_written_locally() {
        let td = tempfile::TempDir::new().unwrap();
        let input = td.path().join("in.json");
        std::fs::write(&input, br#"{"inputs":[]}"#).unwrap();
        let out_file = td.path().join("resp.json");
        let opts = InferArgs {
            input_path: Some(input),
            from_stdin: false,
            serving_url: None,
            timeout_ms: None,
            output_path: Some(out_file.clone()),
        };
        let runner = MockRunner::new(
            0,
            r#"{"schema_version":"0.1","command":"infer","status":"ok","payload":{"endpoint":"http://127.0.0.1:18080/infer","endpoint_source":"agent-discovered","result":{"schema_version":"0.1","request_id":"req-1","status":"success","outputs":[{"name":"scores","tensor":{"shape":[1,3]}}]}}}"#,
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        infer(&runner, &opts, OutputMode::Human, &mut out, &mut err).unwrap();
        // Even in human mode, --output-file forces a JSON capture on the remote.
        assert!(runner.last_args().contains(&"json".to_string()));
        let written: Value = serde_json::from_slice(&std::fs::read(&out_file).unwrap()).unwrap();
        assert_eq!(written["request_id"], "req-1");
        assert_eq!(written["outputs"][0]["name"], "scores");
        assert_eq!(written.get("endpoint"), None);
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.starts_with(
            "infer: endpoint=http://127.0.0.1:18080/infer status=success request_id=req-1"
        ));
        assert!(stdout.contains("scores shape=[1,3]"));
        assert!(!stdout.trim_start().starts_with('{'));
    }

    #[test]
    fn preflight_accepts_reachable_and_rejects_unreachable() {
        let ok = MockRunner::new(
            0,
            r#"{"schema_version":"0.1","command":"status","status":"ok","payload":{}}"#,
        );
        preflight_reachable(&ok, &entry("host")).unwrap();
        // The probe forces the local status envelope.
        assert_eq!(ok.last_args(), vec!["status", "--output", "json"]);

        let unreachable = MockRunner::new(3, "");
        let e = preflight_reachable(&unreachable, &entry("host")).unwrap_err();
        assert!(matches!(e, CliError::Unavailable { .. }));
    }

    #[test]
    fn preflight_reports_old_remote_clearly() {
        // A pre-0.1.5 remote rejects `--local` with a usage error (exit 2).
        let mut old = MockRunner::new(2, "");
        old.stderr = "error: unknown global flag `--local`".into();
        let e = preflight_reachable(&old, &entry("host")).unwrap_err();
        assert!(matches!(e, CliError::Unavailable { .. }));
        assert!(e.hint().unwrap_or_default().contains("0.1.5"));

        // A generic non-usage failure keeps the reachability hint (not "too old").
        let denied = MockRunner::new(3, "");
        let e = preflight_reachable(&denied, &entry("host")).unwrap_err();
        assert!(!e.hint().unwrap_or_default().contains("0.1.5"));
    }

    #[test]
    fn verify_run_as_binary_enforces_ownership_and_perms() {
        let ok = MockRunner::new(0, "0 755\n");
        let mut e = entry("host");
        e.remote_run_as = Some("tensorplate".into());
        verify_run_as_binary(&ok, &e).unwrap();
        assert_eq!(
            ok.last_raw(),
            vec!["stat", "-c", "%u %a", "/usr/bin/tensorplate"]
        );

        let non_root = MockRunner::new(0, "1000 755\n");
        assert!(matches!(
            verify_run_as_binary(&non_root, &e).unwrap_err(),
            CliError::Usage(_)
        ));

        let group_writable = MockRunner::new(0, "0 775\n");
        assert!(matches!(
            verify_run_as_binary(&group_writable, &e).unwrap_err(),
            CliError::Usage(_)
        ));

        let mut relative = entry("host");
        relative.remote_run_as = Some("tensorplate".into());
        relative.remote_tensorplate = Some(std::path::PathBuf::from("bin/tensorplate"));
        assert!(matches!(
            verify_run_as_binary(&MockRunner::new(0, "0 755"), &relative).unwrap_err(),
            CliError::Usage(_)
        ));
    }

    #[test]
    fn fetch_version_facts_parses_cli_and_protocol() {
        let runner = MockRunner::new(
            0,
            r#"{"schema_version":"0.1","command":"version","status":"ok","payload":{"cli":"0.1.5","protocol":"0.1","bundle_format":"0.1"}}"#,
        );
        let facts = fetch_version_facts(&runner, &entry("host")).unwrap();
        assert_eq!(facts.agent_version.as_deref(), Some("0.1.5"));
        assert_eq!(facts.protocol_version.as_deref(), Some("0.1"));
    }

    #[test]
    fn rfc3339_formats_known_epochs() {
        assert_eq!(format_epoch_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_epoch_rfc3339(1_600_000_000), "2020-09-13T12:26:40Z");
    }
}
