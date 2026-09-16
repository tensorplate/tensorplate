// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F01-T01/T02: shared output renderer.
//
// The renderer emits two stable shapes:
//
//   - human: short, table-friendly text designed to be readable on a
//     constrained device. Severity labels are bracketed, never colorized
//     for the sake of color alone, and never depend on terminal width.
//   - json: the envelope documented in
//     `protocol/schemas/cli_output.json`. Stable enough for release validation
//     validation scripts to grep on.
//
// Commands assemble their payload as a `serde_json::Value` and let the
// renderer wrap it in the envelope. This keeps command modules free of
// rendering branches and gives integration tests a deterministic JSON
// shape to assert on.

use std::io::Write;

use serde_json::{json, Value};

use crate::args::OutputMode;
use crate::error::{CliError, CliResult, ExitCode};

/// Output schema version stamped into the JSON envelope. Independent of
/// the wire protocol schema version.
pub const CLI_OUTPUT_SCHEMA_VERSION: &str = "0.1";

/// Coarse renderer used by every subcommand.
///
/// `warnings` carries process-level notes that are not tied to any one
/// command — today, a packaged config that was found and not used. They
/// are rendered into the JSON envelope rather than written to stderr,
/// because a `--output json` caller parses stderr as a single envelope
/// document and a bare line ahead of it is a syntax error. Without them
/// the envelope says nothing at all about a config with no effect, which
/// is the state issue #203 was reported from.
#[derive(Clone, Debug)]
pub struct Renderer {
    mode: OutputMode,
    warnings: Vec<String>,
}

impl Renderer {
    #[must_use]
    pub fn new(mode: OutputMode) -> Self {
        Self {
            mode,
            warnings: Vec::new(),
        }
    }

    /// A renderer that stamps `warnings` into every envelope it writes.
    #[must_use]
    pub fn with_warnings(mode: OutputMode, warnings: Vec<String>) -> Self {
        Self { mode, warnings }
    }

    #[must_use]
    pub fn mode(&self) -> OutputMode {
        self.mode
    }

    /// Add `warnings` to `envelope` when there are any. Absent, not empty,
    /// when there are none: the field is optional in
    /// `protocol/schemas/cli_output.json` and an empty array would be a
    /// new thing for every existing caller to skip past.
    fn stamp_warnings(&self, envelope: &mut Value) {
        if self.warnings.is_empty() {
            return;
        }
        envelope["warnings"] = json!(self.warnings);
    }

    /// Render a successful command result.
    ///
    /// `human_block` is the pre-rendered human output; the renderer
    /// writes it verbatim in human mode and ignores it in JSON mode.
    /// `payload` is the JSON envelope's `payload` field.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Io`] when the writer fails.
    pub fn ok<W: Write + ?Sized>(
        &self,
        out: &mut W,
        command: &'static str,
        human_block: &str,
        payload: Value,
        correlation_id: Option<&str>,
        transaction_id: Option<&str>,
    ) -> CliResult<()> {
        match self.mode {
            OutputMode::Human => {
                writeln!(out, "{human_block}")?;
            }
            OutputMode::Json => {
                let mut envelope = json!({
                    "schema_version": CLI_OUTPUT_SCHEMA_VERSION,
                    "command": command,
                    "status": "ok",
                    "correlation_id": correlation_id,
                    "transaction_id": transaction_id,
                    "payload": payload,
                });
                self.stamp_warnings(&mut envelope);
                writeln!(out, "{}", serde_json::to_string_pretty(&envelope)?)?;
            }
        }
        Ok(())
    }

    /// Render a typed CLI error. Always emits the JSON envelope when
    /// JSON mode is selected; human mode writes a single concise line
    /// plus an optional hint indented for readability.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Io`] only when the writer fails; the error
    /// argument itself is rendered, not re-raised.
    pub fn render_error<W: Write + ?Sized>(
        &self,
        err: &mut W,
        command: &'static str,
        error: &CliError,
    ) -> CliResult<()> {
        match self.mode {
            OutputMode::Human => {
                writeln!(err, "error: {error}")?;
                if let Some(hint) = error.hint() {
                    writeln!(err, "  hint: {hint}")?;
                }
                if let Some(ctx) = error.context() {
                    writeln!(err, "  context: {ctx}")?;
                }
            }
            OutputMode::Json => {
                let mut error_block = json!({
                    "code": error.protocol_code().as_str(),
                    "message": error.to_string(),
                });
                if let Some(hint) = error.hint() {
                    error_block["hint"] = json!(hint);
                }
                if let Some(ctx) = error.context() {
                    error_block["context"] = json!(ctx);
                }
                let mut envelope = json!({
                    "schema_version": CLI_OUTPUT_SCHEMA_VERSION,
                    "command": command,
                    "status": status_for(error),
                    "error": error_block,
                });
                self.stamp_warnings(&mut envelope);
                writeln!(err, "{}", serde_json::to_string_pretty(&envelope)?)?;
            }
        }
        Ok(())
    }

    /// Convenience: write an informational line to stderr in human mode
    /// and drop it in JSON mode. Subcommands use this to surface deploy
    /// progress without polluting the JSON envelope.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Io`] when the writer fails.
    pub fn info<W: Write + ?Sized>(&self, err: &mut W, line: &str) -> CliResult<()> {
        if matches!(self.mode, OutputMode::Human) {
            writeln!(err, "{line}")?;
        }
        Ok(())
    }
}

fn status_for(error: &CliError) -> &'static str {
    use crate::error::CliError as E;
    match error {
        E::Busy { .. } => "busy",
        E::Unavailable { .. } | E::UnsupportedProfile { .. } => "unavailable",
        _ => "error",
    }
}

/// Documented mapping between [`ExitCode`] and severity-readable name.
#[must_use]
pub fn exit_code_label(code: ExitCode) -> &'static str {
    match code {
        ExitCode::Success => "success",
        ExitCode::Failure => "failure",
        ExitCode::Usage => "usage",
        ExitCode::AgentError => "agent_error",
        ExitCode::Transport => "transport",
        ExitCode::Busy => "busy",
        ExitCode::Unavailable => "unavailable",
        ExitCode::DoctorFindings => "doctor_findings",
        ExitCode::InferenceFailed => "inference_failed",
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
        clippy::redundant_closure_for_method_calls
    )]

    use super::*;

    fn render_buf() -> Vec<u8> {
        Vec::new()
    }

    #[test]
    fn json_ok_includes_envelope_fields() {
        let r = Renderer::new(OutputMode::Json);
        let mut out = render_buf();
        r.ok(
            &mut out,
            "status",
            "human-line",
            json!({"agent_state": "ready"}),
            Some("corr-1"),
            None,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["schema_version"], "0.1");
        assert_eq!(parsed["command"], "status");
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["correlation_id"], "corr-1");
        assert_eq!(parsed["payload"]["agent_state"], "ready");
    }

    #[test]
    fn json_error_includes_hint_and_context() {
        let r = Renderer::new(OutputMode::Json);
        let mut err = render_buf();
        let cli_err = CliError::Agent {
            code: tensorplate_protocol::ErrorCode::Unsupported,
            message: "unknown backend".into(),
            context: Some("backend=fancy".into()),
            hint: Some("install the right adapter".into()),
        };
        r.render_error(&mut err, "deploy", &cli_err).unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(err).unwrap()).unwrap();
        assert_eq!(parsed["status"], "error");
        assert_eq!(parsed["error"]["code"], "unsupported");
        assert_eq!(parsed["error"]["hint"], "install the right adapter");
        assert_eq!(parsed["error"]["context"], "backend=fancy");
    }

    #[test]
    fn human_ok_writes_human_block() {
        let r = Renderer::new(OutputMode::Human);
        let mut out = render_buf();
        r.ok(&mut out, "status", "agent: ready", json!({}), None, None)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("agent: ready"));
    }

    /// A packaged config that was found and not used is dropped from
    /// stderr in JSON mode, so the envelope is the only place a scripted
    /// caller can learn the install is running on the built-in defaults.
    /// That is the state issue #203 was reported from, and it must not be
    /// silent on the path every validation harness uses.
    #[test]
    fn a_config_warning_reaches_the_json_envelope_on_both_paths() {
        let warning =
            "tensorplate: cannot read the packaged cli config `/etc/tensorplate/cli.json`";
        let r = Renderer::with_warnings(OutputMode::Json, vec![warning.into()]);

        let mut out = render_buf();
        r.ok(&mut out, "doctor", "human", json!({}), None, None)
            .unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed["warnings"][0], warning);

        let mut err = render_buf();
        r.render_error(&mut err, "logs", &CliError::Busy { hint: None })
            .unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(err).unwrap()).unwrap();
        assert_eq!(parsed["warnings"][0], warning);
    }

    /// Absent, not an empty array: `warnings` is optional in the schema
    /// and every existing caller predates it.
    #[test]
    fn an_envelope_without_warnings_omits_the_field() {
        let r = Renderer::new(OutputMode::Json);
        let mut out = render_buf();
        r.ok(&mut out, "status", "human", json!({}), None, None)
            .unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert!(parsed.get("warnings").is_none(), "{parsed}");
    }

    /// Human output already carries the warning on stderr, from the
    /// binary. Repeating it in the human block would double it.
    #[test]
    fn human_output_does_not_repeat_the_warning() {
        let r = Renderer::with_warnings(OutputMode::Human, vec!["packaged config unused".into()]);
        let mut out = render_buf();
        r.ok(&mut out, "status", "agent: ready", json!({}), None, None)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "agent: ready\n");
    }

    #[test]
    fn busy_status_maps_to_busy_envelope() {
        let r = Renderer::new(OutputMode::Json);
        let mut err = render_buf();
        let cli_err = CliError::Busy { hint: None };
        r.render_error(&mut err, "deploy", &cli_err).unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(err).unwrap()).unwrap();
        assert_eq!(parsed["status"], "busy");
    }
}
