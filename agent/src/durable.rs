// SPDX-License-Identifier: Apache-2.0

//! The sync the state store, the descriptor store and staging share, so a
//! file or a directory entry they write survives a power loss.

use std::fs::File;
use std::io;
use std::path::Path;

/// Sync the file or directory at `path` to disk: a file's contents, or a
/// directory's entries, so a file created or renamed in it is still there
/// after a power loss.
pub(crate) fn sync(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    hooks::seen(path)?;
    File::open(path)?.sync_all()
}

#[cfg(test)]
pub(crate) mod hooks {
    use std::cell::RefCell;
    use std::io;
    use std::path::{Path, PathBuf};

    type Picks = Box<dyn Fn(&Path) -> bool>;

    thread_local! {
        static SYNCED: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
        static FAILS: RefCell<Option<Picks>> = const { RefCell::new(None) };
    }

    /// The next sync on this thread of a path `picks` accepts fails, as a
    /// disk that cannot write does.
    pub(crate) fn fail_next_sync_of(picks: impl Fn(&Path) -> bool + 'static) {
        FAILS.with(|fails| *fails.borrow_mut() = Some(Box::new(picks)));
    }

    /// The paths synced on this thread since the last call, in order.
    pub(crate) fn synced() -> Vec<PathBuf> {
        SYNCED.with(RefCell::take)
    }

    pub(super) fn seen(path: &Path) -> io::Result<()> {
        let picked = FAILS.with(|fails| fails.borrow().as_ref().is_some_and(|picks| picks(path)));
        if picked {
            FAILS.with(|fails| fails.borrow_mut().take());
            return Err(rustix::io::Errno::IO.into());
        }
        SYNCED.with(|synced| synced.borrow_mut().push(path.to_path_buf()));
        Ok(())
    }
}
