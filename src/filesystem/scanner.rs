//! Async recursive audio collection running on the shared Tokio runtime.
//!
//! Policy enforced here:
//!
//! - Symbolic links are skipped entirely during recursion, both directory
//!   and file links, so cycles can never hang a scan.
//! - Each directory level is sorted before walking, giving deterministic
//!   relative order per level even though concurrent scans may interleave.
//! - A hard depth cap guards against pathological trees. Hitting it is
//!   logged and never treated as an error.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tokio::fs;

use crate::error::{HarmoniumError, Result};
use crate::filesystem::entry::is_supported_audio;

/// Maximum recursion depth accepted by a single scan, counted from the
/// requested root whose direct children live at level zero of their parent.
pub const MAX_SCAN_DEPTH: usize = 32;

/// Recursively collect supported audio files under `root`.
///
/// Only the root listing failing aborts the scan. Unreadable subdirectories
/// are logged and skipped so one broken folder cannot hide every other
/// track in the tree. The returned order is deterministic within each
/// directory level.
pub async fn scan_directory(root: &Path) -> Result<Vec<PathBuf>> {
    let mut tracks = Vec::new();
    walk(root, 0, &mut tracks)
        .await
        .map_err(|source| HarmoniumError::io(root, source))?;
    Ok(tracks)
}

/// Boxed recursive walker keeping the future size finite.
fn walk<'a>(
    dir: &'a Path,
    depth: usize,
    tracks: &'a mut Vec<PathBuf>,
) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        if depth >= MAX_SCAN_DEPTH {
            tracing::warn!(
                "scan depth cap {MAX_SCAN_DEPTH} reached at {}",
                dir.display()
            );
            return Ok(());
        }

        let mut reader = match fs::read_dir(dir).await {
            Ok(reader) => reader,
            // Subtree problems degrade to a warning, root errors bubble up
            Err(error) if depth > 0 => {
                tracing::warn!("skipping unreadable {}: {error}", dir.display());
                return Ok(());
            }
            Err(error) => return Err(error),
        };

        // Materialize and sort one full level before walking any of it
        let mut children: Vec<(PathBuf, bool)> = Vec::new();
        loop {
            // A single entry that vanished mid-read (deleted, race with a
            // format/copy, or an unreadable name on FUSE/NFS) must not abort
            // the whole tree: skip it so the rest of the directory still
            // contributes its tracks.
            let entry = match reader.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!("skipping an entry in {}: {error}", dir.display());
                    continue;
                }
            };
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(error) => {
                    tracing::warn!("skipping {} (cannot stat): {error}", entry.path().display());
                    continue;
                }
            };
            // Symlinked directories and files alike are ignored on purpose
            if file_type.is_symlink() || !(file_type.is_dir() || file_type.is_file()) {
                continue;
            }

            children.push((entry.path(), file_type.is_dir()));
        }
        // Match the browser ordering: directories first, then files, each
        // group compared case-insensitively by name. Computing the key once
        // per entry avoids O(n log n) lowercases and keeps the traversal
        // deterministic exactly like `sort_entries` in the browser.
        children
            .sort_by_cached_key(|(path, is_dir)| (!*is_dir, path.to_string_lossy().to_lowercase()));

        for (path, is_dir) in children {
            if is_dir {
                walk(&path, depth + 1, tracks).await?;
            } else if is_supported_audio(&path) {
                tracks.push(path);
            }
        }

        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_dir;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[tokio::test]
    async fn nested_directories_are_recursed_deterministically() {
        let root = unique_temp_dir("scan-nested");
        fs::create_dir_all(root.join("b/inner")).expect("dirs");
        fs::write(root.join("z.mp3"), "").expect("file");
        fs::write(root.join("b/a.flac"), "").expect("file");
        fs::write(root.join("b/inner/deep.ogg"), "").expect("file");
        fs::write(root.join("b/UPPER.WAV"), "").expect("file");

        let tracks = scan_directory(&root).await.expect("scan ok");

        // Mirror the browser order: directories are walked before files at
        // each level, and each group is case-insensitive by name. So `b` (a
        // directory) is walked before `z.mp3`, and inside `b` the `inner`
        // directory is walked before `a.flac`/`UPPER.WAV`.
        assert_eq!(
            tracks,
            vec![
                root.join("b/inner/deep.ogg"),
                root.join("b/a.flac"),
                root.join("b/UPPER.WAV"),
                root.join("z.mp3"),
            ]
        );
    }

    #[tokio::test]
    async fn symlinks_are_skipped_inside_recursive_scans() {
        let root = unique_temp_dir("scan-symlink");
        fs::create_dir_all(root.join("real")).expect("dir");
        fs::write(root.join("real/song.mp3"), "").expect("file");
        fs::write(root.join("real/other.mp3"), "").expect("file");

        let link_result = symlink(root.join("real"), root.join("link"));
        let file_link = symlink(root.join("real/song.mp3"), root.join("alias.mp3"));
        if link_result.is_err() || file_link.is_err() {
            // Platform or mount forbids symlinks, the policy itself is covered elsewhere
            eprintln!("skipping symlink scan test: links unavailable here");
            return;
        }

        let tracks = scan_directory(&root).await.expect("scan ok");

        // The linked directory and the linked file must both stay invisible
        assert_eq!(
            tracks,
            vec![root.join("real/other.mp3"), root.join("real/song.mp3")]
        );
    }

    #[tokio::test]
    async fn unsupported_extensions_are_ignored() {
        let root = unique_temp_dir("scan-filter");
        fs::write(root.join("keep.opus"), "").expect("file");
        fs::write(root.join("drop.txt"), "").expect("file");
        fs::write(root.join("drop.mp4x"), "").expect("file");

        let tracks = scan_directory(&root).await.expect("scan ok");

        assert_eq!(tracks, vec![root.join("keep.opus")]);
    }

    #[tokio::test]
    async fn depth_cap_stops_unbounded_trees() {
        let root = unique_temp_dir("scan-depth");
        let mut level = root.to_path_buf();
        for index in 0..40 {
            let next = level.join(format!("d{index}"));
            fs::create_dir_all(&next).expect("dir");
            fs::write(level.join("t.mp3"), "").expect("file");
            level = next;
        }

        let tracks = scan_directory(&root).await.expect("scan ok");

        assert_eq!(tracks.len(), MAX_SCAN_DEPTH);
    }

    #[tokio::test]
    async fn empty_root_yields_no_tracks() {
        let root = unique_temp_dir("scan-empty");

        let tracks = scan_directory(&root).await.expect("scan ok");

        assert!(tracks.is_empty());
    }
}
