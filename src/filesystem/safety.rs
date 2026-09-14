//! Rename safety validators for the file browser.
//!
//! The rename dialog only ever supplies the final file name; the target
//! directory comes from the source. These rules keep that single user input
//! inside the source directory and away from symlinks, mirroring the
//! never-follow-symlink policy of the scanner.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Structured rejection of a rename target.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SafetyError {
    /// The submitted name is empty or whitespace only.
    #[error("the file name cannot be empty")]
    EmptyName,
    /// The submitted name carries separators or dot segments.
    #[error("the file name cannot contain path separators or dot segments")]
    UnsafeName {
        /// The rejected input, for display.
        name: String,
    },
    /// The target already exists as a symlink, which a rename must never
    /// write through or replace.
    #[error("refusing to rename onto the symlink {}", path.display())]
    SymlinkedTarget {
        /// The symlink that would be clobbered.
        path: PathBuf,
    },
    /// The source directory could not be resolved for a worker-side safety
    /// check.
    #[error("cannot safely access the rename directory {}: {message}", path.display())]
    Filesystem {
        /// Directory that could not be canonicalized.
        path: PathBuf,
        /// Bounded operating-system detail.
        message: String,
    },
}

/// Whether `name` is safe to use as one stored filesystem path component.
///
/// Storage names are user-controlled parts of paths, so the policy is shared
/// by every store instead of being reimplemented by each caller. A name must
/// be non-empty, have no surrounding whitespace, remain a single component,
/// avoid dot-prefixed entries, and contain no control characters.
pub fn is_valid_path_component(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed == name
        && !trimmed.starts_with('.')
        && !trimmed.contains(['/', '\\'])
        && !trimmed.chars().any(char::is_control)
}

/// Validate only the user-controlled part of a rename request.
///
/// This function is intentionally filesystem-free so reducers can use it for
/// immediate textual feedback without observing a stale directory state.
pub fn validate_rename_name(filename: &str) -> Result<(), SafetyError> {
    let name = filename.trim();
    if name.is_empty() {
        return Err(SafetyError::EmptyName);
    }
    if name != filename {
        return Err(SafetyError::UnsafeName {
            name: filename.to_string(),
        });
    }
    if name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(SafetyError::UnsafeName {
            name: filename.to_string(),
        });
    }

    Ok(())
}

/// Validate a rename target immediately before the worker mutates anything.
///
/// The returned path is `dir.join(filename)`. In addition to the textual
/// policy, the worker resolves the parent directory and refuses an existing
/// symlink target. Regular-file collision detection remains separate so the
/// caller can preserve the no-overwrite collision alert.
pub fn validate_rename_target(dir: &Path, filename: &str) -> Result<PathBuf, SafetyError> {
    validate_rename_name(filename)?;

    let canonical_dir = fs::canonicalize(dir).map_err(|error| SafetyError::Filesystem {
        path: dir.to_path_buf(),
        message: error.to_string(),
    })?;
    if !canonical_dir.is_dir() {
        return Err(SafetyError::Filesystem {
            path: dir.to_path_buf(),
            message: "rename parent is not a directory".to_string(),
        });
    }

    let target = dir.join(filename);
    if fs::symlink_metadata(&target)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(SafetyError::SymlinkedTarget { path: target });
    }

    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn empty_and_whitespace_names_are_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");

        for bad in ["", "   ", "\t", "\n"] {
            let error = validate_rename_target(dir.path(), bad).expect_err(bad);
            assert_eq!(error, SafetyError::EmptyName, "input {bad:?}");
        }
    }

    #[test]
    fn whitespace_padded_names_are_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");

        for bad in [" song.mp3", "song.mp3 ", "  hit  "] {
            let error = validate_rename_target(dir.path(), bad).expect_err(bad);
            assert!(
                matches!(error, SafetyError::UnsafeName { .. }),
                "input {bad:?} produced {error:?}"
            );
        }
    }

    #[test]
    fn path_separators_are_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");

        for bad in ["a/b.mp3", "a\\b.mp3", "/abs.mp3", "sub\\", "nul\0name"] {
            let error = validate_rename_target(dir.path(), bad).expect_err(bad);
            assert!(
                matches!(error, SafetyError::UnsafeName { .. }),
                "input {bad:?} produced {error:?}"
            );
        }
    }

    #[test]
    fn dot_segments_are_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");

        for bad in [".", ".."] {
            let error = validate_rename_target(dir.path(), bad).expect_err(bad);
            assert!(
                matches!(error, SafetyError::UnsafeName { .. }),
                "input {bad:?} produced {error:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_target_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("elsewhere dir");
        fs::write(elsewhere.join("real.mp3"), b"audio").expect("real file");
        // A dangling link is the dangerous case: Path::exists follows it and
        // reports false, so only the validator can catch it
        std::os::unix::fs::symlink(elsewhere.join("real.mp3"), dir.path().join("link.mp3"))
            .expect("symlink fixture");

        let error = validate_rename_target(dir.path(), "link.mp3").expect_err("symlink target");

        assert_eq!(
            error,
            SafetyError::SymlinkedTarget {
                path: dir.path().join("link.mp3")
            }
        );
    }

    #[test]
    fn valid_name_returns_the_joined_target() {
        let dir = tempfile::tempdir().expect("temp dir");

        let target = validate_rename_target(dir.path(), "new-name.mp3").expect("valid name");

        assert_eq!(target, dir.path().join("new-name.mp3"));
    }

    #[test]
    fn stored_path_component_policy_rejects_dangerous_names() {
        for name in [
            "",
            "   ",
            " leading",
            "trailing ",
            ".",
            "..",
            ".hidden",
            "../escape",
            "a/b",
            "a\\b",
            "bad\tname",
            "bad\nname",
            "bad\0name",
        ] {
            assert!(!is_valid_path_component(name), "unsafe name {name:?}");
        }
    }

    #[test]
    fn stored_path_component_policy_accepts_unicode_names() {
        assert!(is_valid_path_component("日本語-🎵"));
    }
}
