// SPDX-License-Identifier: Apache-2.0
//
// Crate layering for the platform registry.
//
// The agent, the CLI, and the observability service must each reach the
// registry through `tensorplate-platform` and never through one another.
// If one consumer could depend on another, the shared row vocabulary
// would acquire a second, unversioned path between processes — and the
// first thing to drift would be what "supported" means, which is the one
// answer all three have to give identically.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

/// The workspace members that consume the registry.
const CONSUMERS: [&str; 3] = ["agent", "cli", "observability"];

fn manifest(member: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(member)
        .join("Cargo.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn every_consumer_depends_on_the_platform_crate() {
    for member in CONSUMERS {
        assert!(
            manifest(member).contains("tensorplate-platform"),
            "{member} must consume the registry through tensorplate-platform"
        );
    }
}

#[test]
fn consumers_do_not_depend_on_each_other() {
    // Checked across the whole manifest rather than the `[dependencies]`
    // table alone: a dev-dependency couples the crates at build time just
    // as effectively, and there is no reason for one consumer's tests to
    // reach into another's.
    for member in CONSUMERS {
        let body = manifest(member);
        for other in CONSUMERS {
            if other == member {
                continue;
            }
            assert!(
                !body.contains(&format!("tensorplate-{other}")),
                "{member} must not depend on tensorplate-{other}"
            );
        }
    }
}
