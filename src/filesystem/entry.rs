//! Filesystem entry data model and audio extension rules.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// File name extensions recognized as playable audio, listed lowercase.
///
/// Matching is case-insensitive so mixed or uppercase names from any
/// filesystem still count as supported.
pub const AUDIO_EXTENSIONS: [&str; 12] = [
    "mp3", "flac", "wav", "aiff", "aif", "ogg", "oga", "opus", "m4a", "m4b", "mp4", "aac",
];

/// Nature of a filesystem entry as observed without following links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular directory.
    Dir,
    /// Regular file.
    File,
    /// Symbolic link of any target kind, never followed by scans.
    Symlink,
}

/// One browsable row of the file panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// File name only, without any directory component.
    pub name: String,
    /// Absolute location of the entry.
    pub path: PathBuf,
    /// Observed kind used for sorting, rendering and navigation rules.
    pub kind: EntryKind,
}

impl FileEntry {
    /// Assemble an entry from its parts.
    pub fn new(name: impl Into<String>, path: PathBuf, kind: EntryKind) -> Self {
        Self {
            name: name.into(),
            path,
            kind,
        }
    }
}

/// Whether a path points at a file with a supported audio extension.
///
/// The check is purely lexical, no IO happens here, which keeps it safe for
/// hot paths such as rendering and filtering.
pub fn is_supported_audio(path: &Path) -> bool {
    match path.extension() {
        Some(extension) => AUDIO_EXTENSIONS
            .iter()
            .any(|candidate| OsStr::eq_ignore_ascii_case(extension, candidate)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The ordering rule lives beside the listing helpers but is specified
    // against this module's data model, so tests pin it through the re-export
    use crate::filesystem::sort_entries;

    fn entry(name: &str, kind: EntryKind) -> FileEntry {
        FileEntry::new(name, PathBuf::from(name), kind)
    }

    #[test]
    fn supported_extensions_match_in_any_letter_case() {
        assert!(is_supported_audio(Path::new("/m/song.mp3")));
        assert!(is_supported_audio(Path::new("/m/SONG.FLAC")));
        assert!(is_supported_audio(Path::new("/m/mixed-case.Ogg")));
        assert!(is_supported_audio(Path::new("relative.aif")));
        // Every listed extension matches when written uppercase
        for candidate in AUDIO_EXTENSIONS {
            // A stem is required so the name keeps a dotted extension
            let name = format!("/x/track.{candidate}");
            assert!(
                is_supported_audio(Path::new(&name.to_uppercase())),
                "{candidate} must match in upper case"
            );
        }
    }

    #[test]
    fn files_without_a_supported_extension_are_rejected() {
        assert!(!is_supported_audio(Path::new("/m/notes.txt")));
        assert!(!is_supported_audio(Path::new("/m/noextension")));
        assert!(!is_supported_audio(Path::new("/m/.hidden")));
        assert!(!is_supported_audio(Path::new("/m/song.mp3.bak")));
    }

    #[test]
    fn directories_sort_first_then_names_compare_case_insensitively() {
        let mut entries = vec![
            entry("zebra", EntryKind::File),
            entry("Album", EntryKind::Dir),
            entry("banana", EntryKind::File),
            entry("apple", EntryKind::File),
            entry("archive", EntryKind::Symlink),
            entry("Bulb", EntryKind::Dir),
        ];

        sort_entries(&mut entries);

        let names: Vec<&str> = entries.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(
            names,
            // Case insensitive order inside the non directory group
            ["Album", "Bulb", "apple", "archive", "banana", "zebra"]
        );
    }

    #[test]
    fn sorting_an_empty_or_single_entry_list_is_a_no_op() {
        let mut empty: Vec<FileEntry> = Vec::new();
        sort_entries(&mut empty);
        assert!(empty.is_empty());

        let mut single = vec![entry("only", EntryKind::File)];
        sort_entries(&mut single);
        assert_eq!(single[0].name, "only");
    }
}
