//! A profile snapshot cannot protect profiles created after that snapshot.
//! Builders retain project SH before creating any profile, and whole-project
//! GC only tries project EX. A builder that opened the lock before GC renamed
//! the project must reopen it after locking, not build under an unlinked inode.

use super::TestHook;
use crate::util::flock::{self, is_on_nfs_mount};
use crate::util::{CargoResult, GlobalContext};
use cargo_util::paths;
use same_file::Handle;
use std::fs::{OpenOptions, TryLockError};
use std::io;
use std::path::Path;

const PROJECT_LOCK_FILE: &str = ".cargo-project-gc-lock";

/// Acquired before any profile/package/unit lock and held until Layout drops.
/// Shared guards do not serialize builders, including GC-disabled builders.
pub(in crate::compiler) fn lock_project(
    root: &Path,
    gctx: &GlobalContext,
) -> CargoResult<Option<Handle>> {
    // Match Layout's existing NFS policy. GC also skips these roots, rather
    // than relying on protection the builders cannot provide.
    paths::create_dir_all_excluded_from_backups_atomic(root)?;
    if is_on_nfs_mount(root) {
        return Ok(None);
    }
    let path = root.join(PROJECT_LOCK_FILE);
    let hook = TestHook::for_path(gctx, &path, "layout");
    loop {
        paths::create_dir_all_excluded_from_backups_atomic(root)?;
        let file = match open_lock(&path) {
            Ok(file) => file,
            // GC may have renamed the root between creation and open.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        hook.event("ProjectGcOpened")?;
        match flock::try_lock_shared(file.as_file()) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                gctx.shell()
                    .status("Blocking", "waiting for project build directory GC")?;
                flock::lock_shared(file.as_file())?;
            }
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        if !is_current(&file, &path)? {
            // A collector held EX on this inode, renamed/deleted the root,
            // then let us acquire SH. Drop it and protect the current root.
            continue;
        }
        hook.event("ProjectGcAcquired")?;
        return Ok(Some(file));
    }
}

/// GC never waits for a builder or recreates a vanished project directory.
/// The returned guard must cover rename AND deletion, including trash cleanup.
pub(in crate::compiler) fn try_lock_project(root: &Path) -> CargoResult<Option<Handle>> {
    if is_on_nfs_mount(root) {
        return Ok(None);
    }
    let path = root.join(PROJECT_LOCK_FILE);
    let file = match open_lock(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match flock::try_lock_exclusive(file.as_file()) {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    // Another collector may have replaced this candidate before we got EX.
    if !is_current(&file, &path)? {
        return Ok(None);
    }
    Ok(Some(file))
}

fn open_lock(path: &Path) -> io::Result<Handle> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .and_then(Handle::from_file)
}

fn is_current(file: &Handle, path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    let same = || -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt as _;
        let locked = file.as_file().metadata()?;
        let current = std::fs::metadata(path)?;
        // Do not open/close another descriptor on Unix: that would release
        // process-scoped fcntl locks on Solaris.
        Ok((locked.dev(), locked.ino()) == (current.dev(), current.ino()))
    };
    #[cfg(not(unix))]
    let same = || -> io::Result<bool> { Ok(*file == Handle::from_path(path)?) };
    match same() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        result => result,
    }
}
