// SPDX-License-Identifier: Apache-2.0
//
// V01-E13-F07 fixture helper.
//
// `tensorplate-bundle-tool` is intentionally minimal: it walks a bundle
// directory, computes the sha256 of every file referenced in
// `manifest.json`, and prints the JSON object the manifest's
// `artifacts[].digest` and `manifest_digest` fields should carry. The
// helper uses the same `tensorplate_protocol::bundle` parser the agent
// runs at deploy time, so a stale fixture is caught by re-running this
// tool against the bundle root.
//
// Usage:
//
//   cargo run -p tensorplate-bundle-tool -- <bundle_root>
//
// Output is JSON (one object per artifact + canonical manifest digest).
// The script intentionally does NOT modify manifest.json in place; the
// bundle author updates digests manually after reviewing the diff.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use tensorplate_protocol::bundle::{
    compute_artifact_digest, compute_canonical_manifest_digest, MANIFEST_FILENAME,
};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next() else {
        eprintln!("usage: tensorplate-bundle-tool <bundle_root>");
        return ExitCode::from(2);
    };
    let root = PathBuf::from(root);
    let manifest_path = root.join(MANIFEST_FILENAME);
    let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
        eprintln!("error: cannot read {}", manifest_path.display());
        return ExitCode::from(1);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        eprintln!("error: manifest.json is not valid JSON");
        return ExitCode::from(1);
    };
    let mut artifacts = serde_json::Map::new();
    if let Some(arr) = value.get("artifacts").and_then(|v| v.as_array()) {
        for art in arr {
            let Some(path) = art.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            let abs = root.join(path);
            match compute_artifact_digest(&abs) {
                Ok(digest) => {
                    artifacts.insert(path.to_string(), serde_json::Value::String(digest));
                }
                Err(err) => {
                    eprintln!("error: cannot digest {}: {err}", abs.display());
                    return ExitCode::from(1);
                }
            }
        }
    }
    let manifest_digest = match compute_canonical_manifest_digest(&raw) {
        Ok(d) => d,
        Err(err) => {
            eprintln!("error: cannot canonicalize manifest: {err}");
            return ExitCode::from(1);
        }
    };
    let report = serde_json::json!({
        "artifacts": artifacts,
        "manifest_digest": manifest_digest,
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    ExitCode::SUCCESS
}
