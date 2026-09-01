// SPDX-License-Identifier: Apache-2.0
//
// The reason vocabulary as a cross-language contract.
//
// The Python sidecar emits reason strings of its own (an unavailable MPS
// runtime is the one that exists today). Those strings are compared
// against this enum's spellings at the IPC boundary, so a rename on
// either side that the other does not follow turns a typed reason into an
// unrecognized string at run time -- on the machine, not in CI. This
// asserts the two sides agree, by reading the Python source rather than
// restating its constants here.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tensorplate_platform::PlatformReason;

fn python_protocol_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("backends/python_pytorch/src/tensorplate_pytorch_backend/protocol.py");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn every_python_reason_constant_is_a_spelling_this_enum_owns() {
    let source = python_protocol_source();
    let mut found = 0;
    for line in source.lines() {
        let Some(rest) = line.strip_prefix("REASON_") else {
            continue;
        };
        let Some((_, value)) = rest.split_once('=') else {
            continue;
        };
        let spelling = value.trim().trim_matches(|c| c == '"' || c == ' ');
        if spelling.is_empty() {
            continue;
        }
        found += 1;
        assert!(
            PlatformReason::ALL
                .iter()
                .any(|reason| reason.as_str() == spelling),
            "the sidecar emits `{spelling}`, which this enum does not own"
        );
    }
    assert!(
        found > 0,
        "no REASON_ constants found; the parse is wrong, not the contract"
    );
}
