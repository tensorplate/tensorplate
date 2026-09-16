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

use tensorplate_cli::args::{self, OutputMode, ParseOutcome, Subcommand};
use tensorplate_cli::client::{AgentClient, NetAgentClient};
use tensorplate_cli::config::CliConfig;
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
                let renderer = Renderer::new(err.output_mode);
                // Best-effort: ignore renderer IO errors. If stderr is closed
                // the OS will signal SIGPIPE before this returns anyway.
                let _ = renderer.render_error(&mut stderr_lock, err.command, &err.error);
            }
            err.error.exit_code().as_u8()
        }
    };
    let _ = stdout.flush();
    ExitCode::from(exit)
}

fn command_of(argv: &[String]) -> &'static str {
    const COMMANDS: &[&str] = &[
        "doctor", "deploy", "status", "infer", "logs", "rollback", "device", "version",
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
    output_mode: OutputMode,
    command: &'static str,
}

fn drive<O: Write, E: Write>(
    argv: &[String],
    stdout: &mut O,
    stderr: &mut E,
) -> Result<(), DriveError> {
    let fallback_mode = explicit_output_mode(argv).unwrap_or(OutputMode::Human);
    let outcome = args::parse(argv).map_err(|error| DriveError {
        error,
        output_mode: fallback_mode,
        command: command_of(argv),
    })?;
    let parsed = match outcome {
        ParseOutcome::Help => {
            writeln!(stdout, "{}", args::usage_text()).map_err(|error| DriveError {
                error: CliError::from(error),
                output_mode: fallback_mode,
                command: "tensorplate",
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
            .map_err(|error| DriveError {
                error: CliError::from(error),
                output_mode: fallback_mode,
                command: "version",
            })?;
            return Ok(());
        }
        ParseOutcome::Run(parsed) => parsed,
    };
    let command = command_label(&parsed.subcommand);
    let resolved =
        CliConfig::resolve(parsed.global.config_path.as_deref()).map_err(|error| DriveError {
            error,
            output_mode: fallback_mode,
            command,
        })?;
    let cfg = resolved.config;
    let output_mode = tensorplate_cli::effective_output_mode(&parsed.global, &cfg);
    report_config_warning(output_mode, stderr, resolved.warning.as_deref());
    let factory = |profile: &tensorplate_cli::ResolvedProfile| -> CliResult<Box<dyn AgentClient>> {
        Ok(Box::new(NetAgentClient::new(profile)))
    };
    tensorplate_cli::run(parsed, cfg, factory, stdout, stderr).map_err(|error| DriveError {
        error,
        output_mode,
        command,
    })
}

/// Surface a config file that was found but not used, so a packaged
/// install whose settings are not in effect never stays invisible.
///
/// Human mode only. In JSON mode stderr carries the error envelope and
/// nothing else — [`Renderer::render_error`] writes exactly one document
/// there — so a bare line ahead of it would hand every `--output json`
/// caller that parses stderr a syntax error instead of a typed failure.
/// [`Renderer::info`] is the rule every subcommand already follows for
/// stderr notes.
fn report_config_warning<E: Write>(mode: OutputMode, stderr: &mut E, warning: Option<&str>) {
    let Some(warning) = warning else {
        return;
    };
    // Best-effort, like the error renderer in `main`: a closed stderr must
    // not change the command's exit code.
    let _ = Renderer::new(mode).info(stderr, warning);
}

fn command_label(command: &Subcommand) -> &'static str {
    match command {
        Subcommand::Doctor(_) => "doctor",
        Subcommand::Deploy(_) => "deploy",
        Subcommand::Rollback(_) => "rollback",
        Subcommand::Status(_) => "status",
        Subcommand::Infer(_) => "infer",
        Subcommand::Logs(_) => "logs",
        Subcommand::Device(_) => "device",
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
    use super::*;

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
        report_config_warning(OutputMode::Human, &mut err, Some(WARNING));
        assert_eq!(String::from_utf8(err).unwrap(), format!("{WARNING}\n"));
    }

    /// `--output json` callers parse stderr as one envelope document. A
    /// plain warning line ahead of it makes that parse fail, so the note
    /// is dropped in JSON mode; the envelope and the exit code already
    /// carry the outcome.
    #[test]
    fn a_config_warning_never_precedes_the_json_error_envelope() {
        let mut err = Vec::new();
        report_config_warning(OutputMode::Json, &mut err, Some(WARNING));
        assert!(
            err.is_empty(),
            "stderr must stay the JSON envelope alone, got {:?}",
            String::from_utf8_lossy(&err)
        );
    }

    #[test]
    fn no_config_warning_writes_nothing() {
        let mut err = Vec::new();
        report_config_warning(OutputMode::Human, &mut err, None);
        assert!(err.is_empty());
    }
}
