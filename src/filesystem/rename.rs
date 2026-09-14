//! Filesystem policy for safe file renames and playlist path rewrites.

use std::path::{Path, PathBuf};

use crate::error::WorkerError;
use crate::playlist::{PlaylistRepository, RewriteOutcome};

/// Typed result of a worker-side generic file rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameFileResult {
    /// The playlist rewrite and no-overwrite disk rename both completed.
    Success { refresh_browser: bool },
    /// The target was occupied when the worker performed its safety check or
    /// atomic no-overwrite mutation.
    Conflict,
    /// The worker rejected the request because the filesystem safety policy
    /// could not be satisfied.
    Rejected(WorkerError),
    /// A playlist rewrite or the final rename failed for an operational reason.
    Failed(WorkerError),
}

/// Repository boundary for the filesystem-owned file-rename policy.
pub trait RenameService {
    /// Rewrite saved playlists and rename one file without replacing a target.
    fn rename_file_on_disk<R: PlaylistRepository + ?Sized>(
        &self,
        store: &R,
        from: &Path,
        new_name: &str,
        browser_dir: &Path,
    ) -> (PathBuf, RenameFileResult);
}

/// Default local-filesystem implementation of [`RenameService`].
#[derive(Debug, Default, Clone, Copy)]
pub struct FilesystemRenameService;

impl RenameService for FilesystemRenameService {
    fn rename_file_on_disk<R: PlaylistRepository + ?Sized>(
        &self,
        store: &R,
        from: &Path,
        new_name: &str,
        browser_dir: &Path,
    ) -> (PathBuf, RenameFileResult) {
        rename_file_on_disk(store, from, new_name, browser_dir)
    }
}

/// Atomically rename without replacing an existing directory entry.
///
/// `std::fs::rename` replaces an existing target on Unix, which would reopen
/// the collision TOCTOU window after the worker's preflight check. Linux is
/// the supported runtime for Harmonium, so use the kernel's atomic
/// `RENAME_NOREPLACE` operation. Other targets fail closed rather than
/// silently weakening the no-overwrite guarantee.
#[cfg(target_os = "linux")]
fn rename_without_overwrite(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source path contains NUL")
    })?;
    let to = CString::new(to.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target path contains NUL")
    })?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_without_overwrite(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-overwrite rename is not supported on this platform",
    ))
}

/// Perform all generic file-rename filesystem work on the blocking worker.
fn rename_file_on_disk(
    store: &(impl PlaylistRepository + ?Sized),
    from: &Path,
    new_name: &str,
    browser_dir: &Path,
) -> (PathBuf, RenameFileResult) {
    let parent = from
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let fallback_target = parent.join(new_name);
    let target = match crate::filesystem::safety::validate_rename_target(&parent, new_name) {
        Ok(target) => target,
        Err(error) => {
            return (
                fallback_target,
                RenameFileResult::Rejected(WorkerError::new("file-rename", error)),
            );
        }
    };

    let refresh_browser = from
        .parent()
        .map(|source_parent| {
            std::fs::canonicalize(source_parent)
                .and_then(|canonical_source| {
                    std::fs::canonicalize(browser_dir)
                        .map(|canonical_browser| canonical_source == canonical_browser)
                })
                .unwrap_or(false)
        })
        .unwrap_or(false);

    if let Err(error) = std::fs::symlink_metadata(from) {
        return (
            target,
            RenameFileResult::Failed(WorkerError::new(
                "file-rename",
                crate::error::HarmoniumError::io(from, error),
            )),
        );
    }
    if target.exists() {
        return (target, RenameFileResult::Conflict);
    }

    // Playlist documents must be rewritten before the file move, preserving
    // the established ordering and atomic document replacement behavior.
    let rewrite = match store.rewrite_path_in_all(from, &target) {
        Ok(outcome) => outcome,
        Err(error) => {
            let error = WorkerError::new("file-rename", error);
            tracing::warn!("playlist rewrite failed, aborting rename: {error}");
            return (target, RenameFileResult::Failed(error));
        }
    };

    // Revalidate after the potentially slow playlist pass. The final
    // RENAME_NOREPLACE below is the mutation-time guard against a target that
    // appears between this check and the syscall.
    let target = match crate::filesystem::safety::validate_rename_target(&parent, new_name) {
        Ok(target) => target,
        Err(error) => {
            return (
                fallback_target,
                rollback_rename_rewrite(
                    store,
                    &rewrite,
                    RenameFileResult::Rejected(WorkerError::new("file-rename", error)),
                ),
            );
        }
    };
    if target.exists() {
        return (
            target,
            rollback_rename_rewrite(store, &rewrite, RenameFileResult::Conflict),
        );
    }

    match rename_without_overwrite(from, &target) {
        Ok(()) => (target, RenameFileResult::Success { refresh_browser }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let result = match crate::filesystem::safety::validate_rename_target(&parent, new_name)
            {
                Err(safety_error) => {
                    RenameFileResult::Rejected(WorkerError::new("file-rename", safety_error))
                }
                Ok(_) => RenameFileResult::Conflict,
            };
            (target, rollback_rename_rewrite(store, &rewrite, result))
        }
        Err(error) => {
            let error = WorkerError::new(
                "file-rename",
                crate::error::HarmoniumError::io(target.clone(), error),
            );
            (
                target,
                rollback_rename_rewrite(store, &rewrite, RenameFileResult::Failed(error)),
            )
        }
    }
}

impl FilesystemRenameService {
    /// Roll back the exact playlist documents changed before a file rename
    /// failed. The rewrite operation owns this policy; the worker only reports
    /// a stronger failure if restoring those documents also fails.
    #[cfg(test)]
    pub(crate) fn rollback_rename_rewrite(
        &self,
        store: &impl PlaylistRepository,
        rewrite: &RewriteOutcome,
        result: RenameFileResult,
    ) -> RenameFileResult {
        rollback_rename_rewrite(store, rewrite, result)
    }
}

/// Roll back the exact playlist documents changed before a file rename failed.
fn rollback_rename_rewrite(
    store: &(impl PlaylistRepository + ?Sized),
    rewrite: &RewriteOutcome,
    result: RenameFileResult,
) -> RenameFileResult {
    match store.rollback_rewrite(rewrite) {
        Ok(()) => result,
        Err(error) => {
            let rollback_error = WorkerError::new("file-rename", error);
            tracing::error!("rename rollback failed: {rollback_error}");
            let primary = match result {
                RenameFileResult::Conflict => {
                    WorkerError::message("file-rename", "target already exists")
                }
                RenameFileResult::Rejected(error) | RenameFileResult::Failed(error) => error,
                RenameFileResult::Success { .. } => {
                    WorkerError::message("file-rename", "rename did not complete")
                }
            };
            RenameFileResult::Failed(WorkerError::new(
                "file-rename",
                crate::error::RollbackError::new("file-rename", primary, rollback_error),
            ))
        }
    }
}
