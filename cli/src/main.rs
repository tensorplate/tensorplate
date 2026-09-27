// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F01: `tensorplate` binary entry point.
//
// All real work lives in [`tensorplate_cli`]. The binary parses argv,
// loads the config, constructs the production client factory, and maps
// typed [`CliError`] values onto the documented exit-code table.

#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;

use tensorplate_cli::args::{self, OutputMode, ParseOutcome, ParsedArgs, Subcommand};
use tensorplate_cli::client::{AgentClient, NetAgentClient};
use tensorplate_cli::config::{CliConfig, ConfigSource, ResolvedCliConfig};
use tensorplate_cli::error::{CliError, CliResult};
use tensorplate_cli::output::Renderer;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let stderr = std::io::stderr();
    let mut stdout = std::io::stdout();
    let mut stderr_lock = stderr.lock();
    let exit = match drive(&argv, &mut stdout, &mut stderr_lock) {
        Ok(()) => 0u8,
        Err(err) => {
            // A device-routed remote command already forwarded its own output;
            // mirror its exit code without rendering a second error.
            if !err.error.already_reported() {
                // Best-effort: ignore renderer IO errors. If stderr is closed
                // the OS will signal SIGPIPE before this returns anyway.
                let _ = err
                    .renderer
                    .render_error(&mut stderr_lock, err.command, &err.error);
            }
            err.error.exit_code().as_u8()
        }
    };
    let _ = stdout.flush();
    ExitCode::from(exit)
}

fn command_of(argv: &[String]) -> &'static str {
    const COMMANDS: &[&str] = &[
        "doctor", "deploy", "status", "infer", "logs", "rollback", "undeploy", "recover", "device",
        "bundle", "version",
    ];
    for token in argv {
        if let Some(name) = COMMANDS.iter().find(|c| **c == token) {
            return name;
        }
    }
    "tensorplate"
}

#[derive(Debug)]
struct DriveError {
    error: CliError,
    /// The renderer to report this failure with. Carries the output mode
    /// and any process-level warning, so a `--output json` caller sees a
    /// packaged config that was found and not used in the error envelope
    /// too, not only on a run that succeeded.
    renderer: Renderer,
    command: &'static str,
}

fn drive<O: Write, E: Write>(
    argv: &[String],
    stdout: &mut O,
    stderr: &mut E,
) -> Result<(), Box<DriveError>> {
    let fallback_mode = explicit_output_mode(argv).unwrap_or(OutputMode::Human);
    let outcome = args::parse(argv).map_err(|error| {
        Box::new(DriveError {
            error,
            renderer: Renderer::new(fallback_mode),
            command: command_of(argv),
        })
    })?;
    let parsed = match outcome {
        ParseOutcome::Help => {
            writeln!(stdout, "{}", args::usage_text()).map_err(|error| {
                Box::new(DriveError {
                    error: CliError::from(error),
                    renderer: Renderer::new(fallback_mode),
                    command: "tensorplate",
                })
            })?;
            return Ok(());
        }
        ParseOutcome::Version => {
            writeln!(
                stdout,
                "tensorplate {} (protocol {})",
                tensorplate_cli::version(),
                tensorplate_protocol::PROTOCOL_VERSION
            )
            .map_err(|error| {
                Box::new(DriveError {
                    error: CliError::from(error),
                    renderer: Renderer::new(fallback_mode),
                    command: "version",
                })
            })?;
            return Ok(());
        }
        ParseOutcome::Run(parsed) => parsed,
    };
    let command = command_label(&parsed.subcommand);
    let resolved = CliConfig::resolve(parsed.global.config_path.as_deref()).map_err(|error| {
        Box::new(DriveError {
            error,
            renderer: Renderer::new(fallback_mode),
            command,
        })
    })?;
    run_resolved(parsed, resolved, command, stdout, stderr)
}

/// Everything `drive` does once config resolution has answered: render
/// the fallback warning, refuse commands an unusable packaged config
/// blocks, and run the command with the loader's verdict in hand.
///
/// Separate from `drive` because resolution reads the fixed packaged
/// path; this half takes the resolution as input, so tests can drive it
/// with a packaged config the loader rejects.
fn run_resolved<O: Write, E: Write>(
    parsed: ParsedArgs,
    resolved: ResolvedCliConfig,
    command: &'static str,
    stdout: &mut O,
    stderr: &mut E,
) -> Result<(), Box<DriveError>> {
    let cfg = resolved.config;
    let output_mode = tensorplate_cli::effective_output_mode(&parsed.global, &cfg);
    let warnings: Vec<String> = resolved.warning.clone().into_iter().collect();
    let renderer = Renderer::with_warnings(output_mode, warnings.clone())
        .with_verbosity(parsed.global.verbosity);
    report_config_warning(&renderer, stderr, resolved.warning.as_deref());
    if let Some(error) = blocking_install_fault(&resolved.source, &parsed.subcommand) {
        return Err(Box::new(DriveError {
            error,
            renderer,
            command,
        }));
    }
    let factory = |profile: &tensorplate_cli::ResolvedProfile| -> CliResult<Box<dyn AgentClient>> {
        Ok(Box::new(NetAgentClient::new(profile)))
    };
    let rejection = resolved.source.install_fault().map(str::to_owned);
    tensorplate_cli::run_with_warnings(parsed, cfg, warnings, rejection, factory, stdout, stderr)
        .map_err(|error| {
            Box::new(DriveError {
                error,
                renderer,
                command,
            })
        })
}

/// Surface a config file that was found but not used, so a packaged
/// install whose settings are not in effect never stays invisible.
///
/// This is the human channel only. In JSON mode stderr carries the error
/// envelope and nothing else — [`Renderer::render_error`] writes exactly
/// one document there — so a bare line ahead of it would hand every
/// `--output json` caller that parses stderr a syntax error instead of a
/// typed failure. That caller is not left without the warning: `drive`
/// builds its renderer with [`Renderer::with_warnings`], which stamps the
/// same text into the envelope's `warnings` array on both the ok and the
/// error path.
fn report_config_warning<E: Write>(renderer: &Renderer, stderr: &mut E, warning: Option<&str>) {
    let Some(warning) = warning else {
        return;
    };
    // Best-effort, like the error renderer in `main`: a closed stderr must
    // not change the command's exit code.
    let _ = renderer.info(stderr, warning);
}

/// Raise an unusable packaged conffile as the config error it is, except
/// for the two commands that exist to diagnose exactly that, and `bundle`,
/// which reads no profile: it provisions from a local directory into this
/// host's import directory and never talks to the agent.
///
/// `doctor` is what the docs tell an operator to run when an install
/// misbehaves, and it reports the loader's rejection as a failing
/// `config_files` finding, so it exits non-zero rather than calling the
/// install healthy; `version` says what is installed and reads nothing
/// from the config. Both answer from the built-in defaults, with
/// [`report_config_warning`] saying the packaged settings are not in
/// effect. Aborting before either runs would take the
/// diagnostic away at the moment it is needed — a state this branch made
/// reachable by reading the conffile at all. Every other command needs the
/// configured profile, so for those the fault stays fatal.
fn blocking_install_fault(source: &ConfigSource, command: &Subcommand) -> Option<CliError> {
    let fault = source.install_fault()?;
    if matches!(
        command,
        Subcommand::Doctor(_) | Subcommand::Version | Subcommand::Bundle(_)
    ) {
        return None;
    }
    Some(CliError::Config(fault.to_string()))
}

fn command_label(command: &Subcommand) -> &'static str {
    match command {
        Subcommand::Doctor(_) => "doctor",
        Subcommand::Deploy(_) => "deploy",
        Subcommand::Rollback(_) => "rollback",
        Subcommand::Undeploy(_) => "undeploy",
        Subcommand::Recover(_) => "recover",
        Subcommand::Status(_) => "status",
        Subcommand::Infer(_) => "infer",
        Subcommand::Logs(_) => "logs",
        Subcommand::Device(_) => "device",
        Subcommand::Bundle(_) => "bundle",
        Subcommand::Version => "version",
    }
}

fn explicit_output_mode(argv: &[String]) -> Option<OutputMode> {
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--output" => {
                let value = argv.get(i + 1)?;
                return match value.as_str() {
                    "human" => Some(OutputMode::Human),
                    "json" => Some(OutputMode::Json),
                    _ => None,
                };
            }
            s if s.starts_with("--output=") => {
                return match &s["--output=".len()..] {
                    "human" => Some(OutputMode::Human),
                    "json" => Some(OutputMode::Json),
                    _ => None,
                };
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    // Same allowance every other test module in this crate takes: these
    // tests build real temp files, and a failed fixture is a test bug that
    // should abort the test, not be threaded through a Result.
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use tensorplate_cli::args::Verbosity;

    #[test]
    fn command_of_returns_subcommand_name() {
        assert_eq!(
            command_of(&["--output".into(), "json".into(), "deploy".into()]),
            "deploy"
        );
        assert_eq!(command_of(&["doctor".into()]), "doctor");
        assert_eq!(command_of(&[]), "tensorplate");
        assert_eq!(command_of(&["--help".into()]), "tensorplate");
    }

    const WARNING: &str =
        "tensorplate: cannot read the packaged cli config `/etc/tensorplate/cli.json`";

    #[test]
    fn a_config_warning_is_written_in_human_mode() {
        let mut err = Vec::new();
        report_config_warning(&Renderer::new(OutputMode::Human), &mut err, Some(WARNING));
        assert_eq!(String::from_utf8_lossy(&err), format!("{WARNING}\n"));
    }

    /// `--help` promises `--quiet` suppresses informational stderr. The
    /// config warning is the first line a scripted caller meets, so it is
    /// the one that has to honour it; a `--output json` caller still reads
    /// the same text from the envelope's `warnings` array.
    #[test]
    fn a_config_warning_is_dropped_under_quiet() {
        let mut err = Vec::new();
        report_config_warning(
            &Renderer::new(OutputMode::Human).with_verbosity(Verbosity::Quiet),
            &mut err,
            Some(WARNING),
        );
        assert!(
            err.is_empty(),
            "--quiet must suppress it: {:?}",
            String::from_utf8_lossy(&err)
        );
    }

    /// `--output json` callers parse stderr as one envelope document. A
    /// plain warning line ahead of it makes that parse fail, so the note
    /// is dropped in JSON mode; the envelope and the exit code already
    /// carry the outcome.
    #[test]
    fn a_config_warning_never_precedes_the_json_error_envelope() {
        let mut err = Vec::new();
        report_config_warning(&Renderer::new(OutputMode::Json), &mut err, Some(WARNING));
        assert!(
            err.is_empty(),
            "stderr must stay the JSON envelope alone, got {:?}",
            String::from_utf8_lossy(&err)
        );
    }

    #[test]
    fn no_config_warning_writes_nothing() {
        let mut err = Vec::new();
        report_config_warning(&Renderer::new(OutputMode::Human), &mut err, None);
        assert!(err.is_empty());
    }

    /// Resolve a real unusable conffile rather than hand-building the
    /// source, so the test breaks if resolution stops producing it.
    fn unusable_packaged_config() -> ConfigSource {
        let td = tempfile::tempdir().expect("tempdir");
        let system = td.path().join("cli.json");
        std::fs::write(&system, "{not json").expect("write conffile");
        let resolved =
            CliConfig::resolve_from(None, None, &system).expect("an unusable conffile resolves");
        assert!(resolved.source.install_fault().is_some());
        resolved.source
    }

    /// The command the docs name for diagnosing a broken install, and the
    /// one that reports what is installed, must survive the fault they are
    /// there to report. Reading the conffile at all is what made this
    /// state reachable.
    #[test]
    fn doctor_and_version_still_run_on_an_unusable_packaged_config() {
        let source = unusable_packaged_config();
        assert!(blocking_install_fault(
            &source,
            &Subcommand::Doctor(args::DoctorArgs {
                skip_agent: true,
                record: None,
            })
        )
        .is_none());
        assert!(blocking_install_fault(&source, &Subcommand::Version).is_none());
    }

    /// Provisioning reads only the provisioning manifest and a local
    /// directory, never the CLI config, so a packaged config the loader
    /// rejects does not stand in its way.
    #[test]
    fn bundle_provision_still_runs_on_an_unusable_packaged_config() {
        let command = Subcommand::Bundle(args::BundleCommand::Provision(args::ProvisionArgs {
            name: "smolvla-fixture".to_string(),
            from: std::path::PathBuf::from("/nonexistent"),
            manifest: None,
            into: None,
        }));
        assert!(blocking_install_fault(&unusable_packaged_config(), &command).is_none());
    }

    /// The binary's own wiring: a packaged config the loader rejects is
    /// handed to doctor, which fails instead of reporting a healthy
    /// install. Without the hand-off doctor answers from the defaults and
    /// exits 0 while every command that needs the profile fails.
    #[test]
    fn doctor_run_by_the_binary_fails_on_a_rejected_packaged_config() {
        let td = tempfile::tempdir().expect("tempdir");
        let run_doctor = |document: &str| {
            let system = td.path().join("cli.json");
            std::fs::write(&system, document).expect("write conffile");
            let resolved = CliConfig::resolve_from(None, None, &system).expect("resolves");
            let argv: Vec<String> = ["--output", "json", "doctor", "--skip-agent"]
                .iter()
                .map(ToString::to_string)
                .collect();
            let ParseOutcome::Run(parsed) = args::parse(&argv).expect("parses") else {
                panic!("doctor parses to a run");
            };
            let mut out = Vec::new();
            let mut err = Vec::new();
            let result = run_resolved(parsed, resolved, "doctor", &mut out, &mut err);
            (result, String::from_utf8(out).expect("utf-8"))
        };

        let (healthy, _) = run_doctor(
            r#"{"schema_version":"0.1","default_profile":"local","profiles":{"local":{"mode":"local"}}}"#,
        );
        assert!(
            healthy.is_ok(),
            "an accepted config must leave doctor healthy"
        );

        let (rejected, out) = run_doctor(
            r#"{"schema_version":"0.1","default_profile":"missing","profiles":{"local":{"mode":"local"}}}"#,
        );
        let error = rejected.expect_err("doctor must fail on a rejected packaged config");
        assert!(
            matches!(error.error, CliError::DoctorFindings { .. }),
            "expected failing findings, got {:?}",
            error.error
        );
        assert!(
            out.contains("rejected by the CLI"),
            "the doctor report must say why: {out}"
        );
    }

    /// Every other command needs the configured profile. Running one on
    /// the built-in defaults would target a socket the operator never
    /// configured, which is the silent-fallback half of #203.
    #[test]
    fn an_unusable_packaged_config_still_fails_commands_that_need_the_profile() {
        let source = unusable_packaged_config();
        let error = blocking_install_fault(
            &source,
            &Subcommand::Logs(args::LogsArgs {
                component: None,
                level: None,
                since_ms: None,
                tail: None,
                follow: false,
                correlation_id: None,
                source_override: None,
            }),
        )
        .expect("logs must not run on the defaults");
        assert_eq!(error.exit_code().as_u8(), 2, "{error}");
        assert!(
            error.to_string().contains("cli.json"),
            "the error must name the conffile: {error}"
        );
    }

    /// A conffile that resolved normally raises nothing.
    #[test]
    fn a_usable_config_blocks_nothing() {
        assert!(blocking_install_fault(&ConfigSource::BuiltIn, &Subcommand::Version).is_none());
    }
}
