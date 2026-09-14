//! Application core applying commands to state, free of terminal concerns.
//!
//! Commands that need background work do not touch IO themselves. They
//! return [`Effect`] values which the main loop executes against
//! [`AppServices`], keeping this module pure and unit testable while the
//! single runtime ownership rule stays intact.

mod contract;
mod dialogs;
mod effect;
mod file_actions;
mod modal_router;
mod playback;
mod playlist;
mod runner;
mod search;
mod settings;

use crate::app::dialogs::DialogController;
use crate::app::file_actions::FileActionsController;
use crate::app::modal_router::{
    ConfirmDialogTarget, DialogAction, ModalAction, ModalRouter, SearchAction,
};
use crate::app::playback::PlaybackController;
use crate::app::playlist::PlaylistController;
use crate::app::search::SearchController;
use crate::app::settings::SettingsController;
pub use crate::filesystem::RenameFileResult;
pub use contract::{
    BrowserDirectoryValidation, OutputProviderHandle, PlaylistDeleteResult, PlaylistNamesRequest,
    PlaylistRenameAction, PlaylistRenameResult, PlaylistSaveAction, ThemeLoadPurpose,
};
pub use effect::Effect;
#[cfg(test)]
use effect::request_named_save_validation;
use effect::{
    request_browser_directory, request_playlist_names, start_playlist_delete,
    start_playlist_rename, start_playlist_save,
};
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::app::file_actions::split_file_name;
use crate::artwork::{ArtworkBackend, ArtworkBackendKind, ArtworkOverlay, ArtworkProtocol};
use crate::audio::{
    ArcOutputProvider, AudioCommand, AudioOutput, NullOutputProvider, OutputTarget, PlayStatus,
    SPEED_DEFAULT, SPEED_STEP, VOLUME_STEP_PERCENT, previous_restarts_track, seek_step,
    stepped_speed, stepped_volume,
};
use crate::command::{
    Command, CommandContext, CommandGroup, FileCommand, LifecycleCommand, NavigationCommand,
    PanelTarget, PlaybackCommand, PlaylistCommand, QueueCommand, UiCommand,
};
use crate::config::{AppConfig, ArtworkSource as SourceConfig, KeysConfig, PersistedState};
use crate::error::Result as DomainResult;
use crate::error::{WorkerError, WorkerResult};
use crate::event::{AppEvent, EffectErrorKind, EventSender};
use crate::filesystem::{EntryKind, is_supported_audio, resolve_start_dir, scan_directory};
use crate::input::{HelpContentCache, InputContext, InputMapper, help_line_count};
use crate::lyrics::{LoadOutcome, LyricsRequest};
use crate::metadata::TrackMetadata;
use crate::metadata::reader::collect_metadata;
use crate::playlist::navigation::{manual_next_action, manual_previous_target};
use crate::playlist::{Playlist, PlaylistStore};
use crate::runtime::EffectServices;
use crate::search::{SearchResult, SearchScope, search_browser_paths, search_playlist_tracks};
use crate::state::{
    AppState, DialogMode, FrameMetrics, NOTIFICATIONS_CAP, Panel, PlaylistAppendResult, Popup,
    SettingsDraft, SettingsField, SettingsFocus, SettingsTab, StreamResolutionCancellation,
    next_panel, previous_panel, push_notification,
};
use crate::track::{Track, TrackLocation};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Rows one line scroll step moves inside the help popup.
const HELP_LINE_STEP: u16 = 1;
/// Rows one page scroll moves inside the help popup.
///
/// A fixed step stands in for the viewport height because the command
/// layer never measures render areas, and clamping keeps both ends exact
/// regardless of the actual popup height.
const HELP_PAGE_STEP: u16 = 10;

/// Owns the application state and applies commands to it.
#[derive(Debug)]
pub struct App {
    state: AppState,
    playlist: PlaylistController,
    search: SearchController,
    input_mapper: InputMapper,
    keys: KeysConfig,
    /// Unstyled help rows rebuilt when the authoritative key configuration changes.
    help_cache: HelpContentCache,
    /// Store used by the synchronous playlist-load path. Named saves,
    /// renames, deletes and playlist-name listings run through effects; the
    /// worker-side clone in `AppServices` shares this store's write lock.
    playlist_store: PlaylistStore,
    /// Full user configuration, kept so the settings screen can persist
    /// `[keys]`, `[sound]`, `[ui]` and `[log]` sections to `config.toml`.
    config: AppConfig,
    /// Directory holding `config.toml`, used by the settings apply path.
    config_dir: PathBuf,
    /// Directory holding `state.toml`, used to persist runtime state immediately.
    data_dir: PathBuf,
    /// Border glyph style currently used by the renderer.
    border_type: crate::config::BorderType,
    /// Source of the audio output list, injected by the entrypoint. Kept
    /// behind a trait so the UI never depends on a concrete backend.
    output_provider: ArcOutputProvider,
    /// Monotonic identity for the currently visible Settings visit.
    output_enumeration_request_id: u64,
    /// Immutable panel data produced by the last completed frame tick.
    panel_view: crate::ui::view::PanelViewModel,
}

fn output_list_for_selection(mut outputs: Vec<AudioOutput>, selected_id: &str) -> Vec<AudioOutput> {
    if outputs.is_empty() {
        outputs.push(AudioOutput::session_default());
    } else if !outputs.iter().any(|output| output.id.is_empty()) {
        outputs.insert(0, AudioOutput::session_default());
    }

    if !selected_id.is_empty() && !outputs.iter().any(|output| output.id == selected_id) {
        outputs.push(AudioOutput {
            id: selected_id.to_string(),
            name: "Unavailable".to_string(),
            description: "Saved output is not currently available".to_string(),
            is_default: false,
            available: false,
            node_id: None,
        });
    }
    outputs
}

fn selected_output_index(outputs: &[AudioOutput], selected_id: &str) -> usize {
    outputs
        .iter()
        .position(|output| output.id == selected_id)
        .unwrap_or(0)
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Create an application with default state and a placeholder store.
    ///
    /// Production wiring goes through [`Self::from_config_and_store`] so the
    /// real playlists directory is used; this path points the store at an
    /// empty path that is only ever touched if a named save is triggered.
    pub fn new() -> Self {
        Self::from_config_and_store(
            KeysConfig::default(),
            PlaylistStore::for_dir(PathBuf::new()),
        )
    }

    /// Create an application from user configuration.
    pub fn from_config(keys: KeysConfig) -> Self {
        Self::from_config_and_store(keys, PlaylistStore::for_dir(PathBuf::new()))
    }

    /// Create an application from configuration and the shared playlist store.
    pub fn from_config_and_store(keys: KeysConfig, playlist_store: PlaylistStore) -> Self {
        let input_mapper = InputMapper::from_config(&keys);
        let state = AppState::default();
        let help_cache = HelpContentCache::new(&keys);
        Self {
            panel_view: crate::ui::view::PanelViewModel::from_state(
                &state,
                1,
                &help_cache,
                "",
                None,
            ),
            state,
            playlist: PlaylistController,
            search: SearchController,
            input_mapper,
            help_cache,
            keys,
            playlist_store,
            config: AppConfig::default(),
            config_dir: PathBuf::new(),
            data_dir: PathBuf::new(),
            border_type: crate::config::BorderType::default(),
            output_provider: Arc::new(NullOutputProvider),
            output_enumeration_request_id: 0,
        }
    }

    /// Inject the loaded configuration and its directories so settings can be
    /// persisted to `config.toml` and `state.toml`. Called once by the entrypoint.
    pub fn set_config_paths(&mut self, config: AppConfig, config_dir: PathBuf, data_dir: PathBuf) {
        self.config = config;
        self.border_type = self.config.ui.border_type;
        self.config_dir = config_dir;
        self.data_dir = data_dir;
    }

    /// Inject the audio output provider used to list and apply the selected
    /// output. Called once at startup after the real provider is built.
    pub fn set_output_provider(&mut self, provider: ArcOutputProvider) {
        self.output_provider = provider;
    }

    /// Apply persisted preferences through the startup capability boundary.
    pub fn apply_startup_preferences(&mut self, config: &AppConfig, persisted: &PersistedState) {
        self.state.confirm_quit = config.general.confirm_quit;
        self.state.browser.show_hidden = config.general.show_hidden;
        self.state.now_playing_display = config.general.now_playing_display.clone();
        self.state.playlist_columns = config.general.playlist_columns.clone();
        self.state.persistence.resume_previous_track = config.general.resume_previous_track;
        self.state.persistence.last_track = persisted.track_location();
        self.state.persistence.last_track_position_ms = persisted.last_track_position_ms;
        self.state.playback.volume_percent = config.general.volume_percent;
        self.state.playback_mode =
            crate::playback_mode::PlaybackMode::new(persisted.repeat_mode, persisted.shuffle);
        self.state
            .artwork
            .set_visible(config.general.artwork_visible);
    }

    /// Configure artwork through the application-owned capability boundary.
    pub fn initialize_artwork_backend(&mut self, backend: &ArtworkBackend) {
        backend.synchronize_state(&mut self.state.artwork.state);
        let enabled = backend.kind() != ArtworkBackendKind::Disabled;
        if let Some(loader) = backend.loader() {
            self.state
                .artwork
                .set_playlist_target(loader.playlist_target());
        }
        self.state.artwork.set_enabled(enabled);
    }

    /// Start the selected external artwork renderer without exposing artwork state.
    pub fn start_artwork_backend(&mut self, backend: &mut ArtworkBackend) {
        backend.start_external(&mut self.state.artwork.state);
    }

    /// Take a native artwork failure reported by the resize worker.
    pub fn take_native_artwork_failure(&mut self) -> bool {
        self.state.artwork.take_native_render_failure()
    }

    /// Report a native artwork failure through the backend fallback policy.
    pub fn report_native_artwork_failure(&mut self, backend: &mut ArtworkBackend) {
        backend.report_native_failure(&mut self.state.artwork.state);
    }

    /// Reconcile external artwork after the frame has been rendered.
    pub fn reconcile_artwork(
        &mut self,
        backend: &mut ArtworkBackend,
        desired: Option<ArtworkOverlay>,
    ) {
        backend.reconcile(desired, &mut self.state.artwork.state);
    }

    /// Execute effects using the application-owned pending-operation counter.
    pub fn execute_effects<S: EffectServices + ?Sized>(
        &mut self,
        effects: Vec<Effect>,
        services: &S,
    ) {
        let failures =
            runner::execute_effects(effects, services, &self.state.async_ops.pending_effects);
        for failure in failures {
            self.compensate_dispatch_failure(failure);
        }
    }

    fn compensate_dispatch_failure(&mut self, failure: runner::DispatchFailure) {
        let compensated = match failure.compensation {
            runner::DispatchCompensation::StreamResolution { request_id } => {
                self.state.async_ops.cancel_stream_resolution_if(request_id)
            }
            runner::DispatchCompensation::BrowserValidation { request_id } => self
                .state
                .async_ops
                .cancel_browser_validation_if(request_id),
            runner::DispatchCompensation::Playlist { request_id } => {
                self.state.async_ops.cancel_playlist_request_if(request_id)
            }
            runner::DispatchCompensation::ThemeLoad {
                request_id,
                visit_id,
            } => self
                .state
                .async_ops
                .cancel_theme_request_if(request_id, visit_id),
            runner::DispatchCompensation::ThemeSave {
                request_id,
                visit_id,
            } => self
                .state
                .async_ops
                .cancel_theme_save_request_if(request_id, visit_id),
            runner::DispatchCompensation::FileRename { request_id } => self
                .state
                .async_ops
                .cancel_file_rename_request_if(request_id),
            runner::DispatchCompensation::Search { .. }
            | runner::DispatchCompensation::OutputEnumeration { .. }
            | runner::DispatchCompensation::None => false,
        };
        let suffix = if compensated {
            "; UI state rolled back"
        } else {
            "; state was already superseded"
        };
        self.push_notification(format!(
            "Could not start {}: {}{}",
            failure.operation, failure.error, suffix
        ));
    }

    /// Read-only access to the queue without exposing the root state mutator.
    pub fn playlist(&self) -> &Playlist {
        &self.state.playlist
    }

    /// Read-only access to playback snapshots and startup values.
    pub fn playback(&self) -> &crate::audio::PlaybackState {
        &self.state.playback
    }

    /// Append tracks through the queue capability while preserving its rules.
    pub fn extend_playlist(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> PlaylistAppendResult {
        self.state.extend_playlist(paths)
    }

    /// Append resolved tracks through the queue capability boundary.
    pub fn extend_playlist_tracks(
        &mut self,
        tracks: impl IntoIterator<Item = crate::track::Track>,
    ) -> PlaylistAppendResult {
        self.state.extend_playlist_tracks(tracks)
    }

    /// Set the persisted playback identity through its capability boundary.
    pub fn set_persistence_identity(
        &mut self,
        resume_previous_track: bool,
        last_track: Option<TrackLocation>,
        last_track_position_ms: u64,
    ) {
        self.state.persistence.resume_previous_track = resume_previous_track;
        self.state.persistence.last_track = last_track;
        self.state.persistence.last_track_position_ms = last_track_position_ms;
    }

    /// Configure artwork sources without exposing the root state mutator.
    pub fn configure_artwork(&mut self, source_config: SourceConfig, cache_dir: PathBuf) {
        self.state.artwork.source_config = source_config;
        self.state.artwork.cache_dir = cache_dir;
    }

    /// Replace the active queue and its persisted name during startup replay.
    pub fn restore_playlist(&mut self, playlist: Playlist, name: Option<String>) {
        self.state.playlist = playlist;
        self.state.active_playlist_name = name;
    }

    /// Set the active playlist name without exposing the root state mutator.
    pub fn set_active_playlist_name(&mut self, name: Option<String>) {
        self.state.active_playlist_name = name;
    }

    /// Select a queue entry and preserve the playlist viewport invariant.
    pub fn select_playlist_track(&mut self, index: usize) {
        self.state.playlist.select(index);
        self.state
            .reanchor_playlist_scroll(crate::browser_state::ScrollDirection::Down);
    }

    /// Set the focused panel through the application capability boundary.
    pub fn set_active_panel(&mut self, panel: Panel) {
        self.state.active_panel = panel;
    }

    /// Set the persisted browser start directory used by startup replay.
    pub fn set_browser_start_dir(&mut self, dir: PathBuf) {
        self.state.browser.start_dir = dir;
    }

    /// Current named playlist, if the queue is persisted.
    pub fn active_playlist_name(&self) -> Option<&str> {
        self.state.active_playlist_name.as_deref()
    }

    /// Current playlist scroll offset for rendering and verification.
    pub fn playlist_scroll_offset(&self) -> usize {
        self.state.playlist_scroll_offset
    }

    /// Set the measured playlist viewport for focused fixtures.
    pub fn set_playlist_viewport_height(&mut self, height: u16) {
        self.state.playlist_viewport_height = height;
    }

    fn settings_controller(&mut self) -> SettingsController<'_> {
        SettingsController::new(
            &mut self.state,
            &mut self.config,
            &self.config_dir,
            &self.data_dir,
            &mut self.keys,
            &mut self.input_mapper,
            &mut self.border_type,
            &self.output_provider,
        )
    }

    pub(crate) fn handle_settings_key(&mut self, command: Command) -> Vec<Effect> {
        let effects = self.settings_controller().handle_settings_key(command);
        self.help_cache.refresh(&self.keys);
        effects
    }

    pub(crate) fn commit_settings_edit(&mut self) -> Vec<Effect> {
        let effects = self.settings_controller().commit_settings_edit();
        self.help_cache.refresh(&self.keys);
        effects
    }

    #[cfg(test)]
    pub(crate) fn apply_settings(&mut self) -> Vec<Effect> {
        let effects = self.settings_controller().apply_settings_inner();
        self.help_cache.refresh(&self.keys);
        effects
    }

    #[cfg(test)]
    pub(crate) fn help_content(&self) -> &HelpContentCache {
        &self.help_cache
    }

    pub(crate) fn resolve_file_action_target(&self) -> Option<PathBuf> {
        FileActionsController::resolve_file_action_target(&self.state)
    }

    pub(crate) fn resolve_stream_action_target(&self) -> Option<crate::stream::StreamTarget> {
        FileActionsController::resolve_stream_action_target(&self.state)
    }

    fn file_actions_controller(&mut self) -> FileActionsController<'_> {
        FileActionsController::new(&mut self.state)
    }

    pub(crate) fn open_stream_rename_dialog(
        &mut self,
        url: url::Url,
        kind: crate::stream::StreamKind,
    ) {
        self.file_actions_controller()
            .open_stream_rename_dialog(url, kind);
    }

    #[cfg(test)]
    pub(crate) fn commit_rename_stream(&mut self) -> Vec<Effect> {
        self.file_actions_controller().commit_rename_stream()
    }

    #[cfg(test)]
    pub(crate) fn commit_rename(&mut self) -> Vec<Effect> {
        self.file_actions_controller().commit_rename()
    }

    #[cfg(test)]
    pub(crate) fn commit_metadata_save(&mut self) -> Vec<Effect> {
        self.file_actions_controller().commit_metadata_save()
    }

    pub fn apply_rename_completed(
        &mut self,
        from: PathBuf,
        to: PathBuf,
        ok: bool,
        conflict: Option<PathBuf>,
    ) -> Vec<Effect> {
        self.file_actions_controller()
            .apply_rename_completed(from, to, ok, conflict, false)
    }

    pub fn apply_rename_completed_with_browser_refresh(
        &mut self,
        from: PathBuf,
        to: PathBuf,
        ok: bool,
        conflict: Option<PathBuf>,
        refresh_browser: bool,
    ) -> Vec<Effect> {
        self.file_actions_controller().apply_rename_completed(
            from,
            to,
            ok,
            conflict,
            refresh_browser,
        )
    }

    /// Apply a worker-owned generic file-rename completion after both runtime
    /// operation and UI request identity have been checked.
    pub fn apply_rename_request_completed(
        &mut self,
        request_id: u64,
        from: PathBuf,
        to: PathBuf,
        result: RenameFileResult,
    ) -> Vec<Effect> {
        if !self.state.async_ops.file_rename_request.accept(request_id) {
            tracing::debug!(request_id, "ignoring stale file-rename result");
            return Vec::new();
        }
        self.file_actions_controller()
            .apply_rename_result_completed(from, to, result)
    }

    pub fn apply_metadata_write_completed(
        &mut self,
        path: PathBuf,
        result: WorkerResult<()>,
        fields: [String; 10],
    ) -> Vec<Effect> {
        self.file_actions_controller()
            .apply_metadata_write_completed(path, result, fields)
    }

    pub(crate) fn close_dialog(&mut self) {
        self.file_actions_controller().close_dialog();
    }

    pub(crate) fn handle_dialog_confirm(&mut self) -> Vec<Effect> {
        if self.state.popup_dialog.dialog_mode_ref() == Some(&DialogMode::SettingsEdit) {
            self.commit_settings_edit()
        } else {
            DialogController::new(&mut self.state).handle_dialog_confirm()
        }
    }

    pub(crate) fn confirm_overwrite(&mut self) -> Vec<Effect> {
        DialogController::new(&mut self.state).confirm_overwrite()
    }

    pub(crate) fn confirm_delete(&mut self) -> Vec<Effect> {
        DialogController::new(&mut self.state).confirm_delete()
    }

    fn playback_controller(&mut self) -> PlaybackController<'_> {
        PlaybackController::new(&mut self.state, self.config.playback.crossfade_seconds)
    }

    pub fn apply_playback_progress(&mut self, snapshot: crate::audio::PlaybackSnapshot) {
        self.playback_controller().apply_progress(snapshot);
    }

    pub fn apply_track_ended(&mut self, track_index: usize) -> Vec<Effect> {
        self.playback_controller().apply_track_ended(track_index)
    }

    pub fn apply_crossfade_completed(
        &mut self,
        track_index: usize,
        path: PathBuf,
        elapsed: Duration,
    ) -> Vec<Effect> {
        self.playback_controller()
            .apply_crossfade_completed(track_index, path, elapsed)
    }

    pub fn apply_source_ready(&mut self, url: String) {
        self.playback_controller().apply_source_ready(url);
    }

    pub fn apply_source_ready_with_generation(&mut self, generation: u64, url: String) {
        self.playback_controller()
            .apply_source_ready_with_generation(Some(generation), url);
    }

    pub fn apply_source_failed(&mut self, url: String) {
        self.playback_controller().apply_source_failed(url);
    }

    pub fn apply_source_failed_with_generation(&mut self, generation: u64, url: String) {
        self.playback_controller()
            .apply_source_failed_with_generation(Some(generation), url);
    }

    pub fn apply_artwork_loaded(&mut self, track_index: usize, artwork: Option<ArtworkProtocol>) {
        self.playback_controller()
            .apply_artwork_loaded(track_index, artwork);
    }

    pub fn apply_artwork_resizes(&mut self) {
        self.playback_controller().apply_artwork_resizes();
    }

    pub fn apply_lyrics_loaded(&mut self, track_index: usize, outcome: LoadOutcome) {
        self.playback_controller()
            .apply_lyrics_loaded(track_index, outcome);
    }

    fn next_track(&mut self, effects: &mut Vec<Effect>) {
        self.playback_controller().next_track(effects);
    }

    fn previous_track(&mut self, effects: &mut Vec<Effect>) {
        self.playback_controller().previous_track(effects);
    }

    fn seek(&mut self, backward: bool, effects: &mut Vec<Effect>) {
        self.playback_controller().seek(backward, effects);
    }

    fn play_selected(&mut self, effects: &mut Vec<Effect>) {
        self.playback_controller().play_selected(effects);
    }

    pub fn begin_current_track(&mut self) -> Vec<Effect> {
        self.playback_controller().begin_current_track()
    }

    /// Build the session-scoped state that lives in `state.toml`. User
    /// preferences are NOT part of it; they belong to `config.toml [general]`.
    pub fn persisted_state(&self) -> PersistedState {
        PersistedState {
            repeat_mode: self.state.playback_mode.repeat(),
            shuffle: self.state.playback_mode.shuffle(),
            last_playlist: self.state.active_playlist_name.clone(),
            last_track_path: self
                .state
                .persistence
                .last_track
                .as_ref()
                .map(TrackLocation::to_persisted),
            last_track_position_ms: self.state.persistence.last_track_position_ms,
        }
    }

    /// Persist runtime state to `data_dir/state.toml` when the data directory is
    /// known. Failures are ignored because a failed save must not interrupt use.
    pub fn persist_runtime_state(&mut self) -> Vec<Effect> {
        self.persist_runtime_state_inner(true)
    }

    fn persist_runtime_state_inner(&mut self, persist_browser_directory: bool) -> Vec<Effect> {
        if self.data_dir.as_os_str().is_empty() {
            return Vec::new();
        }
        // Session-scoped state lives in state.toml.
        let state_request_id = self.state.async_ops.begin_runtime_state_save();
        let state_effect = Effect::SaveRuntimeState {
            request_id: state_request_id,
            state: self.persisted_state(),
            data_dir: self.data_dir.clone(),
        };

        // User preferences belong to config.toml [general]; mirror the runtime
        // values into the config we hold so the settings screen and the startup
        // read the same source of truth. This is the single persistence path:
        // the exit hand-off in main.rs delegates here so both callers agree on
        // exactly which preference is written.
        self.config.general.volume_percent = self.state.playback.volume_percent;
        self.config.general.confirm_quit = self.state.confirm_quit;
        self.config.general.resume_previous_track = self.state.persistence.resume_previous_track;
        self.config.general.artwork_visible = self.state.artwork.is_visible();
        self.config.general.show_hidden = self.state.browser.show_hidden;
        self.config.general.playlist_columns = self.state.playlist_columns.clone();
        self.config.general.now_playing_display = self.state.now_playing_display.clone();
        self.config.ui.border_type = self.border_type;
        if persist_browser_directory {
            self.config.general.browser_directory = self
                .state
                .browser
                .current_dir
                .to_string_lossy()
                .into_owned();
        }
        vec![state_effect, self.save_config_effect()]
    }

    /// Capture the current configuration for the blocking persistence worker.
    pub fn save_config_effect(&mut self) -> Effect {
        let request_id = self.state.async_ops.begin_config_save();
        Effect::SaveConfig {
            request_id,
            config: self.config.clone(),
            config_dir: self.config_dir.clone(),
        }
    }

    /// Reorder the active playlist by the explicit Playlist columns action and
    /// persist the new order to its .m3u8 when the queue has a name.
    ///
    /// Non-blocking: it only touches the in-memory queue order and, if needed,
    /// the playlist document; playback keeps running on the same track.
    fn apply_sort_tracks(&mut self, config: &crate::config::PlaylistColumnsConfig) -> Vec<Effect> {
        if self.state.playlist.is_empty() {
            return Vec::new();
        }
        // Capture the playing track's location BEFORE the sort. Once the
        // tracks are reordered, `state.playback.track_index` still points at
        // the same numeric slot, which now hosts a different track. The
        // reanchor must compare against the pre-sort identity, not whatever
        // happens to sit at the old slot after the reorder.
        let playing_location = self.state.playback.track_index.and_then(|index| {
            self.state
                .playlist
                .tracks()
                .get(index)
                .map(Track::track_location)
        });
        crate::playlist::sorter::sort_playlist(&mut self.state.playlist, config);
        self.reanchor_after_playlist_reorder(playing_location.as_ref());
        self.autosave_active_playlist()
    }

    /// Restore selection/playback indices after the queue order changed.
    ///
    /// Re-maps the playing track's queue index to its new position, moves the
    /// visual cursor there too, rebuilds the navigation pools and re-anchors
    /// the scroll so the cursor stays visible. Shared by an explicit sort and
    /// by the explicit Sort tracks action.
    fn reanchor_after_playlist_reorder(&mut self, pre_sort_location: Option<&TrackLocation>) {
        // Look up the pre-sort typed identity after the reorder has settled.
        // If found, both the playback
        // track_index (drives the ▶ glyph and the Now Playing band) and
        // the panel cursor (drives the highlighted row) follow it.
        if let Some(location) = pre_sort_location
            && let Some(new_index) = self
                .state
                .playlist
                .tracks()
                .iter()
                .position(|t| t.track_location() == *location)
        {
            self.state.playback.track_index = Some(new_index);
            self.state.playlist.select(new_index);
        }

        // The queue order changed, so the navigation pools are stale.
        self.state
            .navigation
            .rebuild_after_mutation(self.state.playlist.len(), self.state.playback.track_index);
        // Re-anchor the playlist scroll so the cursor stays visible.
        self.state
            .reanchor_playlist_scroll(crate::browser_state::ScrollDirection::Down);
    }

    /// Store driving synchronous playlist IO. Tests inject a temp store here.
    pub fn playlist_store(&self) -> &PlaylistStore {
        &self.playlist_store
    }

    /// Read only access to the underlying state.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Read-only user-facing notifications for integration boundaries.
    pub fn notifications(&self) -> &[String] {
        &self.state.notifications
    }

    /// Immutable panel data produced by the last explicit frame tick.
    pub fn panel_view(&self) -> &crate::ui::view::PanelViewModel {
        &self.panel_view
    }

    /// Crate-local compatibility mutator for legacy unit fixtures.
    ///
    /// Production code must use capability-specific methods. This remains
    /// crate-local until existing in-crate fixtures migrate without widening
    /// the application API again.
    #[cfg(test)]
    pub(crate) fn state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    /// Advance application-owned frame state before drawing a frame.
    pub fn tick_frame(&mut self, metrics: FrameMetrics, now: Instant) {
        let elapsed = self
            .state
            .frame
            .spinner_last_tick
            .map(|previous| now.saturating_duration_since(previous))
            .unwrap_or_default();
        self.state.tick_frame(metrics, elapsed);
        if let Some(target) = metrics.artwork_target {
            self.state.artwork.submit_resize(target);
        }
        self.state.frame.spinner_last_tick = Some(now);
        self.panel_view = crate::ui::view::PanelViewModel::from_state(
            &self.state,
            metrics.lyrics_layout_width,
            &self.help_cache,
            self.config.ui.theme.as_str(),
            metrics.artwork_target,
        );
    }

    /// Panel currently focused.
    pub fn active_panel(&self) -> Panel {
        self.state.active_panel
    }

    /// Name of the theme currently applied and persisted in the config, used by
    /// the Appearance tab to mark the active theme in the list. An empty string
    /// means no theme has been applied yet.
    pub fn applied_theme(&self) -> &str {
        self.config.ui.theme.as_str()
    }

    /// Border glyph style currently applied to the UI.
    pub fn border_type(&self) -> crate::config::BorderType {
        self.border_type
    }

    /// Whether the main loop should exit.
    pub fn should_quit(&self) -> bool {
        self.state.should_quit
    }

    /// Popup currently owning the input focus, if any, borrowed from app state.
    ///
    /// Renderers should use this accessor so payloads such as settings drafts
    /// and search results are not cloned for every frame.
    pub fn active_popup_ref(&self) -> Option<&Popup> {
        self.state.popup_dialog.active_popup_ref()
    }

    /// Owned snapshot of the active popup for callers that need ownership.
    pub fn active_popup_owned(&self) -> Option<Popup> {
        self.active_popup_ref().cloned()
    }

    /// Compatibility accessor retained for existing consumers of the public API.
    pub fn active_popup(&self) -> Option<Popup> {
        self.active_popup_owned()
    }

    /// Context describing how the next key event must be routed.
    pub fn input_context(&self) -> InputContext {
        InputContext {
            browser_focused: self.state.active_panel == Panel::Browser,
            playlist_focused: self.state.active_panel == Panel::Playlist,
            lyrics_focused: self.state.active_panel == Panel::Lyrics,
        }
    }

    /// The current key configuration for display in the help popup.
    pub fn keys(&self) -> &KeysConfig {
        &self.keys
    }

    /// Open the default start directory, degrading to a notification when
    /// the environment offers nothing usable.
    ///
    /// Called once from the entrypoint before the event loop starts.
    pub fn request_browser_dir(&mut self, dir: PathBuf) -> Effect {
        self.request_browser_dir_with_restore(dir, None)
    }

    fn request_browser_dir_with_restore(
        &mut self,
        dir: PathBuf,
        restore_cursor_name: Option<String>,
    ) -> Effect {
        request_browser_directory(&mut self.state, dir, restore_cursor_name)
    }

    /// Apply one completed browser listing when it still belongs to the
    /// latest navigation request.
    pub fn apply_browser_directory_loaded(
        &mut self,
        request_id: u64,
        dir: PathBuf,
        entries: Vec<crate::filesystem::FileEntry>,
        restore_cursor_name: Option<String>,
    ) -> Vec<Effect> {
        if !self.state.browser.accepts_directory_request(request_id) {
            tracing::debug!(request_id, "ignoring stale browser directory result");
            return Vec::new();
        }
        self.state.change_browser_dir(dir, entries);
        if let Some(name) = restore_cursor_name {
            if let Some(index) = self
                .state
                .browser
                .entries
                .iter()
                .position(|entry| entry.name == name)
            {
                self.state.browser.set_cursor(index);
            } else {
                let current_dir = self.state.browser.current_dir.clone();
                self.state.browser.pop_focus(&current_dir);
            }
        }
        // Persist only after the accepted listing replaced the current view.
        // A settings request may still be enumerating here, so saving before
        // this point would record the previous directory.
        self.persist_runtime_state()
    }

    /// Apply a finished Settings browser-directory validation without doing
    /// filesystem work on the reducer thread.
    pub fn apply_browser_directory_validated(
        &mut self,
        request_id: u64,
        _path: PathBuf,
        validation: BrowserDirectoryValidation,
        result: WorkerResult<PathBuf>,
    ) -> Vec<Effect> {
        match validation {
            BrowserDirectoryValidation::Settings {
                input,
                previous_dir,
            } => {
                if !self
                    .state
                    .async_ops
                    .browser_validation_request
                    .accept(request_id)
                {
                    tracing::debug!(request_id, "ignoring stale browser directory validation");
                    return Vec::new();
                }

                let validated_path = match result {
                    Ok(path) => path,
                    Err(error) => {
                        if let Some(Popup::Settings { draft, .. }) =
                            self.state.popup_dialog.active_popup_mut()
                        {
                            draft.browser_directory = previous_dir.to_string_lossy().into_owned();
                        }
                        ModalRouter::set_alert(&mut self.state, error.to_string());
                        self.state.popup_dialog.set_dialog_input(input);
                        self.state.popup_dialog.set_dialog_error(None);
                        return Vec::new();
                    }
                };

                if self.state.popup_dialog.dialog_mode_ref() != Some(&DialogMode::SettingsEdit) {
                    tracing::debug!(
                        request_id,
                        "browser directory validation completed without its editor"
                    );
                    return Vec::new();
                }
                if let Some(Popup::Settings { draft, .. }) =
                    self.state.popup_dialog.active_popup_mut()
                {
                    draft.browser_directory = input;
                }
                let effect = self.request_browser_dir(validated_path);
                ModalRouter::close_dialog(&mut self.state);
                vec![effect]
            }
            BrowserDirectoryValidation::Symlink {
                parent_dir,
                folder_name,
            } => {
                if !self.state.browser.accepts_directory_request(request_id) {
                    tracing::debug!(request_id, "ignoring stale browser symlink validation");
                    return Vec::new();
                }
                match result {
                    Ok(resolved) => {
                        self.state.browser.push_focus(parent_dir, folder_name);
                        vec![self.request_browser_dir(resolved)]
                    }
                    Err(error) => {
                        self.push_notification(error.to_string());
                        Vec::new()
                    }
                }
            }
        }
    }

    /// Validate a named-save dialog against a worker-owned playlist listing.
    #[cfg(test)]
    pub(crate) fn request_named_save_validation(
        &mut self,
        action: PlaylistSaveAction,
        name: String,
    ) -> Vec<Effect> {
        request_named_save_validation(&mut self.state, action, name)
    }

    /// Apply a typed named-playlist load completion without touching the
    /// filesystem on the reducer thread.
    pub fn apply_playlist_loaded(
        &mut self,
        request_id: u64,
        name: String,
        result: WorkerResult<Playlist>,
    ) -> Vec<Effect> {
        if !self.state.async_ops.playlist_request.accept(request_id) {
            tracing::debug!(request_id, "ignoring stale playlist-load result");
            return Vec::new();
        }

        let playlist = match result {
            Ok(playlist) => playlist,
            Err(error) => {
                self.push_notification(format!("Could not load {name}: {error}"));
                return Vec::new();
            }
        };

        self.state.playback.status = PlayStatus::Stopped;
        self.state.playback.track_index = None;
        self.state.playback.elapsed = Duration::ZERO;
        self.state.playback.duration = None;
        self.state.persistence.last_track = None;
        self.state.persistence.last_track_position_ms = 0;
        self.state.async_ops.cancel_stream_activity();
        self.state.artwork.loading = false;
        self.state.playlist = playlist;
        self.state.active_playlist_name = Some(name.clone());
        self.state
            .navigation
            .rebuild_after_mutation(self.state.playlist.len(), None);
        self.state
            .reanchor_playlist_scroll(crate::browser_state::ScrollDirection::Down);
        ModalRouter::clear(&mut self.state);
        self.push_notification(format!("Loaded playlist {name}"));

        let mut effects = vec![Effect::Audio(AudioCommand::Stop)];
        effects.extend(self.playlist.load_metadata_for_queue(&self.state));
        effects
    }

    /// Apply a typed saved-playlist rename completion without touching the
    /// filesystem on the reducer thread.
    pub fn apply_playlist_renamed(
        &mut self,
        request_id: u64,
        old_name: String,
        new_name: String,
        action: PlaylistRenameAction,
        result: PlaylistRenameResult,
    ) -> Vec<Effect> {
        if !self.state.async_ops.playlist_request.accept(request_id) {
            tracing::debug!(request_id, "ignoring stale playlist-rename result");
            return Vec::new();
        }

        match result {
            PlaylistRenameResult::Conflict => {
                self.state
                    .popup_dialog
                    .set_dialog_error(Some(format!("Playlist {new_name} already exists")));
                return Vec::new();
            }
            PlaylistRenameResult::Failed(error) => {
                self.state
                    .popup_dialog
                    .set_dialog_error(Some(format!("Rename failed: {error}")));
                return Vec::new();
            }
            PlaylistRenameResult::Success => {}
        }

        if matches!(action, PlaylistRenameAction::Playing)
            || self.state.active_playlist_name.as_deref() == Some(old_name.as_str())
        {
            self.state.active_playlist_name = Some(new_name.clone());
        }
        self.push_notification(format!("Renamed to {new_name}"));
        ModalRouter::close_dialog(&mut self.state);

        let cursor = self
            .state
            .popup_dialog
            .active_popup_ref()
            .and_then(|popup| match popup {
                Popup::PlaylistManager { cursor, .. } => Some(*cursor),
                _ => None,
            });
        cursor
            .map(|cursor| {
                request_playlist_names(
                    &mut self.state,
                    PlaylistNamesRequest::RefreshManager { cursor },
                )
            })
            .unwrap_or_default()
    }

    /// Apply a typed delete result and its worker-owned manager refresh.
    pub fn apply_playlist_deleted(
        &mut self,
        request_id: u64,
        name: String,
        cursor: Option<usize>,
        was_active: bool,
        result: PlaylistDeleteResult,
    ) -> Vec<Effect> {
        if !self.state.async_ops.playlist_request.accept(request_id) {
            tracing::debug!(request_id, "ignoring stale playlist-delete result");
            return Vec::new();
        }

        let names = match result.names {
            Ok(names) => names,
            Err(error) => {
                self.push_notification(format!("Could not refresh playlists: {error}"));
                match self.state.popup_dialog.active_popup_ref() {
                    Some(Popup::PlaylistManager { names, .. }) => names.clone(),
                    _ => Vec::new(),
                }
            }
        };

        match result.deletion {
            Ok(()) => {
                if was_active {
                    if cursor.is_some() {
                        self.state.active_playlist_name = None;
                    } else {
                        self.state.clear_queue();
                        self.state.active_playlist_name =
                            Some(crate::state::default_playlist_name(&names));
                    }
                    self.push_notification("Playing playlist deleted".to_string());
                } else {
                    self.push_notification(format!("Deleted playlist {name}"));
                }
            }
            Err(error) => {
                self.push_notification(format!("Could not delete {name}: {error}"));
            }
        }

        let new_cursor = cursor.unwrap_or(0).min(names.len().saturating_sub(1));
        ModalRouter::open_popup(
            &mut self.state,
            Popup::PlaylistManager {
                cursor: new_cursor,
                names,
            },
        );
        Vec::new()
    }

    /// Apply a playlist-name listing without performing filesystem work on the
    /// reducer thread.
    pub fn apply_playlist_names_completed(
        &mut self,
        request_id: u64,
        request: PlaylistNamesRequest,
        result: WorkerResult<Vec<String>>,
    ) -> Vec<Effect> {
        if !self.state.async_ops.playlist_request.accept(request_id) {
            tracing::debug!(request_id, "ignoring stale playlist-name result");
            return Vec::new();
        }

        let names = match result {
            Ok(names) => names,
            Err(error) => {
                match request {
                    PlaylistNamesRequest::ConfirmSave { .. }
                    | PlaylistNamesRequest::BeginSaveAs
                    | PlaylistNamesRequest::BeginNew => {
                        self.state
                            .popup_dialog
                            .set_dialog_error(Some(format!("Could not list playlists: {error}")));
                    }
                    PlaylistNamesRequest::OpenManager
                    | PlaylistNamesRequest::RefreshManager { .. } => {
                        self.push_notification(format!("Could not list playlists: {error}"));
                    }
                }
                return Vec::new();
            }
        };

        match request {
            PlaylistNamesRequest::OpenManager => {
                if let Some(Popup::PlaylistManager { cursor, .. }) =
                    self.state.popup_dialog.active_popup_ref()
                {
                    let cursor = (*cursor).min(names.len().saturating_sub(1));
                    self.state
                        .popup_dialog
                        .replace_popup(Popup::PlaylistManager { cursor, names });
                }
            }
            PlaylistNamesRequest::BeginSaveAs => {
                if matches!(
                    self.state.popup_dialog.dialog_mode_ref(),
                    Some(DialogMode::SaveAs)
                ) && self.state.popup_dialog.dialog_input_ref() == Some("")
                {
                    let input = self
                        .state
                        .active_playlist_name
                        .clone()
                        .unwrap_or_else(|| crate::state::default_playlist_name(&names));
                    self.state.popup_dialog.set_dialog_input(input);
                }
            }
            PlaylistNamesRequest::BeginNew => {
                if matches!(
                    self.state.popup_dialog.dialog_mode_ref(),
                    Some(DialogMode::NewPlaylist)
                ) && self.state.popup_dialog.dialog_input_ref() == Some("")
                {
                    self.state
                        .popup_dialog
                        .set_dialog_input(crate::state::next_playlist_name(&names));
                }
            }
            PlaylistNamesRequest::ConfirmSave {
                action,
                name,
                current_name,
            } => {
                let expected_mode = match action {
                    PlaylistSaveAction::SaveAs => DialogMode::SaveAs,
                    PlaylistSaveAction::NewPlaylist => DialogMode::NewPlaylist,
                    PlaylistSaveAction::RenamePlaying => DialogMode::RenamePlaying,
                    PlaylistSaveAction::Overwrite { .. } => return Vec::new(),
                };
                if self.state.popup_dialog.dialog_mode_ref() != Some(&expected_mode)
                    || self
                        .state
                        .popup_dialog
                        .dialog_input_ref()
                        .unwrap_or_default()
                        .trim()
                        != name
                {
                    return Vec::new();
                }
                let conflicts = names.iter().any(|candidate| {
                    candidate == &name
                        && (matches!(action, PlaylistSaveAction::NewPlaylist)
                            || current_name.as_deref() != Some(candidate.as_str()))
                });
                if conflicts {
                    if matches!(action, PlaylistSaveAction::RenamePlaying) {
                        self.state
                            .popup_dialog
                            .set_dialog_error(Some(format!("Playlist {name} already exists")));
                    } else {
                        self.state
                            .popup_dialog
                            .push_popup(Popup::ConfirmOverwrite { name });
                    }
                } else {
                    if matches!(action, PlaylistSaveAction::RenamePlaying) {
                        if let Some(old_name) = current_name {
                            return start_playlist_rename(
                                &mut self.state,
                                PlaylistRenameAction::Playing,
                                old_name,
                                name,
                            );
                        }
                    }
                    return start_playlist_save(&mut self.state, action, name);
                }
            }
            PlaylistNamesRequest::RefreshManager { cursor } => {
                if self
                    .state
                    .popup_dialog
                    .active_popup_ref()
                    .is_some_and(|popup| matches!(popup, Popup::PlaylistManager { .. }))
                {
                    let cursor = cursor.min(names.len().saturating_sub(1));
                    self.state
                        .popup_dialog
                        .replace_popup(Popup::PlaylistManager { cursor, names });
                }
            }
        }
        Vec::new()
    }

    /// Apply a typed named-playlist write completion and preserve the dialog on
    /// failure so the user can correct or retry the same input.
    pub fn apply_playlist_saved(
        &mut self,
        request_id: u64,
        name: String,
        action: PlaylistSaveAction,
        result: WorkerResult<PathBuf>,
    ) -> Vec<Effect> {
        if !self.state.async_ops.playlist_request.accept(request_id) {
            tracing::debug!(request_id, "ignoring stale playlist-save result");
            return Vec::new();
        }

        if let Err(error) = result {
            self.state
                .popup_dialog
                .set_dialog_error(Some(format!("Save failed: {error}")));
            if matches!(action, PlaylistSaveAction::Overwrite { .. }) {
                ModalRouter::close_modal(&mut self.state);
            }
            return Vec::new();
        }

        let refresh_cursor = match action {
            PlaylistSaveAction::SaveAs | PlaylistSaveAction::NewPlaylist => self
                .state
                .popup_dialog
                .active_popup_ref()
                .and_then(|popup| match popup {
                    Popup::PlaylistManager { cursor, .. } => Some(*cursor),
                    _ => None,
                }),
            PlaylistSaveAction::Overwrite { .. } | PlaylistSaveAction::RenamePlaying => None,
        };

        let new_playlist = matches!(
            action,
            PlaylistSaveAction::NewPlaylist | PlaylistSaveAction::Overwrite { new_playlist: true }
        );
        self.state.active_playlist_name = Some(name.clone());
        let mut effects = Vec::new();
        if new_playlist {
            self.state.playlist = Playlist::new();
            self.state.playback.status = PlayStatus::Stopped;
            self.state.playback.elapsed = Duration::ZERO;
            self.state.active_panel = Panel::Browser;
            effects.push(Effect::Audio(AudioCommand::Stop));
        }
        let message = match action {
            PlaylistSaveAction::SaveAs => format!("Saved playlist {name}"),
            PlaylistSaveAction::NewPlaylist => format!("Created playlist {name}"),
            PlaylistSaveAction::RenamePlaying => format!("Renamed to {name}"),
            PlaylistSaveAction::Overwrite {
                new_playlist: false,
            } => {
                format!("Overwrote playlist {name}")
            }
            PlaylistSaveAction::Overwrite { new_playlist: true } => {
                format!("Created playlist {name}")
            }
        };
        self.push_notification(message);

        ModalRouter::close_dialog(&mut self.state);
        if matches!(action, PlaylistSaveAction::Overwrite { .. }) {
            ModalRouter::clear(&mut self.state);
        }

        if let Some(cursor) = refresh_cursor {
            effects.extend(request_playlist_names(
                &mut self.state,
                PlaylistNamesRequest::RefreshManager { cursor },
            ));
        }
        effects
    }

    pub fn open_start_dir(&mut self) -> Vec<Effect> {
        let start_dir = match resolve_start_dir() {
            Ok(start_dir) => start_dir,
            Err(error) => {
                self.push_notification(error.to_string());
                return Vec::new();
            }
        };
        self.state.browser.start_dir = start_dir.clone();
        tracing::info!(start_dir = %start_dir.display(), "browser opening at the start directory");
        vec![self.request_browser_dir(start_dir)]
    }

    /// Apply a single command to the state, returning requested effects.
    pub fn handle_command(&mut self, command: Command) -> Vec<Effect> {
        let mut effects = Vec::new();
        let active_panel = match self.state.active_panel {
            Panel::Browser => PanelTarget::Browser,
            Panel::Playlist => PanelTarget::Playlist,
            Panel::Lyrics => PanelTarget::Lyrics,
        };

        match command.into_grouped(CommandContext { active_panel }) {
            CommandGroup::Lifecycle(LifecycleCommand::Quit) => {
                if self.state.confirm_quit {
                    ModalRouter::open_popup(&mut self.state, Popup::ConfirmQuit);
                } else {
                    self.state.should_quit = true;
                }
            }
            CommandGroup::Lifecycle(LifecycleCommand::ConfirmQuitYes) => {
                ModalRouter::clear(&mut self.state);
                self.state.should_quit = true;
            }
            CommandGroup::Lifecycle(LifecycleCommand::CancelPopup) => {
                self.state.async_ops.playlist_request.cancel();
                self.state.async_ops.file_rename_request.cancel();
                if matches!(
                    self.state.popup_dialog.dialog_mode_ref(),
                    Some(DialogMode::AddStream { .. })
                ) {
                    self.state.cancel_stream_resolution();
                }
                if let Some(Popup::ConfirmDelete { .. }) =
                    self.state.popup_dialog.active_popup_ref()
                {
                    // Cancelling the delete warning restores the playlist
                    // manager popup with the cursor position captured when the
                    // confirmation opened. The deletion does not happen, and
                    // the names are refreshed by a worker.
                    let cursor = match self.state.popup_dialog.active_popup_ref() {
                        Some(Popup::ConfirmDelete { cursor, .. }) => *cursor,
                        _ => None,
                    };
                    ModalRouter::open_popup(
                        &mut self.state,
                        Popup::PlaylistManager {
                            cursor: cursor.unwrap_or(0),
                            names: Vec::new(),
                        },
                    );
                    effects.extend(request_playlist_names(
                        &mut self.state,
                        PlaylistNamesRequest::RefreshManager {
                            cursor: cursor.unwrap_or(0),
                        },
                    ));
                } else {
                    ModalRouter::close_modal(&mut self.state);
                }
                // Esc is also the lyrics panel close key: cancelling a popup
                // while the lyrics panel holds focus hides the panel instead.
                if self.state.active_panel == Panel::Lyrics && self.state.lyrics.visible {
                    self.hide_lyrics(&mut effects);
                }
            }
            CommandGroup::Ui(UiCommand::OpenSettings) => {
                self.state.async_ops.playlist_request.cancel();
                let themes_dir = self.config_dir.join("themes");
                let visit_id = self.state.async_ops.begin_settings_visit();
                let theme_request_id = self
                    .state
                    .async_ops
                    .begin_theme_request()
                    .expect("Settings visit must own its initial theme request")
                    .0;
                let mut draft = SettingsDraft::from_state(&self.state, &self.config, &themes_dir);
                self.output_enumeration_request_id =
                    self.output_enumeration_request_id.wrapping_add(1);
                let request_id = self.output_enumeration_request_id;
                let selected_id = self.config.sound.output_sink_id.clone();
                draft.outputs = self.output_list_for_selection(
                    self.output_provider
                        .cached_outputs()
                        .unwrap_or_else(|| vec![AudioOutput::session_default()]),
                    &selected_id,
                );
                draft.set_active_field(
                    SettingsTab::Sound,
                    SettingsField::SoundOutput(selected_output_index(&draft.outputs, &selected_id)),
                );
                ModalRouter::open_popup(
                    &mut self.state,
                    Popup::Settings {
                        tab: SettingsTab::General,
                        focus: SettingsFocus::Content,
                        draft,
                    },
                );
                effects.push(Effect::EnumerateOutputs {
                    request_id,
                    provider: OutputProviderHandle(Arc::clone(&self.output_provider)),
                });
                effects.push(Effect::LoadTheme {
                    request_id: theme_request_id,
                    visit_id,
                    themes_dir,
                    name: self.config.ui.theme.clone(),
                    include_names: true,
                    purpose: ThemeLoadPurpose::SettingsOpen,
                });
            }
            CommandGroup::Ui(UiCommand::ToggleHelp) => {
                ModalRouter::open_popup(&mut self.state, Popup::Help { scroll: 0 });
            }
            CommandGroup::Ui(UiCommand::ToggleArtwork) => {
                self.state.artwork.toggle_visible();
                let visible = self.state.artwork.is_visible();
                self.push_notification(format!(
                    "Artwork {}",
                    if visible { "visible" } else { "hidden" }
                ));
            }
            CommandGroup::Ui(UiCommand::ToggleLyrics) => {
                if self.state.lyrics.visible {
                    self.hide_lyrics(&mut effects);
                } else {
                    // The panel replaces the browser and takes focus; the
                    // browser state stays untouched underneath.
                    self.state.lyrics.visible = true;
                    self.state.active_panel = Panel::Lyrics;
                    self.load_lyrics_for_current(&mut effects);
                }
            }
            CommandGroup::Navigation(NavigationCommand::FocusNextPanel) => {
                self.state.active_panel =
                    next_panel(self.state.active_panel, self.state.lyrics.visible);
            }
            CommandGroup::Navigation(NavigationCommand::FocusPreviousPanel) => {
                self.state.active_panel =
                    previous_panel(self.state.active_panel, self.state.lyrics.visible);
            }
            // Plain cursor moves are context aware: they drive whichever
            // panel holds focus, one command vocabulary for both lists
            CommandGroup::Navigation(NavigationCommand::CursorUp { target }) => match target {
                PanelTarget::Browser => self.state.browser.move_up(),
                PanelTarget::Playlist => self.state.move_playlist_cursor_up(),
                PanelTarget::Lyrics => self.scroll_lyrics(false, LyricsScrollStep::Line),
            },
            CommandGroup::Navigation(NavigationCommand::CursorDown { target }) => match target {
                PanelTarget::Browser => self.state.browser.move_down(),
                PanelTarget::Playlist => self.state.move_playlist_cursor_down(),
                PanelTarget::Lyrics => self.scroll_lyrics(true, LyricsScrollStep::Line),
            },
            CommandGroup::Navigation(NavigationCommand::CursorTop { target }) => match target {
                PanelTarget::Browser => self.state.browser.goto_top(),
                PanelTarget::Playlist => self.state.move_playlist_to_top(),
                PanelTarget::Lyrics => self.scroll_lyrics(false, LyricsScrollStep::JumpTo(0)),
            },
            CommandGroup::Navigation(NavigationCommand::CursorBottom { target }) => match target {
                PanelTarget::Browser => self.state.browser.goto_bottom(),
                PanelTarget::Playlist => self.state.move_playlist_to_bottom(),
                PanelTarget::Lyrics => {
                    self.scroll_lyrics(true, LyricsScrollStep::JumpTo(usize::MAX))
                }
            },
            CommandGroup::Navigation(NavigationCommand::PageUp { target }) => match target {
                PanelTarget::Browser => self.state.browser.page_up(),
                PanelTarget::Playlist => self.state.page_playlist_up(),
                PanelTarget::Lyrics => self.scroll_lyrics(false, LyricsScrollStep::Page),
            },
            CommandGroup::Navigation(NavigationCommand::PageDown { target }) => match target {
                PanelTarget::Browser => self.state.browser.page_down(),
                PanelTarget::Playlist => self.state.page_playlist_down(),
                PanelTarget::Lyrics => self.scroll_lyrics(true, LyricsScrollStep::Page),
            },
            CommandGroup::Navigation(NavigationCommand::ParentDir { target: _ }) => {
                if let Some(parent) = self.state.browser.go_parent() {
                    // The folder we are leaving (e.g. "Music" when going up
                    // from /home/ger/Music): the cursor should land on it in
                    // the parent listing regardless of how we got here.
                    let leaving = self
                        .state
                        .browser
                        .current_dir
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned());
                    effects.push(self.request_browser_dir_with_restore(parent, leaving));
                }
            }
            CommandGroup::Navigation(NavigationCommand::EnterSelected { target: _ }) => {
                effects = self.activate_cursor_entry();
                self.append_autosave(&mut effects);
            }
            CommandGroup::Navigation(NavigationCommand::ToggleMark { target: _ }) => {
                let mut selected = std::mem::take(&mut self.state.browser.selected_entries);
                self.state.browser.toggle_mark(&mut selected);
                self.state.browser.selected_entries = selected;
            }
            CommandGroup::Navigation(NavigationCommand::AddSelected { target: _ }) => {
                effects = self.add_selected();
                self.append_autosave(&mut effects);
            }
            CommandGroup::File(FileCommand::RenameFile { target: _ }) => {
                // The rename shortcut handles both local files and stream
                // tracks; the resolver decides which variant of the dialog
                // to open based on the cursor's source kind.
                if let Some(target) = self.resolve_stream_action_target() {
                    self.open_stream_rename_dialog(target.url, target.kind);
                } else {
                    match self.resolve_file_action_target() {
                        Some(path) => {
                            let original_name = path
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            // The popup preloads only the base name; the
                            // extension is held in a locked suffix that is
                            // rendered (so the user can see it) but never
                            // edited. Splitting here keeps the editable
                            // buffer, the locked suffix and the rendered
                            // line in lock-step so the cursor never strays
                            // past the dot.
                            let (base, extension) = split_file_name(&original_name);
                            ModalRouter::open_dialog(
                                &mut self.state,
                                DialogMode::RenameFile {
                                    path,
                                    original_name: original_name.clone(),
                                    error: None,
                                },
                                base,
                                extension,
                            );
                        }
                        None => self.push_notification(
                            "No audio file or stream selected to rename".to_string(),
                        ),
                    }
                }
            }
            CommandGroup::File(FileCommand::EditMetadata { target: _ }) => {
                // Streams only expose the title field, so editing a stream
                // is the same dialog as renaming it. Local files keep the
                // lofty-backed ten-field editor.
                if let Some(target) = self.resolve_stream_action_target() {
                    self.open_stream_rename_dialog(target.url, target.kind);
                } else {
                    match self.resolve_file_action_target() {
                        Some(path) => {
                            // The form opens in a loading state; the prefill
                            // effect fills the ten fields without blocking the
                            // UI
                            let prefill_path = path.clone();
                            ModalRouter::open_dialog(
                                &mut self.state,
                                DialogMode::EditMetadata {
                                    path,
                                    fields: Default::default(),
                                    cursor: 0,
                                    error: None,
                                    loading: true,
                                },
                                String::new(),
                                None,
                            );
                            effects.push(Effect::EditMetadataPrefill { path: prefill_path });
                        }
                        None => self.push_notification(
                            "No audio file or stream selected to edit".to_string(),
                        ),
                    }
                }
            }
            CommandGroup::File(FileCommand::AddStream) => {
                // Open the popup with a clean input buffer and a "type your
                // URL" focus. The popup is owned by the input handler while
                // the user types, and the resolver runs off the UI thread so
                // a slow Radio Browser lookup never freezes Ratatui.
                self.state.cancel_stream_resolution();
                ModalRouter::open_dialog(
                    &mut self.state,
                    DialogMode::AddStream {
                        error: None,
                        loading: false,
                    },
                    String::new(),
                    None,
                );
            }
            CommandGroup::Navigation(NavigationCommand::OpenSearch { target }) => {
                let scope = match target {
                    PanelTarget::Browser => SearchScope::Browser,
                    PanelTarget::Playlist => SearchScope::Playlist,
                    PanelTarget::Lyrics => return effects,
                };
                ModalRouter::open_search(&mut self.state, scope);
            }

            // Playback commands update state optimistically so rapid key
            // repeats compose correctly before any worker snapshot returns
            CommandGroup::Playback(PlaybackCommand::TogglePause) => {
                match self.state.playback.status {
                    PlayStatus::Playing => {
                        self.state.playback.status = PlayStatus::Paused;
                        effects.push(Effect::Audio(AudioCommand::Pause));
                    }
                    PlayStatus::Paused => {
                        self.state.playback.status = PlayStatus::Playing;
                        effects.push(Effect::Audio(AudioCommand::Resume));
                    }
                    PlayStatus::Stopped => {}
                }
            }
            CommandGroup::Playback(PlaybackCommand::NextTrack) => self.next_track(&mut effects),
            CommandGroup::Playback(PlaybackCommand::PreviousTrack) => {
                self.previous_track(&mut effects)
            }
            CommandGroup::Playback(PlaybackCommand::VolumeUp) => {
                let volume = stepped_volume(
                    self.state.playback.volume_percent,
                    i32::from(VOLUME_STEP_PERCENT),
                );
                self.state.playback.volume_percent = volume;
                effects.push(Effect::Audio(AudioCommand::SetVolume(volume)));
            }
            CommandGroup::Playback(PlaybackCommand::VolumeDown) => {
                let volume = stepped_volume(
                    self.state.playback.volume_percent,
                    -i32::from(VOLUME_STEP_PERCENT),
                );
                self.state.playback.volume_percent = volume;
                effects.push(Effect::Audio(AudioCommand::SetVolume(volume)));
            }
            CommandGroup::Playback(PlaybackCommand::SpeedUp) => {
                let speed = stepped_speed(self.state.playback.speed, SPEED_STEP);
                self.state.playback.speed = speed;
                effects.push(Effect::Audio(AudioCommand::SetSpeed(speed)));
            }
            CommandGroup::Playback(PlaybackCommand::SpeedDown) => {
                let speed = stepped_speed(self.state.playback.speed, -SPEED_STEP);
                self.state.playback.speed = speed;
                effects.push(Effect::Audio(AudioCommand::SetSpeed(speed)));
            }
            CommandGroup::Playback(PlaybackCommand::SpeedReset) => {
                self.state.playback.speed = SPEED_DEFAULT;
                effects.push(Effect::Audio(AudioCommand::SetSpeed(SPEED_DEFAULT)));
            }
            CommandGroup::Playback(PlaybackCommand::SeekForward) => self.seek(false, &mut effects),
            CommandGroup::Playback(PlaybackCommand::SeekBackward) => self.seek(true, &mut effects),
            CommandGroup::Playback(PlaybackCommand::PlaySelected { target: _ }) => {
                self.play_selected(&mut effects)
            }
            CommandGroup::Playback(PlaybackCommand::CycleRepeat) => {
                let repeat = self.state.playback_mode.cycle_repeat();
                tracing::info!(repeat = repeat.label(), "playback order changed");
                self.push_notification(format!("Repeat mode: {}", repeat.label()));
            }
            CommandGroup::Playback(PlaybackCommand::ToggleShuffle) => {
                let enabled = self.state.playback_mode.toggle_shuffle();
                if enabled {
                    // A fresh pass reserves the playing track so shuffle
                    // never hands it back as the immediate next
                    let len = self.state.playlist.len();
                    let playing = self.state.playback.track_index;
                    self.state.navigation.reshuffle(len, playing);
                }
                tracing::info!(shuffle = enabled, "playback order changed");
                self.push_notification(format!("Shuffle {}", if enabled { "on" } else { "off" }));
            }

            // Queue management only ever edits playlist rows, the backing
            // files stay untouched by construction
            CommandGroup::Queue(QueueCommand::SwapSelectedUp { target: _ }) => {
                let result = self.state.swap_queue_entry_up();
                if result.requires_stop_playback() {
                    effects.push(Effect::Audio(AudioCommand::Stop));
                }
                self.append_autosave(&mut effects);
            }
            CommandGroup::Queue(QueueCommand::SwapSelectedDown { target: _ }) => {
                let result = self.state.swap_queue_entry_down();
                if result.requires_stop_playback() {
                    effects.push(Effect::Audio(AudioCommand::Stop));
                }
                self.append_autosave(&mut effects);
            }
            CommandGroup::Queue(QueueCommand::DeleteQueueEntry { target: _ }) => {
                let result = self.state.delete_queue_entry_at_cursor();
                if matches!(result, crate::state::QueueMutationResult::Unchanged) {
                    self.push_notification("Queue is empty".to_string());
                }
                if result.requires_stop_playback() {
                    effects.push(Effect::Audio(AudioCommand::Stop));
                }
                self.append_autosave(&mut effects);
            }
            CommandGroup::Queue(QueueCommand::ClearQueue { target: _ }) => {
                if self.state.playlist.is_empty() {
                    self.push_notification("Queue is empty".to_string());
                } else {
                    let removed = self.state.playlist.len();
                    self.state.clear_queue();
                    tracing::info!("queue cleared with {removed} entries");
                    self.push_notification(format!("Cleared {removed} queued entries"));
                }
                self.append_autosave(&mut effects);
            }

            // Named-playlist management.
            CommandGroup::Playlist(PlaylistCommand::OpenPlaylistManager) => {
                self.playlist.open_manager(&mut self.state);
                effects.extend(request_playlist_names(
                    &mut self.state,
                    PlaylistNamesRequest::OpenManager,
                ));
            }
            CommandGroup::Playlist(PlaylistCommand::LoadPlaylist) => {
                effects.extend(self.playlist.load_selected(&mut self.state));
            }
            CommandGroup::Playlist(PlaylistCommand::DeletePlaylist) => {
                self.playlist.begin_delete(&mut self.state);
            }
            CommandGroup::Playlist(PlaylistCommand::RenamePlaylist) => {
                self.playlist.begin_rename(&mut self.state);
            }
            CommandGroup::Playlist(PlaylistCommand::SaveAsPlaylist) => {
                self.playlist.begin_save_as(&mut self.state);
                effects.extend(request_playlist_names(
                    &mut self.state,
                    PlaylistNamesRequest::BeginSaveAs,
                ));
            }
            CommandGroup::Playlist(PlaylistCommand::NewPlaylist) => {
                self.playlist.begin_new(&mut self.state);
                effects.extend(request_playlist_names(
                    &mut self.state,
                    PlaylistNamesRequest::BeginNew,
                ));
            }
            CommandGroup::Playlist(PlaylistCommand::ConfirmDialog) => {
                effects = match ModalRouter::confirm_dialog(&self.state) {
                    ConfirmDialogTarget::Overwrite => self.confirm_overwrite(),
                    ConfirmDialogTarget::Delete => self.confirm_delete(),
                    ConfirmDialogTarget::Dialog => self.handle_dialog_confirm(),
                    ConfirmDialogTarget::None => Vec::new(),
                };
            }
            CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerUp) => {
                self.playlist.move_manager_up(&mut self.state);
            }
            CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerDown) => {
                self.playlist.move_manager_down(&mut self.state);
            }
            CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerTop) => {
                self.playlist.move_manager_top(&mut self.state);
            }
            CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerBottom) => {
                self.playlist.move_manager_bottom(&mut self.state);
            }
            CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerPageUp) => {
                self.playlist.page_manager_up(&mut self.state);
            }
            CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerPageDown) => {
                self.playlist.page_manager_down(&mut self.state);
            }
            CommandGroup::Playlist(PlaylistCommand::ClosePopup) => {
                self.state.async_ops.playlist_request.cancel();
                ModalRouter::close_modal(&mut self.state);
            }

            // Column moves only mean something inside the settings popup,
            // which routes them in `handle_settings_key` before this match
            CommandGroup::Ui(UiCommand::SettingsNextColumn | UiCommand::SettingsPreviousColumn) => {
            }
        }

        effects
    }

    fn output_list_for_selection(
        &self,
        outputs: Vec<AudioOutput>,
        selected_id: &str,
    ) -> Vec<AudioOutput> {
        output_list_for_selection(outputs, selected_id)
    }

    pub fn apply_outputs_enumerated(&mut self, request_id: u64, outputs: Vec<AudioOutput>) -> bool {
        if request_id != self.output_enumeration_request_id {
            return false;
        }
        let Some(Popup::Settings { draft, .. }) = self.state.popup_dialog.active_popup_mut() else {
            return false;
        };
        let selected_id = draft
            .selected_output()
            .and_then(|index| draft.outputs.get(index))
            .map(|output| output.id.clone())
            .unwrap_or_else(|| self.config.sound.output_sink_id.clone());
        draft.outputs = output_list_for_selection(outputs, &selected_id);
        draft.set_active_field(
            SettingsTab::Sound,
            SettingsField::SoundOutput(selected_output_index(&draft.outputs, &selected_id)),
        );
        true
    }

    /// Apply a completed blocking theme load if it still belongs to Settings.
    ///
    /// The returned flag tells the event loop whether the loaded palette is the
    /// active runtime theme. Preview and Settings-open loads only update the
    /// editable draft.
    pub fn apply_theme_loaded(
        &mut self,
        request_id: u64,
        visit_id: u64,
        _themes_dir: PathBuf,
        name: String,
        theme_names: Option<Vec<String>>,
        result: WorkerResult<crate::ui::theme::ThemeColors>,
        purpose: ThemeLoadPurpose,
    ) -> bool {
        if !self
            .state
            .async_ops
            .accept_theme_request(request_id, visit_id)
        {
            tracing::debug!(request_id, visit_id, "ignoring stale theme load result");
            return false;
        }

        let colors = match result {
            Ok(colors) => colors,
            Err(error) => {
                self.push_notification(format!("Could not load theme: {error}"));
                return false;
            }
        };

        if matches!(purpose, ThemeLoadPurpose::Apply) {
            return true;
        }

        let Some(Popup::Settings { draft, .. }) = self.state.popup_dialog.active_popup_mut() else {
            return false;
        };
        if matches!(purpose, ThemeLoadPurpose::SettingsOpen) {
            if let Some(names) = theme_names {
                draft.theme_names = names;
            }
            if let Some(position) = draft
                .theme_names
                .iter()
                .position(|candidate| candidate == &self.config.ui.theme)
            {
                draft.set_active_field(
                    SettingsTab::Appearance,
                    SettingsField::AppearanceTheme(position),
                );
            }
        } else if draft
            .theme_names
            .get(draft.selected_theme().unwrap_or(0))
            .is_none_or(|selected| selected != &name)
        {
            tracing::debug!(
                request_id,
                visit_id,
                "theme preview no longer matches selection"
            );
            return false;
        }
        draft.loaded_theme_palette = Some((name, colors.clone()));
        draft.colors = colors;
        false
    }

    /// Apply a completed blocking theme save and preserve the editor on error.
    pub fn apply_theme_saved(
        &mut self,
        request_id: u64,
        visit_id: u64,
        _themes_dir: PathBuf,
        name: String,
        colors: crate::ui::theme::ThemeColors,
        result: WorkerResult<()>,
    ) -> (Vec<Effect>, Option<crate::ui::theme::ThemeColors>) {
        if !self
            .state
            .async_ops
            .accept_theme_save_request(request_id, visit_id)
        {
            tracing::debug!(request_id, visit_id, "ignoring stale theme save result");
            return (Vec::new(), None);
        }

        let Err(error) = result else {
            if let Some(Popup::Settings { draft, .. }) = self.state.popup_dialog.active_popup_mut()
            {
                if !draft.theme_names.contains(&name) {
                    draft.theme_names.push(name.clone());
                    draft.theme_names.sort();
                }
                if let Some(position) = draft
                    .theme_names
                    .iter()
                    .position(|candidate| candidate == &name)
                {
                    draft.set_active_field(
                        SettingsTab::Appearance,
                        SettingsField::AppearanceTheme(position),
                    );
                }
                draft.loaded_theme_palette = Some((name.clone(), colors.clone()));
                draft.colors = colors.clone();
            }
            self.config.ui.theme = name;
            ModalRouter::close_dialog(&mut self.state);
            return (vec![self.save_config_effect()], Some(colors));
        };

        self.state
            .popup_dialog
            .set_dialog_error(Some(format!("Could not save theme: {error}")));
        (Vec::new(), None)
    }

    /// Queue a background task notification for display, honoring the cap.
    pub fn push_notification(&mut self, message: String) {
        push_notification(&mut self.state.notifications, message, NOTIFICATIONS_CAP);
    }

    /// Emit an autosave effect for the active named playlist, if any.
    fn autosave_active_playlist(&self) -> Vec<Effect> {
        self.playlist.autosave_active(&self.state)
    }

    /// Append an autosave effect when the active playlist is named.
    fn append_autosave(&self, effects: &mut Vec<Effect>) {
        effects.extend(self.autosave_active_playlist());
    }

    /// Route one command to the open help popup.
    ///
    /// Only the scroll and close vocabulary is meaningful here. The scroll
    /// offset clamps against the rendered content length so both jump keys
    /// land exactly on the ends no matter how tall the popup turns out to
    /// be. Anything else is swallowed on purpose, a modal must not leak
    /// commands into the panels underneath.
    fn handle_help_key(&mut self, command: Command) {
        let Some(Popup::Help { scroll }) = self.state.popup_dialog.active_popup_ref() else {
            return;
        };
        let scroll = *scroll;
        // The last row index is the highest offset that still shows
        // content, so scrolling can never run past the end of the table
        let max_scroll = help_line_count(&self.keys).saturating_sub(1) as u16;

        let next = match command {
            Command::CursorUp => scroll.saturating_sub(HELP_LINE_STEP),
            Command::CursorDown => scroll.saturating_add(HELP_LINE_STEP).min(max_scroll),
            Command::PageUp => scroll.saturating_sub(HELP_PAGE_STEP),
            Command::PageDown => scroll.saturating_add(HELP_PAGE_STEP).min(max_scroll),
            Command::CursorTop => 0,
            Command::CursorBottom => max_scroll,
            Command::CancelPopup | Command::ToggleHelp => {
                ModalRouter::close_modal(&mut self.state);
                return;
            }
            _ => return,
        };

        ModalRouter::replace_popup(&mut self.state, Popup::Help { scroll: next });
    }

    /// Confirm or cancel a sort while preserving the settings working copy.
    fn handle_sort_confirmation(&mut self, command: Command) -> Vec<Effect> {
        let draft = match self.state.popup_dialog.active_popup_ref() {
            Some(Popup::ConfirmSortTracks { draft }) => draft.clone(),
            _ => return Vec::new(),
        };

        match command {
            Command::ConfirmDialog => {
                let effects = self.apply_sort_tracks(&draft.playlist_columns);
                ModalRouter::open_popup(
                    &mut self.state,
                    Popup::Settings {
                        tab: SettingsTab::General,
                        focus: SettingsFocus::Content,
                        draft,
                    },
                );
                effects
            }
            Command::CancelPopup => {
                ModalRouter::open_popup(
                    &mut self.state,
                    Popup::Settings {
                        tab: SettingsTab::General,
                        focus: SettingsFocus::Content,
                        draft,
                    },
                );
                Vec::new()
            }
            _ => {
                ModalRouter::replace_popup(&mut self.state, Popup::ConfirmSortTracks { draft });
                Vec::new()
            }
        }
    }

    /// Start a contextual search from the query popup without blocking the UI.
    fn start_search(&mut self) -> Vec<Effect> {
        self.search.start(&mut self.state)
    }

    /// Close any search phase and discard its query buffer.
    fn close_search(&mut self) {
        self.search.close(&mut self.state);
    }

    /// Apply a worker completion only when it belongs to the visible request.
    pub fn apply_search_completed(
        &mut self,
        request_id: u64,
        scope: SearchScope,
        results: Vec<SearchResult>,
        message: Option<WorkerError>,
    ) {
        self.search
            .apply_completed(&mut self.state, request_id, scope, results, message);
    }

    /// Move the highlighted result while keeping it inside the result list.
    fn move_search_cursor(&mut self, down: bool) {
        self.search.move_cursor(&mut self.state, down);
    }

    /// Reveal the selected result on the main thread using its stable identity.
    fn reveal_search_result(&mut self) -> Vec<Effect> {
        self.search.reveal(&mut self.state)
    }

    fn metadata_effect_for_indices(&self, indices: &[usize]) -> Option<Effect> {
        let paths = indices
            .iter()
            .filter_map(|&index| {
                self.state
                    .playlist
                    .tracks()
                    .get(index)
                    .and_then(|track| track.path())
                    .map(Path::to_path_buf)
            })
            .collect::<Vec<_>>();
        (!paths.is_empty()).then_some(Effect::LoadMetadata(paths))
    }

    /// Commit one finished directory scan into the playlist atomically.
    ///
    /// Concurrent scans are fine: each completion lands as one indivisible
    /// append, only their relative order may interleave, which is harmless
    /// for a queue. The returned effects request metadata for the freshly
    /// queued paths.
    pub fn apply_scan_completed(
        &mut self,
        requested_dir: PathBuf,
        tracks: Vec<PathBuf>,
    ) -> Vec<Effect> {
        let append = self.state.extend_playlist(tracks);
        let added = append.added();

        if added == 0 {
            self.push_notification("No supported audio files found".to_string());
            return Vec::new();
        }

        tracing::info!("added {added} tracks from {}", requested_dir.display());
        self.push_notification(format!(
            "Added {added} tracks from {}",
            requested_dir.display()
        ));
        // Only the newly queued paths need metadata; duplicates were skipped.
        let mut effects = self
            .metadata_effect_for_indices(&append.added_indices)
            .into_iter()
            .collect();
        self.append_autosave(&mut effects);
        effects
    }

    /// Attach finished extraction snapshots to the matching queue entries.
    ///
    /// Individual failures were already logged by the worker, so only the
    /// aggregate reaches the user because one broken file must not drown
    /// the status area.
    pub fn apply_metadata_completed(
        &mut self,
        loaded: Vec<(PathBuf, TrackMetadata)>,
        failed: usize,
    ) -> Vec<Effect> {
        // Apply the whole batch in one pass: indexing by path once costs
        // O(n + k) instead of O(n·k) for `k` metadata results over an `n`
        // entry queue.
        let updated = self.state.playlist.apply_metadata_batch(&loaded);

        // When the currently playing track receives its metadata after
        // playback already started, the duration arrives late. Sync it
        // into the gauge so the bar fills correctly instead of staying
        // stuck at --:--
        if let Some(index) = self.state.playback.track_index
            && let Some(track) = self.state.playlist.tracks().get(index)
            && self.state.playback.duration.is_none()
            && let Some(duration) = track.metadata().map(|m| m.duration)
        {
            self.state.playback.duration = Some(duration);
        }

        // Metadata arrival updates labels and playback duration only. Startup
        // must preserve the playlist document's stored order; reordering is an
        // explicit user action from Settings.
        let effects = Vec::new();

        tracing::debug!(updated, failed, "metadata pass committed");
        if failed > 0 {
            self.push_notification(format!(
                "Metadata unavailable for {failed} track{}",
                if failed == 1 { "" } else { "s" }
            ));
        }
        effects
    }

    /// Merge a resolved stream URL into the active playlist.
    ///
    /// On success the new track is appended to the queue, the playlist
    /// cursor jumps onto it, and the popup closes. On failure the popup
    /// stays open with the resolver message rendered inside the dialog so
    /// the user can edit the URL and retry.
    pub fn apply_stream_resolved(
        &mut self,
        request_id: u64,
        url: url::Url,
        track: Option<Box<crate::track::Track>>,
        message: Option<WorkerError>,
    ) -> Vec<Effect> {
        if !self.state.accept_stream_resolution(request_id) {
            tracing::debug!(request_id, "ignoring stale stream resolution");
            return Vec::new();
        }
        match track {
            Some(track) => {
                let append = self.state.extend_playlist_tracks([*track]);
                let added = append.added();
                let skipped = append.skipped;
                let mut effects = Vec::new();
                if added == 0 {
                    self.push_notification(format!(
                        "Stream already queued: {}",
                        crate::net::safe_url(&url)
                    ));
                } else {
                    self.state.active_panel = Panel::Playlist;
                    self.select_playlist_track(self.state.playlist.len() - 1);
                    self.push_notification(format!(
                        "Added stream: {}",
                        self.state
                            .playlist
                            .current()
                            .map(|t| t.display_name().into_owned())
                            .unwrap_or_else(|| crate::net::safe_url(&url))
                    ));
                    // A new track mutated the queue; any active named
                    // playlist must be rewritten so the persisted `.m3u8`
                    // reflects what the user sees on screen. The shared
                    // helper produces the same `Effect::SaveActivePlaylist`
                    // we emit from `Command::AddSelected` and friends.
                    self.append_autosave(&mut effects);
                }
                let _ = skipped;
                self.close_dialog();
                effects
            }
            None => {
                // Surface the resolver failure inside the popup rather than
                // closing it, so the user can fix the URL and retry.
                let detail = message
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "unknown error".to_string());
                if let Some(DialogMode::AddStream { error, loading }) =
                    self.state.popup_dialog.dialog_mode_mut()
                {
                    *error = Some(detail.clone());
                    *loading = false;
                }
                self.push_notification(format!("Could not resolve stream: {detail}"));
                Vec::new()
            }
        }
    }

    /// Merge prefilled tag values into the open metadata editor.
    ///
    /// The values are dropped when no editor is open or when it moved to a
    /// different file, mirroring the artwork/lyrics staleness gates: a late
    /// delivery must never paint over a newer target.
    pub fn apply_metadata_prefill_ready(&mut self, path: PathBuf, fields: [String; 10]) {
        let first = fields[0].clone();
        let open = match self.state.popup_dialog.dialog_mode_mut() {
            Some(DialogMode::EditMetadata {
                path: dialog_path,
                fields: draft,
                loading,
                ..
            }) if *dialog_path == path => {
                **draft = fields;
                *loading = false;
                true
            }
            _ => false,
        };
        if open {
            // The active field buffer starts on the first field; place the
            // cursor at the end so typing appends without an extra End key.
            self.state.popup_dialog.set_dialog_input(first);
        }
    }

    /// Effect that (re)loads the tags of every local track in the queue.
    ///
    /// A playlist restored from disk stores resource locations, so the queue rows
    /// and the Now Playing band would otherwise fall back to file names
    /// until the user re-adds tracks, making metadata-driven display and
    /// sorting silently ignore the configured strategy. Called once on
    /// startup after a restored playlist replaces the queue.
    ///
    /// Restored local tracks can carry a provisional EXTINF snapshot, so they
    /// are still reloaded from disk. Stream tracks already carry metadata from
    /// the resolver and are skipped here.
    pub fn load_metadata_for_queue(&self) -> Vec<Effect> {
        self.playlist.load_metadata_for_queue(&self.state)
    }

    /// Reset the lyrics panel to a fresh load for the currently playing
    /// track (if any) and emit the matching effect.
    ///
    /// Stream tracks have no local file to look lyrics up for, so the
    /// panel falls back to its unavailable state for them.
    fn load_lyrics_for_current(&mut self, effects: &mut Vec<Effect>) {
        let current = self.state.playback.track_index.and_then(|index| {
            self.state
                .playlist
                .tracks()
                .get(index)
                .map(|track| (index, track))
        });

        match current {
            Some((index, track)) if track.is_local() => {
                let path = track.path().expect("local track has a path").to_path_buf();
                let metadata = track.metadata().cloned();
                let (request, display_title) = Self::lyrics_request_for(&path, metadata.as_ref());
                self.prepare_lyrics_request(index, request, display_title, effects);
            }
            _ => self.set_lyrics_unavailable(),
        }
    }

    /// Build the lyrics request and the panel title for a track.
    ///
    /// Free of `self` so it can run while a track snapshot is borrowed from
    /// the playlist and before the mutable state update begins. Metadata
    /// placeholders such as `Unknown Artist` carry no identifying value and
    /// are dropped here so the remote lookup never searches or validates
    /// against them.
    fn lyrics_request_for(
        path: &Path,
        metadata: Option<&TrackMetadata>,
    ) -> (LyricsRequest, String) {
        let title = metadata
            .map(|meta| meta.title.clone())
            .filter(|value| Self::is_known_metadata(value));
        let request_title = title.or_else(|| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        });
        let artist = metadata
            .map(|meta| meta.artist.clone())
            .filter(|value| Self::is_known_metadata(value));
        let album = metadata
            .map(|meta| meta.album.clone())
            .filter(|value| Self::is_known_metadata(value));
        let display_title = request_title
            .clone()
            .unwrap_or_else(|| path.to_string_lossy().into_owned());

        (
            LyricsRequest {
                audio_path: path.to_path_buf(),
                title: request_title,
                artist,
                album,
            },
            display_title,
        )
    }

    /// True when the metadata carries a real value instead of a display
    /// placeholder such as `Unknown Artist` or `Unknown Album`.
    fn is_known_metadata(value: &str) -> bool {
        let trimmed = value.trim();
        !trimmed.is_empty()
            && !trimmed.eq_ignore_ascii_case(crate::metadata::UNKNOWN_ARTIST)
            && !trimmed.eq_ignore_ascii_case(crate::metadata::UNKNOWN_ALBUM)
            && !trimmed.eq_ignore_ascii_case(crate::metadata::UNKNOWN_TITLE)
    }

    /// Set lyrics state for a fresh resolution and push its effect.
    fn prepare_lyrics_request(
        &mut self,
        index: usize,
        request: LyricsRequest,
        display_title: String,
        effects: &mut Vec<Effect>,
    ) {
        let lyrics = &mut self.state.lyrics;
        lyrics.loading = true;
        lyrics.track_index = Some(index);
        lyrics.set_document(None);
        lyrics.origin = None;
        lyrics.error = None;
        lyrics.scroll = 0;
        lyrics.active_line = None;
        lyrics.display_title = Some(display_title);

        effects.push(Effect::LoadLyrics {
            track_index: index,
            request,
        });
    }

    /// Set the panel to the "no lyrics" state with a short reason.
    fn set_lyrics_unavailable(&mut self) {
        let lyrics = &mut self.state.lyrics;
        lyrics.loading = false;
        lyrics.track_index = None;
        lyrics.set_document(None);
        lyrics.origin = None;
        lyrics.scroll = 0;
        lyrics.active_line = None;
        lyrics.error = Some("No track is playing".to_string());
        lyrics.display_title = None;
    }

    /// Hide the lyrics panel and hand focus back to the browser.
    ///
    /// The browser underneath was never touched, so hiding just removes the
    /// panel and restores focus. Artwork was never suppressed while the panel
    /// was visible (its overlay targets the playlist), so no reload is needed
    /// here: the overlay targets are recomputed every render.
    fn hide_lyrics(&mut self, _effects: &mut Vec<Effect>) {
        if !self.state.lyrics.visible {
            return;
        }
        self.state.lyrics.visible = false;
        if self.state.active_panel == Panel::Lyrics {
            self.state.active_panel = Panel::Browser;
        }
    }

    /// Move the lyrics scroll offset inside its physical layout, clamped.
    ///
    /// The clamp lives here, next to the measured viewport, so the renderer
    /// never has to fix up an out-of-range offset: scroll commands are safe
    /// no-ops while loading or when the document is shorter than the panel.
    fn scroll_lyrics(&mut self, down: bool, step: LyricsScrollStep) {
        let viewport = usize::from(self.state.lyrics.viewport_height.max(1));
        let total_rows = self.state.lyrics.cached_total_rows().unwrap_or(0);
        let max_scroll = total_rows.saturating_sub(viewport);

        let current = self.state.lyrics.scroll;
        let next = match step {
            LyricsScrollStep::Line => {
                if down {
                    current.saturating_add(1).min(max_scroll)
                } else {
                    current.saturating_sub(1)
                }
            }
            LyricsScrollStep::Page => {
                let step = viewport.saturating_sub(2).max(1);
                if down {
                    current.saturating_add(step).min(max_scroll)
                } else {
                    current.saturating_sub(step)
                }
            }
            LyricsScrollStep::JumpTo(offset) => offset.min(max_scroll),
        };
        self.state.lyrics.scroll = next;
    }

    /// Route a key event through the mapper and apply the resulting command.
    ///
    /// Returns the effects the caller must execute via [`execute_effects`].
    ///
    /// While the naming dialog is open every key is intercepted for text
    /// entry, so the underlying mapping (including popup navigation) never
    /// sees those events. The popup stays on screen behind the dialog.
    pub fn handle_key_event(&mut self, key_event: KeyEvent) -> Vec<Effect> {
        match ModalRouter::route(&self.state, key_event) {
            ModalAction::DismissAlert => {
                ModalRouter::dismiss_alert(&mut self.state);
                Vec::new()
            }
            ModalAction::Dialog(DialogAction::Key) => self.handle_dialog_key_event(key_event),
            ModalAction::Search(SearchAction::Key) => self.handle_search_key_event(key_event),
            ModalAction::Help(command) => {
                self.handle_help_key(command);
                Vec::new()
            }
            ModalAction::Settings(command) => self.handle_settings_key(command),
            ModalAction::ConfirmSortTracks(command) => self.handle_sort_confirmation(command),
            ModalAction::Command(command) => self.handle_command(command),
            ModalAction::Consume => Vec::new(),
            ModalAction::Normal => match self.input_mapper.map_key(key_event, self.input_context())
            {
                Some(command) => self.handle_command(command),
                None => Vec::new(),
            },
        }
    }

    /// Capture query editing and result navigation without involving the
    /// generic naming-dialog state.
    fn handle_search_key_event(&mut self, key_event: KeyEvent) -> Vec<Effect> {
        if key_event.kind != KeyEventKind::Press {
            return Vec::new();
        }
        match self.state.popup_dialog.active_popup_ref().cloned() {
            Some(Popup::SearchQuery { .. }) => match key_event.code {
                KeyCode::Enter => self.start_search(),
                KeyCode::Esc => {
                    self.close_search();
                    Vec::new()
                }
                KeyCode::Left => {
                    let cursor = self.state.popup_dialog.search_cursor().unwrap_or(0);
                    self.state
                        .popup_dialog
                        .set_search_cursor(cursor.saturating_sub(1));
                    Vec::new()
                }
                KeyCode::Right => {
                    let len = self
                        .state
                        .popup_dialog
                        .search_query()
                        .unwrap_or_default()
                        .chars()
                        .count();
                    let cursor = self.state.popup_dialog.search_cursor().unwrap_or(0);
                    self.state
                        .popup_dialog
                        .set_search_cursor((cursor + 1).min(len));
                    Vec::new()
                }
                KeyCode::Home => {
                    self.state.popup_dialog.set_search_cursor(0);
                    Vec::new()
                }
                KeyCode::End => {
                    let len = self
                        .state
                        .popup_dialog
                        .search_query()
                        .unwrap_or_default()
                        .chars()
                        .count();
                    self.state.popup_dialog.set_search_cursor(len);
                    Vec::new()
                }
                KeyCode::Backspace => {
                    self.state.popup_dialog.edit_search(|query, cursor| {
                        crate::app::dialogs::delete_dialog_char_before(query, cursor);
                    });
                    Vec::new()
                }
                KeyCode::Delete => {
                    let cursor = self.state.popup_dialog.search_cursor().unwrap_or(0);
                    if let Some(query) = self.state.popup_dialog.search_query_mut() {
                        crate::app::dialogs::delete_dialog_char_at(query, cursor);
                    }
                    Vec::new()
                }
                KeyCode::Char(c) if !key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.state.popup_dialog.edit_search(|query, cursor| {
                        crate::app::dialogs::insert_dialog_char(query, cursor, c);
                    });
                    Vec::new()
                }
                _ => Vec::new(),
            },
            Some(Popup::SearchLoading { .. }) => {
                if key_event.code == KeyCode::Esc {
                    self.close_search();
                }
                Vec::new()
            }
            Some(Popup::SearchResults { .. }) => match key_event.code {
                KeyCode::Char('k') | KeyCode::Up => {
                    self.move_search_cursor(false);
                    Vec::new()
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.move_search_cursor(true);
                    Vec::new()
                }
                KeyCode::Enter => self.reveal_search_result(),
                KeyCode::Esc => {
                    self.close_search();
                    Vec::new()
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn handle_dialog_key_event(&mut self, key_event: KeyEvent) -> Vec<Effect> {
        if self.state.popup_dialog.dialog_mode_ref() == Some(&DialogMode::SettingsEdit)
            && key_event.kind == KeyEventKind::Press
            && key_event.code == KeyCode::Enter
        {
            if self.state.async_ops.playlist_request.is_active() {
                return Vec::new();
            }
            return self.commit_settings_edit();
        }
        DialogController::new(&mut self.state).handle_dialog_key_event(key_event)
    }

    /// Enter directories under the cursor or add regular files directly.
    ///
    /// Playback autoplay wiring arrives in phase 4, so adding never fakes a
    /// play command today. Queued files immediately request extraction so
    /// tags appear without user interaction.
    fn activate_cursor_entry(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let entry = match self.state.browser.entries.get(self.state.browser.cursor()) {
            Some(entry) => entry.clone(),
            None => return Vec::new(),
        };

        if entry.kind == EntryKind::File {
            if is_supported_audio(&entry.path) {
                let append = self.state.extend_playlist([entry.path.clone()]);
                let added = append.added();
                let message = if added == 0 {
                    format!("{} already in queue", entry.name)
                } else {
                    format!("Added {}", entry.name)
                };
                self.push_notification(message);
                return self
                    .metadata_effect_for_indices(&append.added_indices)
                    .into_iter()
                    .collect();
            }
            self.push_notification(format!("Unsupported file type: {}", entry.name));
            return Vec::new();
        }

        let opened: DomainResult<PathBuf> = self.state.browser.enter_selected();
        match opened {
            Ok(dir) => {
                // Remember (parent, folder) before descending, so ascending can
                // restore the cursor on this folder at any nesting level.
                let parent_dir = self.state.browser.current_dir.clone();
                if entry.kind == EntryKind::Symlink {
                    let request_id = self.state.browser.begin_directory_request();
                    effects.push(Effect::ValidateBrowserDirectory {
                        request_id,
                        path: dir,
                        validation: BrowserDirectoryValidation::Symlink {
                            parent_dir,
                            folder_name: entry.name.clone(),
                        },
                    });
                } else {
                    self.state
                        .browser
                        .push_focus(parent_dir, entry.name.clone());
                    effects.push(self.request_browser_dir(dir));
                }
            }
            Err(error) => self.push_notification(error.to_string()),
        }
        effects
    }

    /// Collect marked entries or the cursor fallback and queue them.
    ///
    /// Supported files join the playlist synchronously, every directory
    /// yields exactly one scan effect executed by the main loop, and queued
    /// files request metadata in the same batch.
    fn add_selected(&mut self) -> Vec<Effect> {
        let mut selected_entries = std::mem::take(&mut self.state.browser.selected_entries);
        let taken = self
            .state
            .browser
            .take_marked_or_cursor(&mut selected_entries);
        self.state.browser.selected_entries = selected_entries;

        let mut files = Vec::new();
        let mut effects = Vec::new();
        for entry in taken {
            match entry.kind {
                EntryKind::Dir | EntryKind::Symlink => {
                    effects.push(Effect::ScanDirectory(entry.path));
                }
                EntryKind::File => files.push(entry.path),
            }
        }

        if !files.is_empty() {
            let count = files.len();
            let append = self.state.extend_playlist(files);
            let added = append.added();
            let skipped = append.skipped;
            if skipped > 0 {
                self.push_notification(format!(
                    "Added {added} tracks ({skipped} already in queue)"
                ));
            } else {
                self.push_notification(format!("Added {count} tracks"));
            }
            if let Some(effect) = self.metadata_effect_for_indices(&append.added_indices) {
                effects.push(effect);
            }
        } else if effects.is_empty() {
            self.push_notification("Nothing to add".to_string());
        }

        effects
    }
}
/// Scroll step granularity of the lyrics panel.
enum LyricsScrollStep {
    /// Move one row, directional.
    Line,
    /// Move one viewport (minus the two border rows) of rows.
    Page,
    /// Jump to an absolute offset; the value is clamped by the handler.
    JumpTo(usize),
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
