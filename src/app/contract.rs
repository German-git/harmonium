//! Application contracts shared by the reducer, controllers, effects, and event bridge.

use super::*;

/// Why the playlist-name worker was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistSaveAction {
    /// Save the current queue under a name without replacing the queue.
    SaveAs,
    /// Create and activate a new empty playlist.
    NewPlaylist,
    /// Complete an overwrite confirmation; `new_playlist` preserves its
    /// distinct queue-reset semantics.
    Overwrite { new_playlist: bool },
    /// Save an anonymous queue as the first named playlist through the rename
    /// dialog.
    RenamePlaying,
}

/// Which saved-playlist rename workflow requested a worker operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistRenameAction {
    /// Rename the playlist currently owning the queue.
    Playing,
    /// Rename a playlist selected in the manager popup.
    Saved,
}

/// Typed result of a worker-side saved-playlist rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistRenameResult {
    /// The source was moved to the requested name.
    Success,
    /// The requested target name was already present when the worker checked.
    Conflict,
    /// The worker reached the store but the operation failed.
    Failed(WorkerError),
}

/// Typed result of a worker-side saved-playlist deletion and refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistDeleteResult {
    /// Delete outcome, or the worker-side failure detail.
    pub deletion: WorkerResult<()>,
    /// Names observed after the delete attempt, used to refresh the manager.
    pub names: WorkerResult<Vec<String>>,
}

/// State context captured by a playlist-name listing request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistNamesRequest {
    /// Populate the manager popup after it opens.
    OpenManager,
    /// Supply the generated default for Save As or validate its later submit.
    BeginSaveAs,
    /// Supply the generated default for a new playlist.
    BeginNew,
    /// Validate a naming dialog against the worker's snapshot.
    ConfirmSave {
        action: PlaylistSaveAction,
        name: String,
        current_name: Option<String>,
    },
    /// Refresh an already visible manager popup after a successful save.
    RefreshManager { cursor: usize },
}

/// Browser-directory validation context carried through a blocking worker.
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserDirectoryValidation {
    /// Validate a path submitted by the Settings editor.
    Settings {
        /// Trimmed input used to preserve the dialog error text.
        input: String,
        /// Directory shown before the edit began.
        previous_dir: PathBuf,
    },
    /// Resolve one explicitly activated browser symlink.
    Symlink {
        /// Directory containing the activated link.
        parent_dir: PathBuf,
        /// Link name restored when returning from the child directory.
        folder_name: String,
    },
}

/// Why a blocking theme load was requested by the Settings workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeLoadPurpose {
    /// Populate a newly opened Settings draft.
    SettingsOpen,
    /// Refresh the palette preview after moving the theme cursor.
    SettingsPreview,
    /// Load the palette selected with Enter for the running UI.
    Apply,
}

/// Provider captured by an output enumeration effect.
///
/// The wrapper keeps [`Effect`] comparable for existing tests without making
/// the trait object itself part of the equality contract.
#[derive(Clone)]
pub struct OutputProviderHandle(pub(crate) ArcOutputProvider);

impl std::fmt::Debug for OutputProviderHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OutputProviderHandle(..)")
    }
}

impl PartialEq for OutputProviderHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for OutputProviderHandle {}

impl OutputProviderHandle {
    pub(crate) fn provider(&self) -> ArcOutputProvider {
        Arc::clone(&self.0)
    }
}
