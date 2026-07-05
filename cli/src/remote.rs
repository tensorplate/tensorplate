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
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::args::{InferArgs, OutputMode, Subcommand};
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
    /// Run `<remote tensorplate> --local <args...>` on `entry` over SSH,
    /// optionally piping `stdin` to the remote process.
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
        let ssh_args = build_ssh_args(entry, args);
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
}

fn build_ssh_args(entry: &DeviceEntry, args: &[String]) -> Vec<String> {
    let mut ssh_args = Vec::new();
    if let Some(port) = entry.ssh_port {
        ssh_args.push("-p".to_string());
        ssh_args.push(port.to_string());
    }
    ssh_args.push(entry.ssh_target.clone());
    ssh_args.push(remote_command_string(entry, args));
    ssh_args
}

fn remote_command_string(entry: &DeviceEntry, args: &[String]) -> String {
    let bin = entry.remote_tensorplate.as_deref().map_or_else(
        || "tensorplate".to_string(),
        |p| p.to_string_lossy().into_owned(),
    );
    let mut tokens = Vec::with_capacity(args.len() + 2);
    tokens.push(bin);
    tokens.push("--local".to_string());
    tokens.extend(args.iter().cloned());
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
    renderer: &'a Renderer,
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
    renderer: &Renderer,
    stdout: &mut O,
    stderr: &mut E,
) -> CliResult<()> {
    let ctx = RemoteCtx {
        runner,
        entry,
        device_name,
        renderer,
    };
    // Human runs get a one-line banner; JSON runs carry the device in the
    // envelope instead.
    renderer.info(
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
        Subcommand::Deploy(_) => Err(CliError::Unavailable {
            message: "deploy over `--device` is not available yet".into(),
            hint: Some(
                "remote deploy staging lands in a later change; run deploy on the device for now"
                    .into(),
            ),
        }),
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
            let envelope = parse_and_check_envelope(&out.stdout)?;
            let envelope = with_device_field(envelope, ctx.device_name, ctx.entry);
            writeln!(stdout, "{}", serde_json::to_string_pretty(&envelope)?)?;
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
    if let Some(t) = opts.timeout_ms {
        args.push("--timeout-ms".to_string());
        args.push(t.to_string());
    }

    if let Some(out_path) = &opts.output_path {
        // A local output file needs the structured response, so force JSON and
        // emit JSON on stdout regardless of the display mode.
        args.push("--output".to_string());
        args.push("json".to_string());
        let out = ctx.runner.run(ctx.entry, &args, Some(&input))?;
        ssh_transport_guard(&out)?;
        let envelope = parse_and_check_envelope(&out.stdout)?;
        if let Some(payload) = envelope.get("payload") {
            std::fs::write(out_path, serde_json::to_vec_pretty(payload)?).map_err(|e| {
                CliError::Io(format!(
                    "failed to write --output-file `{}`: {e}",
                    out_path.display()
                ))
            })?;
        }
        let envelope = with_device_field(envelope, ctx.device_name, ctx.entry);
        writeln!(stdout, "{}", serde_json::to_string_pretty(&envelope)?)?;
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

fn ssh_transport_guard(out: &RemoteOutput) -> CliResult<()> {
    if out.status == 255 {
        return Err(CliError::Transport {
            message: format!("ssh failed: {}", out.stderr.trim()),
            hint: Some("check the SSH target, host-key trust, and network reachability".into()),
        });
    }
    Ok(())
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
        let e = entry(target);
        let r = Renderer::new(mode);
        let ctx = RemoteCtx {
            runner,
            entry: &e,
            device_name: "orin",
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
        let e = entry("host");
        let r = Renderer::new(mode);
        let ctx = RemoteCtx {
            runner,
            entry: &e,
            device_name: "orin",
            renderer: &r,
        };
        run_remote_infer(&ctx, opts, out, err)
    }

    struct MockRunner {
        calls: RefCell<Vec<(Vec<String>, Option<Vec<u8>>)>>,
        status: i32,
        stdout: Vec<u8>,
        stderr: String,
    }

    impl MockRunner {
        fn new(status: i32, stdout: &str) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
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
    }

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b;c"), "'a b;c'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn ssh_args_include_port_target_and_forced_local() {
        let mut e = entry("reid@orin.local");
        e.ssh_port = Some(2222);
        let args = build_ssh_args(&e, &["status".into(), "--output".into(), "json".into()]);
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], "2222");
        assert_eq!(args[2], "reid@orin.local");
        // The remote command always forces --local.
        assert!(args[3].contains("'--local'"));
        assert!(args[3].contains("'tensorplate'"));
        assert!(args[3].contains("'status'"));
    }

    #[test]
    fn remote_command_uses_configured_binary_path() {
        let mut e = entry("host");
        e.remote_tensorplate = Some(std::path::PathBuf::from("/usr/bin/tensorplate"));
        let s = remote_command_string(&e, &["version".into()]);
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

        let doctor = Subcommand::Doctor(DoctorArgs { skip_agent: true });
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

    #[test]
    fn deploy_over_device_is_unavailable() {
        // A minimal DeployArgs; deploy routing is deferred.
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
            Err(CliError::Unavailable { .. })
        ));
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
            r#"{"schema_version":"0.1","command":"infer","status":"ok","payload":{"outputs":[1,2,3]}}"#,
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        infer(&runner, &opts, OutputMode::Human, &mut out, &mut err).unwrap();
        // Even in human mode, --output-file forces a JSON capture on the remote.
        assert!(runner.last_args().contains(&"json".to_string()));
        let written: Value = serde_json::from_slice(&std::fs::read(&out_file).unwrap()).unwrap();
        assert_eq!(written["outputs"], json!([1, 2, 3]));
    }
}
