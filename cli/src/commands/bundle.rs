// SPDX-License-Identifier: Apache-2.0
//
// `tensorplate bundle provision <name> --from <dir>`: put a bundle the
// provisioning manifest lists into the bundle import directory, verified
// file by file, so `tensorplate deploy` can take it from there.
//
// The provisioning manifest is the trust root: it lists every file of the
// bundle -- its `manifest.json` included -- with the SHA-256 and size each
// must have. Files are copied into a partial root beside the destination,
// hashed as they are copied, never through a symbolic link. The partial root
// must then hold exactly the listed files and pass the same bundle check
// `tensorplate deploy` makes, and only then is it renamed into place. Any
// failure removes the partial root, so a failed run leaves nothing a deploy
// could pick up. A destination that already exists is verified in place and
// never overwritten.
//
// This runs in the operator's shell, never under the agent's service unit,
// and reads only a local directory. Fetching from the files' upstream
// sources is separate work.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};
use tensorplate_protocol::bundle::parse_bundle;
use tensorplate_protocol::install_paths::{BUNDLE_IMPORT_DIR, PROVISIONING_MANIFEST_PATH};
use tensorplate_protocol::provisioning_manifest::{
    ProvisionedBundle, ProvisionedFile, ProvisioningManifest,
};
use tensorplate_protocol::ErrorCode;

use crate::args::{BundleCommand, ProvisionArgs};
use crate::error::{CliError, CliResult};
use crate::output::Renderer;

/// Why provisioning failed. Every variant leaves no deployable directory:
/// the partial root is removed, and an existing destination is left as it
/// was found.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error("the provisioning manifest {path} cannot be used: {reason}")]
    Manifest { path: PathBuf, reason: String },
    #[error("the provisioning manifest lists no bundle `{name}`{available}")]
    UnknownBundle { name: String, available: String },
    #[error("{path} is not a directory; the tensorplate-agent package creates it")]
    ImportDirMissing { path: PathBuf },
    #[error("`{path}` is not in the source directory")]
    SourceMissing { path: String },
    #[error(
        "`{path}` is a symbolic link or has one in its path; files are copied only from real files"
    )]
    SymbolicLink { path: String },
    #[error("`{path}` is not a regular file")]
    NotRegular { path: String },
    #[error("`{path}` is {actual} bytes; the provisioning manifest says {expected}")]
    SizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("`{path}` has SHA-256 {actual}; the provisioning manifest says {expected}")]
    DigestMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("`{path}` is in the bundle directory but not in the provisioning manifest")]
    UnlistedFile { path: String },
    #[error("the provisioned files do not form a bundle deploy accepts: {reason}")]
    BundleRejected { reason: String },
    #[error("{what}: {detail}")]
    Io { what: String, detail: String },
}

impl ProvisionError {
    /// A stable token for scripts and the JSON error envelope's context.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Manifest { .. } => "manifest_invalid",
            Self::UnknownBundle { .. } => "unknown_bundle",
            Self::ImportDirMissing { .. } => "import_dir_missing",
            Self::SourceMissing { .. } => "source_missing",
            Self::SymbolicLink { .. } => "symbolic_link",
            Self::NotRegular { .. } => "not_regular",
            Self::SizeMismatch { .. } => "size_mismatch",
            Self::DigestMismatch { .. } => "digest_mismatch",
            Self::UnlistedFile { .. } => "unlisted_file",
            Self::BundleRejected { .. } => "bundle_rejected",
            Self::Io { .. } => "io",
        }
    }

    fn io(what: impl Into<String>, err: &std::io::Error) -> Self {
        Self::Io {
            what: what.into(),
            detail: err.to_string(),
        }
    }
}

impl From<ProvisionError> for CliError {
    fn from(err: ProvisionError) -> Self {
        let protocol = match &err {
            ProvisionError::Manifest { .. } | ProvisionError::UnknownBundle { .. } => {
                ErrorCode::ConfigInvalid
            }
            ProvisionError::ImportDirMissing { .. } | ProvisionError::Io { .. } => {
                ErrorCode::Internal
            }
            _ => ErrorCode::LoadFailed,
        };
        let hint = match &err {
            ProvisionError::ImportDirMissing { .. } => {
                Some("install tensorplate-agent on this host, or pass --into".to_string())
            }
            ProvisionError::UnlistedFile { .. } => Some(
                "the destination already exists and holds something else; remove it and run again"
                    .to_string(),
            ),
            ProvisionError::SizeMismatch { .. } | ProvisionError::DigestMismatch { .. } => Some(
                "the source file is not the one the manifest pins; fetch it again from its pinned revision"
                    .to_string(),
            ),
            _ => None,
        };
        CliError::Provision {
            code: err.code(),
            protocol,
            message: err.to_string(),
            hint,
        }
    }
}

/// What a successful run did.
#[derive(Debug, Eq, PartialEq)]
pub struct ProvisionReport {
    pub name: String,
    pub path: PathBuf,
    pub files: usize,
    pub bytes: u64,
    /// `true` when the destination already held exactly this bundle and
    /// nothing was written.
    pub already_provisioned: bool,
}

pub fn run<O: Write>(renderer: &Renderer, command: BundleCommand, stdout: &mut O) -> CliResult<()> {
    match command {
        BundleCommand::Provision(args) => {
            let report = provision(&args)?;
            let human = if report.already_provisioned {
                format!(
                    "{} is already provisioned at {} ({} files verified)",
                    report.name,
                    report.path.display(),
                    report.files
                )
            } else {
                format!(
                    "provisioned {} at {} ({} files, {} bytes, every file verified)",
                    report.name,
                    report.path.display(),
                    report.files,
                    report.bytes
                )
            };
            renderer.ok(
                stdout,
                "bundle",
                &human,
                json!({
                    "bundle": report.name,
                    "path": report.path.display().to_string(),
                    "files": report.files,
                    "bytes": report.bytes,
                    "outcome": if report.already_provisioned { "verified" } else { "provisioned" },
                }),
                None,
                None,
            )
        }
    }
}

/// Provision `args.name` from `args.from`.
///
/// # Errors
///
/// A [`ProvisionError`] naming the first thing that is wrong. None of them
/// leaves a deployable directory behind.
pub fn provision(args: &ProvisionArgs) -> Result<ProvisionReport, ProvisionError> {
    let manifest_path = args
        .manifest
        .clone()
        .unwrap_or_else(|| PathBuf::from(PROVISIONING_MANIFEST_PATH));
    let text = fs::read_to_string(&manifest_path).map_err(|err| ProvisionError::Manifest {
        path: manifest_path.clone(),
        reason: err.to_string(),
    })?;
    let manifest = ProvisioningManifest::parse(&text).map_err(|err| ProvisionError::Manifest {
        path: manifest_path.clone(),
        reason: err.to_string(),
    })?;
    let bundle = manifest
        .bundle(&args.name)
        .ok_or_else(|| ProvisionError::UnknownBundle {
            name: args.name.clone(),
            available: if manifest.bundles.is_empty() {
                "; it lists no bundles yet".to_string()
            } else {
                format!(
                    "; it lists {}",
                    manifest
                        .bundles
                        .iter()
                        .map(|b| format!("`{}`", b.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
        })?;

    let into = args
        .into
        .clone()
        .unwrap_or_else(|| PathBuf::from(BUNDLE_IMPORT_DIR));
    if !fs::symlink_metadata(&into).is_ok_and(|m| m.is_dir()) {
        return Err(ProvisionError::ImportDirMissing { path: into });
    }
    let destination = into.join(&bundle.name);
    let bytes = bundle.files.iter().map(|f| f.size).sum();

    // An existing destination is verified where it is and never replaced:
    // it may be deployed from already.
    if fs::symlink_metadata(&destination).is_ok() {
        verify_directory(&destination, bundle, Hash::EveryFile)?;
        return Ok(ProvisionReport {
            name: bundle.name.clone(),
            path: destination,
            files: bundle.files.len(),
            bytes,
            already_provisioned: true,
        });
    }

    // A partial root is only ever this command's, left by a run that was
    // killed; start over from nothing.
    let partial = into.join(format!(".{}.partial", bundle.name));
    remove_partial(&partial)?;
    fs::create_dir(&partial)
        .map_err(|err| ProvisionError::io(format!("cannot create {}", partial.display()), &err))?;
    let outcome = fill(&args.from, &partial, bundle).and_then(|()| {
        fs::rename(&partial, &destination).map_err(|err| {
            ProvisionError::io(
                format!(
                    "cannot move the verified bundle to {}",
                    destination.display()
                ),
                &err,
            )
        })
    });
    if let Err(err) = outcome {
        // Best effort: the error being reported is the one that matters.
        let _ = remove_partial(&partial);
        return Err(err);
    }
    Ok(ProvisionReport {
        name: bundle.name.clone(),
        path: destination,
        files: bundle.files.len(),
        bytes,
        already_provisioned: false,
    })
}

/// Copy and verify every file into `partial`, then check the whole.
fn fill(from: &Path, partial: &Path, bundle: &ProvisionedBundle) -> Result<(), ProvisionError> {
    for file in &bundle.files {
        copy_verified(from, partial, file)?;
    }
    // Every file was hashed as it was copied; reading them all again would
    // triple the reads of files that can be gigabytes, since deploy's check
    // hashes the artifacts too.
    verify_directory(partial, bundle, Hash::AlreadyHashed)
}

/// Whether [`verify_directory`] hashes the files itself.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Hash {
    /// A directory this run did not write: every file is hashed.
    EveryFile,
    /// The partial root this run filled, hashing every file as it copied.
    AlreadyHashed,
}

/// The directory holds exactly `bundle`'s files, each with its size and
/// digest, nothing through a link, and deploy's own bundle check accepts it.
fn verify_directory(
    root: &Path,
    bundle: &ProvisionedBundle,
    hash: Hash,
) -> Result<(), ProvisionError> {
    let mut found = Vec::new();
    walk(root, root, &mut found)?;
    for path in &found {
        if !bundle.files.iter().any(|f| &f.path == path) {
            return Err(ProvisionError::UnlistedFile { path: path.clone() });
        }
    }
    for file in &bundle.files {
        let path = checked_path(root, &file.path)?;
        if hash == Hash::AlreadyHashed {
            continue;
        }
        let digest = hash_file(&path, file)?;
        if digest != file.sha256 {
            return Err(ProvisionError::DigestMismatch {
                path: file.path.clone(),
                expected: file.sha256.clone(),
                actual: digest,
            });
        }
    }
    parse_bundle(root)
        .map(|_| ())
        .map_err(|err| ProvisionError::BundleRejected {
            reason: err.to_string(),
        })
}

/// Every regular file under `dir`, as a `/`-separated path relative to
/// `root`. A link or anything but a file or directory is refused.
fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) -> Result<(), ProvisionError> {
    let entries = fs::read_dir(dir)
        .map_err(|err| ProvisionError::io(format!("cannot list {}", dir.display()), &err))?;
    for entry in entries {
        let entry = entry
            .map_err(|err| ProvisionError::io(format!("cannot list {}", dir.display()), &err))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let kind = entry
            .file_type()
            .map_err(|err| ProvisionError::io(format!("cannot read {}", path.display()), &err))?;
        if kind.is_symlink() {
            return Err(ProvisionError::SymbolicLink { path: relative });
        } else if kind.is_dir() {
            walk(root, &path, found)?;
        } else if kind.is_file() {
            found.push(relative);
        } else {
            return Err(ProvisionError::NotRegular { path: relative });
        }
    }
    Ok(())
}

/// `root` joined with a manifest path, refusing a link at any component.
fn checked_path(root: &Path, relative: &str) -> Result<PathBuf, ProvisionError> {
    let mut path = root.to_path_buf();
    for segment in relative.split('/') {
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(ProvisionError::SymbolicLink {
                    path: relative.to_string(),
                })
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(ProvisionError::SourceMissing {
                    path: relative.to_string(),
                })
            }
            Err(err) => {
                return Err(ProvisionError::io(
                    format!("cannot read {}", path.display()),
                    &err,
                ))
            }
        }
    }
    if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        return Err(ProvisionError::NotRegular {
            path: relative.to_string(),
        });
    }
    Ok(path)
}

/// Hash a file already known to be a regular file, checking its size first.
fn hash_file(path: &Path, file: &ProvisionedFile) -> Result<String, ProvisionError> {
    let mut source = fs::File::open(path)
        .map_err(|err| ProvisionError::io(format!("cannot open {}", path.display()), &err))?;
    let actual = source
        .metadata()
        .map_err(|err| ProvisionError::io(format!("cannot read {}", path.display()), &err))?
        .len();
    if actual != file.size {
        return Err(ProvisionError::SizeMismatch {
            path: file.path.clone(),
            expected: file.size,
            actual,
        });
    }
    stream(&mut source, None, path)
}

/// Copy one listed file from `from` into `partial`, hashing as it goes.
fn copy_verified(
    from: &Path,
    partial: &Path,
    file: &ProvisionedFile,
) -> Result<(), ProvisionError> {
    let source_path = checked_path(from, &file.path)?;
    let mut source = fs::File::open(&source_path).map_err(|err| {
        ProvisionError::io(format!("cannot open {}", source_path.display()), &err)
    })?;
    let actual = source
        .metadata()
        .map_err(|err| ProvisionError::io(format!("cannot read {}", source_path.display()), &err))?
        .len();
    if actual != file.size {
        return Err(ProvisionError::SizeMismatch {
            path: file.path.clone(),
            expected: file.size,
            actual,
        });
    }
    let target_path = partial.join(&file.path);
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            ProvisionError::io(format!("cannot create {}", parent.display()), &err)
        })?;
    }
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target_path)
        .map_err(|err| {
            ProvisionError::io(format!("cannot create {}", target_path.display()), &err)
        })?;
    let digest = stream(&mut source, Some(&mut target), &source_path)?;
    target.sync_all().map_err(|err| {
        ProvisionError::io(format!("cannot write {}", target_path.display()), &err)
    })?;
    if digest != file.sha256 {
        return Err(ProvisionError::DigestMismatch {
            path: file.path.clone(),
            expected: file.sha256.clone(),
            actual: digest,
        });
    }
    Ok(())
}

/// Read `source` to its end, writing to `target` when given, and return the
/// lowercase hex SHA-256 of what was read.
fn stream(
    source: &mut fs::File,
    mut target: Option<&mut fs::File>,
    path: &Path,
) -> Result<String, ProvisionError> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let n = source
            .read(&mut buffer)
            .map_err(|err| ProvisionError::io(format!("cannot read {}", path.display()), &err))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        if let Some(target) = target.as_deref_mut() {
            target.write_all(&buffer[..n]).map_err(|err| {
                ProvisionError::io(format!("cannot copy {}", path.display()), &err)
            })?;
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Remove a partial root without following a link there.
fn remove_partial(partial: &Path) -> Result<(), ProvisionError> {
    match fs::symlink_metadata(partial) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(partial),
        Ok(_) => fs::remove_file(partial),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => Err(err),
    }
    .map_err(|err| ProvisionError::io(format!("cannot remove {}", partial.display()), &err))
}
