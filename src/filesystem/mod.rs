//! Filesystem domain shared by the interactive browser and the scanner.
//!
//! Layout rationale: [`entry`] holds the pure data model, `browser` holds
//! synchronous single level listing plus sorting helpers used by the UI
//! thread, and `scanner` is the only async piece performing recursive
//! collection on the shared Tokio runtime.

pub mod browser;
pub mod entry;
pub(crate) mod persistence;
pub mod rename;
pub mod safety;
pub mod scanner;

pub use browser::{read_sorted_entries, resolve_start_dir, resolve_start_dir_from, sort_entries};
pub use entry::{AUDIO_EXTENSIONS, EntryKind, FileEntry, is_supported_audio};
pub use rename::{FilesystemRenameService, RenameFileResult, RenameService};
pub use scanner::{MAX_SCAN_DEPTH, scan_directory};
