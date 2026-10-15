// SPDX-License-Identifier: Apache-2.0

//! The staged copies of deployed bundles. Each deploy copies its bundle into
//! a tree of its own, `staging_dir/<deployment_id>/<transaction id>/`, before
//! the candidate record names it, and no tree is written again: an update
//! under the same id leaves the tree the earlier record names as it was. A
//! tree no record names is removed at a start and when a deploy begins.

use std::collections::BTreeSet;
use std::fs::{self, DirEntry};
use std::io::{self, ErrorKind};
use std::iter;
use std::path::{Path, PathBuf};

use tensorplate_protocol::agent_control::is_valid_deployment_id;
use tensorplate_protocol::agent_state::AgentState;

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
    for artifact in &verified.manifest.artifacts {
        let destination = tree.join(&artifact.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source.join(&artifact.path), &destination)?;
    }
    Ok(())
}

/// The staged path of every record of `state`: the singleton records, and
/// the resident set's members with the generations they retain.
fn named_by(state: &AgentState) -> impl Iterator<Item = PathBuf> + '_ {
    let records = [&state.active, &state.previous_active, &state.candidate];
    let records = records.into_iter().flatten().map(|r| &r.staged_path);
    let members = state.resident_set.iter().flat_map(|set| &set.members);
    let members = members.flat_map(|member| {
        let retained = member.previous.iter().map(|p| &p.staged_path);
        iter::once(&member.staged_path).chain(retained)
    });
    records.chain(members).map(PathBuf::from)
}

/// Remove every tree under `staging_dir` that no record of either state
/// file names, and return how many were removed. Only a directory a deploy
/// named, under a directory a deployment id names, is removed: a tree of
/// the layout before this one, `staging_dir/<deployment_id>/` itself, is
/// never. No other deploy may stage meanwhile.
///
/// # Errors
///
/// As [`StateStore::files`], when what a state file holds cannot be told,
/// and then nothing is removed; [`AgentError::Io`] when a directory cannot
/// be read or a tree cannot be removed.
pub fn prune(staging_dir: &Path, store: &StateStore) -> AgentResult<usize> {
    #[cfg(test)]
    if PRUNE_FAILS.with(|fails| fails.replace(false)) {
        return Err(io::Error::new(ErrorKind::Other, "prune stopped").into());
    }
    let mut named = BTreeSet::new();
    for state in store.files()? {
        named.extend(named_by(&state));
    }
    let mut removed = 0;
    for id in entries(staging_dir)? {
        let id = id?;
        if !is_directory(&id, is_valid_deployment_id)? {
            continue;
        }
        for tree in entries(&id.path())? {
            let tree = tree?;
            if is_directory(&tree, names_a_tree)? && !named.contains(&tree.path()) {
                fs::remove_dir_all(tree.path())?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn entries(dir: &Path) -> AgentResult<Vec<io::Result<DirEntry>>> {
    match fs::read_dir(dir) {
        Ok(entries) => Ok(entries.collect()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
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
