// SPDX-License-Identifier: Apache-2.0
//
// The provisioning manifest: the trust root for `tensorplate bundle
// provision`. Mirrors `protocol/schemas/provisioning_manifest.json`.
//
// Every bundle the command can provision is listed with every file it
// holds -- the bundle's own `manifest.json` included -- and the SHA-256 and
// size each file must have. A provisioned directory holds exactly those
// files. The reader refuses what the schema cannot state: a repeated bundle
// name, a repeated path, a path that is also another path's directory, and
// a bundle without a `manifest.json`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The only manifest layout this release reads.
pub const PROVISIONING_MANIFEST_SCHEMA_VERSION: &str = "0.1";

/// The file every provisioned bundle must list: the bundle manifest
/// `tensorplate deploy` reads.
pub const BUNDLE_MANIFEST_FILE: &str = "manifest.json";

/// The whole manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningManifest {
    /// Optional editor hint; ignored.
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub schema_version: String,
    pub bundles: Vec<ProvisionedBundle>,
}

/// One bundle the command can provision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionedBundle {
    /// The name the operator gives, and the directory it is provisioned into.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub files: Vec<ProvisionedFile>,
}

/// One file of a bundle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionedFile {
    /// Relative, `/`-separated path inside the bundle directory.
    pub path: String,
    /// Lowercase hex SHA-256 of the file's exact bytes.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
}

/// Why a manifest cannot be used.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ProvisioningManifestError {
    #[error("not a provisioning manifest: {0}")]
    Malformed(String),
    #[error("{0}")]
    Invalid(String),
}

impl ProvisioningManifest {
    /// Parse and validate a manifest's text.
    ///
    /// # Errors
    ///
    /// [`ProvisioningManifestError::Malformed`] when the text is not a
    /// manifest of this layout, and [`ProvisioningManifestError::Invalid`]
    /// naming the first rule it breaks.
    pub fn parse(text: &str) -> Result<Self, ProvisioningManifestError> {
        let manifest: Self = serde_json::from_str(text)
            .map_err(|err| ProvisioningManifestError::Malformed(err.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// The bundle named `name`, if the manifest lists one.
    #[must_use]
    pub fn bundle(&self, name: &str) -> Option<&ProvisionedBundle> {
        self.bundles.iter().find(|bundle| bundle.name == name)
    }

    fn validate(&self) -> Result<(), ProvisioningManifestError> {
        let invalid = |message: String| Err(ProvisioningManifestError::Invalid(message));
        if self.schema_version != PROVISIONING_MANIFEST_SCHEMA_VERSION {
            return invalid(format!(
                "schema_version `{}` is not {PROVISIONING_MANIFEST_SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        let mut names = BTreeSet::new();
        for bundle in &self.bundles {
            let name = &bundle.name;
            if !is_bundle_name(name) {
                return invalid(format!(
                    "bundle name `{name}` must be lowercase letters, digits, `.`, `_` or `-`, starting with a letter or digit"
                ));
            }
            if !names.insert(name.as_str()) {
                return invalid(format!("bundle `{name}` is listed more than once"));
            }
            if bundle.files.is_empty() {
                return invalid(format!("bundle `{name}` lists no files"));
            }
            let mut paths = BTreeSet::new();
            for file in &bundle.files {
                let path = &file.path;
                if !is_bundle_path(path) {
                    return invalid(format!(
                        "bundle `{name}`: `{path}` is not a relative `/`-separated path of plain segments"
                    ));
                }
                if !is_sha256_hex(&file.sha256) {
                    return invalid(format!(
                        "bundle `{name}`: `{path}` needs a lowercase hex SHA-256"
                    ));
                }
                if !paths.insert(path.as_str()) {
                    return invalid(format!("bundle `{name}` lists `{path}` more than once"));
                }
            }
            // A path cannot be a file and another file's directory at once.
            for path in &paths {
                let as_directory = format!("{path}/");
                if paths.iter().any(|other| other.starts_with(&as_directory)) {
                    return invalid(format!(
                        "bundle `{name}` lists `{path}` both as a file and as a directory"
                    ));
                }
            }
            if !paths.contains(BUNDLE_MANIFEST_FILE) {
                return invalid(format!(
                    "bundle `{name}` does not list `{BUNDLE_MANIFEST_FILE}`, the bundle manifest deploy reads"
                ));
            }
        }
        Ok(())
    }
}

fn is_bundle_name(name: &str) -> bool {
    name.len() <= 128
        && name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// Relative and `/`-separated, every segment starting with a letter, digit
/// or `_` -- so no empty, `.`, `..` or hidden segment -- and made of letters,
/// digits, `.`, `_` and `-`.
fn is_bundle_path(path: &str) -> bool {
    path.len() <= 512
        && path.split('/').all(|segment| {
            segment
                .bytes()
                .next()
                .is_some_and(|first| first.is_ascii_alphanumeric() || first == b'_')
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

fn is_sha256_hex(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
