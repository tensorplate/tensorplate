// SPDX-License-Identifier: Apache-2.0
//
// `tensorplate bundle provision <name> --from <dir>`: put a bundle the
// provisioning manifest lists into the bundle import directory, verified
// file by file, so `tensorplate deploy` can take it from there.
//
// The provisioning manifest is the trust root: it lists every file of the
// bundle -- its `manifest.json` included -- with the SHA-256 and size each
// must have. Files are copied into a partial root beside the destination,
// hashed as they are copied, never through a symbolic link below `--from`,
// and given modes that do not depend on the operator's umask. The partial
// root must then pass the bundle parser `tensorplate deploy` runs first,
// and only then is it renamed into place. Any failure removes the partial
// root, so a failed run never creates a directory a deploy could pick up.
// A destination that already exists is verified in place and left as it
// was found, whatever the outcome.
//
// The partial root is created exclusively, so two runs for one bundle
// never share one: the second is refused, as is a run that finds one left
// by an interrupted run.
//
// This runs in the operator's shell, never under the agent's service unit,
// and reads only a local directory. Fetching from the files' upstream
// sources is separate work.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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

/// Mode of every directory the command creates: the agent reads bundles
/// through their other bits, and nothing but the operator's user may write.
const DIR_MODE: u32 = 0o755;
/// Mode of every file the command creates.
const FILE_MODE: u32 = 0o644;

/// Why provisioning failed. No variant creates a deployable directory: the
/// partial root is removed, and an existing destination is left as it was
/// found.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error("the provisioning manifest {path} cannot be used: {reason}")]
    Manifest { path: PathBuf, reason: String },
    #[error("the provisioning manifest lists no bundle `{name}`{available}")]
    UnknownBundle { name: String, available: String },
    /// `given` is whether the operator named the directory with `--into`.
    #[error("{path} is not a directory; a symbolic link to one is not followed")]
    ImportDirMissing { path: PathBuf, given: bool },
    #[error(
        "{path} exists: another run is provisioning this bundle, or an earlier run was interrupted"
    )]
    PartialRootExists { path: PathBuf },
    #[error("`{path}` is not in the source directory")]
    SourceMissing { path: String },
    #[error("`{path}` is a symbolic link or has one in its path")]
    SymbolicLink { path: String },
    #[error("`{path}` is not a regular file")]
    NotRegular { path: String },
    #[error("`{path}` changed between being checked and being opened")]
    Changed { path: String },
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
    #[error("the provisioned files do not form a bundle deploy's bundle parser accepts: {reason}")]
    BundleRejected { reason: String },
    #[error("{path} already exists and is not this bundle, so it is left as found: {reason}")]
    DestinationMismatch { path: PathBuf, reason: String },
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
            Self::PartialRootExists { .. } => "partial_root_exists",
            Self::SourceMissing { .. } => "source_missing",
            Self::SymbolicLink { .. } => "symbolic_link",
            Self::NotRegular { .. } => "not_regular",
            Self::Changed { .. } => "changed_during_run",
            Self::SizeMismatch { .. } => "size_mismatch",
            Self::DigestMismatch { .. } => "digest_mismatch",
            Self::BundleRejected { .. } => "bundle_rejected",
            Self::DestinationMismatch { .. } => "destination_mismatch",
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
            ProvisionError::ImportDirMissing { given: false, .. } => Some(
                "the tensorplate-agent Debian package creates it; install that package, or pass --into"
                    .to_string(),
            ),
            ProvisionError::ImportDirMissing { given: true, .. } => {
                Some("pass --into a directory that exists".to_string())
            }
            ProvisionError::PartialRootExists { .. } => Some(
                "if no other `bundle provision` of this bundle is running, remove it and run again"
                    .to_string(),
            ),
            ProvisionError::SymbolicLink { .. } => Some(
                "files are read only from real files below --from; copy the file itself there"
                    .to_string(),
            ),
            ProvisionError::SizeMismatch { .. } | ProvisionError::DigestMismatch { .. } => Some(
                "the source file is not the one the manifest pins; fetch it again from its pinned revision"
                    .to_string(),
            ),
            ProvisionError::DestinationMismatch { .. } => Some(
                "nothing was changed; once nothing is deployed from it, remove it and run again"
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
        return Err(ProvisionError::ImportDirMissing {
            path: into,
            given: args.into.is_some(),
        });
    }
    let destination = into.join(&bundle.name);
    let bytes = bundle.files.iter().map(|f| f.size).sum();

    // An existing destination is verified where it is and never replaced:
    // it may be deployed from already.
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            verify_existing(&destination, bundle)?;
            return Ok(ProvisionReport {
                name: bundle.name.clone(),
                path: destination,
                files: bundle.files.len(),
                bytes,
                already_provisioned: true,
            });
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(ProvisionError::io(
                format!("cannot read {}", destination.display()),
                &err,
            ))
        }
    }

    // Created exclusively: a partial root that exists belongs to another
    // run, live or interrupted, and is neither used nor removed.
    let partial = into.join(format!(".{}.partial", bundle.name));
    match fs::create_dir(&partial) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            return Err(ProvisionError::PartialRootExists { path: partial })
        }
        Err(err) => {
            return Err(ProvisionError::io(
                format!("cannot create {}", partial.display()),
                &err,
            ))
        }
    }
    let outcome = set_mode(&partial, DIR_MODE)
        .and_then(|()| fill(&args.from, &partial, bundle))
        .and_then(|()| {
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
        // The partial root is this run's own directory; std's
        // remove_dir_all does not follow links inside it.
        let _ = fs::remove_dir_all(&partial);
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
///
/// The partial root is this run's own: created exclusively, writable only
/// by the operator's user, and every file in it created here and hashed as
/// it was written. Reading them all again would triple the reads of files
/// that can be gigabytes, since deploy's bundle parser hashes the artifacts
/// too, so what is left to check is that parser.
fn fill(from: &Path, partial: &Path, bundle: &ProvisionedBundle) -> Result<(), ProvisionError> {
    for file in &bundle.files {
        copy_verified(from, partial, file)?;
    }
    parse_bundle(partial)
        .map(|_| ())
        .map_err(|err| ProvisionError::BundleRejected {
            reason: err.to_string(),
        })
}

/// The existing `destination` is exactly `bundle`: a real directory holding
/// the listed files and nothing else, each with its size and digest, nothing
/// through a link, and deploy's bundle parser accepts it. Anything else is a
/// [`ProvisionError::DestinationMismatch`], except a failure to read.
fn verify_existing(destination: &Path, bundle: &ProvisionedBundle) -> Result<(), ProvisionError> {
    let mismatch = |reason: String| ProvisionError::DestinationMismatch {
        path: destination.to_path_buf(),
        reason,
    };
    let in_destination = |err: ProvisionError| match err {
        ProvisionError::Io { .. } => err,
        ProvisionError::SourceMissing { path } => mismatch(format!("`{path}` is missing")),
        other => mismatch(other.to_string()),
    };
    let meta = fs::symlink_metadata(destination).map_err(|err| {
        ProvisionError::io(format!("cannot read {}", destination.display()), &err)
    })?;
    if meta.file_type().is_symlink() {
        return Err(mismatch("it is a symbolic link".to_string()));
    }
    if !meta.is_dir() {
        return Err(mismatch("it is not a directory".to_string()));
    }
    let mut found = Vec::new();
    walk(destination, destination, &mut found).map_err(in_destination)?;
    if let Some(path) = found
        .iter()
        .find(|path| !bundle.files.iter().any(|f| &f.path == *path))
    {
        return Err(mismatch(format!(
            "`{path}` is not in the provisioning manifest"
        )));
    }
    for file in &bundle.files {
        let mut opened = open_listed(destination, file).map_err(in_destination)?;
        let digest = stream(&mut opened, file, None, destination).map_err(in_destination)?;
        if digest != file.sha256 {
            return Err(in_destination(ProvisionError::DigestMismatch {
                path: file.path.clone(),
                expected: file.sha256.clone(),
                actual: digest,
            }));
        }
    }
    parse_bundle(destination)
        .map(|_| ())
        .map_err(|err| mismatch(format!("deploy's bundle parser refuses it: {err}")))
}

/// Every regular file under `dir`, as a `/`-separated path relative to
/// `root`, each name exactly as the file system holds it: a backslash is
/// part of a name here, never a separator, so a name no manifest path can
/// spell stays unlisted. A link or anything but a file or directory is
/// refused.
fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) -> Result<(), ProvisionError> {
    let entries = fs::read_dir(dir)
        .map_err(|err| ProvisionError::io(format!("cannot list {}", dir.display()), &err))?;
    for entry in entries {
        let entry = entry
            .map_err(|err| ProvisionError::io(format!("cannot list {}", dir.display()), &err))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
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

/// `root` joined with a manifest path, refusing a link at any component
/// below `root`, with the final component's own metadata: a regular file.
fn checked_path(root: &Path, relative: &str) -> Result<(PathBuf, fs::Metadata), ProvisionError> {
    let mut path = root.to_path_buf();
    let mut last = None;
    for segment in relative.split('/') {
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(ProvisionError::SymbolicLink {
                    path: relative.to_string(),
                })
            }
            Ok(meta) => last = Some(meta),
            Err(err) if err.kind() == ErrorKind::NotFound => {
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
    // Checked before opening: opening a FIFO would block.
    match last {
        Some(meta) if meta.is_file() => Ok((path, meta)),
        _ => Err(ProvisionError::NotRegular {
            path: relative.to_string(),
        }),
    }
}

/// Open a listed file below `root`, checked link by link, at the size the
/// manifest pins.
fn open_listed(root: &Path, file: &ProvisionedFile) -> Result<fs::File, ProvisionError> {
    let (path, checked) = checked_path(root, &file.path)?;
    open_checked(&path, &checked, file)
}

/// Open `path`, which was `checked` a moment ago, and refuse what was opened
/// unless it is that same regular file: a link, a FIFO or a device swapped
/// in after the check is refused rather than read.
///
/// The open neither follows a final link nor blocks, so a FIFO swapped in
/// is opened at once and refused by the descriptor check instead of
/// waiting for a writer. Non-blocking mode does not change reads of a
/// regular file.
fn open_checked(
    path: &Path,
    checked: &fs::Metadata,
    file: &ProvisionedFile,
) -> Result<fs::File, ProvisionError> {
    let opened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|err| {
            // A path that no longer holds the checked file -- a link now
            // refused by O_NOFOLLOW, a socket, nothing -- changed under the
            // run; the checked file failing to open is an I/O failure.
            let unchanged = fs::symlink_metadata(path).is_ok_and(|now| {
                now.is_file() && now.dev() == checked.dev() && now.ino() == checked.ino()
            });
            if unchanged {
                ProvisionError::io(format!("cannot open {}", path.display()), &err)
            } else {
                ProvisionError::Changed {
                    path: file.path.clone(),
                }
            }
        })?;
    let meta = opened
        .metadata()
        .map_err(|err| ProvisionError::io(format!("cannot read {}", path.display()), &err))?;
    if !meta.is_file() || meta.dev() != checked.dev() || meta.ino() != checked.ino() {
        return Err(ProvisionError::Changed {
            path: file.path.clone(),
        });
    }
    if meta.len() != file.size {
        return Err(ProvisionError::SizeMismatch {
            path: file.path.clone(),
            expected: file.size,
            actual: meta.len(),
        });
    }
    Ok(opened)
}

/// Copy one listed file from `from` into `partial`, hashing as it goes.
fn copy_verified(
    from: &Path,
    partial: &Path,
    file: &ProvisionedFile,
) -> Result<(), ProvisionError> {
    let mut source = open_listed(from, file)?;
    let mut target_path = partial.to_path_buf();
    let mut segments = file.path.split('/').peekable();
    while let Some(segment) = segments.next() {
        target_path.push(segment);
        if segments.peek().is_some() && !target_path.is_dir() {
            fs::create_dir(&target_path).map_err(|err| {
                ProvisionError::io(format!("cannot create {}", target_path.display()), &err)
            })?;
            set_mode(&target_path, DIR_MODE)?;
        }
    }
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target_path)
        .map_err(|err| {
            ProvisionError::io(format!("cannot create {}", target_path.display()), &err)
        })?;
    set_mode(&target_path, FILE_MODE)?;
    let digest = stream(&mut source, file, Some(&mut target), from)?;
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

/// Give a path this command created an exact mode, whatever the umask.
fn set_mode(path: &Path, mode: u32) -> Result<(), ProvisionError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|err| {
        ProvisionError::io(format!("cannot set the mode of {}", path.display()), &err)
    })
}

/// Read `source` to its end, but never past one byte more than the manifest
/// pins, writing to `target` when given, and return the lowercase hex
/// SHA-256 of what was read. A file that grew or shrank after it was opened
/// is a size mismatch.
fn stream(
    source: &mut impl Read,
    file: &ProvisionedFile,
    mut target: Option<&mut fs::File>,
    root: &Path,
) -> Result<String, ProvisionError> {
    let mut source = source.take(file.size.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut read = 0_u64;
    let what = || root.join(&file.path).display().to_string();
    loop {
        let n = source
            .read(&mut buffer)
            .map_err(|err| ProvisionError::io(format!("cannot read {}", what()), &err))?;
        if n == 0 {
            break;
        }
        read += n as u64;
        hasher.update(&buffer[..n]);
        if let Some(target) = target.as_deref_mut() {
            target
                .write_all(&buffer[..n])
                .map_err(|err| ProvisionError::io(format!("cannot copy {}", what()), &err))?;
        }
    }
    if read != file.size {
        return Err(ProvisionError::SizeMismatch {
            path: file.path.clone(),
            expected: file.size,
            actual: read,
        });
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;

    fn listed(path: &str, size: u64) -> ProvisionedFile {
        ProvisionedFile {
            path: path.to_string(),
            sha256: "0".repeat(64),
            size,
        }
    }

    #[test]
    fn what_is_opened_must_be_the_regular_file_that_was_checked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&a, "same").expect("write");
        fs::write(&b, "same").expect("write");
        let checked_a = fs::symlink_metadata(&a).expect("lstat");
        assert!(open_checked(&a, &checked_a, &listed("a", 4)).is_ok());
        // Another file at the checked path: a swap after the check.
        assert!(matches!(
            open_checked(&b, &checked_a, &listed("a", 4)),
            Err(ProvisionError::Changed { .. })
        ));
        // A device opened in its place, even one checked as itself.
        let null = Path::new("/dev/null");
        let checked_null = fs::symlink_metadata(null).expect("lstat");
        assert!(matches!(
            open_checked(null, &checked_null, &listed("a", 0)),
            Err(ProvisionError::Changed { .. })
        ));
    }

    #[test]
    fn a_fifo_swapped_in_after_the_check_is_refused_without_waiting_for_a_writer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f");
        fs::write(&path, "abc").expect("write");
        let checked = fs::symlink_metadata(&path).expect("lstat");
        fs::remove_file(&path).expect("remove");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        // A blocking open would wait for a writer forever: bound the wait.
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(open_checked(&path, &checked, &listed("f", 3)).err());
        });
        match outcome
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("opening the FIFO blocked")
        {
            Some(ProvisionError::Changed { .. }) => {}
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn a_link_swapped_in_after_the_check_is_refused_even_to_the_checked_file() {
        // The link leads to the very inode that was checked, so only
        // refusing to follow it tells the swap apart.
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, moved) = (dir.path().join("f"), dir.path().join("moved"));
        fs::write(&path, "abc").expect("write");
        let checked = fs::symlink_metadata(&path).expect("lstat");
        fs::rename(&path, &moved).expect("rename");
        std::os::unix::fs::symlink(&moved, &path).expect("symlink");
        assert!(matches!(
            open_checked(&path, &checked, &listed("f", 3)),
            Err(ProvisionError::Changed { .. })
        ));
    }

    #[test]
    fn a_stream_is_read_no_further_than_one_byte_past_the_pinned_size() {
        let root = Path::new("/nowhere");
        // Far longer than pinned; an unbounded read would take all of it.
        let mut long = std::io::repeat(0).take(1 << 20);
        match stream(&mut long, &listed("f", 3), None, root) {
            Err(ProvisionError::SizeMismatch {
                expected, actual, ..
            }) => assert_eq!((expected, actual), (3, 4)),
            other => panic!("expected SizeMismatch, got {other:?}"),
        }
        let mut short = std::io::Cursor::new(b"ab".to_vec());
        assert!(matches!(
            stream(&mut short, &listed("f", 3), None, root),
            Err(ProvisionError::SizeMismatch { actual: 2, .. })
        ));
    }
}
