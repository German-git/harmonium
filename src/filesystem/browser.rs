//! Synchronous browser helpers: listing, sorting and start directory rules.
//!
//! Directory inspection is invoked by blocking effects so interactive
//! navigation and Settings validation do not block the reducer/UI thread.
//! Recursive collection is delegated to the async scanner on the shared
//! runtime.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{HarmoniumError, Result};
use crate::filesystem::entry::{EntryKind, FileEntry};

/// Order entries for display: directories first, then every other kind,
/// each group compared case-insensitively by name.
///
/// Pure so the ordering rule stays unit testable without a filesystem. The
/// key is computed once per entry (`sort_by_cached_key`) instead of on every
/// comparison, which would lowercase the name O(n log n) times.
pub fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by_cached_key(|entry| {
        // Directories sort first: `Dir` maps to `false`, everything else to
        // `true`, and `false < true` puts directories ahead.
        (entry.kind != EntryKind::Dir, entry.name.to_lowercase())
    });
}

/// Read one directory level without following symlinks.
///
/// Symbolic links are reported as [`EntryKind::Symlink`] regardless of their
/// target, honoring the anti cycle policy. When `show_hidden` is false, entries
/// whose name starts with a dot (files and folders) are filtered out; when
/// true every entry is listed.
pub fn read_sorted_entries(dir: &Path, show_hidden: bool) -> Result<Vec<FileEntry>> {
    let mut entries = Vec::new();

    for item in fs::read_dir(dir).map_err(|source| HarmoniumError::io(dir, source))? {
        // One entry that fails mid-listing (deleted, unreadable on FUSE/NFS,
        // or a permissions change) must not hide the whole directory: skip it
        // so the rest is still visible, matching the scanner's policy.
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                tracing::warn!("skipping an entry in {}: {error}", dir.display());
                continue;
            }
        };
        let name = item.file_name().to_string_lossy().into_owned();
        // Hidden entries (leading dot) are skipped unless the user opted in.
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let file_type = match item.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                tracing::warn!("skipping {} (cannot stat): {error}", item.path().display());
                continue;
            }
        };
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };

        entries.push(FileEntry::new(name, item.path(), kind));
    }

    sort_entries(&mut entries);
    Ok(entries)
}

/// Resolve the directory the browser opens at startup from a home location.
///
/// Priority order:
/// 1. `$XDG_MUSIC_DIR` when set and existing as a directory
/// 2. `$HOME/Music` when it exists as a directory
/// 3. `$HOME` itself when it exists as a directory
/// 4. Error when none are available
pub fn resolve_start_dir_from(home: &Path) -> Result<PathBuf> {
    // Honour the XDG user directories specification first
    if let Some(xdg_music) = std::env::var_os("XDG_MUSIC_DIR") {
        let path = PathBuf::from(xdg_music);
        if path.is_dir() {
            return Ok(path);
        }
    }

    let music = home.join("Music");
    if music.is_dir() {
        return Ok(music);
    }

    if home.is_dir() {
        return Ok(home.to_path_buf());
    }

    Err(HarmoniumError::NoHomeDir)
}

/// Environment backed wrapper over [`resolve_start_dir_from`].
pub fn resolve_start_dir() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => resolve_start_dir_from(Path::new(&home)),
        _ => Err(HarmoniumError::NoHomeDir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_dir;
    use std::fs;

    #[test]
    fn music_subdirectory_wins_when_it_exists_as_a_directory() {
        let home = unique_temp_dir("start-music");
        fs::create_dir_all(home.join("Music")).expect("music dir");
        // A plain home file must never be mistaken for the Music folder
        fs::write(home.join("Music.txt"), "").expect("decoy file");

        let resolved = resolve_start_dir_from(&home).expect("start dir");

        assert_eq!(resolved, home.join("Music"));
    }

    #[test]
    fn xdg_music_dir_wins_over_home_music() {
        let home = unique_temp_dir("start-xdg");
        let xdg_dir = unique_temp_dir("start-xdg-music");
        fs::create_dir_all(&xdg_dir).expect("xdg music dir");
        fs::create_dir_all(home.join("Music")).expect("home music dir");

        // Temporarily set XDG_MUSIC_DIR
        let original = std::env::var_os("XDG_MUSIC_DIR");
        // SAFETY: test runs single-threaded and restores the env var
        unsafe {
            std::env::set_var("XDG_MUSIC_DIR", &*xdg_dir);
        }

        let resolved = resolve_start_dir_from(&home).expect("start dir");

        assert_eq!(resolved, xdg_dir.path());

        // Restore original env
        // SAFETY: test runs single-threaded and restores the env var
        unsafe {
            match original {
                Some(val) => std::env::set_var("XDG_MUSIC_DIR", val),
                None => std::env::remove_var("XDG_MUSIC_DIR"),
            }
        }
    }

    #[test]
    fn xdg_music_dir_fallback_to_home_music_when_missing() {
        let home = unique_temp_dir("start-xdg-fallback");
        fs::create_dir_all(home.join("Music")).expect("home music dir");

        // Set XDG_MUSIC_DIR to a non-existent path
        let original = std::env::var_os("XDG_MUSIC_DIR");
        // SAFETY: test runs single-threaded and restores the env var
        unsafe {
            std::env::set_var("XDG_MUSIC_DIR", "/nonexistent/path/xdgMusic");
        }

        let resolved = resolve_start_dir_from(&home).expect("start dir");

        assert_eq!(resolved, home.join("Music"));

        // Restore original env
        // SAFETY: test runs single-threaded and restores the env var
        unsafe {
            match original {
                Some(val) => std::env::set_var("XDG_MUSIC_DIR", val),
                None => std::env::remove_var("XDG_MUSIC_DIR"),
            }
        }
    }

    #[test]
    fn home_itself_is_used_when_music_is_absent() {
        let home = unique_temp_dir("start-home");

        let resolved = resolve_start_dir_from(&home).expect("start dir");

        assert_eq!(resolved, home.path());
    }

    #[test]
    fn unusable_home_locations_are_rejected() {
        let missing = unique_temp_dir("start-missing");
        let fake_home = missing.join("nope");
        // Neither fake_home nor fake_home/Music exist

        let resolved = resolve_start_dir_from(&fake_home);

        assert!(matches!(resolved, Err(HarmoniumError::NoHomeDir)));
    }

    #[test]
    fn listing_reports_kinds_and_sorted_order() {
        let root = unique_temp_dir("listing");
        fs::create_dir_all(root.join("b-dir")).expect("dir");
        fs::create_dir_all(root.join("A-dir")).expect("dir");
        fs::write(root.join("z.mp3"), "").expect("file");
        fs::write(root.join("a.txt"), "").expect("file");

        let entries = read_sorted_entries(&root, false).expect("listing");

        let names: Vec<&str> = entries.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["A-dir", "b-dir", "a.txt", "z.mp3"]);
        assert_eq!(entries[0].kind, EntryKind::Dir);
        assert_eq!(entries[2].kind, EntryKind::File);
    }

    #[test]
    fn hidden_entries_are_skipped_unless_show_hidden_is_set() {
        let root = unique_temp_dir("listing-hidden");
        fs::create_dir_all(root.join(".config")).expect("hidden dir");
        fs::write(root.join(".hidden.mp3"), "").expect("hidden file");
        fs::write(root.join("visible.mp3"), "").expect("visible file");

        // Default: hidden entries are filtered out.
        let filtered = read_sorted_entries(&root, false).expect("listing");
        let names: Vec<&str> = filtered.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["visible.mp3"]);

        // Opt in: dotfiles and dot-directories appear.
        let shown = read_sorted_entries(&root, true).expect("listing");
        let names: Vec<&str> = shown.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, [".config", ".hidden.mp3", "visible.mp3"]);
    }
}
