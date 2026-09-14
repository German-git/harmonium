//! Small, conservative primitives for replacing application-owned files.
//!
//! Writers use a temporary file in the destination directory and an exclusive
//! create so two processes can never share a temporary pathname. The final
//! rename is atomic on the supported Unix filesystems, while the API remains
//! portable enough for the non-Unix build.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_TEMP_ATTEMPTS: u64 = 64;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `contents` without exposing a partially written file.
pub(crate) fn atomic_replace(path: &Path, contents: &[u8]) -> io::Result<()> {
    atomic_replace_with(path, |file| file.write_all(contents))
}

/// A fully written replacement that has not been installed yet.
///
/// Staging is deliberately separate from committing so callers can prepare
/// several files and only mutate their targets after every input and write has
/// succeeded.
pub(crate) struct StagedReplacement {
    target: PathBuf,
    temporary_path: PathBuf,
    committed: bool,
}

/// Write a replacement beside its target without changing the target.
pub(crate) fn stage_replacement(path: &Path, contents: &[u8]) -> io::Result<StagedReplacement> {
    let original = inspect_target(path)?;
    let (temporary_path, mut temporary) = create_unique_temp_file(path)?;

    let result = (|| {
        temporary.write_all(contents)?;
        temporary.flush()?;
        if let Some(metadata) = original.as_ref() {
            temporary.set_permissions(metadata.permissions())?;
        }
        temporary.sync_all()?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }

    Ok(StagedReplacement {
        target: path.to_path_buf(),
        temporary_path,
        committed: false,
    })
}

/// Copy an existing file beside its target without changing the target.
pub(crate) fn stage_copy(path: &Path) -> io::Result<StagedReplacement> {
    let original = inspect_target(path)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "replacement target does not exist")
    })?;
    let (temporary_path, mut temporary) = create_unique_temp_file(path)?;

    let result = (|| {
        let mut source = File::open(path)?;
        io::copy(&mut source, &mut temporary)?;
        temporary.flush()?;
        temporary.set_permissions(original.permissions())?;
        temporary.sync_all()?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }

    Ok(StagedReplacement {
        target: path.to_path_buf(),
        temporary_path,
        committed: false,
    })
}

impl StagedReplacement {
    /// Path of the staged file, intended for format-specific preparation.
    pub(crate) fn temporary_path(&self) -> &Path {
        &self.temporary_path
    }

    /// Install this replacement atomically.
    pub(crate) fn commit(&mut self) -> io::Result<()> {
        File::open(&self.temporary_path)?.sync_all()?;
        // Re-check immediately before replacement. A newly introduced symlink
        // or directory must never be treated as an application-owned target.
        inspect_target(&self.target)?;
        fs::rename(&self.temporary_path, &self.target)?;
        self.committed = true;
        sync_parent_directory(&self.target)
    }

    pub(crate) fn committed(&self) -> bool {
        self.committed
    }
}

impl Drop for StagedReplacement {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.temporary_path);
        }
    }
}

/// Replace a file while keeping the write operation injectable for failure
/// tests. Production callers should use [`atomic_replace`].
fn atomic_replace_with<F>(path: &Path, write: F) -> io::Result<()>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    atomic_replace_with_hooks(path, write, sync_parent_directory)
}

pub(crate) fn atomic_replace_with_hooks<F, S>(
    path: &Path,
    write: F,
    sync_directory: S,
) -> io::Result<()>
where
    F: FnOnce(&mut File) -> io::Result<()>,
    S: FnOnce(&Path) -> io::Result<()>,
{
    let original = inspect_target(path)?;
    let (temporary_path, mut temporary) = create_unique_temp_file(path)?;

    let result = (|| {
        write(&mut temporary)?;
        temporary.flush()?;

        if let Some(metadata) = original.as_ref() {
            // Write with the temporary file's initial permissions, then apply
            // the destination mode before the rename. This preserves a user's
            // deliberate mode without making a read-only target writable.
            temporary.set_permissions(metadata.permissions())?;
        }
        temporary.sync_all()?;
        drop(temporary);

        // Re-check immediately before replacement. A newly introduced symlink
        // is rejected rather than being treated as an application-owned file.
        inspect_target(path)?;
        fs::rename(&temporary_path, path)?;

        // On Unix, the rename is not durable until the containing directory is
        // synchronized. A failure is propagated after the atomic replacement;
        // cleanup remains best effort and the replacement stays in place.
        sync_directory(path)
    })();

    if result.is_err() {
        // Cleanup is deliberately best effort and bounded to one attempt. The
        // original file remains untouched when writing or replacement fails.
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

/// Synchronize the directory containing an atomically replaced file.
///
/// Unix opens and synchronizes the parent directory so the completed rename
/// is durable. Non-Unix builds intentionally use a no-op because this
/// directory-sync contract is not portable through the standard library.
#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Create an exclusive temporary file beside `path`.
fn create_unique_temp_file(path: &Path) -> io::Result<(PathBuf, File)> {
    let seed = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    create_unique_temp_file_from(path, seed)
}

fn create_unique_temp_file_from(path: &Path, seed: u64) -> io::Result<(PathBuf, File)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "target path has no file name")
    })?;
    let file_name = file_name.to_string_lossy();

    for offset in 0..MAX_TEMP_ATTEMPTS {
        let id = seed.saturating_add(offset);
        let temporary_path = parent.join(format!(".{file_name}.tmp-{}-{id}", std::process::id()));
        match OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file name",
    ))
}

/// Inspect a destination without following a symlink.
fn inspect_target(path: &Path) -> io::Result<Option<Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to replace a symbolic link",
        ));
    }
    if metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "refusing to replace a directory",
        ));
    }
    if metadata.permissions().readonly() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to replace a read-only file",
        ));
    }
    Ok(Some(metadata))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn collision_skips_an_existing_temporary_name() {
        let directory = tempfile::tempdir().expect("temp directory");
        let target = directory.path().join("config.toml");
        let seed = 17;
        let occupied = directory
            .path()
            .join(format!(".config.toml.tmp-{}-{seed}", std::process::id()));
        fs::write(&occupied, b"occupied").expect("occupied temp");

        let (temporary_path, temporary) = create_unique_temp_file_from(&target, seed)
            .expect("second candidate must be available");
        drop(temporary);
        assert_ne!(temporary_path, occupied);
        fs::remove_file(temporary_path).expect("remove allocated temp");
    }

    #[test]
    fn failed_write_keeps_original_and_cleans_temporary_file() {
        let directory = tempfile::tempdir().expect("temp directory");
        let target = directory.path().join("state.toml");
        fs::write(&target, b"original").expect("original");

        let error = atomic_replace_with(&target, |file| {
            file.write_all(b"partial")?;
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "injected failure",
            ))
        })
        .expect_err("injected failure must propagate");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(&target).expect("read original"), b"original");
        assert_eq!(temporary_files(directory.path()).len(), 0);
    }

    #[test]
    fn staged_copy_keeps_target_untouched_and_cleans_on_drop() {
        let directory = tempfile::tempdir().expect("temp directory");
        let target = directory.path().join("track.wav");
        fs::write(&target, b"original").expect("original");

        let staged = stage_copy(&target).expect("stage copy");
        assert_eq!(fs::read(&target).expect("target"), b"original");
        assert_eq!(
            fs::read(staged.temporary_path()).expect("staged copy"),
            b"original"
        );
        drop(staged);

        assert_eq!(temporary_files(directory.path()).len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn syncs_parent_directory_after_rename() {
        let directory = tempfile::tempdir().expect("temp directory");
        let target = directory.path().join("state.toml");
        fs::write(&target, b"original").expect("original");

        let sync_path = target.clone();
        atomic_replace_with_hooks(
            &target,
            |file| file.write_all(b"replacement"),
            move |path| {
                assert_eq!(path, sync_path);
                assert_eq!(
                    fs::read(path).expect("replacement after rename"),
                    b"replacement"
                );
                Ok(())
            },
        )
        .expect("replacement and directory sync");

        assert_eq!(fs::read(&target).expect("replacement"), b"replacement");
        assert_eq!(temporary_files(directory.path()).len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn propagates_parent_directory_sync_failure_after_rename() {
        let directory = tempfile::tempdir().expect("temp directory");
        let target = directory.path().join("state.toml");
        fs::write(&target, b"original").expect("original");

        let error = atomic_replace_with_hooks(
            &target,
            |file| file.write_all(b"replacement"),
            |_| Err(io::Error::other("injected directory sync failure")),
        )
        .expect_err("directory sync failure must propagate");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            fs::read(&target).expect("replacement remains in place"),
            b"replacement"
        );
        assert_eq!(temporary_files(directory.path()).len(), 0);
    }

    #[test]
    fn read_only_target_is_not_replaced() {
        let directory = tempfile::tempdir().expect("temp directory");
        let target = directory.path().join("readonly.toml");
        fs::write(&target, b"original").expect("original");
        let mut permissions = fs::metadata(&target).expect("metadata").permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&target, permissions).expect("readonly target");

        let error = atomic_replace(&target, b"replacement").expect_err("must reject readonly");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&target).expect("read original"), b"original");
        assert_eq!(temporary_files(directory.path()).len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn preserves_existing_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temp directory");
        let target = directory.path().join("mode.toml");
        fs::write(&target, b"original").expect("original");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).expect("mode");

        atomic_replace(&target, b"replacement").expect("replace");

        assert_eq!(
            fs::metadata(&target)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_target_without_touching_referent() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temp directory");
        let referent = directory.path().join("referent.toml");
        let target = directory.path().join("target.toml");
        fs::write(&referent, b"referent").expect("referent");
        symlink(&referent, &target).expect("symlink");

        let error = atomic_replace(&target, b"replacement").expect_err("must reject symlink");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&referent).expect("read referent"), b"referent");
        assert!(
            fs::symlink_metadata(&target)
                .expect("target metadata")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_replacements_leave_one_complete_document() {
        let directory = tempfile::tempdir().expect("temp directory");
        let target = Arc::new(directory.path().join("concurrent.toml"));
        fs::write(target.as_ref(), b"initial").expect("initial");
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();

        for index in 0..8 {
            let target = Arc::clone(&target);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                let contents = format!("writer-{index}\n");
                barrier.wait();
                atomic_replace(target.as_ref(), contents.as_bytes()).expect("replace");
                contents
            }));
        }

        let written: Vec<String> = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker"))
            .collect();
        let final_contents = fs::read_to_string(target.as_ref()).expect("final contents");
        assert!(written.iter().any(|contents| contents == &final_contents));
        assert_eq!(temporary_files(directory.path()).len(), 0);
    }

    fn temporary_files(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains(".tmp-"))
            })
            .collect()
    }
}
