// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{checked_path, open_checked, set_mode, stream, ProvisionError};
use tensorplate_protocol::provisioning_manifest::{ProvisionedBundle, ProvisionedFile};

pub(super) struct Cache {
    root: PathBuf,
}

impl Cache {
    pub(super) fn prepare(
        into: &Path,
        manifest: &Path,
        bundle: &ProvisionedBundle,
    ) -> Result<Self, ProvisionError> {
        let cache = Self {
            root: into.join(format!(".{}.download", bundle.name)),
        };
        match fs::create_dir(&cache.root) {
            Ok(()) => set_mode(&cache.root, 0o700)?,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&cache.root)
                    .map_err(|e| ProvisionError::io("cannot read transfer cache", &e))?;
                if !meta.is_dir() || meta.permissions().mode() & 0o777 != 0o700 {
                    return Err(ProvisionError::Fetch {
                        path: bundle.name.clone(),
                        reason: "cache must be a real directory with mode 0700".into(),
                    });
                }
            }
            Err(err) => return Err(ProvisionError::io("cannot create transfer cache", &err)),
        }
        // The caller's exclusive partial root locks this cache for the whole run.
        let result = cache.fill(manifest.parent().unwrap_or(Path::new(".")), bundle);
        if let Err(err) = result {
            if !matches!(
                err,
                ProvisionError::Fetch { .. } | ProvisionError::Io { .. }
            ) {
                cache.clear()?;
            }
            return Err(err);
        }
        Ok(cache)
    }

    fn fill(&self, packaged: &Path, bundle: &ProvisionedBundle) -> Result<(), ProvisionError> {
        for entry in fs::read_dir(&self.root)
            .map_err(|e| ProvisionError::io("cannot list transfer cache", &e))?
        {
            let entry =
                entry.map_err(|e| ProvisionError::io("cannot read transfer cache entry", &e))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !bundle
                .files
                .iter()
                .any(|f| name == f.sha256 || name == format!("{}.part", f.sha256))
            {
                return Err(ProvisionError::Fetch {
                    path: bundle.name.clone(),
                    reason: "cache holds an unlisted file; remove the cache before retrying".into(),
                });
            }
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|e| ProvisionError::io("cannot read cached file", &e))?;
            if !meta.is_file() {
                return Err(ProvisionError::SymbolicLink {
                    path: name.into_owned(),
                });
            }
        }
        for file in &bundle.files {
            let complete = self.root.join(&file.sha256);
            if complete.exists() {
                self.verify(file)?;
                continue;
            }
            let part = self.root.join(format!("{}.part", file.sha256));
            if let Some(relative) = &file.source_path {
                let (source, meta) = checked_path(packaged, relative)?;
                let mut source = open_checked(&source, &meta, file)?;
                let mut target = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .mode(0o600)
                    .open(&part)
                    .map_err(|e| ProvisionError::io("cannot create packaged-file cache", &e))?;
                let digest = stream(&mut source, file, Some(&mut target), packaged)?;
                if digest != file.sha256 {
                    return Err(mismatch(file, digest));
                }
            } else if let Some(url) = &file.url {
                download(url, &part, file)?;
            } else {
                return Err(ProvisionError::FetchSourceMissing {
                    path: file.path.clone(),
                });
            }
            let meta = fs::symlink_metadata(&part)
                .map_err(|e| ProvisionError::io("cannot read downloaded file", &e))?;
            let mut opened = open_checked(&part, &meta, file)?;
            let digest = stream(&mut opened, file, None, &self.root)?;
            if digest != file.sha256 {
                return Err(mismatch(file, digest));
            }
            fs::rename(&part, &complete)
                .map_err(|e| ProvisionError::io("cannot retain verified download", &e))?;
        }
        Ok(())
    }

    pub(super) fn open(&self, file: &ProvisionedFile) -> Result<File, ProvisionError> {
        let path = self.root.join(&file.sha256);
        let meta = fs::symlink_metadata(&path)
            .map_err(|e| ProvisionError::io("cannot read verified download", &e))?;
        open_checked(&path, &meta, file)
    }

    fn verify(&self, file: &ProvisionedFile) -> Result<(), ProvisionError> {
        let mut opened = self.open(file)?;
        let digest = stream(&mut opened, file, None, &self.root)?;
        if digest != file.sha256 {
            return Err(mismatch(file, digest));
        }
        Ok(())
    }

    pub(super) fn clear(&self) -> Result<(), ProvisionError> {
        fs::remove_dir_all(&self.root)
            .map_err(|e| ProvisionError::io("cannot remove transfer cache", &e))
    }
}

fn mismatch(file: &ProvisionedFile, actual: String) -> ProvisionError {
    ProvisionError::DigestMismatch {
        path: file.path.clone(),
        expected: file.sha256.clone(),
        actual,
    }
}

fn download(url: &str, path: &Path, file: &ProvisionedFile) -> Result<(), ProvisionError> {
    let mut target = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| ProvisionError::io("cannot open partial download", &e))?;
    let meta = target
        .metadata()
        .map_err(|e| ProvisionError::io("cannot read partial download", &e))?;
    if !meta.is_file() {
        return Err(ProvisionError::NotRegular {
            path: file.path.clone(),
        });
    }
    let offset = meta.len();
    if offset > file.size {
        return Err(ProvisionError::SizeMismatch {
            path: file.path.clone(),
            expected: file.size,
            actual: offset,
        });
    }
    if offset == file.size {
        return Ok(());
    }
    let mut child = Command::new("curl")
        .args([
            "--disable",
            "--globoff",
            "--silent",
            "--fail",
            "--location",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "15",
            "--max-time",
            "600",
            "--max-redirs",
            "5",
            "--continue-at",
            &offset.to_string(),
            "--url",
            url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ProvisionError::Fetch {
            path: file.path.clone(),
            reason: format!("cannot start curl: {e}"),
        })?;
    let outcome = (|| {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProvisionError::Fetch {
                path: file.path.clone(),
                reason: "curl output unavailable".into(),
            })?
            .take((file.size - offset).saturating_add(1));
        let mut bytes = offset;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let n = stdout
                .read(&mut buffer)
                .map_err(|e| ProvisionError::io("cannot read curl output", &e))?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            if bytes > file.size {
                return Err(ProvisionError::SizeMismatch {
                    path: file.path.clone(),
                    expected: file.size,
                    actual: bytes,
                });
            }
            target
                .write_all(&buffer[..n])
                .map_err(|e| ProvisionError::io("cannot write partial download", &e))?;
        }
        target
            .sync_all()
            .map_err(|e| ProvisionError::io("cannot persist partial download", &e))?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|e| ProvisionError::io("cannot reap curl", &e))?;
    outcome?;
    if !status.success() {
        return Err(ProvisionError::Fetch {
            path: file.path.clone(),
            reason: format!(
                "curl exit {}",
                status.code().map_or("signal".into(), |n| n.to_string())
            ),
        });
    }
    Ok(())
}
