//! Background effects and request builders emitted by the application reducer.

use super::*;

/// Background work requested by a command and executed by the main loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Load one browser directory level off the UI thread.
    LoadBrowserDirectory {
        /// Identity used to discard a completion superseded by navigation.
        request_id: u64,
        /// Directory to enumerate.
        dir: PathBuf,
        /// Whether hidden entries should be included.
        show_hidden: bool,
        /// Folder name to select after returning to a parent directory.
        restore_cursor_name: Option<String>,
    },
    /// Validate a Settings browser-directory input on a blocking worker.
    ValidateBrowserDirectory {
        /// Identity used to discard a cancelled or superseded validation.
        request_id: u64,
        /// Path submitted for validation or symlink resolution.
        path: PathBuf,
        /// Consumer-specific state and safety policy for the validation.
        validation: BrowserDirectoryValidation,
    },
    /// Recursively scan a directory for supported audio on the shared runtime.
    ScanDirectory(PathBuf),
    /// Extract tags for queued paths inside a blocking worker.
    LoadMetadata(Vec<PathBuf>),
    /// Search the focused browser root or an in-memory playlist snapshot.
    Search {
        /// Request identity used to discard stale completions.
        request_id: u64,
        /// Search surface.
        scope: SearchScope,
        /// Browser root captured when the query was submitted.
        root: PathBuf,
        /// User query.
        query: String,
        /// Playlist snapshot captured when the query was submitted.
        tracks: Vec<crate::track::Track>,
    },
    /// Enumerate audio outputs off the UI thread for the current Settings visit.
    EnumerateOutputs {
        /// Settings visit that requested the enumeration.
        request_id: u64,
        /// Provider captured when the Settings popup opened.
        provider: OutputProviderHandle,
    },
    /// Forward one playback request to the dedicated audio worker.
    Audio(AudioCommand),
    /// Resolve and decode the cover of one queue entry inside a blocking
    /// worker, keeping source IO and decoding off the UI thread. Target
    /// resize and encoding are handled by the protocol's worker.
    LoadArtwork {
        /// Queue index the artwork belongs to, for the staleness gate.
        track_index: usize,
        /// Optional track file the loader probes for embedded and folder covers.
        /// Streams omit this path but can still resolve configured remote art.
        path: Option<PathBuf>,
        /// Track metadata for MusicBrainz search when remote is enabled.
        metadata: Option<TrackMetadata>,
        /// Which sources the artwork pipeline should consult.
        source_config: SourceConfig,
        /// Cache directory for remote artwork files.
        cache_dir: PathBuf,
    },
    /// Resolve lyrics for one queue entry inside a blocking worker.
    ///
    /// The resolution runs the local/embedded/remote chain; the outcome
    /// arrives as [`AppEvent::LyricsLoaded`] and the controller applies it
    /// with the same staleness gate used by artwork.
    LoadLyrics {
        /// Queue index the lyrics belong to, for the staleness gate.
        track_index: usize,
        /// Everything the chain needs to find the lyrics.
        request: LyricsRequest,
    },
    /// Persist the active named playlist without blocking the UI.
    ///
    /// Carries a rendered M3U snapshot taken at command time so the worker
    /// always writes a consistent queue even if the user keeps editing. The
    /// store lives in `AppServices`, so this effect stays `PartialEq` and free
    /// of IO. Rendering on the UI thread (instead of cloning the whole
    /// `Playlist` with its metadata) keeps large queues from being copied
    /// field by field just to be re-rendered in the worker.
    SaveActivePlaylist {
        /// Name of the playlist file to write, without extension.
        name: String,
        /// Serialized queue contents (`#EXTM3U` ...).
        contents: String,
    },
    /// List saved playlist names without blocking the reducer/UI thread.
    ListPlaylistNames {
        /// UI request identity used to discard stale completions.
        request_id: u64,
        /// State context that must still match when the worker completes.
        request: PlaylistNamesRequest,
    },
    /// Save a named playlist snapshot without blocking the reducer/UI thread.
    SavePlaylistNamed {
        /// UI request identity used to discard stale completions.
        request_id: u64,
        /// Name of the target playlist.
        name: String,
        /// Rendered queue snapshot.
        contents: String,
        /// User-visible state transition to apply after the write succeeds.
        action: PlaylistSaveAction,
    },
    /// Rename a saved playlist and perform the collision check off the UI
    /// thread.
    RenamePlaylistNamed {
        /// UI request identity used to discard stale completions.
        request_id: u64,
        /// Existing playlist name, without extension.
        old_name: String,
        /// Requested playlist name, without extension.
        new_name: String,
        /// Dialog workflow that owns the completion.
        action: PlaylistRenameAction,
    },
    /// Delete a saved playlist and collect the refreshed manager names on the
    /// same blocking worker.
    DeletePlaylistNamed {
        /// UI request identity used to discard stale completions.
        request_id: u64,
        /// Playlist name selected for deletion.
        name: String,
        /// Cursor captured before the confirmation popup opened.
        cursor: Option<usize>,
        /// Whether the deleted playlist owns the current queue.
        was_active: bool,
    },
    /// Load a named playlist without blocking the reducer/UI thread.
    LoadPlaylistNamed {
        /// UI request identity used to discard stale completions.
        request_id: u64,
        /// Playlist name captured by value for the blocking worker.
        name: String,
    },
    /// Persist an owned configuration snapshot without blocking the reducer.
    SaveConfig {
        /// Monotonic identity used to prevent stale snapshots from winning.
        request_id: u64,
        /// Configuration snapshot captured before the worker starts.
        config: AppConfig,
        /// Directory containing `config.toml`.
        config_dir: PathBuf,
    },
    /// Persist an owned runtime-state snapshot without blocking the reducer.
    SaveRuntimeState {
        /// Monotonic identity used to prevent stale snapshots from winning.
        request_id: u64,
        /// Runtime state captured before the worker starts.
        state: PersistedState,
        /// Directory containing `state.toml`.
        data_dir: PathBuf,
    },
    /// Enumerate available themes and load one palette on a blocking worker.
    LoadTheme {
        /// Settings-owned request identity used to reject stale results.
        request_id: u64,
        /// Settings visit that owns the request.
        visit_id: u64,
        /// Directory containing user theme files.
        themes_dir: PathBuf,
        /// Theme name captured by value for the worker.
        name: String,
        /// Whether the worker should also enumerate all theme names.
        include_names: bool,
        /// Consumer of the loaded palette.
        purpose: ThemeLoadPurpose,
    },
    /// Persist one editable theme palette on a blocking worker.
    SaveTheme {
        /// Settings-owned request identity used to reject stale results.
        request_id: u64,
        /// Settings visit that owns the request.
        visit_id: u64,
        /// Directory containing user theme files.
        themes_dir: PathBuf,
        /// Validated theme name captured by value for the worker.
        name: String,
        /// Editable palette captured before the worker starts.
        colors: crate::ui::theme::ThemeColors,
    },
    /// Resolve a stream URL off the UI thread and queue the result.
    ///
    /// The resolver inspects the URL, picks the right provider
    /// (YouTube, Radio Browser, Icecast/SHOUTcast, generic HTTP), and
    /// builds a [`crate::track::Track`] with full metadata attached. The
    /// UI thread receives the outcome through
    /// [`AppEvent::StreamResolved`] and either appends the new track or
    /// surfaces a notification.
    ResolveStream {
        /// Request identity used to discard a cancelled or superseded result.
        request_id: u64,
        /// URL the user typed into the Add Stream popup.
        url: url::Url,
        /// Cooperative cancellation shared with the resolver worker.
        cancellation: StreamResolutionCancellation,
    },
    /// Propagate the Remote lyrics preference into the shared lyrics service.
    ///
    /// Runs on the UI thread: the flag is atomic, so it applies instantly
    /// and background resolutions started before the switch may still finish.
    SetLyricsRemote(bool),
    /// Read the ten editable tag fields of one file inside a blocking worker.
    ///
    /// The metadata editor opens with a loading state and receives the raw
    /// values as [`AppEvent::MetadataPrefillReady`], keeping lofty decoding
    /// off the UI thread per the reader contract.
    EditMetadataPrefill {
        /// Audio file whose tag fields the editor will show.
        path: PathBuf,
    },
    /// Validate and rename one audio file after rewriting every playlist that
    /// references it.
    ///
    /// The worker runs the whole sequence off the UI thread: it rewrites all
    /// matching `.m3u8` documents first (aborting without touching the file
    /// when any rewrite fails) and only then renames on disk, reporting the
    /// outcome as [`AppEvent::RenameCompleted`]. The reducer supplies only the
    /// owned textual intent; collision, symlink, canonicalization and final
    /// no-overwrite checks stay in the worker.
    RenameFileOnDisk {
        /// UI request identity used to discard stale or cancelled results.
        request_id: u64,
        /// Current location of the file.
        from: PathBuf,
        /// New direct-child name, without a preflight filesystem decision.
        new_name: String,
        /// Browser directory captured for the worker-side refresh decision.
        browser_dir: PathBuf,
    },
    /// Persist the ten edited tag fields into one file inside a blocking worker.
    ///
    /// The write goes through the file's primary tag and reports the outcome
    /// as [`AppEvent::MetadataWriteCompleted`].
    EditMetadataWrite {
        /// File whose tags are written.
        path: PathBuf,
        /// Field values in [`crate::metadata::MetaField::ALL`] order.
        fields: [String; 10],
    },
    /// Rewrite the `#EXTINF` label that precedes every saved playlist line
    /// referencing `path`, setting the trailing display label to
    /// `new_title`. Dispatched by the rename flow when the renamed file
    /// has no lofty Title tag, and by the metadata-write flow when the user
    /// edits the Title field.
    UpdateExtinfTitle {
        /// Path of the file whose EXTINF labels must be updated.
        path: PathBuf,
        /// New display label (the new file name on rename, the new title
        /// from the metadata editor on a Title write).
        new_title: String,
    },
    /// Rewrite the EXTINF label that precedes every saved playlist line
    /// matching the given stream URL, setting the trailing display label
    /// to `new_title`. Dispatched by the rename/edit-metadata shortcuts on
    /// a stream track.
    UpdateStreamExtinf {
        /// URL of the stream whose EXTINF labels must be updated.
        url: url::Url,
        /// New display label the user typed in the rename dialog.
        new_title: String,
    },
}

/// Start one browser listing request and capture the current visibility
/// setting in its effect. All browser callers use this state transition so a
/// late listing cannot outlive the request identity that created it.
pub(crate) fn request_browser_directory(
    state: &mut AppState,
    dir: PathBuf,
    restore_cursor_name: Option<String>,
) -> Effect {
    let request_id = state.browser.begin_directory_request();
    Effect::LoadBrowserDirectory {
        request_id,
        dir,
        show_hidden: state.browser.show_hidden,
        restore_cursor_name,
    }
}

/// Start a playlist-name listing while retaining its UI request context.
pub(crate) fn request_playlist_names(
    state: &mut AppState,
    request: PlaylistNamesRequest,
) -> Vec<Effect> {
    let Some(request_id) = state.async_ops.playlist_request.try_begin() else {
        return Vec::new();
    };
    vec![Effect::ListPlaylistNames {
        request_id,
        request,
    }]
}

/// Validate a named-save dialog against the worker-owned playlist listing.
pub(crate) fn request_named_save_validation(
    state: &mut AppState,
    action: PlaylistSaveAction,
    name: String,
) -> Vec<Effect> {
    let current_name = state.active_playlist_name.clone();
    request_playlist_names(
        state,
        PlaylistNamesRequest::ConfirmSave {
            action,
            name,
            current_name,
        },
    )
}

/// Start a named playlist save from an immutable queue snapshot.
pub(crate) fn start_playlist_save(
    state: &mut AppState,
    action: PlaylistSaveAction,
    name: String,
) -> Vec<Effect> {
    let Some(request_id) = state.async_ops.playlist_request.try_begin() else {
        return Vec::new();
    };
    let contents = match action {
        PlaylistSaveAction::NewPlaylist | PlaylistSaveAction::Overwrite { new_playlist: true } => {
            crate::playlist::render_m3u(&Playlist::new())
        }
        PlaylistSaveAction::SaveAs
        | PlaylistSaveAction::Overwrite {
            new_playlist: false,
        }
        | PlaylistSaveAction::RenamePlaying => crate::playlist::render_m3u(&state.playlist),
    };
    vec![Effect::SavePlaylistNamed {
        request_id,
        name,
        contents,
        action,
    }]
}

/// Start a saved-playlist rename with a UI request identity.
pub(crate) fn start_playlist_rename(
    state: &mut AppState,
    action: PlaylistRenameAction,
    old_name: String,
    new_name: String,
) -> Vec<Effect> {
    let Some(request_id) = state.async_ops.playlist_request.try_begin() else {
        return Vec::new();
    };
    vec![Effect::RenamePlaylistNamed {
        request_id,
        old_name,
        new_name,
        action,
    }]
}

/// Start deletion of the playlist selected by the confirmation popup.
pub(crate) fn start_playlist_delete(state: &mut AppState) -> Vec<Effect> {
    let (name, cursor) = match state.popup_dialog.active_popup_ref() {
        Some(Popup::ConfirmDelete { name, cursor }) => (name.clone(), *cursor),
        _ => return Vec::new(),
    };
    let Some(request_id) = state.async_ops.playlist_request.try_begin() else {
        return Vec::new();
    };
    let was_active = state.active_playlist_name.as_deref() == Some(name.as_str());
    vec![Effect::DeletePlaylistNamed {
        request_id,
        name,
        cursor,
        was_active,
    }]
}
