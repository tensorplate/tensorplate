// SPDX-License-Identifier: Apache-2.0

//! The staged copies of deployed bundles. Each deploy copies its bundle into
//! a tree of its own, `staging_dir/<deployment_id>/<transaction id>/`, before
//! the candidate record names it, and no tree is written again: an update
//! under the same id leaves the tree the earlier record names as it was. A
//! tree no record names is removed at a start and when a deploy begins.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, DirEntry};
use std::io::{self, ErrorKind};
use std::iter;
use std::path::{Component, Path, PathBuf};

use tensorplate_protocol::agent_control::is_valid_deployment_id;
use tensorplate_protocol::agent_state::AgentState;
use tensorplate_protocol::bundle::{parse_bundle_with, ParseOptions};

use crate::bundle::VerifiedBundle;
use crate::error::{AgentError, AgentResult};
use crate::state::StateStore;

const TRANSACTION_PREFIX: &str = "tx-";

#[cfg(test)]
thread_local! {
    /// The next [`prune`] on this thread fails before it reads anything.
    static PRUNE_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_prune() {
    PRUNE_FAILS.with(|fails| fails.set(true));
}

#[cfg(test)]
thread_local! {
    /// The next copy on this thread finds the disk full after the manifest.
    static DISK_FULL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fill_disk_at_next_copy() {
    DISK_FULL.with(|full| full.set(true));
}

/// Whether `name` is a transaction id as
/// [`crate::coordinator::new_transaction_id`] builds it: the only name a
/// tree is given, and the only directory [`prune`] removes.
fn names_a_tree(name: &str) -> bool {
    let id = name.strip_prefix(TRANSACTION_PREFIX);
    let id = id.and_then(|id| uuid::Uuid::parse_str(id).ok());
    id.is_some_and(|id| format!("{TRANSACTION_PREFIX}{id}") == name)
}

/// Copy `verified`'s manifest and declared artifacts into a new tree for
/// the deploy `transaction_id` of `deployment_id`, and return the tree. A
/// tree that is there is not written; a copy that fails removes its tree.
///
/// # Errors
///
/// [`AgentError::Internal`] for an id or a transaction id that cannot name
/// a tree, and [`AgentError::Io`] naming the tree when a step fails.
pub(crate) fn stage(
    staging_dir: &Path,
    deployment_id: &str,
    transaction_id: &str,
    verified: &VerifiedBundle,
) -> AgentResult<PathBuf> {
    if !is_valid_deployment_id(deployment_id) || !names_a_tree(transaction_id) {
        return Err(AgentError::Internal(format!(
            "`{deployment_id}` cannot be staged for transaction `{transaction_id}`: they do not name a tree"
        )));
    }
    let tree = staging_dir.join(deployment_id).join(transaction_id);
    let failed = |e: io::Error| {
        let named = format!("staging `{}`: {e}", tree.display());
        AgentError::Io(io::Error::new(e.kind(), named))
    };
    fs::create_dir_all(staging_dir.join(deployment_id)).map_err(failed)?;
    fs::create_dir(&tree).map_err(failed)?;
    if let Err(e) = copy(verified, &tree) {
        let _ = fs::remove_dir_all(&tree);
        return Err(failed(e));
    }
    Ok(tree)
}

fn copy(verified: &VerifiedBundle, tree: &Path) -> io::Result<()> {
    let source = &verified.root_path;
    fs::copy(source.join("manifest.json"), tree.join("manifest.json"))?;
    #[cfg(test)]
    if DISK_FULL.with(|full| full.replace(false)) {
        return Err(rustix::io::Errno::NOSPC.into());
    }
    for artifact in &verified.manifest.artifacts {
        let destination = tree.join(&artifact.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source.join(&artifact.path), &destination)?;
    }
    Ok(())
}

/// The staged path of every record of `state` (the singleton records, and
/// the resident set's members with the generations they retain), and the
/// bundle an in-flight transaction reads, which can be a tree no record names.
fn named_by(state: &AgentState) -> impl Iterator<Item = PathBuf> + '_ {
    let records = [&state.active, &state.previous_active, &state.candidate];
    let records = records.into_iter().flatten().map(|r| &r.staged_path);
    let members = state.resident_set.iter().flat_map(|set| &set.members);
    let members = members.flat_map(|member| {
        let retained = member.previous.iter().map(|p| &p.staged_path);
        iter::once(&member.staged_path).chain(retained)
    });
    let transaction = state.in_flight_transaction.iter();
    let source = transaction.filter_map(|tx| tx.bundle_path.as_ref());
    records.chain(members).chain(source).map(PathBuf::from)
}

/// Remove every tree under `staging_dir` that no record of either state
/// file names, and return how many were removed. A tree is matched by where
/// its path resolves, so the records may spell `staging_dir` another way,
/// and is kept when a path they name is in it. Only a directory a deploy
/// named, under a directory a deployment id names, is removed: a tree of the
/// layout before this one, `staging_dir/<deployment_id>/` itself with its
/// manifest at its root, is never, nor a directory in it that its manifest
/// lists an artifact under, nor anything in it while this build cannot read
/// or parse that manifest. No other deploy may stage meanwhile.
///
/// # Errors
///
/// As [`StateStore::every_file`], when a state file cannot be read, and
/// [`AgentError::Io`] when a path they name cannot be resolved or
/// `staging_dir` cannot be read: then nothing is removed. Otherwise the
/// first [`AgentError::Io`] of an entry that could not be read or removed,
/// once every other entry has been seen to.
pub fn prune(staging_dir: &Path, store: &StateStore) -> AgentResult<usize> {
    #[cfg(test)]
    if PRUNE_FAILS.with(|fails| fails.replace(false)) {
        return Err(io::Error::new(ErrorKind::Other, "prune stopped").into());
    }
    let mut named = BTreeSet::new();
    for state in store.every_file()? {
        for path in named_by(&state) {
            named.extend(resolved(&path)?);
        }
    }
    let mut removed = 0;
    let mut first_error = None;
    for id in entries(staging_dir)? {
        let trees = id.and_then(|id| trees_of(&id));
        for tree in trees.unwrap_or_else(|e| vec![Err(e)]) {
            match tree.and_then(|tree| remove_unnamed(&tree, &named)) {
                Ok(true) => removed += 1,
                Ok(false) => {}
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
    }
    first_error.map_or(Ok(removed), |e| Err(e.into()))
}

/// Where `path` resolves once its links and `..` are followed; none when
/// nothing is there.
fn resolved(path: &Path) -> io::Result<Option<PathBuf>> {
    match fs::canonicalize(path) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => {
            let unresolved = format!("named path `{}` does not resolve: {e}", path.display());
            Err(io::Error::new(e.kind(), unresolved))
        }
    }
}

/// The entries of `id` when it is a directory a deployment id names, less
/// those that are part of a tree of the earlier layout at `id`.
fn trees_of(id: &DirEntry) -> io::Result<Vec<io::Result<DirEntry>>> {
    let path = id.path();
    if !is_directory(id, is_valid_deployment_id)? {
        return Ok(Vec::new());
    }
    if !holds_a_manifest(&path)? {
        return entries(&path);
    }
    let Some(listed) = artifact_roots(&path) else {
        return Ok(Vec::new());
    };
    let mut trees = entries(&path)?;
    trees.retain(|tree| {
        tree.as_ref()
            .map_or(true, |t| !listed.contains(&t.file_name()))
    });
    Ok(trees)
}

/// The first segment of each artifact path the manifest at `dir` lists; none
/// when it cannot be read or parsed.
fn artifact_roots(dir: &Path) -> Option<BTreeSet<OsString>> {
    let options = ParseOptions {
        verify_artifact_digests: false,
    };
    let flat = parse_bundle_with(dir, options).ok()?;
    let first = |path: &String| {
        let mut segments = Path::new(path).components();
        segments.find_map(|segment| match segment {
            Component::Normal(name) => Some(name.to_owned()),
            _ => None,
        })
    };
    Some(
        flat.manifest
            .artifacts
            .iter()
            .filter_map(|a| first(&a.path))
            .collect(),
    )
}

/// Whether `dir` holds `manifest.json` at its root, as a tree does.
fn holds_a_manifest(dir: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(dir.join("manifest.json")) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(at(dir, &e)),
    }
}

/// Remove `tree` when it is a tree that no path in `named` resolves to or
/// into, and say whether it was removed.
fn remove_unnamed(tree: &DirEntry, named: &BTreeSet<PathBuf>) -> io::Result<bool> {
    if !is_directory(tree, names_a_tree)? {
        return Ok(false);
    }
    let path = tree.path();
    let resolved = fs::canonicalize(&path).map_err(|e| at(&path, &e))?;
    if named.iter().any(|named| named.starts_with(&resolved)) {
        return Ok(false);
    }
    fs::remove_dir_all(&path).map_err(|e| at(&path, &e))?;
    Ok(true)
}

/// The entries of `dir` in name order, so a prune that meets one it cannot
/// remove does the same each time; none when `dir` is not there.
fn entries(dir: &Path) -> io::Result<Vec<io::Result<DirEntry>>> {
    match fs::read_dir(dir) {
        Ok(entries) => {
            let mut entries: Vec<_> = entries.collect();
            entries.sort_by_key(|entry| entry.as_ref().ok().map(DirEntry::file_name));
            Ok(entries)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(at(dir, &e)),
    }
}

fn at(path: &Path, e: &io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("`{}`: {e}", path.display()))
}

/// Whether `entry` is a directory, not a link to one, with a name `rule`
/// accepts.
fn is_directory(entry: &DirEntry, rule: fn(&str) -> bool) -> io::Result<bool> {
    let name = entry.file_name();
    let named = name.to_str().is_some_and(rule);
    Ok(named && entry.file_type()?.is_dir())
}

#[cfg(test)]
mod tests;
