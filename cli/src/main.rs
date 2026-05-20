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

use tensorplate_cli::args::{self, ParseOutcome};
use tensorplate_cli::client::{AgentClient, NetAgentClient};
use tensorplate_cli::config::CliConfig;
use tensorplate_cli::error::CliResult;
use tensorplate_cli::output::Renderer;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let stderr = std::io::stderr();
    let mut stdout = std::io::stdout();
    let mut stderr_lock = stderr.lock();
    let exit = match drive(&argv, &mut stdout, &mut stderr_lock) {
        Ok(()) => 0u8,
        Err(err) => {
            let renderer = Renderer::new(args::OutputMode::Human);
            // Best-effort: ignore renderer IO errors. If stderr is closed
            // the OS will signal SIGPIPE before this returns anyway.
            let _ = renderer.render_error(&mut stderr_lock, command_of(&argv), &err);
            err.exit_code().as_u8()
        }
    };
    let _ = stdout.flush();
    ExitCode::from(exit)
}

fn command_of(argv: &[String]) -> &'static str {
    const COMMANDS: &[&str] = &[
        "doctor", "deploy", "status", "infer", "logs", "rollback", "version",
    ];
    for token in argv {
        if let Some(name) = COMMANDS.iter().find(|c| **c == token) {
            return name;
        }
    }
    "tensorplate"
}

fn drive<O: Write, E: Write>(argv: &[String], stdout: &mut O, stderr: &mut E) -> CliResult<()> {
    let outcome = args::parse(argv)?;
    let parsed = match outcome {
        ParseOutcome::Help => {
            writeln!(stdout, "{}", args::usage_text())?;
            return Ok(());
        }
        ParseOutcome::Version => {
            writeln!(
                stdout,
                "tensorplate {} (protocol {})",
                tensorplate_cli::version(),
                tensorplate_protocol::PROTOCOL_VERSION
            )?;
            return Ok(());
        }
        ParseOutcome::Run(parsed) => parsed,
    };
    let cfg = CliConfig::load_or_default(parsed.global.config_path.as_deref())?;
    let factory = |profile: &tensorplate_cli::ResolvedProfile| -> CliResult<Box<dyn AgentClient>> {
        Ok(Box::new(NetAgentClient::new(profile)))
    };
    tensorplate_cli::run(parsed, cfg, factory, stdout, stderr)
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
}
