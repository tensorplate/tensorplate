// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F01-T02: `tensorplate version` — prints CLI, protocol, and
// bundle format versions. Useful for support / V01-E15 validation scripts
// that need to assert the binary on the device matches the expected
// release.

use std::io::Write;

use serde_json::json;

use crate::error::CliResult;
use crate::output::Renderer;

/// Run the `version` command.
///
/// # Errors
///
/// Returns [`crate::error::CliError::Io`] when the writer fails.
pub fn run<W: Write>(renderer: &Renderer, out: &mut W) -> CliResult<()> {
    let cli_version = crate::version();
    let protocol = tensorplate_protocol::PROTOCOL_VERSION;
    let bundle = tensorplate_protocol::BUNDLE_FORMAT_VERSION;
    let human = format!("tensorplate {cli_version}\nprotocol: {protocol}\nbundle_format: {bundle}");
    let payload = json!({
        "cli": cli_version,
        "protocol": protocol,
        "bundle_format": bundle,
    });
    renderer.ok(out, "version", &human, payload, None, None)
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
    use crate::args::OutputMode;

    #[test]
    fn human_output_includes_version_lines() {
        let mut out = Vec::new();
        let r = Renderer::new(OutputMode::Human);
        run(&r, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("tensorplate "));
        assert!(text.contains("protocol:"));
    }

    #[test]
    fn json_output_carries_versions() {
        let mut out = Vec::new();
        let r = Renderer::new(OutputMode::Json);
        run(&r, &mut out).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(
            parsed["payload"]["protocol"],
            tensorplate_protocol::PROTOCOL_VERSION
        );
    }
}
