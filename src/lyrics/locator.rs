//! Local `.lrc` discovery next to the audio file.
//!
//! The search runs from the directory holding the audio file, preferring the
//! shallowest match and descending into subdirectories up to a fixed depth
//! so a `lyrics/` folder created next to the audio is found on the first
//! level. Everything is deterministic: the same input always yields the same
//! candidate, which matters because the player also writes remote results
//! into `lyrics/` and would otherwise fight itself.

use std::path::{Path, PathBuf};

/// Maximum number of subdirectory levels searched below the audio directory.
///
/// The audio directory itself is level 0, so `MAX_DEPTH = 3` covers the
/// `audio/`, `audio/lyrics/`, `audio/lyrics/album/` shapes without turning
/// into a full volume scan.
const MAX_DEPTH: usize = 3;

/// Find the best `.lrc` file for `audio_path`.
///
/// Matching requires the same file stem as the audio file and a `lrc`
/// extension (case-insensitive). Candidates are collected shallowest first
/// (the audio directory, then each subdirectory level up to [`MAX_DEPTH`])
/// and, within a level, in lexical order, so the result never depends on
/// directory enumeration order.
pub fn find_local_lrc(audio_path: &Path) -> Option<PathBuf> {
    let dir = audio_path.parent()?;
    let base = audio_path.file_stem()?;
    scan_level(dir, base, 0)
}

/// Recursively look for the best candidate LRC path at `depth`.
///
/// Returns `Some` as soon as a match is found at this level, so the search
/// short-circuits once the shallowest match (the one a player should prefer)
/// is located instead of walking the whole subtree first. Files of the
/// current directory are checked before descending, which guarantees the
/// shallowest match wins. Descending stops after [`MAX_DEPTH`]; unreadable
/// directories degrade to a debug log and are skipped, they must never fail
/// the search.
fn scan_level(dir: &Path, base: &std::ffi::OsStr, depth: usize) -> Option<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        tracing::debug!(path = %dir.display(), "lyrics search cannot read directory");
        return None;
    };

    let mut entries: Vec<std::fs::DirEntry> = read_dir.filter_map(|entry| entry.ok()).collect();
    entries.sort_by_key(|a| a.file_name());

    for entry in &entries {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lrc"))
            && path.file_stem() == Some(base)
            && path.is_file()
        {
            return Some(path);
        }
    }

    if depth >= MAX_DEPTH {
        return None;
    }
    for entry in &entries {
        if entry.file_type().is_ok_and(|t| t.is_dir())
            && let Some(found) = scan_level(&entry.path(), base, depth + 1)
        {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_dir;
    use std::fs;

    #[test]
    fn finds_the_lrc_next_to_the_audio() {
        let root = unique_temp_dir("lrc-same-dir");
        fs::write(root.join("song.mp3"), "audio").expect("audio fixture");
        fs::write(root.join("song.lrc"), "[00:01.00]same dir").expect("lrc fixture");

        let found = find_local_lrc(&root.join("song.mp3"));
        assert_eq!(found, Some(root.join("song.lrc")));
    }

    #[test]
    fn finds_the_lrc_one_level_deep() {
        let root = unique_temp_dir("lrc-level-1");
        fs::create_dir_all(root.join("lyrics")).expect("lyrics dir");
        fs::write(root.join("song.mp3"), "audio").expect("audio fixture");
        fs::write(root.join("lyrics/song.lrc"), "[00:01.00]level 1").expect("lrc fixture");

        let found = find_local_lrc(&root.join("song.mp3"));
        assert_eq!(found, Some(root.join("lyrics/song.lrc")));
    }

    #[test]
    fn finds_the_lrc_three_levels_deep() {
        let root = unique_temp_dir("lrc-level-3");
        let deep = root.join("a/b/c");
        fs::create_dir_all(&deep).expect("deep dirs");
        fs::write(root.join("song.mp3"), "audio").expect("audio fixture");
        fs::write(deep.join("song.lrc"), "[00:01.00]deep").expect("lrc fixture");

        let found = find_local_lrc(&root.join("song.mp3"));
        assert_eq!(found, Some(deep.join("song.lrc")));
    }

    #[test]
    fn ignores_the_level_beyond_the_max_depth() {
        let root = unique_temp_dir("lrc-level-4");
        let too_deep = root.join("a/b/c/d");
        fs::create_dir_all(&too_deep).expect("deep dirs");
        fs::write(root.join("song.mp3"), "audio").expect("audio fixture");
        fs::write(too_deep.join("song.lrc"), "too deep").expect("lrc fixture");

        let found = find_local_lrc(&root.join("song.mp3"));
        assert_eq!(found, None, "level 4 must stay out of the search");
    }

    #[test]
    fn ignores_files_with_a_different_base_name() {
        let root = unique_temp_dir("lrc-other-base");
        fs::write(root.join("song.mp3"), "audio").expect("audio fixture");
        fs::write(root.join("other.lrc"), "other").expect("lrc fixture");

        let found = find_local_lrc(&root.join("song.mp3"));
        assert_eq!(found, None);
    }

    #[test]
    fn matches_an_uppercase_lrc_extension() {
        let root = unique_temp_dir("lrc-uppercase");
        fs::write(root.join("song.mp3"), "audio").expect("audio fixture");
        fs::write(root.join("song.LRC"), "[00:01.00]upper").expect("lrc fixture");

        let found = find_local_lrc(&root.join("song.mp3"));
        assert_eq!(found, Some(root.join("song.LRC")));
    }

    #[test]
    fn prefers_the_shallowest_candidate_in_the_case_of_duplicates() {
        let root = unique_temp_dir("lrc-shallowest");
        fs::create_dir_all(root.join("c/b")).expect("dirs");
        fs::write(root.join("song.mp3"), "audio").expect("audio fixture");
        fs::write(root.join("song.lrc"), "shallow").expect("same dir lrc");
        fs::write(root.join("c/b/song.lrc"), "deep").expect("deep lrc");
        fs::write(root.join("c/song.lrc"), "mid").expect("mid lrc");

        let found = find_local_lrc(&root.join("song.mp3"));
        assert_eq!(found, Some(root.join("song.lrc")), "same dir must win");
    }

    #[test]
    fn same_level_candidates_are_order_stable() {
        let root = unique_temp_dir("lrc-deterministic");
        fs::create_dir_all(root.join("b")).expect("dir");
        fs::create_dir_all(root.join("a")).expect("dir");
        fs::write(root.join("song.mp3"), "audio").expect("audio fixture");
        fs::write(root.join("a/song.lrc"), "a").expect("a lrc");
        fs::write(root.join("b/song.lrc"), "b").expect("b lrc");

        let first = find_local_lrc(&root.join("song.mp3"));
        let second = find_local_lrc(&root.join("song.mp3"));
        assert_eq!(first, second, "lexical order must be reproducible");
        assert_eq!(first, Some(root.join("a/song.lrc")));
    }
}
