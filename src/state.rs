//! Core application state kept independent of IO and backend details.

use std::collections::HashSet;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ratatui::layout::Size;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui_cheese::spinner::{SpinnerState as CheeseSpinnerState, SpinnerType};

use crate::artwork::ArtworkState;
use crate::audio::{CrossfadeSeconds, GainDb, PlaybackState};
use crate::browser_state::BrowserState;
use crate::config::{ArtworkSource as SourceConfig, KeysConfig};
use crate::filesystem::FileEntry;
use crate::lyrics::{
    FollowDirection, LyricsDocument, LyricsOrigin, active_line_index_from_starts, follow_scroll,
    layout_document, line_char_times, next_line_starts, wrap_rows_for,
};
use crate::playback_mode::PlaybackMode;
use crate::playlist::Playlist;
use crate::playlist::navigation::NavigationState;
use crate::search::{SearchResult, SearchScope};
use crate::track::{Track, TrackLocation};

/// Cancellation shared by one Add Stream resolver worker and the UI request
/// that owns its result.
pub(crate) type StreamResolutionCancellation = crate::stream::StreamCancellation;

/// Result of appending entries to the playlist queue.
///
/// The indices identify the entries that were actually appended, so callers
/// can target those entries even when skipped duplicates appear in the input.
/// Indices work for both path-backed and already-built stream tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistAppendResult {
    /// Queue indices assigned to entries added by this append operation.
    pub added_indices: Vec<usize>,
    /// Number of input entries skipped because their identity was duplicated.
    pub skipped: usize,
}

/// Outcome of a structural queue mutation.
///
/// The application layer turns [`Self::PlayingTrackRemoved`] into the audio
/// worker's explicit stop effect; state owns the queue reanchor and cleanup,
/// while the state reducer owns effect emission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMutationResult {
    /// The requested swap or deletion did not change the queue.
    Unchanged,
    /// The queue changed and the playing track remains queued.
    Applied,
    /// The queue changed because the currently playing entry was deleted.
    PlayingTrackRemoved,
}

impl QueueMutationResult {
    /// Whether the caller must send `AudioCommand::Stop` to the worker.
    pub const fn requires_stop_playback(self) -> bool {
        matches!(self, Self::PlayingTrackRemoved)
    }
}

impl PlaylistAppendResult {
    /// Number of entries actually appended.
    pub fn added(&self) -> usize {
        self.added_indices.len()
    }
}

/// The single in-flight stream lifecycle owned by application state.
///
/// Resolution and playback acquisition are mutually exclusive. Keeping their
/// identity data with the lifecycle variant prevents a loading flag from
/// becoming detached from the token needed to accept or cancel its result.
#[derive(Debug)]
pub(crate) enum StreamActivity {
    /// A URL is being resolved for the Add Stream dialog.
    Resolving {
        /// Identity used to reject stale resolver completions.
        request_id: u64,
        /// Token used to stop the superseded resolver worker.
        cancellation: StreamResolutionCancellation,
    },
    /// The audio worker is acquiring or decoding a stream source.
    Acquiring {
        /// Stable identity of the stream being prepared.
        source: TrackLocation,
        /// Playback generation used to reject stale audio completions.
        generation: u64,
    },
}

/// Focusable panels shown in the main horizontal split.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Panel {
    /// File system browser occupying the left third.
    #[default]
    Browser,
    /// Playlist editor occupying the right two thirds.
    Playlist,
    /// Lyrics panel replacing the browser while it is visible.
    Lyrics,
}

/// Next panel in the focus order, honoring the lyrics visibility.
///
/// With the lyrics panel hidden the ring stays `Browser <-> Playlist`; once
/// it is shown the ring becomes `Browser -> Playlist -> Lyrics` (and the
/// reverse), so focus always lands on the panel that is actually rendered.
pub fn next_panel(active: Panel, lyrics_visible: bool) -> Panel {
    match active {
        Panel::Browser => Panel::Playlist,
        Panel::Playlist => {
            if lyrics_visible {
                Panel::Lyrics
            } else {
                Panel::Browser
            }
        }
        Panel::Lyrics => Panel::Playlist,
    }
}

/// Previous panel in the focus order, honoring the lyrics visibility.
pub fn previous_panel(active: Panel, lyrics_visible: bool) -> Panel {
    match active {
        Panel::Browser => {
            if lyrics_visible {
                Panel::Lyrics
            } else {
                Panel::Playlist
            }
        }
        Panel::Playlist => Panel::Browser,
        Panel::Lyrics => Panel::Playlist,
    }
}

/// Normal-height inner row count of the saved-playlist manager popup.
///
/// The frame metrics replace this default with the actual terminal-bound
/// height before every render, including when the terminal is smaller than
/// the popup's preferred height.
pub(crate) const DEFAULT_PLAYLIST_MANAGER_VIEWPORT_HEIGHT: u16 = 14;

/// Modal popups that can own the screen and the input focus.
///
/// Exactly one popup can be active at a time, which keeps the input
/// routing trivial: while `Some` is stored here every key is interpreted
/// in the popup context and the panels behind it stay frozen.
///
/// Not `Copy` because [`Popup::PlaylistManager`] owns a `Vec`; the small
/// extra clone cost is irrelevant for a value that changes only on user
/// actions.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Popup {
    /// Confirm or reject application exit.
    ConfirmQuit,
    /// Confirm the explicit playlist reorder action.
    ConfirmSortTracks {
        /// Settings draft to restore after confirmation or cancellation.
        draft: SettingsDraft,
    },
    /// Scrollable keybinding reference, carrying its row offset.
    Help {
        /// First content row currently shown at the top of the popup.
        scroll: u16,
    },
    /// Saved-playlist manager listing every stored name.
    PlaylistManager {
        /// Index of the highlighted entry in `names`.
        cursor: usize,
        /// Names currently shown, kept in sync with the store.
        names: Vec<String>,
    },
    /// Warning shown when "Save as" targets a name that already exists and is
    /// different from the current playlist. Confirms or cancels an overwrite.
    ConfirmOverwrite {
        /// The target name that would be overwritten.
        name: String,
    },
    /// Warning shown when deleting a playlist. Confirms or cancels the removal.
    ConfirmDelete {
        /// The playlist name that would be removed.
        name: String,
        /// Cursor position in the manager popup before the confirmation opened,
        /// used to restore it after deletion or cancellation. `None` when the
        /// delete was triggered with no manager popup open (the playing
        /// playlist path), so the source of the action can be distinguished.
        cursor: Option<usize>,
    },
    /// Alert shown when a rename target already exists in the same directory.
    ///
    /// Part of the alert family: Enter or Esc both dismiss it and the rename
    /// dialog stays open and editable underneath. There is deliberately NO
    /// overwrite option, matching the spec's no-overwrite collision rule.
    RenameCollision {
        /// File that already occupies the attempted name.
        existing: PathBuf,
        /// The name the user tried to rename to.
        attempted: String,
    },
    /// Query input for a contextual file or playlist search.
    SearchQuery {
        /// Surface searched by the query.
        scope: SearchScope,
    },
    /// Background search currently in flight.
    SearchLoading {
        /// Surface searched by the request.
        scope: SearchScope,
        /// Identity used to discard stale completions.
        request_id: u64,
    },
    /// Search results ready for navigation and selection.
    SearchResults {
        /// Surface searched by the request.
        scope: SearchScope,
        /// Identity used to keep a late completion from replacing newer UI.
        request_id: u64,
        /// Results in scanner or playlist order.
        results: Vec<SearchResult>,
        /// Highlighted result index.
        cursor: usize,
    },
    /// Full-window settings editor holding a working copy of the
    /// configuration and runtime state. `Esc` applies changes and closes the
    /// popup, while `Enter` confirms the field being edited.
    Settings {
        /// Which tab is currently shown.
        tab: SettingsTab,
        /// Whether the tab strip or the tab body owns the input.
        focus: SettingsFocus,
        /// Working copy of every editable value.
        draft: SettingsDraft,
    },
}

/// Tabs shown inside the settings popup, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    /// Runtime preferences: browser directory, quit confirmation, resume.
    General,
    /// Theme selection and live color editing.
    Appearance,
    /// Key bindings editor.
    Keys,
    /// Playback behavior preferences: remote lyrics lookup.
    Playback,
    /// Output host and device selection.
    Sound,
}

impl SettingsTab {
    pub fn all() -> [Self; 5] {
        [
            Self::General,
            Self::Appearance,
            Self::Keys,
            Self::Playback,
            Self::Sound,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Keys => "Keys",
            Self::Playback => "Playback",
            Self::Sound => "Sound",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::General => Self::Appearance,
            Self::Appearance => Self::Keys,
            Self::Keys => Self::Playback,
            Self::Playback => Self::Sound,
            Self::Sound => Self::General,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::General => Self::Sound,
            Self::Appearance => Self::General,
            Self::Keys => Self::Appearance,
            Self::Playback => Self::Keys,
            Self::Sound => Self::Playback,
        }
    }
}

/// Which Appearance sub-panel owns the typed selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceColumn {
    Display,
    Themes,
    Colors,
}

impl AppearanceColumn {
    pub fn next(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Display, true) => Self::Themes,
            (Self::Themes, true) => Self::Colors,
            (Self::Colors, true) => Self::Colors,
            (Self::Colors, false) => Self::Themes,
            (Self::Themes, false) => Self::Display,
            (Self::Display, false) => Self::Colors,
        }
    }
}

/// Selectable rows in the Appearance display column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceDisplayField {
    Border(crate::config::BorderType),
    NowPlayingSort(crate::config::SortBy),
    NowPlayingMetadata(crate::config::SortMetadataField),
    PlaylistSort(crate::config::SortBy),
    PlaylistMetadata(crate::config::SortMetadataField),
}

impl AppearanceDisplayField {
    pub const ALL: [Self; 15] = [
        Self::Border(crate::config::BorderType::Plain),
        Self::Border(crate::config::BorderType::Rounded),
        Self::Border(crate::config::BorderType::Double),
        Self::Border(crate::config::BorderType::Thick),
        Self::NowPlayingSort(crate::config::SortBy::Filename),
        Self::NowPlayingSort(crate::config::SortBy::Metadata),
        Self::NowPlayingMetadata(crate::config::SortMetadataField::TrackNumber),
        Self::NowPlayingMetadata(crate::config::SortMetadataField::Artist),
        Self::NowPlayingMetadata(crate::config::SortMetadataField::Album),
        Self::NowPlayingMetadata(crate::config::SortMetadataField::Title),
        Self::PlaylistSort(crate::config::SortBy::Filename),
        Self::PlaylistSort(crate::config::SortBy::Metadata),
        Self::PlaylistMetadata(crate::config::SortMetadataField::Artist),
        Self::PlaylistMetadata(crate::config::SortMetadataField::Album),
        Self::PlaylistMetadata(crate::config::SortMetadataField::TrackNumber),
    ];

    pub const fn row(self) -> usize {
        match self {
            Self::Border(crate::config::BorderType::Plain) => 1,
            Self::Border(crate::config::BorderType::Rounded) => 2,
            Self::Border(crate::config::BorderType::Double) => 3,
            Self::Border(crate::config::BorderType::Thick) => 4,
            Self::NowPlayingSort(crate::config::SortBy::Filename) => 6,
            Self::NowPlayingSort(crate::config::SortBy::Metadata) => 7,
            Self::NowPlayingMetadata(crate::config::SortMetadataField::TrackNumber) => 8,
            Self::NowPlayingMetadata(crate::config::SortMetadataField::Artist) => 9,
            Self::NowPlayingMetadata(crate::config::SortMetadataField::Album) => 10,
            Self::NowPlayingMetadata(crate::config::SortMetadataField::Title) => 11,
            Self::PlaylistSort(crate::config::SortBy::Filename) => 13,
            Self::PlaylistSort(crate::config::SortBy::Metadata) => 14,
            Self::PlaylistMetadata(crate::config::SortMetadataField::Artist) => 15,
            Self::PlaylistMetadata(crate::config::SortMetadataField::Album) => 16,
            Self::PlaylistMetadata(crate::config::SortMetadataField::TrackNumber) => 17,
            Self::PlaylistMetadata(crate::config::SortMetadataField::Title) => 18,
        }
    }
}

/// Typed fields owned by the settings reducer. Integer positions are kept only
/// at rendering/serialization boundaries for compatibility with persisted
/// drafts; all navigation decisions go through this table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    GeneralBrowserDirectory,
    GeneralConfirmQuit,
    GeneralResumePreviousTrack,
    GeneralShowHidden,
    GeneralSortTracks,
    AppearanceDisplay(AppearanceDisplayField),
    AppearanceTheme(usize),
    AppearanceColor(crate::ui::theme::ThemeColorField),
    Keys(crate::config::KeySettingsRow),
    PlaybackRemoteLyrics,
    PlaybackGain,
    PlaybackCrossfade,
    SoundOutput(usize),
}

impl SettingsField {
    pub const GENERAL_ALL: [Self; 5] = [
        Self::GeneralBrowserDirectory,
        Self::GeneralConfirmQuit,
        Self::GeneralResumePreviousTrack,
        Self::GeneralShowHidden,
        Self::GeneralSortTracks,
    ];
    pub const PLAYBACK_ALL: [Self; 3] = [
        Self::PlaybackRemoteLyrics,
        Self::PlaybackGain,
        Self::PlaybackCrossfade,
    ];

    pub fn appearance_display_fields(draft: &SettingsDraft) -> Vec<Self> {
        Self::appearance_display_fields_for(draft)
            .into_iter()
            .map(Self::AppearanceDisplay)
            .collect()
    }

    fn appearance_display_fields_for(draft: &SettingsDraft) -> Vec<AppearanceDisplayField> {
        AppearanceDisplayField::ALL
            .into_iter()
            .filter(|field| match field {
                AppearanceDisplayField::NowPlayingMetadata(_) => {
                    draft.now_playing_display.sort_by == crate::config::SortBy::Metadata
                }
                AppearanceDisplayField::PlaylistMetadata(_) => {
                    draft.playlist_columns.display_by == crate::config::SortBy::Metadata
                }
                _ => true,
            })
            .collect()
    }

    pub fn is_enabled(self, draft: &SettingsDraft) -> bool {
        match self {
            Self::AppearanceDisplay(field) => match field {
                AppearanceDisplayField::NowPlayingMetadata(_) => {
                    draft.now_playing_display.sort_by == crate::config::SortBy::Metadata
                }
                AppearanceDisplayField::PlaylistMetadata(_) => {
                    draft.playlist_columns.display_by == crate::config::SortBy::Metadata
                }
                _ => true,
            },
            Self::AppearanceTheme(index) => index < draft.theme_names.len(),
            Self::SoundOutput(index) => index < draft.outputs.len(),
            Self::AppearanceColor(field) => {
                field.index() < crate::ui::theme::ThemeColorField::ALL.len()
            }
            _ => true,
        }
    }

    pub fn activate(self, draft: &mut SettingsDraft) -> Option<Self> {
        self.is_enabled(draft).then_some(self)
    }

    pub fn next_enabled(
        tab: SettingsTab,
        current: Self,
        down: bool,
        draft: &SettingsDraft,
    ) -> Self {
        let fields: Vec<Self> = match current {
            Self::AppearanceDisplay(_) => Self::appearance_display_fields(draft),
            Self::AppearanceTheme(_) => (0..draft.theme_names.len())
                .map(Self::AppearanceTheme)
                .collect(),
            Self::AppearanceColor(_) => crate::ui::theme::ThemeColorField::ALL
                .into_iter()
                .map(Self::AppearanceColor)
                .collect(),
            Self::SoundOutput(_) => (0..draft.outputs.len()).map(Self::SoundOutput).collect(),
            Self::Keys(_) => crate::config::KeySettingsRow::ALL
                .into_iter()
                .map(Self::Keys)
                .collect(),
            _ => match tab {
                SettingsTab::General => Self::GENERAL_ALL.to_vec(),
                SettingsTab::Playback => Self::PLAYBACK_ALL.to_vec(),
                SettingsTab::Keys => crate::config::KeySettingsRow::ALL
                    .into_iter()
                    .map(Self::Keys)
                    .collect(),
                SettingsTab::Appearance => Self::appearance_display_fields(draft),
                SettingsTab::Sound => (0..draft.outputs.len()).map(Self::SoundOutput).collect(),
            },
        };
        if fields.is_empty() {
            return current;
        }
        let position = fields
            .iter()
            .position(|field| *field == current)
            .unwrap_or(0);
        if down {
            fields
                .iter()
                .skip(position.saturating_add(1))
                .find(|field| field.is_enabled(draft))
                .copied()
                .unwrap_or(current)
        } else {
            fields[..position]
                .iter()
                .rev()
                .find(|field| field.is_enabled(draft))
                .copied()
                .unwrap_or(current)
        }
    }
}

/// What currently owns the input inside the settings popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFocus {
    /// The tab strip at the top is focused; Tab cycles tabs.
    TabBar,
    /// The body of the active tab is focused; arrows move within it.
    Content,
}

/// Working copy of every editable value in the settings popup.
///
/// `PartialEq` is required because this value is stored in `Popup`, which also
/// derives `PartialEq`. Every field is simple data, so comparison is cheap.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsDraft {
    /// Mirror of `GeneralConfig.browser_directory` (single source of truth).
    pub browser_directory: String,
    /// Mirror of `GeneralConfig.confirm_quit`.
    pub confirm_quit: bool,
    /// Mirror of `GeneralConfig.resume_previous_track`.
    pub resume_previous_track: bool,
    /// Mirror of `GeneralConfig.show_hidden`.
    pub show_hidden: bool,
    /// Mirror of `GeneralConfig.playlist_columns` (playlist presentation).
    pub playlist_columns: crate::config::PlaylistColumnsConfig,
    /// `playlist_columns` value when the popup opened, used to detect an
    /// effective change on close.
    pub playlist_columns_initial: crate::config::PlaylistColumnsConfig,
    /// Available theme names in alphabetical order.
    pub theme_names: Vec<String>,
    /// Editable color strings for the selected theme.
    pub colors: crate::ui::theme::ThemeColors,
    /// Disk-loaded palette cache keyed by theme name, so moving through the
    /// theme list does not reread TOML on every key. `(name, palette)`;
    /// `None` means that no palette has been loaded yet.
    pub loaded_theme_palette: Option<(String, crate::ui::theme::ThemeColors)>,
    /// Mirror of `GeneralConfig.now_playing_display` (Now Playing display).
    pub now_playing_display: crate::config::SortTracksConfig,
    /// `now_playing_display` value when the popup opened, used to detect changes.
    pub now_playing_display_initial: crate::config::SortTracksConfig,
    /// Available audio outputs; index 0 is always "Default".
    pub outputs: Vec<crate::audio::AudioOutput>,
    /// Working copy of the key bindings.
    pub keys_draft: KeysConfig,
    /// Mirror of `PlaybackConfig.remote_lyrics` (Remote lyrics toggle).
    pub remote_lyrics: bool,
    /// Mirror of `PlaybackConfig.gain_db` (preamp gain slider, in dB).
    pub gain_db: GainDb,
    /// `gain_db` value when the popup opened, used to detect an effective change.
    pub gain_db_initial: GainDb,
    /// Mirror of `PlaybackConfig.crossfade_seconds` (crossfade slider, in s).
    pub crossfade_seconds: CrossfadeSeconds,
    /// `crossfade_seconds` value when the popup opened, used to detect changes.
    pub crossfade_initial: CrossfadeSeconds,
    /// Mirror of `UiConfig.border_type` (border glyph style).
    pub border_type: crate::config::BorderType,
    /// Border type when the popup opened, used to detect changes.
    pub border_type_initial: crate::config::BorderType,
    /// Typed navigation state for every settings tab and Appearance column.
    pub general_field: SettingsField,
    pub appearance_column: AppearanceColumn,
    pub appearance_display_field: AppearanceDisplayField,
    pub appearance_theme_field: SettingsField,
    pub appearance_color_field: crate::ui::theme::ThemeColorField,
    pub keys_field: crate::config::KeySettingsRow,
    pub playback_field: SettingsField,
    pub sound_field: SettingsField,
}

impl Default for SettingsDraft {
    fn default() -> Self {
        Self {
            browser_directory: String::new(),
            confirm_quit: false,
            resume_previous_track: false,
            show_hidden: false,
            playlist_columns: crate::config::PlaylistColumnsConfig::default(),
            playlist_columns_initial: crate::config::PlaylistColumnsConfig::default(),
            theme_names: Vec::new(),
            colors: crate::ui::theme::ThemeColors::default(),
            loaded_theme_palette: None,
            now_playing_display: crate::config::SortTracksConfig::default(),
            now_playing_display_initial: crate::config::SortTracksConfig::default(),
            outputs: Vec::new(),
            keys_draft: KeysConfig::default(),
            remote_lyrics: false,
            gain_db: GainDb::default(),
            gain_db_initial: GainDb::default(),
            crossfade_seconds: CrossfadeSeconds::default(),
            crossfade_initial: CrossfadeSeconds::default(),
            border_type: crate::config::BorderType::default(),
            border_type_initial: crate::config::BorderType::default(),
            general_field: SettingsField::GeneralBrowserDirectory,
            appearance_column: AppearanceColumn::Display,
            appearance_display_field: AppearanceDisplayField::Border(
                crate::config::BorderType::Plain,
            ),
            appearance_theme_field: SettingsField::AppearanceTheme(0),
            appearance_color_field: crate::ui::theme::ThemeColorField::Background,
            keys_field: crate::config::KeySettingsRow::Quit,
            playback_field: SettingsField::PlaybackRemoteLyrics,
            sound_field: SettingsField::SoundOutput(0),
        }
    }
}

impl SettingsDraft {
    /// Resolve the focused settings row without exposing integer row meanings
    /// to the input reducer.
    pub fn active_field(&self, tab: SettingsTab) -> Option<SettingsField> {
        let field = match tab {
            SettingsTab::General => self.general_field,
            SettingsTab::Appearance => match self.appearance_column {
                AppearanceColumn::Display => {
                    SettingsField::AppearanceDisplay(self.appearance_display_field)
                }
                AppearanceColumn::Themes => self.appearance_theme_field,
                AppearanceColumn::Colors => {
                    SettingsField::AppearanceColor(self.appearance_color_field)
                }
            },
            SettingsTab::Keys => SettingsField::Keys(self.keys_field),
            SettingsTab::Playback => self.playback_field,
            SettingsTab::Sound => self.sound_field,
        };
        field.is_enabled(self).then_some(field)
    }

    /// Apply a typed settings field at the rendering/serialization boundary.
    pub fn set_active_field(&mut self, tab: SettingsTab, field: SettingsField) {
        match (tab, field) {
            (SettingsTab::General, field) => {
                if SettingsField::GENERAL_ALL.contains(&field) {
                    self.general_field = field;
                }
            }
            (SettingsTab::Appearance, SettingsField::AppearanceDisplay(field)) => {
                self.appearance_column = AppearanceColumn::Display;
                self.appearance_display_field = field;
            }
            (SettingsTab::Appearance, SettingsField::AppearanceTheme(index)) => {
                self.appearance_column = AppearanceColumn::Themes;
                self.appearance_theme_field = SettingsField::AppearanceTheme(index);
            }
            (SettingsTab::Appearance, SettingsField::AppearanceColor(field)) => {
                self.appearance_column = AppearanceColumn::Colors;
                self.appearance_color_field = field;
            }
            (SettingsTab::Keys, SettingsField::Keys(row)) => {
                self.keys_field = row;
            }
            (SettingsTab::Playback, field) => {
                if SettingsField::PLAYBACK_ALL.contains(&field) {
                    self.playback_field = field;
                }
            }
            (SettingsTab::Sound, SettingsField::SoundOutput(index)) => {
                self.sound_field = SettingsField::SoundOutput(index);
            }
            _ => {}
        }
    }

    /// Resolve the color cursor through the canonical theme field list.
    pub fn appearance_color_field(&self) -> Option<crate::ui::theme::ThemeColorField> {
        Some(self.appearance_color_field)
    }

    /// Resolve the Keys cursor through the canonical settings row list.
    pub fn key_settings_row(&self) -> Option<crate::config::KeySettingsRow> {
        Some(self.keys_field)
    }

    /// Return the selected theme list index from the typed field.
    pub fn selected_theme(&self) -> Option<usize> {
        match self.appearance_theme_field {
            SettingsField::AppearanceTheme(index) => Some(index),
            _ => None,
        }
    }

    /// Return the selected output list index from the typed field.
    pub fn selected_output(&self) -> Option<usize> {
        match self.sound_field {
            SettingsField::SoundOutput(index) => Some(index),
            _ => None,
        }
    }

    /// Build a draft from runtime values that have no useful compile-time
    /// default, such as paths and theme names.
    ///
    /// `keys_draft` starts with bundled defaults; the caller must replace it
    /// with the active configuration before opening the popup. The output list
    /// is filled later by the `OutputProvider`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        browser_directory: String,
        confirm_quit: bool,
        resume_previous_track: bool,
        show_hidden: bool,
        playlist_columns: crate::config::PlaylistColumnsConfig,
        now_playing_display: crate::config::SortTracksConfig,
        border_type: crate::config::BorderType,
        theme_names: Vec<String>,
        colors: crate::ui::theme::ThemeColors,
    ) -> Self {
        Self {
            browser_directory,
            confirm_quit,
            resume_previous_track,
            show_hidden,
            playlist_columns_initial: playlist_columns.clone(),
            playlist_columns,
            now_playing_display_initial: now_playing_display.clone(),
            now_playing_display,
            border_type_initial: border_type,
            border_type,
            theme_names,
            colors,
            loaded_theme_palette: None,
            outputs: Vec::new(),
            keys_draft: KeysConfig::default(),
            general_field: SettingsField::GeneralBrowserDirectory,
            appearance_column: AppearanceColumn::Themes,
            appearance_display_field: AppearanceDisplayField::Border(
                crate::config::BorderType::Plain,
            ),
            appearance_theme_field: SettingsField::AppearanceTheme(0),
            appearance_color_field: crate::ui::theme::ThemeColorField::Background,
            keys_field: crate::config::KeySettingsRow::Quit,
            remote_lyrics: false,
            gain_db: GainDb::default(),
            gain_db_initial: GainDb::default(),
            crossfade_seconds: CrossfadeSeconds::default(),
            crossfade_initial: CrossfadeSeconds::default(),
            playback_field: SettingsField::PlaybackRemoteLyrics,
            sound_field: SettingsField::SoundOutput(0),
        }
    }

    /// Captures runtime state in an editable draft without touching the filesystem.
    ///
    /// Theme names and colors are an in-memory placeholder until the Settings
    /// theme-load effect completes. The path parameter remains for source
    /// compatibility with existing render fixtures, but is intentionally unused.
    pub fn from_state(
        state: &AppState,
        config: &crate::config::AppConfig,
        _themes_dir: &Path,
    ) -> Self {
        // The browser directory is the single source of truth in runtime state
        // and is reflected live in the panel.
        let browser_directory = state.browser.current_dir.to_string_lossy().into_owned();

        // Keep the editor usable while the blocking worker enumerates and loads
        // the real theme data.
        let theme_names = vec!["default".to_string(), "gruvbox".to_string()];
        let colors = crate::ui::theme::Theme::default().to_colors();

        let mut draft = Self::new(
            browser_directory,
            state.confirm_quit,
            state.persistence.resume_previous_track,
            state.browser.show_hidden,
            config.general.playlist_columns.clone(),
            // The Now Playing band reads `state.now_playing_display`, so the
            // draft must be seeded from that runtime truth: seeding from the
            // config file would let Appearance show checkboxes that disagree
            // with the panel, and closing Settings without an effective change
            // would silently revert the band to the persisted value.
            state.now_playing_display.clone(),
            config.ui.border_type,
            theme_names,
            colors,
        );
        draft.keys_draft = config.keys.clone();
        draft.remote_lyrics = config.playback.remote_lyrics;
        draft.gain_db = config.playback.gain_db;
        draft.gain_db_initial = config.playback.gain_db;
        draft.crossfade_seconds = config.playback.crossfade_seconds;
        draft.crossfade_initial = config.playback.crossfade_seconds;
        // Align the selected theme with the persisted name when possible.
        if !config.ui.theme.is_empty()
            && let Some(pos) = draft.theme_names.iter().position(|n| n == &config.ui.theme)
        {
            draft.appearance_theme_field = SettingsField::AppearanceTheme(pos);
        }
        draft
    }
}

/// Mode of the active free-text dialog used to name or rename a playlist.
///
/// The dialog captures [`DialogState::input`] while it is active, and the
/// matching command resolves the target from context (the manager selection
/// or the playing playlist) when the user confirms.
///
/// Not `Copy`: the rename-file and metadata-editor dialogs carry owned
/// payloads (a path and a field array) that the dialog owns for its whole
/// lifetime, so matching happens by reference or clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogMode {
    /// Rename a saved playlist selected in the manager popup.
    RenameSaved,
    /// Rename the currently playing playlist, saving it if it was unsaved.
    RenamePlaying,
    /// Save the current queue under a new name, keeping the active name.
    SaveAs,
    /// Create a fresh empty playlist and switch the queue to it.
    NewPlaylist,
    /// Edit a text field inside the settings popup (browser directory or a
    /// per-color string). Mirrors the naming dialog but scoped to settings.
    SettingsEdit,
    /// Rename the audio file under the browser or playlist cursor.
    ///
    /// The dialog input is preloaded with the current file name. Committing
    /// validates the name (non empty, same directory, no collision) and only
    /// then rewrites every playlist and renames the file on disk.
    RenameFile {
        /// Current location of the file being renamed.
        path: PathBuf,
        /// File name the dialog opened with, to detect an unchanged input.
        original_name: String,
        /// Validation error shown inside the dialog, if any.
        error: Option<String>,
    },
    /// Edit the ten tag fields of one audio file.
    ///
    /// The form opens in a loading state until the prefill effect delivers
    /// the raw tag values, then Enter commits each field and advances while
    /// Esc discards everything without writing. The final confirmation
    /// dispatches the write effect.
    EditMetadata {
        /// File whose tags the form edits.
        path: PathBuf,
        /// Committed draft values, indexed by [`crate::metadata::MetaField`].
        ///
        /// Boxed so the variant stays small: the ten owned strings would push
        /// `DialogMode` past the clippy `large_enum_variant` guidance.
        fields: Box<[String; 10]>,
        /// Index of the focused field inside `fields`.
        cursor: usize,
        /// Error shown inside the form, if any.
        error: Option<String>,
        /// True until the prefill values arrive from the worker.
        loading: bool,
    },
    /// Edit the display title of one stream entry.
    ///
    /// Streams do not have a file on disk, so the rename and edit-metadata
    /// shortcuts route here when the cursor points at a stream track. The
    /// dialog uses the same free-text input as the local rename popup, but
    /// committing it dispatches [`Effect::UpdateStreamExtinf`] so every
    /// saved playlist picks up the new EXTINF label without touching disk.
    RenameStream {
        /// URL identifying the stream entry being renamed.
        url: url::Url,
        /// Title the dialog opened with, used to detect a no-op submit.
        original_title: String,
        /// Validation or rename error shown inside the dialog, if any.
        error: Option<String>,
    },
    /// Free-text dialog that captures a stream URL and queues it as a
    /// remote stream.
    ///
    /// The typed text lives in [`DialogState::input`] so the input handler keeps
    /// editing it through the same code path as rename / new-playlist / save-as.
    /// The `loading` field drives the popup title and action hint.
    AddStream {
        /// Validation or resolver error rendered inside the popup. Cleared
        /// on the next keystroke so a fixed URL stops showing a stale error.
        error: Option<String>,
        /// True while the resolver is running off the UI thread.
        loading: bool,
    },
}

/// Smallest positive `N` not colliding with an existing `Playlist N` name.
///
/// Used when a generated default is needed, for example after deleting the
/// playlist that was playing. Comparison is exact: only names that already
/// match `Playlist N` for some `N` consume that number, so unrelated names
/// never shift the generated value. The result is always `Playlist 1` or
/// higher, never zero.
pub fn default_playlist_name(existing: &[String]) -> String {
    let mut n = 1;
    loop {
        let candidate = format!("Playlist {n}");
        if !existing.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Move through the typed General fields without converting through a row.
pub fn next_general_field(current: SettingsField, moving_down: bool) -> SettingsField {
    SettingsField::next_enabled(
        SettingsTab::General,
        current,
        moving_down,
        &SettingsDraft::default(),
    )
}

/// Move through typed Appearance display fields while skipping disabled
/// metadata options.
pub fn next_appearance_display_field(
    current: AppearanceDisplayField,
    moving_down: bool,
    now_playing_metadata: bool,
    playlist_columns_metadata: bool,
) -> AppearanceDisplayField {
    let mut draft = SettingsDraft::default();
    draft.now_playing_display.sort_by = if now_playing_metadata {
        crate::config::SortBy::Metadata
    } else {
        crate::config::SortBy::Filename
    };
    draft.playlist_columns.display_by = if playlist_columns_metadata {
        crate::config::SortBy::Metadata
    } else {
        crate::config::SortBy::Filename
    };
    match SettingsField::next_enabled(
        SettingsTab::Appearance,
        SettingsField::AppearanceDisplay(current),
        moving_down,
        &draft,
    ) {
        SettingsField::AppearanceDisplay(field) => field,
        _ => current,
    }
}

/// Next "Playlist N" name using the highest existing numeric suffix.
///
/// Scans names that start with "Playlist " followed by digits and returns
/// "Playlist {max+1}". Names not matching that shape are ignored, so arbitrary
/// playlists never shift the counter. Falls back to "Playlist 1" when none match.
pub fn next_playlist_name(existing: &[String]) -> String {
    let mut max: Option<u32> = None;
    for name in existing {
        if let Some(rest) = name.strip_prefix("Playlist ")
            && let Ok(n) = rest.trim().parse::<u32>()
        {
            max = Some(max.map_or(n, |m| m.max(n)));
        }
    }
    format!("Playlist {}", max.map_or(1, |m| m + 1))
}

/// Maximum number of notifications retained for display in the status area.
pub const NOTIFICATIONS_CAP: usize = 50;

/// Record a notification, keeping only the newest cap messages.
///
/// Kept as a pure helper so the retention rule stays unit testable in
/// isolation from the rest of the application state.
pub fn push_notification(notifications: &mut Vec<String>, message: String, cap: usize) {
    notifications.push(message);
    let excess = notifications.len().saturating_sub(cap);
    notifications.drain(..excess);
}

/// Pure geometry derived from one lyrics document at one layout width.
#[derive(Debug, Clone)]
pub(crate) struct LyricsLayoutCache {
    pub(crate) document_version: u64,
    pub(crate) width: usize,
    pub(crate) layout: crate::lyrics::DocumentLayout,
    pub(crate) wrap_ranges: Arc<Vec<Arc<Vec<(usize, usize)>>>>,
    pub(crate) char_times: Arc<Vec<Option<Arc<Vec<i64>>>>>,
    pub(crate) timed_starts: Vec<(usize, i64)>,
}

/// Lyrics panel display state.
///
/// The panel reuses the browser area while visible, so its viewport height
/// is measured at render time and its scroll offset is clamped against the
/// cached physical row count by the command layer, mirroring the help popup
/// approach.
#[derive(Debug, Default, Clone)]
pub struct LyricsState {
    /// Whether the lyrics panel replaces the browser.
    pub visible: bool,
    /// First visible lyrics row, moved directionally by scroll commands.
    pub scroll: usize,
    /// Visible row count refreshed by the explicit frame tick.
    pub viewport_height: u16,
    /// Whether a resolution is currently in flight.
    pub loading: bool,
    /// Queue index the panel is currently showing lyrics for.
    pub track_index: Option<usize>,
    /// Parsed lyrics once the background worker delivered them.
    pub document: Option<LyricsDocument>,
    /// Where the document came from, when known.
    pub origin: Option<LyricsOrigin>,
    /// Short reason for the "not available" state, when there is one.
    pub error: Option<String>,
    /// Panel border title: tagged title or file stem, set at load time.
    pub display_title: Option<String>,
    /// Index of the line being sung, used as the panel focus for
    /// directional follow; `None` when untimed or unavailable.
    pub active_line: Option<usize>,
    /// Panel inner width used for the last follow layout, so a terminal resize
    /// re-anchors the scroll even when the active line did not change.
    pub layout_width: Option<usize>,
    /// Monotonic identity for the current document contents.
    pub(crate) document_version: u64,
    /// Cached pure layout data for the current document and width.
    pub(crate) layout_cache: Option<LyricsLayoutCache>,
}

impl LyricsState {
    /// Replace the document and invalidate all derived layout data.
    pub(crate) fn set_document(&mut self, document: Option<LyricsDocument>) {
        self.document = document;
        self.document_version = self.document_version.wrapping_add(1);
        self.layout_cache = None;
    }

    /// Ensure the pure layout data is available for `width`.
    pub(crate) fn ensure_layout_cache(&mut self, width: usize) {
        let Some(document) = self.document.as_ref() else {
            self.layout_cache = None;
            return;
        };

        if self.layout_cache.as_ref().is_some_and(|cache| {
            cache.document_version == self.document_version && cache.width == width
        }) {
            return;
        }

        let next_line_starts = next_line_starts(document);
        let wrap_ranges = Arc::new(
            document
                .lines
                .iter()
                .map(|line| Arc::new(wrap_rows_for(&line.text, width)))
                .collect(),
        );
        let char_times = Arc::new(
            document
                .lines
                .iter()
                .enumerate()
                .map(|(index, line)| line_char_times(line, next_line_starts[index]).map(Arc::new))
                .collect(),
        );
        self.layout_cache = Some(LyricsLayoutCache {
            document_version: self.document_version,
            width,
            layout: layout_document(&document.lines, width),
            wrap_ranges,
            char_times,
            timed_starts: document
                .lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| line.timestamp_ms.map(|start| (index, start)))
                .collect(),
        });
    }

    /// Read cached layout data without computing during rendering.
    pub(crate) fn layout_cache_for(&self, width: usize) -> Option<&LyricsLayoutCache> {
        self.layout_cache
            .as_ref()
            .filter(|cache| cache.document_version == self.document_version && cache.width == width)
    }

    /// Return the physical row count from the current render-width cache.
    pub(crate) fn cached_total_rows(&self) -> Option<usize> {
        self.layout_cache
            .as_ref()
            .filter(|cache| cache.document_version == self.document_version)
            .map(|cache| cache.layout.total_rows)
    }
}

/// Geometry-dependent values refreshed once per application frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMetrics {
    /// Visible browser rows after panel borders are removed.
    pub browser_viewport_height: u16,
    /// Visible playlist rows after panel borders are removed.
    pub playlist_viewport_height: u16,
    /// Visible lyrics rows after panel borders are removed.
    pub lyrics_viewport_height: u16,
    /// Inner lyrics width used to resolve wrapped physical rows.
    pub lyrics_layout_width: usize,
    /// Native artwork resize target decided before rendering.
    pub artwork_target: Option<Size>,
    /// Visible row count inside the saved-playlist manager popup.
    pub playlist_manager_viewport_height: u16,
}

impl FrameMetrics {
    /// Build metrics from the measured panel geometry.
    pub const fn new(
        browser_viewport_height: u16,
        playlist_viewport_height: u16,
        lyrics_viewport_height: u16,
        lyrics_layout_width: usize,
    ) -> Self {
        Self {
            browser_viewport_height,
            playlist_viewport_height,
            lyrics_viewport_height,
            lyrics_layout_width,
            artwork_target: None,
            playlist_manager_viewport_height: DEFAULT_PLAYLIST_MANAGER_VIEWPORT_HEIGHT,
        }
    }

    /// Supply the terminal-bound height of the saved-playlist manager list.
    pub const fn with_playlist_manager_viewport_height(mut self, height: u16) -> Self {
        self.playlist_manager_viewport_height = height;
        self
    }
}

/// Browser capability state, including the panel listing and its selection
/// policy. Directory contents are supplied by the application effect layer.
#[derive(Debug)]
pub struct BrowserCapability {
    /// State used to navigate and render the current listing.
    pub(crate) state: BrowserState,
    /// Whether dotfiles are included in listings.
    pub(crate) show_hidden: bool,
    /// Marked entries queued for a batch add.
    pub(crate) selected_entries: HashSet<PathBuf>,
    /// Monotonic identity for directory listing requests.
    directory_request_id: u64,
}

/// Frame-local animation state shared by every spinner surface.
#[derive(Debug)]
pub struct FrameState {
    /// Single global spinner phase.
    pub(crate) spinner: CheeseSpinnerState,
    /// Wall-clock anchor used by the application frame tick.
    pub(crate) spinner_last_tick: Option<Instant>,
}

impl Default for FrameState {
    fn default() -> Self {
        Self {
            spinner: CheeseSpinnerState::new(SpinnerType::Dot),
            spinner_last_tick: None,
        }
    }
}

/// A typed request identity slot with one active completion gate.
///
/// The marker type keeps unrelated request families from being accidentally
/// exchanged while the slot owns the shared monotonic-ID lifecycle.
#[derive(Debug)]
pub(crate) struct RequestSlot<T> {
    next: u64,
    active: Option<u64>,
    marker: PhantomData<T>,
}

impl<T> Default for RequestSlot<T> {
    fn default() -> Self {
        Self {
            next: 0,
            active: None,
            marker: PhantomData,
        }
    }
}

impl<T> RequestSlot<T> {
    /// Start a request, superseding any older active request.
    pub(crate) fn begin(&mut self) -> u64 {
        self.next = self.next.wrapping_add(1).max(1);
        self.active = Some(self.next);
        self.next
    }

    /// Start a request only when no request is currently active.
    pub(crate) fn try_begin(&mut self) -> Option<u64> {
        self.active.is_none().then(|| self.begin())
    }

    /// Accept and clear only the currently active request.
    pub(crate) fn accept(&mut self, request_id: u64) -> bool {
        if self.active == Some(request_id) {
            self.active = None;
            true
        } else {
            false
        }
    }

    /// Cancel the active request without changing the next identity.
    pub(crate) fn cancel(&mut self) {
        self.active = None;
    }

    /// Cancel only when the supplied identity still owns the slot.
    pub(crate) fn cancel_if(&mut self, request_id: u64) -> bool {
        if self.active == Some(request_id) {
            self.active = None;
            true
        } else {
            false
        }
    }

    /// Return the active request identity, if any.
    #[cfg(test)]
    pub(crate) const fn active(&self) -> Option<u64> {
        self.active
    }

    /// Whether this slot currently owns a request.
    pub(crate) const fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

#[derive(Debug)]
pub(crate) struct PlaylistRequest;

#[derive(Debug)]
pub(crate) struct FileRenameRequest;

#[derive(Debug)]
pub(crate) struct BrowserValidationRequest;

#[derive(Debug)]
pub(crate) struct ThemeRequest;

#[derive(Debug)]
pub(crate) struct ThemeSaveRequest;

/// State owned by asynchronous application operations and their UI gates.
#[derive(Debug)]
pub struct AsyncOperationState {
    /// Monotonic identity for contextual search requests.
    pub(crate) next_search_request_id: u64,
    /// Monotonic identity for stream resolution requests.
    pub(crate) next_stream_request_id: u64,
    /// Monotonic identity for playback starts.
    pub(crate) next_playback_generation: u64,
    /// Playlist filesystem request identity and ownership.
    pub(crate) playlist_request: RequestSlot<PlaylistRequest>,
    /// Generic file-rename request identity and ownership.
    pub(crate) file_rename_request: RequestSlot<FileRenameRequest>,
    /// Browser-directory validation identity and ownership.
    pub(crate) browser_validation_request: RequestSlot<BrowserValidationRequest>,
    /// Monotonic identity for configuration snapshots sent to the writer.
    pub(crate) next_config_request_id: u64,
    /// Monotonic identity for runtime-state snapshots sent to the writer.
    pub(crate) next_runtime_state_request_id: u64,
    /// Monotonic identity for visible Settings visits.
    pub(crate) next_settings_visit_id: u64,
    /// Settings visit currently allowed to receive theme completions.
    pub(crate) settings_visit_id: Option<u64>,
    /// Theme-load identity and ownership, paired with `settings_visit_id`.
    pub(crate) theme_request: RequestSlot<ThemeRequest>,
    /// Theme-save identity and ownership, paired with `settings_visit_id`.
    pub(crate) theme_save_request: RequestSlot<ThemeSaveRequest>,
    /// Number of background effects currently in flight.
    pub(crate) pending_effects: Arc<AtomicUsize>,
    /// The one active stream lifecycle, if resolution or acquisition is in
    /// flight.
    stream_activity: Option<StreamActivity>,
}

impl Default for AsyncOperationState {
    fn default() -> Self {
        Self {
            next_search_request_id: 0,
            next_stream_request_id: 0,
            next_playback_generation: 0,
            playlist_request: RequestSlot::default(),
            file_rename_request: RequestSlot::default(),
            browser_validation_request: RequestSlot::default(),
            next_config_request_id: 0,
            next_runtime_state_request_id: 0,
            next_settings_visit_id: 0,
            settings_visit_id: None,
            theme_request: RequestSlot::default(),
            theme_save_request: RequestSlot::default(),
            pending_effects: Arc::new(AtomicUsize::new(0)),
            stream_activity: None,
        }
    }
}

impl AsyncOperationState {
    /// Start stream resolution, cancelling any previous stream activity.
    pub(crate) fn begin_stream_resolution(&mut self) -> (u64, StreamResolutionCancellation) {
        self.cancel_stream_activity();
        self.next_stream_request_id = self.next_stream_request_id.wrapping_add(1);
        let request_id = self.next_stream_request_id;
        let cancellation = StreamResolutionCancellation::new();
        self.stream_activity = Some(StreamActivity::Resolving {
            request_id,
            cancellation: cancellation.clone(),
        });
        (request_id, cancellation)
    }

    /// Accept only the active stream-resolution result. A stale result leaves
    /// the active activity untouched.
    pub(crate) fn accept_stream_resolution(&mut self, request_id: u64) -> bool {
        let accepts = matches!(
            self.stream_activity.as_ref(),
            Some(StreamActivity::Resolving {
                request_id: active_request_id,
                ..
            }) if *active_request_id == request_id
        );
        if accepts {
            self.stream_activity = None;
        }
        accepts
    }

    /// Cancel stream resolution or acquisition and cooperatively stop a
    /// resolver worker when one owns the activity.
    pub(crate) fn cancel_stream_resolution(&mut self) {
        self.cancel_stream_activity();
    }

    /// Compensate a failed resolver dispatch without touching a newer stream
    /// activity that may already own the UI.
    pub(crate) fn cancel_stream_resolution_if(&mut self, request_id: u64) -> bool {
        let matches = matches!(
            self.stream_activity.as_ref(),
            Some(StreamActivity::Resolving {
                request_id: active_request_id,
                ..
            }) if *active_request_id == request_id
        );
        if matches {
            self.cancel_stream_activity();
        }
        matches
    }

    pub(crate) fn cancel_browser_validation_if(&mut self, request_id: u64) -> bool {
        self.browser_validation_request.cancel_if(request_id)
    }

    pub(crate) fn cancel_playlist_request_if(&mut self, request_id: u64) -> bool {
        self.playlist_request.cancel_if(request_id)
    }

    pub(crate) fn cancel_file_rename_request_if(&mut self, request_id: u64) -> bool {
        self.file_rename_request.cancel_if(request_id)
    }

    pub(crate) fn cancel_theme_request_if(&mut self, request_id: u64, visit_id: u64) -> bool {
        if self.settings_visit_id != Some(visit_id) {
            return false;
        }
        self.theme_request.cancel_if(request_id)
    }

    pub(crate) fn cancel_theme_save_request_if(&mut self, request_id: u64, visit_id: u64) -> bool {
        if self.settings_visit_id != Some(visit_id) {
            return false;
        }
        self.theme_save_request.cancel_if(request_id)
    }

    /// Mark playback acquisition as active for one stream identity.
    pub(crate) fn begin_stream_acquisition(&mut self, source: TrackLocation, generation: u64) {
        self.cancel_stream_activity();
        self.stream_activity = Some(StreamActivity::Acquiring { source, generation });
    }

    /// Clear the active stream lifecycle and cancel a resolver if it owns it.
    pub(crate) fn cancel_stream_activity(&mut self) {
        if let Some(StreamActivity::Resolving { cancellation, .. }) = self.stream_activity.take() {
            cancellation.cancel();
        }
    }

    /// Accept and clear stream acquisition only when its identity is current.
    pub(crate) fn accept_stream_acquisition(
        &mut self,
        source: &TrackLocation,
        generation: Option<u64>,
    ) -> bool {
        let accepts = matches!(
            self.stream_activity.as_ref(),
            Some(StreamActivity::Acquiring {
                source: active_source,
                generation: active_generation,
            }) if active_source == source
                && !generation.is_some_and(|generation| *active_generation != generation)
        );
        if accepts {
            self.stream_activity = None;
        }
        accepts
    }

    /// Read-only view of the active stream lifecycle for rendering and tests.
    pub(crate) fn stream_activity(&self) -> Option<&StreamActivity> {
        self.stream_activity.as_ref()
    }

    /// Assign the next configuration snapshot identity.
    pub(crate) fn begin_config_save(&mut self) -> u64 {
        self.next_config_request_id = self.next_config_request_id.wrapping_add(1).max(1);
        self.next_config_request_id
    }

    /// Assign the next runtime-state snapshot identity.
    pub(crate) fn begin_runtime_state_save(&mut self) -> u64 {
        self.next_runtime_state_request_id =
            self.next_runtime_state_request_id.wrapping_add(1).max(1);
        self.next_runtime_state_request_id
    }

    /// Start a Settings visit and invalidate all theme work from an older visit.
    pub(crate) fn begin_settings_visit(&mut self) -> u64 {
        self.next_settings_visit_id = self.next_settings_visit_id.wrapping_add(1).max(1);
        self.settings_visit_id = Some(self.next_settings_visit_id);
        self.cancel_theme_requests();
        self.next_settings_visit_id
    }

    /// Start a theme preview or apply load for one Settings visit.
    pub(crate) fn begin_theme_request(&mut self) -> Option<(u64, u64)> {
        let visit_id = self.settings_visit_id?;
        Some((self.theme_request.begin(), visit_id))
    }

    /// Accept only the current theme-load completion for the current visit.
    pub(crate) fn accept_theme_request(&mut self, request_id: u64, visit_id: u64) -> bool {
        self.settings_visit_id == Some(visit_id) && self.theme_request.accept(request_id)
    }

    /// Start a theme save for one Settings visit.
    pub(crate) fn begin_theme_save_request(&mut self) -> Option<(u64, u64)> {
        let visit_id = self.settings_visit_id?;
        Some((self.theme_save_request.begin(), visit_id))
    }

    /// Accept only the current theme-save completion for the current visit.
    pub(crate) fn accept_theme_save_request(&mut self, request_id: u64, visit_id: u64) -> bool {
        self.settings_visit_id == Some(visit_id) && self.theme_save_request.accept(request_id)
    }

    /// Invalidate pending theme work while keeping the Settings popup open.
    pub(crate) fn cancel_theme_requests(&mut self) {
        self.theme_request.cancel();
        self.theme_save_request.cancel();
    }
}

/// Artwork capability state, including source selection and loading metadata.
#[derive(Debug)]
pub struct ArtworkCapability {
    /// Render state consumed by the UI.
    pub(crate) state: ArtworkState,
    /// Configured source precedence for artwork lookup.
    pub(crate) source_config: SourceConfig,
    /// Cache directory used by remote and terminal artwork materialization.
    pub(crate) cache_dir: PathBuf,
    /// Whether the current track's artwork request is in flight.
    pub(crate) loading: bool,
}

impl Default for ArtworkCapability {
    fn default() -> Self {
        Self {
            state: ArtworkState::default(),
            source_config: SourceConfig::default(),
            cache_dir: PathBuf::new(),
            loading: false,
        }
    }
}

// Intentional local capability projection: callers working with the artwork
// capability can use its `ArtworkState` operations while the wrapper retains
// source configuration, cache, and loading ownership beside that state.
impl std::ops::Deref for ArtworkCapability {
    type Target = ArtworkState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl std::ops::DerefMut for ArtworkCapability {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

/// Persistence and resume identity carried between sessions.
#[derive(Debug, Default)]
pub struct PersistenceState {
    /// Whether startup should resume the saved track.
    pub(crate) resume_previous_track: bool,
    /// Canonical runtime identity used for persistence and queue remapping.
    pub(crate) last_track: Option<TrackLocation>,
    /// Last persisted playback position in milliseconds.
    pub(crate) last_track_position_ms: u64,
}

/// Text and validation state owned by one free-text dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DialogState {
    pub(crate) mode: DialogMode,
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) extension: Option<String>,
    pub(crate) error: Option<String>,
}

impl DialogState {
    pub(crate) fn new(mode: DialogMode, input: String, extension: Option<String>) -> Self {
        let cursor = input.chars().count();
        Self {
            mode,
            input,
            cursor,
            extension,
            error: None,
        }
    }
}

/// Search state is kept together so query editing cannot coexist with a
/// non-search popup or dialog.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SearchState {
    pub(crate) popup: Popup,
    pub(crate) query: String,
    pub(crate) cursor: usize,
}

/// Valid modal configurations and the small set of supported nested flows.
///
/// The fields are private to this module's transition API. In particular,
/// alerts, dialogs, and popups cannot be independently assigned by callers.
/// Nested variants are limited to the workflows that have a documented
/// underlying surface: settings/playlist-manager editing, overwrite and file
/// collision confirmation, and playlist deletion confirmation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ModalStack {
    Normal,
    Alert {
        message: String,
        underlying: Box<ModalStack>,
    },
    Search(SearchState),
    Dialog(DialogState),
    Popup(Popup),
    DialogInPopup {
        popup: Popup,
        dialog: DialogState,
    },
    PopupInDialog {
        popup: Popup,
        dialog: DialogState,
    },
    PopupInPopup {
        popup: Popup,
        underlying: Popup,
    },
}

impl Default for ModalStack {
    fn default() -> Self {
        Self::Normal
    }
}

/// Popup, dialog, and text-editing state owned by modal input surfaces.
#[derive(Debug)]
pub struct PopupDialogState {
    pub(crate) modal: ModalStack,
    manager_scroll_offset: usize,
    manager_viewport_height: u16,
}

impl Default for PopupDialogState {
    fn default() -> Self {
        Self {
            modal: ModalStack::default(),
            manager_scroll_offset: 0,
            manager_viewport_height: DEFAULT_PLAYLIST_MANAGER_VIEWPORT_HEIGHT,
        }
    }
}

impl PopupDialogState {
    /// First visible row of the saved-playlist manager.
    pub(crate) fn manager_scroll_offset(&self) -> usize {
        self.manager_scroll_offset
    }

    /// Update the popup viewport and normalize its stored window before draw.
    pub(crate) fn set_manager_viewport_height(&mut self, height: u16) {
        self.manager_viewport_height = height.max(1);
        self.clamp_manager_scroll();
    }

    /// Move the saved-playlist manager selection toward the head.
    pub(crate) fn move_manager_up(&mut self) {
        if let Some(Popup::PlaylistManager { cursor, .. }) = self.active_popup_mut() {
            *cursor = cursor.saturating_sub(1);
        }
        self.reanchor_manager_scroll(crate::browser_state::ScrollDirection::Up);
    }

    /// Move the saved-playlist manager selection toward the tail.
    pub(crate) fn move_manager_down(&mut self) {
        if let Some(Popup::PlaylistManager { cursor, names }) = self.active_popup_mut() {
            *cursor = (*cursor + 1).min(names.len().saturating_sub(1));
        }
        self.reanchor_manager_scroll(crate::browser_state::ScrollDirection::Down);
    }

    /// Jump the saved-playlist manager selection to its first row.
    pub(crate) fn move_manager_top(&mut self) {
        if let Some(Popup::PlaylistManager { cursor, .. }) = self.active_popup_mut() {
            *cursor = 0;
        }
        self.manager_scroll_offset = 0;
    }

    /// Jump the saved-playlist manager selection to its last row.
    pub(crate) fn move_manager_bottom(&mut self) {
        if let Some(Popup::PlaylistManager { cursor, names }) = self.active_popup_mut() {
            *cursor = names.len().saturating_sub(1);
        }
        self.reanchor_manager_scroll(crate::browser_state::ScrollDirection::Down);
    }

    /// Page the saved-playlist manager selection toward the head.
    pub(crate) fn page_manager_up(&mut self) {
        let step = self.manager_page_step();
        if let Some(Popup::PlaylistManager { cursor, .. }) = self.active_popup_mut() {
            *cursor = cursor.saturating_sub(step);
        }
        self.reanchor_manager_scroll(crate::browser_state::ScrollDirection::Up);
    }

    /// Page the saved-playlist manager selection toward the tail.
    pub(crate) fn page_manager_down(&mut self) {
        let step = self.manager_page_step();
        if let Some(Popup::PlaylistManager { cursor, names }) = self.active_popup_mut() {
            *cursor = cursor
                .saturating_add(step)
                .min(names.len().saturating_sub(1));
        }
        self.reanchor_manager_scroll(crate::browser_state::ScrollDirection::Down);
    }

    pub(crate) fn active_popup_ref(&self) -> Option<&Popup> {
        self.modal.active_popup_ref()
    }

    #[cfg(test)]
    pub(crate) fn active_popup_value(&self) -> Option<Popup> {
        self.active_popup_ref().cloned()
    }

    pub(crate) fn active_popup_mut(&mut self) -> Option<&mut Popup> {
        self.modal.active_popup_mut()
    }

    pub(crate) fn dialog_ref(&self) -> Option<&DialogState> {
        self.modal.dialog_ref()
    }

    pub(crate) fn dialog_mut(&mut self) -> Option<&mut DialogState> {
        self.modal.dialog_mut()
    }

    pub(crate) fn search_ref(&self) -> Option<&SearchState> {
        self.modal.search_ref()
    }

    pub(crate) fn search_mut(&mut self) -> Option<&mut SearchState> {
        self.modal.search_mut()
    }

    pub(crate) fn search_query(&self) -> Option<&str> {
        self.search_ref().map(|search| search.query.as_str())
    }

    pub(crate) fn search_query_mut(&mut self) -> Option<&mut String> {
        self.search_mut().map(|search| &mut search.query)
    }

    #[cfg(test)]
    pub(crate) fn set_search_query(&mut self, query: String) {
        if let Some(search) = self.search_mut() {
            search.cursor = query.chars().count();
            search.query = query;
        }
    }

    pub(crate) fn search_cursor(&self) -> Option<usize> {
        self.search_ref().map(|search| search.cursor)
    }

    pub(crate) fn set_search_cursor(&mut self, cursor: usize) {
        if let Some(search) = self.search_mut() {
            search.cursor = cursor;
        }
    }

    pub(crate) fn edit_search<F>(&mut self, edit: F)
    where
        F: FnOnce(&mut String, &mut usize),
    {
        if let Some(search) = self.search_mut() {
            edit(&mut search.query, &mut search.cursor);
        }
    }

    pub(crate) fn alert_message(&self) -> Option<&str> {
        self.modal.alert_message()
    }

    pub(crate) fn open_popup(&mut self, popup: Popup) {
        if matches!(&popup, Popup::PlaylistManager { .. }) {
            self.manager_scroll_offset = 0;
        }
        if matches!(
            &popup,
            Popup::ConfirmOverwrite { .. }
                | Popup::ConfirmDelete { .. }
                | Popup::RenameCollision { .. }
        ) {
            return;
        }
        self.modal = match popup {
            popup @ (Popup::SearchQuery { .. }
            | Popup::SearchLoading { .. }
            | Popup::SearchResults { .. }) => ModalStack::Search(SearchState {
                popup,
                query: String::new(),
                cursor: 0,
            }),
            popup => ModalStack::Popup(popup),
        };
    }

    pub(crate) fn open_search(&mut self, scope: SearchScope) {
        self.modal = ModalStack::Search(SearchState {
            popup: Popup::SearchQuery { scope },
            query: String::new(),
            cursor: 0,
        });
    }

    pub(crate) fn open_dialog(
        &mut self,
        mode: DialogMode,
        input: String,
        extension: Option<String>,
    ) {
        let dialog = DialogState::new(mode, input, extension);
        let current = std::mem::replace(&mut self.modal, ModalStack::Normal);
        self.modal = match current {
            ModalStack::Popup(popup @ (Popup::PlaylistManager { .. } | Popup::Settings { .. })) => {
                ModalStack::DialogInPopup { popup, dialog }
            }
            _ => ModalStack::Dialog(dialog),
        };
    }

    pub(crate) fn push_popup(&mut self, popup: Popup) {
        let current = std::mem::replace(&mut self.modal, ModalStack::Normal);
        self.modal = match (popup, current) {
            (
                popup @ (Popup::ConfirmOverwrite { .. } | Popup::RenameCollision { .. }),
                ModalStack::Dialog(dialog),
            ) => ModalStack::PopupInDialog { popup, dialog },
            (
                popup @ Popup::ConfirmDelete { .. },
                ModalStack::Popup(underlying @ Popup::PlaylistManager { .. }),
            ) => ModalStack::PopupInPopup { popup, underlying },
            (
                popup @ (Popup::ConfirmDelete { .. } | Popup::RenameCollision { .. }),
                ModalStack::Normal,
            ) => ModalStack::Popup(popup),
            (popup, current) => {
                debug_assert!(false, "popup {popup:?} cannot be pushed over {current:?}");
                current
            }
        };
    }

    pub(crate) fn replace_popup(&mut self, popup: Popup) {
        if matches!(
            popup,
            Popup::ConfirmOverwrite { .. }
                | Popup::ConfirmDelete { .. }
                | Popup::RenameCollision { .. }
        ) {
            return;
        }
        let is_manager = matches!(&popup, Popup::PlaylistManager { .. });
        match &mut self.modal {
            ModalStack::Alert { underlying, .. } => {
                let mut state = PopupDialogState {
                    modal: *std::mem::replace(underlying, Box::new(ModalStack::Normal)),
                    manager_scroll_offset: self.manager_scroll_offset,
                    manager_viewport_height: self.manager_viewport_height,
                };
                state.replace_popup(popup);
                *underlying = Box::new(state.modal);
                self.manager_scroll_offset = state.manager_scroll_offset;
                self.manager_viewport_height = state.manager_viewport_height;
            }
            ModalStack::DialogInPopup { popup: current, .. }
            | ModalStack::PopupInPopup { popup: current, .. }
            | ModalStack::PopupInDialog { popup: current, .. }
            | ModalStack::Popup(current) => *current = popup,
            ModalStack::Search(search) => search.popup = popup,
            ModalStack::Normal | ModalStack::Dialog(_) => self.modal = ModalStack::Popup(popup),
        }
        if is_manager {
            self.manager_scroll_offset = 0;
            self.reanchor_manager_scroll(crate::browser_state::ScrollDirection::Down);
        }
    }

    pub(crate) fn dismiss_alert(&mut self) {
        if let ModalStack::Alert { underlying, .. } =
            std::mem::replace(&mut self.modal, ModalStack::Normal)
        {
            self.modal = *underlying;
        }
    }

    pub(crate) fn set_alert(&mut self, message: String) {
        let current = std::mem::replace(&mut self.modal, ModalStack::Normal);
        self.modal = match current {
            ModalStack::Alert { underlying, .. } => ModalStack::Alert {
                message,
                underlying,
            },
            underlying => ModalStack::Alert {
                message,
                underlying: Box::new(underlying),
            },
        };
    }

    pub(crate) fn close_dialog(&mut self) {
        let current = std::mem::replace(&mut self.modal, ModalStack::Normal);
        let (modal, manager_scroll_offset, manager_viewport_height) = match current {
            ModalStack::DialogInPopup { popup, .. } => (
                ModalStack::Popup(popup),
                self.manager_scroll_offset,
                self.manager_viewport_height,
            ),
            ModalStack::PopupInDialog { .. } => (
                current,
                self.manager_scroll_offset,
                self.manager_viewport_height,
            ),
            ModalStack::Alert { underlying, .. } => {
                let mut state = PopupDialogState {
                    modal: *underlying,
                    manager_scroll_offset: self.manager_scroll_offset,
                    manager_viewport_height: self.manager_viewport_height,
                };
                state.close_dialog();
                (
                    state.modal,
                    state.manager_scroll_offset,
                    state.manager_viewport_height,
                )
            }
            _ => (
                ModalStack::Normal,
                self.manager_scroll_offset,
                self.manager_viewport_height,
            ),
        };
        self.modal = modal;
        self.manager_scroll_offset = manager_scroll_offset;
        self.manager_viewport_height = manager_viewport_height;
    }

    pub(crate) fn close_modal(&mut self) {
        let current = std::mem::replace(&mut self.modal, ModalStack::Normal);
        self.modal = match current {
            ModalStack::Alert { underlying, .. } => *underlying,
            ModalStack::PopupInDialog { dialog, .. } => ModalStack::Dialog(dialog),
            ModalStack::PopupInPopup { underlying, .. } => ModalStack::Popup(underlying),
            ModalStack::DialogInPopup { .. }
            | ModalStack::Search(_)
            | ModalStack::Dialog(_)
            | ModalStack::Popup(_)
            | ModalStack::Normal => ModalStack::Normal,
        };
    }

    pub(crate) fn clear(&mut self) {
        self.modal = ModalStack::Normal;
        self.manager_scroll_offset = 0;
    }

    fn reanchor_manager_scroll(&mut self, direction: crate::browser_state::ScrollDirection) {
        let Some(Popup::PlaylistManager { cursor, names }) = self.active_popup_ref() else {
            self.manager_scroll_offset = 0;
            return;
        };
        self.manager_scroll_offset = crate::browser_state::scroll_offset_for_direction(
            self.manager_scroll_offset,
            *cursor,
            names.len(),
            usize::from(self.manager_viewport_height.max(1)),
            crate::browser_state::SCROLL_CONTEXT_ROWS,
            direction,
        );
    }

    fn clamp_manager_scroll(&mut self) {
        let Some(Popup::PlaylistManager { cursor, names }) = self.active_popup_ref() else {
            return;
        };
        self.manager_scroll_offset = crate::browser_state::clamp_scroll_offset(
            self.manager_scroll_offset,
            *cursor,
            names.len(),
            usize::from(self.manager_viewport_height.max(1)),
        );
    }

    /// Rows per manager page: one viewport minus one overlap row.
    fn manager_page_step(&self) -> usize {
        usize::from(self.manager_viewport_height.max(1))
            .saturating_sub(1)
            .max(1)
    }

    pub(crate) fn dialog_mode_ref(&self) -> Option<&DialogMode> {
        self.dialog_ref().map(|dialog| &dialog.mode)
    }

    #[cfg(test)]
    pub(crate) fn dialog_mode_value(&self) -> Option<DialogMode> {
        self.dialog_mode_ref().cloned()
    }

    pub(crate) fn dialog_mode_mut(&mut self) -> Option<&mut DialogMode> {
        self.dialog_mut().map(|dialog| &mut dialog.mode)
    }

    pub(crate) fn dialog_input_ref(&self) -> Option<&str> {
        self.dialog_ref().map(|dialog| dialog.input.as_str())
    }

    #[cfg(test)]
    pub(crate) fn dialog_input_value(&self) -> &str {
        self.dialog_input_ref().unwrap_or_default()
    }

    pub(crate) fn dialog_input_mut(&mut self) -> Option<&mut String> {
        self.dialog_mut().map(|dialog| &mut dialog.input)
    }

    pub(crate) fn set_dialog_input(&mut self, input: String) {
        if let Some(dialog) = self.dialog_mut() {
            dialog.cursor = input.chars().count();
            dialog.input = input;
        }
    }

    pub(crate) fn dialog_cursor(&self) -> Option<usize> {
        self.dialog_ref().map(|dialog| dialog.cursor)
    }

    #[cfg(test)]
    pub(crate) fn dialog_cursor_value(&self) -> usize {
        self.dialog_cursor().unwrap_or(0)
    }

    pub(crate) fn set_dialog_cursor(&mut self, cursor: usize) {
        if let Some(dialog) = self.dialog_mut() {
            dialog.cursor = cursor;
        }
    }

    pub(crate) fn dialog_extension(&self) -> Option<&str> {
        self.dialog_ref()
            .and_then(|dialog| dialog.extension.as_deref())
    }

    pub(crate) fn set_dialog_error(&mut self, error: Option<String>) {
        if let Some(dialog) = self.dialog_mut() {
            dialog.error = error;
        }
    }

    #[cfg(test)]
    pub(crate) fn dialog_error(&self) -> Option<&str> {
        self.dialog_ref().and_then(|dialog| dialog.error.as_deref())
    }

    #[cfg(test)]
    pub(crate) fn dialog_extension_value(&self) -> Option<&str> {
        self.dialog_extension()
    }
}

impl ModalStack {
    fn active_popup_ref(&self) -> Option<&Popup> {
        match self {
            Self::Alert { underlying, .. } => underlying.active_popup_ref(),
            Self::Search(search) => Some(&search.popup),
            Self::Popup(popup)
            | Self::DialogInPopup { popup, .. }
            | Self::PopupInDialog { popup, .. }
            | Self::PopupInPopup { popup, .. } => Some(popup),
            Self::Normal | Self::Dialog(_) => None,
        }
    }

    fn active_popup_mut(&mut self) -> Option<&mut Popup> {
        match self {
            Self::Alert { underlying, .. } => underlying.active_popup_mut(),
            Self::Search(search) => Some(&mut search.popup),
            Self::Popup(popup)
            | Self::DialogInPopup { popup, .. }
            | Self::PopupInDialog { popup, .. }
            | Self::PopupInPopup { popup, .. } => Some(popup),
            Self::Normal | Self::Dialog(_) => None,
        }
    }

    fn dialog_ref(&self) -> Option<&DialogState> {
        match self {
            Self::Alert { underlying, .. } => underlying.dialog_ref(),
            Self::Dialog(dialog)
            | Self::DialogInPopup { dialog, .. }
            | Self::PopupInDialog { dialog, .. } => Some(dialog),
            _ => None,
        }
    }

    fn dialog_mut(&mut self) -> Option<&mut DialogState> {
        match self {
            Self::Alert { underlying, .. } => underlying.dialog_mut(),
            Self::Dialog(dialog)
            | Self::DialogInPopup { dialog, .. }
            | Self::PopupInDialog { dialog, .. } => Some(dialog),
            _ => None,
        }
    }

    fn search_ref(&self) -> Option<&SearchState> {
        match self {
            Self::Alert { underlying, .. } => underlying.search_ref(),
            Self::Search(search) => Some(search),
            _ => None,
        }
    }

    fn search_mut(&mut self) -> Option<&mut SearchState> {
        match self {
            Self::Alert { underlying, .. } => underlying.search_mut(),
            Self::Search(search) => Some(search),
            _ => None,
        }
    }

    fn alert_message(&self) -> Option<&str> {
        match self {
            Self::Alert { message, .. } => Some(message),
            _ => None,
        }
    }
}

impl Default for BrowserCapability {
    fn default() -> Self {
        Self {
            state: BrowserState::new(PathBuf::from(".")),
            show_hidden: false,
            selected_entries: HashSet::new(),
            directory_request_id: 0,
        }
    }
}

// Intentional local capability projection: callers working with the browser
// capability can use its `BrowserState` navigation operations while the
// wrapper retains selection and request-identity ownership beside that state.
impl std::ops::Deref for BrowserCapability {
    type Target = BrowserState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl std::ops::DerefMut for BrowserCapability {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

impl BrowserCapability {
    /// Start a new listing request and return its stale-result identity.
    pub fn begin_directory_request(&mut self) -> u64 {
        self.directory_request_id = self.directory_request_id.wrapping_add(1);
        self.directory_request_id
    }

    /// Whether a completion belongs to the most recent listing request.
    pub fn accepts_directory_request(&self, request_id: u64) -> bool {
        self.directory_request_id == request_id
    }

    /// Current listing request identity, primarily useful to tests.
    pub fn directory_request_id(&self) -> u64 {
        self.directory_request_id
    }
}

/// Root state of the running application.
#[derive(Debug)]
pub struct AppState {
    /// Panel currently holding keyboard focus.
    pub(crate) active_panel: Panel,
    /// Set once the loop should stop and restore the terminal.
    pub(crate) should_quit: bool,
    /// Whether quitting asks for confirmation first, mirroring the Termusic
    /// default until the config file takes it over in a later phase.
    pub(crate) confirm_quit: bool,
    /// How the Now Playing band labels the current track (display config).
    pub(crate) now_playing_display: crate::config::SortTracksConfig,
    /// How the playlist panel labels its rows and which criteria the explicit
    /// reorder action will use.
    pub(crate) playlist_columns: crate::config::PlaylistColumnsConfig,
    /// Popup and dialog capability state.
    pub(crate) popup_dialog: PopupDialogState,
    /// Transient user facing messages rendered by later phases.
    pub(crate) notifications: Vec<String>,
    /// Lyrics panel display state; the resolution chain itself lives in
    /// [`crate::lyrics::LyricsService`] inside [`crate::runtime::AppServices`].
    pub(crate) lyrics: LyricsState,
    /// File browser capability state.
    pub(crate) browser: BrowserCapability,
    /// Playlist panel visible row count refreshed by the explicit frame tick.
    ///
    /// The playlist queue is a domain model and must not carry screen
    /// chrome, so its scroll window lives here beside the panel, mirroring
    /// how the browser owns its own `scroll_offset` inside `BrowserState`.
    pub(crate) playlist_viewport_height: u16,
    /// First visible playlist row index, updated directionally on every
    /// queue navigation command so the cursor climbs toward the edge it is
    /// approaching while content remains in that direction.
    pub(crate) playlist_scroll_offset: usize,
    /// Playback queue in user order with its selection cursor.
    pub(crate) playlist: Playlist,
    /// Name of the active playlist when it has been saved or loaded, or
    /// `None` for an anonymous queue that is never autosaved.
    pub(crate) active_playlist_name: Option<String>,
    /// State owned by asynchronous operations and their UI gates.
    pub(crate) async_ops: AsyncOperationState,
    /// Owner playback order model driving repeat and shuffle.
    pub(crate) playback_mode: PlaybackMode,
    /// Shuffle pools and the played history trail consumed by the
    /// selection engine.
    ///
    /// Kept beside the queue because every structural queue edit must
    /// rebuild it, see the mutation helpers below.
    pub(crate) navigation: NavigationState,
    /// Live playback view fed by the audio worker snapshots and updated
    /// optimistically by commands for instant feedback.
    pub(crate) playback: PlaybackState,
    /// Album artwork capability state for the now playing band.
    pub(crate) artwork: ArtworkCapability,
    /// Persistence and resume identity shared by startup, playback, and exit.
    pub(crate) persistence: PersistenceState,
    /// Frame-local animation state shared by every spinner surface.
    pub(crate) frame: FrameState,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            active_panel: Panel::default(),
            should_quit: false,
            confirm_quit: true,
            now_playing_display: crate::config::SortTracksConfig::default(),
            playlist_columns: crate::config::PlaylistColumnsConfig::default(),
            popup_dialog: PopupDialogState::default(),
            notifications: Vec::new(),
            lyrics: LyricsState::default(),
            browser: BrowserCapability::default(),
            playlist_viewport_height: crate::browser_state::DEFAULT_VIEWPORT_HEIGHT,
            playlist_scroll_offset: 0,
            playlist: Playlist::new(),
            active_playlist_name: None,
            async_ops: AsyncOperationState::default(),
            playback_mode: PlaybackMode::default(),
            navigation: NavigationState::new(),
            playback: PlaybackState::default(),
            artwork: ArtworkCapability::default(),
            persistence: PersistenceState::default(),
            frame: FrameState::default(),
        }
    }
}

impl AppState {
    /// Begin stream resolution and mark the Add Stream dialog as loading.
    pub(crate) fn begin_stream_resolution(&mut self) -> (u64, StreamResolutionCancellation) {
        let result = self.async_ops.begin_stream_resolution();
        if let Some(DialogMode::AddStream { loading, .. }) = self.popup_dialog.dialog_mode_mut() {
            *loading = true;
        }
        result
    }

    /// Accept a current stream-resolution result and clear all related UI
    /// loading state. Stale results do not change any state.
    pub(crate) fn accept_stream_resolution(&mut self, request_id: u64) -> bool {
        if !self.async_ops.accept_stream_resolution(request_id) {
            return false;
        }
        if let Some(DialogMode::AddStream { loading, .. }) = self.popup_dialog.dialog_mode_mut() {
            *loading = false;
        }
        true
    }

    /// Cancel stream resolution, its worker token, and the Add Stream spinner.
    pub(crate) fn cancel_stream_resolution(&mut self) {
        self.async_ops.cancel_stream_resolution();
        if let Some(DialogMode::AddStream { loading, .. }) = self.popup_dialog.dialog_mode_mut() {
            *loading = false;
        }
    }

    /// Apply one explicit frame tick before rendering.
    ///
    /// Rendering only consumes the resulting immutable state. Geometry,
    /// spinner animation, and lyrics follow are all updated here so repeated
    /// draws cannot advance or re-anchor the application.
    pub fn tick_frame(&mut self, metrics: FrameMetrics, elapsed: Duration) {
        self.frame.spinner.tick(elapsed);
        self.browser
            .set_viewport_height(metrics.browser_viewport_height);
        self.playlist_viewport_height = metrics.playlist_viewport_height.max(1);
        self.clamp_playlist_scroll();
        self.popup_dialog
            .set_manager_viewport_height(metrics.playlist_manager_viewport_height);
        self.lyrics.viewport_height = metrics.lyrics_viewport_height.max(1);
        self.lyrics.ensure_layout_cache(metrics.lyrics_layout_width);

        let elapsed_ms = self.playback.elapsed.as_millis() as i64;
        let active = self.lyrics.document.as_ref().and_then(|document| {
            self.lyrics
                .layout_cache
                .as_ref()
                .map(|cache| {
                    active_line_index_from_starts(
                        &cache.timed_starts,
                        document.lines.len(),
                        elapsed_ms,
                    )
                })
                .flatten()
        });
        let Some(active) = active else {
            return;
        };

        let previous = self.lyrics.active_line;
        let width = metrics.lyrics_layout_width;
        let width_changed = self.lyrics.layout_width != Some(width);
        let line_changed = previous != Some(active);
        if !width_changed && !line_changed {
            return;
        }

        let direction = match previous {
            Some(previous) if previous > active => FollowDirection::Up,
            _ => FollowDirection::Down,
        };
        let (active_row, total_rows) = self
            .lyrics
            .layout_cache_for(width)
            .map(|cache| {
                (
                    cache.layout.row_starts.get(active).copied().unwrap_or(0),
                    cache.layout.total_rows,
                )
            })
            .unwrap_or((0, 0));

        self.lyrics.scroll = follow_scroll(
            active_row,
            usize::from(self.lyrics.viewport_height),
            total_rows,
            direction,
        );
        self.lyrics.active_line = Some(active);
        self.lyrics.layout_width = Some(width);
    }

    /// Replace the browser listing with entries loaded by an effect.
    ///
    /// Clearing on every directory change is a documented decision, see the
    /// field docs above. Scrolling never reaches this path so paging keeps
    /// marks intact.
    pub fn change_browser_dir(&mut self, dir: PathBuf, entries: Vec<FileEntry>) {
        tracing::debug!(dir = %dir.display(), entries = entries.len(), "browser directory changed");
        self.browser.replace_contents(dir, entries);
        self.browser.selected_entries.clear();
    }

    /// Append direct paths to the playlist queue, preserving order and
    /// skipping duplicates.
    ///
    /// Relative direct-add inputs are resolved against the process working
    /// directory at this boundary. The queue therefore has a stable playback
    /// path before a playlist is saved, while M3U loading independently uses
    /// the playlist directory as its relative-entry base.
    /// A path already queued (or repeated within the same batch) is not added
    /// again: the queue keeps a single identity per file, which avoids the
    /// "first occurrence wins" ambiguities everywhere the app resolves a track
    /// by path (metadata arrival, seek re-maps, crossfade hand-off). Returns
    /// the indices actually appended plus the number skipped so callers can
    /// target the new entries and tell the user what happened.
    pub fn extend_playlist(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> PlaylistAppendResult {
        let before = self.playlist.len();
        let mut added_indices = Vec::new();
        let mut skipped = 0usize;
        let present: HashSet<TrackLocation> = self
            .playlist
            .tracks()
            .iter()
            .map(Track::track_location)
            .collect();
        let mut batch: HashSet<TrackLocation> = HashSet::new();
        for path in paths {
            let path = crate::track::resolve_process_relative_path(path);
            let track = Track::local(path);
            let location = track.track_location();
            if present.contains(&location) || !batch.insert(location) {
                skipped += 1;
                continue;
            }
            self.playlist.extend(std::iter::once(track));
            added_indices.push(self.playlist.len() - 1);
        }
        let added = added_indices.len();
        let tail_started = before;
        let tail_ended = self.playlist.len();
        tracing::debug!(
            added,
            skipped,
            queue_len = self.playlist.len(),
            "tracks appended to queue"
        );
        // Tail appends never move existing indices, so the fresh entries
        // can join the shuffle pool incrementally without a full rebuild
        self.navigation.note_appended(tail_started, tail_ended);
        PlaylistAppendResult {
            added_indices,
            skipped,
        }
    }

    /// Append already-built [`Track`] entries to the playlist queue,
    /// deduplicating by [`TrackLocation`].
    ///
    /// Used by the Add Stream flow, where the caller has resolved the URL
    /// into a fully-formed [`Track`] (with metadata attached). The same
    /// "first occurrence wins" rule from [`Self::extend_playlist`] applies,
    /// keyed on the typed identity so a stream URL is distinct from a local
    /// path even when their printable forms match.
    pub fn extend_playlist_tracks(
        &mut self,
        tracks: impl IntoIterator<Item = Track>,
    ) -> PlaylistAppendResult {
        let before = self.playlist.len();
        let mut added_indices = Vec::new();
        let mut skipped = 0usize;
        let mut present: std::collections::HashSet<TrackLocation> = self
            .playlist
            .tracks()
            .iter()
            .map(Track::track_location)
            .collect();
        let mut batch: std::collections::HashSet<TrackLocation> = std::collections::HashSet::new();
        for track in tracks {
            let location = track.track_location();
            if present.contains(&location) || !batch.insert(location.clone()) {
                skipped += 1;
                continue;
            }
            present.insert(location);
            self.playlist.extend(std::iter::once(track));
            added_indices.push(self.playlist.len() - 1);
        }
        let added = added_indices.len();
        let tail_started = before;
        let tail_ended = self.playlist.len();
        tracing::debug!(
            added,
            skipped,
            queue_len = self.playlist.len(),
            "stream tracks appended to queue"
        );
        self.navigation.note_appended(tail_started, tail_ended);
        PlaylistAppendResult {
            added_indices,
            skipped,
        }
    }

    /// Remove the queue entry under the playlist cursor.
    ///
    /// Returns a mutation result so the application can emit a stop effect
    /// when the currently playing entry is removed. Only the entry leaves the
    /// queue, the backing file is never touched. Navigation bookkeeping is
    /// rebuilt because every later index shifted, which is cheaper to reason
    /// about than piecemeal remapping.
    pub fn delete_queue_entry_at_cursor(&mut self) -> QueueMutationResult {
        if self.playlist.is_empty() {
            return QueueMutationResult::Unchanged;
        }
        let playing_location = self.playing_track_location();
        let cursor = self.playlist.cursor();
        let removed_location = self.playlist.current().map(Track::track_location);
        if self.playlist.remove_selected(&[cursor]) == 0 {
            return QueueMutationResult::Unchanged;
        }
        tracing::debug!(
            cursor,
            queue_len = self.playlist.len(),
            "queue entry deleted"
        );
        self.reanchor_after_queue_mutation(playing_location.as_ref());

        let playing_removed = playing_location
            .as_ref()
            .zip(removed_location.as_ref())
            .is_some_and(|(playing, removed)| playing == removed);
        if playing_removed {
            self.clear_deleted_playback();
            QueueMutationResult::PlayingTrackRemoved
        } else {
            QueueMutationResult::Applied
        }
    }

    /// Swap the selected queue entry with its upper neighbour.
    pub fn swap_queue_entry_up(&mut self) -> QueueMutationResult {
        self.mutate_queue_order(|playlist, cursor| playlist.swap_up(cursor))
    }

    /// Swap the selected queue entry with its lower neighbour.
    pub fn swap_queue_entry_down(&mut self) -> QueueMutationResult {
        self.mutate_queue_order(|playlist, cursor| playlist.swap_down(cursor))
    }

    /// Drop every queued entry without touching any file.
    ///
    /// The running track keeps playing until its natural end, after which
    /// the completion path sees an empty queue and stops gracefully.
    pub fn clear_queue(&mut self) {
        let removed = self.playlist.len();
        self.playlist.clear();
        self.navigation.reset();
        tracing::debug!(removed, "queue cleared");
    }

    /// Run one order changing operation and rebuild navigation on success.
    fn mutate_queue_order(
        &mut self,
        swap: impl FnOnce(&mut Playlist, usize) -> bool,
    ) -> QueueMutationResult {
        if self.playlist.is_empty() {
            return QueueMutationResult::Unchanged;
        }
        let playing_location = self.playing_track_location();
        let cursor = self.playlist.cursor();
        if !swap(&mut self.playlist, cursor) {
            return QueueMutationResult::Unchanged;
        }
        self.reanchor_after_queue_mutation(playing_location.as_ref());
        QueueMutationResult::Applied
    }

    /// Capture the playing identity before a queue mutation changes indices.
    ///
    /// Persistence is the authoritative typed identity. The indexed queue
    /// entry is only a fallback for older or test state that has not populated
    /// persistence yet.
    fn playing_track_location(&self) -> Option<TrackLocation> {
        self.persistence.last_track.clone().or_else(|| {
            self.playback
                .track_index
                .and_then(|index| self.playlist.tracks().get(index))
                .map(Track::track_location)
        })
    }

    /// Re-anchor playback by typed identity, then rebuild index-based
    /// navigation with the corrected index.
    fn reanchor_after_queue_mutation(&mut self, playing_location: Option<&TrackLocation>) {
        let new_index = playing_location.and_then(|location| {
            self.playlist
                .tracks()
                .iter()
                .position(|track| track.track_location() == *location)
        });
        self.playback.track_index = new_index;
        self.navigation
            .rebuild_after_mutation(self.playlist.len(), new_index);
    }

    /// Clear all state tied to a queue entry that was just deleted.
    fn clear_deleted_playback(&mut self) {
        self.playback.status = crate::audio::PlayStatus::Stopped;
        self.playback.track_index = None;
        self.playback.elapsed = Duration::ZERO;
        self.playback.duration = None;
        self.persistence.last_track = None;
        self.persistence.last_track_position_ms = 0;
        self.async_ops.cancel_stream_activity();
        self.artwork.loading = false;
        self.lyrics.loading = false;
        self.lyrics.track_index = None;
    }

    /// Move the playlist selection one row up and re-anchor its scroll.
    pub fn move_playlist_cursor_up(&mut self) {
        self.playlist.move_cursor_up();
        self.reanchor_playlist_scroll(crate::browser_state::ScrollDirection::Up);
    }

    /// Move the playlist selection one row down and re-anchor its scroll.
    pub fn move_playlist_cursor_down(&mut self) {
        self.playlist.move_cursor_down();
        self.reanchor_playlist_scroll(crate::browser_state::ScrollDirection::Down);
    }

    /// Jump the playlist selection to the head and re-anchor its scroll.
    pub fn move_playlist_to_top(&mut self) {
        self.playlist.goto_top();
        self.reanchor_playlist_scroll(crate::browser_state::ScrollDirection::Up);
    }

    /// Jump the playlist selection to the tail and re-anchor its scroll.
    pub fn move_playlist_to_bottom(&mut self) {
        self.playlist.goto_bottom();
        self.reanchor_playlist_scroll(crate::browser_state::ScrollDirection::Down);
    }

    /// Page the playlist selection up and re-anchor its scroll.
    pub fn page_playlist_up(&mut self) {
        self.playlist.page_up(self.playlist_page_step());
        self.reanchor_playlist_scroll(crate::browser_state::ScrollDirection::Up);
    }

    /// Page the playlist selection down and re-anchor its scroll.
    pub fn page_playlist_down(&mut self) {
        self.playlist.page_down(self.playlist_page_step());
        self.reanchor_playlist_scroll(crate::browser_state::ScrollDirection::Down);
    }

    /// Rows per playlist page: one viewport minus one overlap row.
    fn playlist_page_step(&self) -> usize {
        self.playlist_viewport_height.saturating_sub(1).max(1) as usize
    }

    /// Re-anchor the playlist scroll window so the selection cursor stays
    /// visible with [`SCROLL_CONTEXT_ROWS`] of breathing room toward the edge
    /// it is moving toward.
    pub fn reanchor_playlist_scroll(&mut self, direction: crate::browser_state::ScrollDirection) {
        self.playlist_scroll_offset = crate::browser_state::scroll_offset_for_direction(
            self.playlist_scroll_offset,
            self.playlist.cursor(),
            self.playlist.len(),
            usize::from(self.playlist_viewport_height.max(1)),
            crate::browser_state::SCROLL_CONTEXT_ROWS,
            direction,
        );
    }

    /// Normalize the playlist window after its viewport or cursor geometry
    /// changes without inventing a new directional anchor.
    fn clamp_playlist_scroll(&mut self) {
        self.playlist_scroll_offset = crate::browser_state::clamp_scroll_offset(
            self.playlist_scroll_offset,
            self.playlist.cursor(),
            self.playlist.len(),
            usize::from(self.playlist_viewport_height.max(1)),
        );
    }

    /// Advance the global spinner by `elapsed` wall-clock time and update the
    /// last-tick anchor.
    ///
    /// The main loop calls this once per frame with the time delta between
    /// draws so every visible spinner advances in lock-step from the same
    /// phase. `last_tick` is initialised to `Some(now)` on the first call so
    /// a long pause between frames cannot advance the spinner by minutes.
    pub fn tick_spinner(&mut self, elapsed: Duration) {
        self.frame.spinner.tick(elapsed);
    }

    /// Whether any [`crate::app::Effect`] is currently in flight.
    ///
    /// The dispatch helper increments the counter for every spawned task and
    /// the RAII guard decrements it on drop, so `load(Ordering::Relaxed) > 0`
    /// is exactly the set of moments when at least one background task has
    /// not yet reported back. Relaxed ordering is fine: the value is only
    /// used for visual feedback, never for control flow.
    pub fn has_pending_effects(&self) -> bool {
        self.async_ops.pending_effects.load(Ordering::Relaxed) > 0
    }

    /// Compose the standard `[spinner frame] [space] "Loading"` line used by
    /// every in-flight surface.
    ///
    /// Returns the line as plain `Span` slices so the caller can place it
    /// inside a `Paragraph` (status bar), a `Line` (Add Stream popup) or a
    /// progress-bar replacement (Now Playing). The frame character comes
    /// straight from `ratatui-cheese`'s `frame_str`, which advances
    /// independently for every call site; tying all of them to
    /// [`Self::frame`] keeps them visually synchronised.
    pub fn spinner_loading_line(&self) -> Vec<Span<'static>> {
        let frame = self.frame.spinner.frame_str().to_string();
        vec![
            Span::styled(frame, Style::new()),
            Span::styled(" ", Style::new()),
            Span::styled("Loading", Style::new()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_dir;
    use std::fs;

    #[test]
    fn change_browser_dir_loads_sorted_entries_and_clears_marks() {
        let root = unique_temp_dir("state-dir");
        fs::create_dir_all(root.join("Album")).expect("dir");
        fs::write(root.join("song.mp3"), "").expect("file");

        let mut state = AppState::default();
        state
            .browser
            .selected_entries
            .insert(PathBuf::from("/stale/mark.mp3"));

        state.change_browser_dir(
            root.to_path_buf(),
            crate::filesystem::read_sorted_entries(&root, false).expect("listing"),
        );

        assert_eq!(state.browser.current_dir, root.path());
        assert_eq!(state.browser.entries.len(), 2);
        assert!(state.browser.selected_entries.is_empty());
    }

    #[test]
    fn change_browser_dir_is_state_only_and_accepts_preloaded_entries() {
        let mut state = AppState::default();
        let entries = vec![crate::filesystem::FileEntry::new(
            "preloaded.mp3",
            PathBuf::from("/not-read/preloaded.mp3"),
            crate::filesystem::EntryKind::File,
        )];

        state.change_browser_dir(PathBuf::from("/not-read"), entries.clone());

        assert_eq!(state.browser.current_dir, PathBuf::from("/not-read"));
        assert_eq!(state.browser.entries, entries);
        assert_eq!(state.browser.cursor(), 0);
        assert_eq!(state.browser.scroll_offset(), 0);
    }

    #[test]
    fn browser_capability_request_identity_accepts_only_the_latest_result() {
        let mut browser = BrowserCapability::default();
        let first = browser.begin_directory_request();
        let second = browser.begin_directory_request();

        assert!(!browser.accepts_directory_request(first));
        assert!(browser.accepts_directory_request(second));
        assert_eq!(browser.directory_request_id(), second);
    }

    #[test]
    fn request_slot_preserves_ids_supersession_acceptance_and_cancellation() {
        let mut slot = RequestSlot::<PlaylistRequest>::default();
        let first = slot.begin();
        assert_eq!(first, 1);
        assert_eq!(slot.active(), Some(first));

        let second = slot.begin();
        assert_eq!(second, 2);
        assert!(!slot.accept(first));
        assert_eq!(slot.active(), Some(second));
        assert!(slot.accept(second));
        assert_eq!(slot.active(), None);

        let cancelled = slot.begin();
        slot.cancel();
        assert_eq!(slot.active(), None);
        assert!(!slot.accept(cancelled));
    }

    #[test]
    fn request_slot_try_begin_refuses_overlap_without_advancing() {
        let mut slot = RequestSlot::<FileRenameRequest>::default();
        let first = slot.try_begin().expect("first request starts");

        assert_eq!(slot.try_begin(), None);
        assert_eq!(slot.active(), Some(first));
        assert!(slot.accept(first));
        assert_eq!(slot.try_begin(), Some(first + 1));
    }

    #[test]
    fn async_request_slots_accept_current_results_and_cancel_pending_work() {
        let mut operations = AsyncOperationState::default();

        let playlist_request = operations
            .playlist_request
            .try_begin()
            .expect("playlist request starts");
        assert_eq!(operations.playlist_request.active(), Some(playlist_request));
        assert!(operations.playlist_request.try_begin().is_none());
        assert!(!operations.playlist_request.accept(playlist_request + 1));
        assert_eq!(operations.playlist_request.active(), Some(playlist_request));
        assert!(operations.playlist_request.accept(playlist_request));
        assert_eq!(operations.playlist_request.active(), None);
        let cancelled_playlist_request = operations
            .playlist_request
            .try_begin()
            .expect("second playlist request starts after acceptance");
        assert_eq!(
            operations.playlist_request.active(),
            Some(cancelled_playlist_request)
        );
        operations.playlist_request.cancel();
        assert_eq!(operations.playlist_request.active(), None);

        let rename_request = operations.file_rename_request.begin();
        assert_eq!(
            operations.file_rename_request.active(),
            Some(rename_request)
        );
        assert!(!operations.file_rename_request.accept(rename_request + 1));
        assert_eq!(
            operations.file_rename_request.active(),
            Some(rename_request)
        );
        assert!(operations.file_rename_request.accept(rename_request));
        assert_eq!(operations.file_rename_request.active(), None);
        let cancelled_rename_request = operations.file_rename_request.begin();
        operations.file_rename_request.cancel();
        assert_eq!(operations.file_rename_request.active(), None);
        assert!(
            !operations
                .file_rename_request
                .accept(cancelled_rename_request)
        );

        let browser_request = operations.browser_validation_request.begin();
        assert_eq!(
            operations.browser_validation_request.active(),
            Some(browser_request)
        );
        assert!(
            !operations
                .browser_validation_request
                .accept(browser_request + 1)
        );
        assert_eq!(
            operations.browser_validation_request.active(),
            Some(browser_request)
        );
        assert!(
            operations
                .browser_validation_request
                .accept(browser_request)
        );
        assert_eq!(operations.browser_validation_request.active(), None);
        let cancelled_browser_request = operations.browser_validation_request.begin();
        operations.browser_validation_request.cancel();
        assert_eq!(operations.browser_validation_request.active(), None);
        assert!(
            !operations
                .browser_validation_request
                .accept(cancelled_browser_request)
        );
    }

    #[test]
    fn settings_visit_rejects_old_theme_results_and_cancels_current_work() {
        let mut operations = AsyncOperationState::default();
        let first_visit = operations.begin_settings_visit();
        let (first_theme, theme_visit) = operations
            .begin_theme_request()
            .expect("first visit owns theme request");
        let (first_save, save_visit) = operations
            .begin_theme_save_request()
            .expect("first visit owns theme save");
        assert_eq!(theme_visit, first_visit);
        assert_eq!(save_visit, first_visit);

        let second_visit = operations.begin_settings_visit();
        assert_ne!(first_visit, second_visit);
        assert!(!operations.accept_theme_request(first_theme, first_visit));
        assert!(!operations.accept_theme_save_request(first_save, first_visit));
        assert_eq!(operations.theme_request.active(), None);
        assert_eq!(operations.theme_save_request.active(), None);

        let (second_theme, visit) = operations
            .begin_theme_request()
            .expect("second visit owns theme request");
        assert_eq!(visit, second_visit);
        assert!(operations.accept_theme_request(second_theme, second_visit));

        let (second_save, visit) = operations
            .begin_theme_save_request()
            .expect("second visit owns theme save");
        assert_eq!(visit, second_visit);
        operations.cancel_theme_requests();
        assert_eq!(operations.theme_save_request.active(), None);
        assert!(!operations.accept_theme_save_request(second_save, second_visit));
    }

    #[test]
    fn theme_requests_require_an_active_settings_visit() {
        let mut operations = AsyncOperationState::default();

        assert!(operations.begin_theme_request().is_none());
        assert!(operations.begin_theme_save_request().is_none());

        let visit = operations.begin_settings_visit();
        let (_, theme_visit) = operations
            .begin_theme_request()
            .expect("active visit owns theme request");
        assert_eq!(theme_visit, visit);
    }

    #[test]
    fn stream_resolution_transitions_gate_results_and_cancel_the_worker() {
        let mut state = AppState::default();
        state.popup_dialog.open_dialog(
            DialogMode::AddStream {
                error: None,
                loading: false,
            },
            String::new(),
            None,
        );

        let (first_request, first_cancellation) = state.begin_stream_resolution();
        assert!(matches!(
            state.async_ops.stream_activity(),
            Some(StreamActivity::Resolving {
                request_id,
                cancellation: _
            }) if *request_id == first_request
        ));
        assert!(matches!(
            state.popup_dialog.dialog_mode_ref(),
            Some(DialogMode::AddStream { loading: true, .. })
        ));

        let (second_request, second_cancellation) = state.begin_stream_resolution();
        assert!(first_cancellation.is_cancelled());
        assert!(!second_cancellation.is_cancelled());
        assert!(!state.accept_stream_resolution(first_request));
        assert!(matches!(
            state.async_ops.stream_activity(),
            Some(StreamActivity::Resolving {
                request_id,
                cancellation: _
            }) if *request_id == second_request
        ));
        assert!(state.accept_stream_resolution(second_request));
        assert!(state.async_ops.stream_activity().is_none());
        assert!(matches!(
            state.popup_dialog.dialog_mode_ref(),
            Some(DialogMode::AddStream { loading: false, .. })
        ));

        let (_, cancellation) = state.begin_stream_resolution();
        state.cancel_stream_resolution();
        assert!(cancellation.is_cancelled());
        assert!(state.async_ops.stream_activity().is_none());
        assert!(matches!(
            state.popup_dialog.dialog_mode_ref(),
            Some(DialogMode::AddStream { loading: false, .. })
        ));
    }

    #[test]
    fn stream_acquisition_acceptance_preserves_source_and_generation_gates() {
        let mut operations = AsyncOperationState::default();
        let source = TrackLocation::url(url::Url::parse("https://example.com/live").unwrap());
        operations.begin_stream_acquisition(source.clone(), 7);

        assert!(!operations.accept_stream_acquisition(&source, Some(8)));
        assert!(matches!(
            operations.stream_activity(),
            Some(StreamActivity::Acquiring {
                source: active_source,
                generation: 7
            }) if active_source == &source
        ));
        assert!(operations.accept_stream_acquisition(&source, Some(7)));
        assert!(operations.stream_activity().is_none());
    }

    #[test]
    fn stream_acquisition_replaces_resolution_and_cancels_its_worker() {
        let mut operations = AsyncOperationState::default();
        let (_, cancellation) = operations.begin_stream_resolution();
        let source = TrackLocation::url(url::Url::parse("https://example.com/live").unwrap());

        operations.begin_stream_acquisition(source.clone(), 9);

        assert!(cancellation.is_cancelled());
        assert!(matches!(
            operations.stream_activity(),
            Some(StreamActivity::Acquiring {
                source: active_source,
                generation: 9
            }) if active_source == &source
        ));
    }

    #[test]
    fn browser_hides_dotfiles_unless_show_hidden_is_enabled() {
        let root = unique_temp_dir("state-hidden");
        fs::create_dir_all(root.join(".config")).expect("hidden dir");
        fs::write(root.join(".hidden.mp3"), "").expect("hidden file");
        fs::write(root.join("visible.mp3"), "").expect("visible file");

        let mut state = AppState::default();
        state.change_browser_dir(
            root.to_path_buf(),
            crate::filesystem::read_sorted_entries(&root, false).expect("listing"),
        );
        assert_eq!(
            state
                .browser
                .entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["visible.mp3"],
            "dotfiles must be hidden by default"
        );

        state.browser.show_hidden = true;
        state.change_browser_dir(
            root.to_path_buf(),
            crate::filesystem::read_sorted_entries(&root, true).expect("listing"),
        );
        let names: Vec<&str> = state
            .browser
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec![".config", ".hidden.mp3", "visible.mp3"]);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn from_state_seeds_now_playing_display_from_runtime_state() {
        let theme_dir = unique_temp_dir("from-state-themes");
        // The config file persists artist only, but the running session made
        // the band show track number + title: the draft must mirror the band.
        let mut config = crate::config::AppConfig::default();
        config.general.now_playing_display = crate::config::SortTracksConfig {
            sort_by: crate::config::SortBy::Metadata,
            metadata_artist: true,
            ..Default::default()
        };
        let mut state = AppState::default();
        state.now_playing_display = crate::config::SortTracksConfig {
            sort_by: crate::config::SortBy::Metadata,
            metadata_track_number: true,
            metadata_title: true,
            ..Default::default()
        };

        let draft = SettingsDraft::from_state(&state, &config, &theme_dir);

        assert_eq!(
            draft.now_playing_display, state.now_playing_display,
            "the draft must mirror the runtime state the Now Playing band uses"
        );
        assert_eq!(
            draft.now_playing_display_initial, state.now_playing_display,
            "the change-detection baseline must also come from state"
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn from_state_seeds_playlist_columns_from_config_alone() {
        let theme_dir = unique_temp_dir("from-state-sort-themes");
        // The config is the single source of truth for Playlist columns:
        // unlike Now Playing display (seeded from state), the draft must keep
        // whatever the config holds even when the runtime mirror disagrees.
        let mut config = crate::config::AppConfig::default();
        config.general.playlist_columns = crate::config::PlaylistColumnsConfig {
            display_by: crate::config::SortBy::Metadata,
            metadata_artist: true,
            ..Default::default()
        };
        let mut state = AppState::default();
        state.playlist_columns = crate::config::PlaylistColumnsConfig {
            display_by: crate::config::SortBy::Metadata,
            metadata_album: true,
            ..Default::default()
        };

        let draft = SettingsDraft::from_state(&state, &config, &theme_dir);

        assert_eq!(
            draft.playlist_columns, config.general.playlist_columns,
            "the draft must seed the Sort Tracks config from the config file"
        );
        assert_eq!(
            draft.playlist_columns_initial, config.general.playlist_columns,
            "the change-detection baseline must come from config too"
        );
    }

    #[test]
    fn settings_field_selections_are_typed_without_out_of_range_positions() {
        let draft = SettingsDraft::default();
        assert_eq!(
            draft.appearance_color_field(),
            Some(crate::ui::theme::ThemeColorField::Background)
        );
        assert_eq!(
            draft.key_settings_row(),
            Some(crate::config::KeySettingsRow::Quit)
        );
        assert_eq!(
            draft.active_field(SettingsTab::Playback),
            Some(SettingsField::PlaybackRemoteLyrics)
        );
    }

    #[test]
    fn appearance_cursor_skips_disabled_metadata_and_playlist_title_rows() {
        let filename_fields = [
            AppearanceDisplayField::Border(crate::config::BorderType::Plain),
            AppearanceDisplayField::Border(crate::config::BorderType::Rounded),
            AppearanceDisplayField::Border(crate::config::BorderType::Double),
            AppearanceDisplayField::Border(crate::config::BorderType::Thick),
            AppearanceDisplayField::NowPlayingSort(crate::config::SortBy::Filename),
            AppearanceDisplayField::NowPlayingSort(crate::config::SortBy::Metadata),
            AppearanceDisplayField::PlaylistSort(crate::config::SortBy::Filename),
            AppearanceDisplayField::PlaylistSort(crate::config::SortBy::Metadata),
        ];
        for pair in filename_fields.windows(2) {
            assert_eq!(
                next_appearance_display_field(pair[0], true, false, false),
                pair[1]
            );
        }
        assert_eq!(
            next_appearance_display_field(
                AppearanceDisplayField::NowPlayingSort(crate::config::SortBy::Metadata),
                true,
                false,
                false,
            ),
            AppearanceDisplayField::PlaylistSort(crate::config::SortBy::Filename),
            "disabled Now Playing metadata rows must be skipped"
        );
        assert_eq!(
            next_appearance_display_field(
                AppearanceDisplayField::PlaylistSort(crate::config::SortBy::Metadata),
                true,
                true,
                false,
            ),
            AppearanceDisplayField::PlaylistSort(crate::config::SortBy::Metadata),
            "disabled Playlist metadata rows must not become selectable"
        );
        assert_eq!(
            next_appearance_display_field(
                AppearanceDisplayField::PlaylistMetadata(
                    crate::config::SortMetadataField::TrackNumber,
                ),
                false,
                true,
                true,
            ),
            AppearanceDisplayField::PlaylistMetadata(crate::config::SortMetadataField::Album),
            "the non-selectable playlist title row must climb to Track Number"
        );
    }

    #[test]
    fn extend_playlist_wraps_every_path_as_a_track() {
        let mut state = AppState::default();

        state.extend_playlist([PathBuf::from("/m/one.mp3"), PathBuf::from("/m/two.flac")]);

        assert_eq!(state.playlist.len(), 2);
        assert_eq!(state.playlist.tracks()[0].display_name(), "one");
        assert_eq!(
            state.playlist.tracks()[1].path(),
            Some(PathBuf::from("/m/two.flac").as_path())
        );
    }

    #[test]
    fn direct_playlist_add_normalizes_the_same_identity_as_track_local() {
        let mut state = AppState::default();
        let input = PathBuf::from("/music/./album/../song.mp3");

        state.extend_playlist([input]);

        assert_eq!(
            state.playlist.tracks()[0].track_location(),
            Track::local("/music/song.mp3").track_location()
        );
    }

    #[test]
    fn relative_direct_adds_are_anchored_at_the_add_boundary() {
        let mut state = AppState::default();
        let base = std::env::current_dir().expect("working directory");

        state.extend_playlist([PathBuf::from("music/./song.mp3")]);

        assert_eq!(
            state.playlist.tracks()[0].track_location(),
            Track::local(base.join("music/song.mp3")).track_location()
        );
    }

    #[test]
    fn extend_playlist_tracks_deduplicates_by_typed_identity() {
        let mut state = AppState::default();
        let text = "https://example.com/live";
        let url = url::Url::parse(text).expect("valid URL");

        state.extend_playlist([PathBuf::from(text)]);
        let result = state.extend_playlist_tracks([
            Track::from_stream(url.clone(), crate::stream::StreamKind::Http),
            Track::from_stream(url, crate::stream::StreamKind::Http),
        ]);

        assert_eq!(result.added_indices, vec![1]);
        assert_eq!(result.skipped, 1);
        assert_eq!(state.playlist.len(), 2);
        assert_eq!(
            state.playlist.tracks()[0].track_location(),
            TrackLocation::local(std::env::current_dir().unwrap().join(text))
        );
        assert_eq!(
            state.playlist.tracks()[1].track_location(),
            TrackLocation::url(url::Url::parse(text).expect("valid URL"))
        );
    }

    #[test]
    fn push_notification_drops_oldest_beyond_cap() {
        let mut notifications = Vec::new();

        for index in 0..60 {
            push_notification(
                &mut notifications,
                format!("message-{index}"),
                NOTIFICATIONS_CAP,
            );
        }

        assert_eq!(notifications.len(), NOTIFICATIONS_CAP);
        assert_eq!(notifications.first(), Some(&"message-10".to_string()));
        assert_eq!(notifications.last(), Some(&"message-59".to_string()));
    }

    #[test]
    fn push_notification_keeps_everything_below_cap() {
        let mut notifications = Vec::new();

        push_notification(&mut notifications, "first".to_string(), NOTIFICATIONS_CAP);
        push_notification(&mut notifications, "second".to_string(), NOTIFICATIONS_CAP);

        assert_eq!(
            notifications,
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn push_notification_with_zero_cap_retains_nothing() {
        let mut notifications = Vec::new();

        push_notification(&mut notifications, "only".to_string(), 0);

        assert!(notifications.is_empty());
    }

    /// Queue three paths with the middle entry marked as playing.
    fn playing_queue_state() -> AppState {
        let mut state = AppState::default();
        state.extend_playlist([
            PathBuf::from("/a.mp3"),
            PathBuf::from("/b.mp3"),
            PathBuf::from("/c.mp3"),
        ]);
        state.playlist.select(1);
        state.playback.track_index = Some(1);
        state.persistence.last_track = Some(TrackLocation::local("/b.mp3"));
        state
    }

    #[test]
    fn extend_playlist_registers_new_entries_in_the_shuffle_pool() {
        let mut state = AppState {
            playback_mode: crate::playback_mode::PlaybackMode::new(
                crate::playback_mode::RepeatMode::Off,
                true,
            ),
            ..AppState::default()
        };
        state.navigation.reshuffle(0, None);
        state.extend_playlist([PathBuf::from("/one.mp3")]);
        state.playlist.select(0);

        // The single queued entry was drawn as the current track
        state.navigation.record_started(0);
        assert!(!state.navigation.unplayed_contains(0));

        state.extend_playlist([PathBuf::from("/two.mp3"), PathBuf::from("/three.mp3")]);

        assert!(state.navigation.unplayed_contains(1));
        assert!(state.navigation.unplayed_contains(2));
    }

    #[test]
    fn delete_queue_entry_reanchors_playback_after_a_preceding_row_is_removed() {
        let mut state = playing_queue_state();
        state.playlist.select(0);

        assert_eq!(
            state.delete_queue_entry_at_cursor(),
            QueueMutationResult::Applied
        );

        assert_eq!(state.playlist.len(), 2);
        assert_eq!(
            state.playlist.tracks()[0].path(),
            Some(PathBuf::from("/b.mp3").as_path())
        );
        assert_eq!(state.playback.track_index, Some(0));
        assert_eq!(state.navigation.history_len(), 0, "stale trail dropped");
        assert!(!state.navigation.unplayed_contains(0), "playing reserved");
    }

    #[test]
    fn delete_on_an_empty_queue_reports_no_change() {
        let mut state = AppState::default();

        assert_eq!(
            state.delete_queue_entry_at_cursor(),
            QueueMutationResult::Unchanged
        );
    }

    #[test]
    fn queue_swaps_reorder_rows_and_carry_the_selection() {
        let mut state = playing_queue_state();
        state.playlist.select(2);

        assert_eq!(state.swap_queue_entry_up(), QueueMutationResult::Applied);
        assert_eq!(
            state.playlist.tracks()[1].display_name(),
            "c",
            "the swapped entry followed the cursor"
        );
        assert_eq!(state.playlist.cursor(), 1);
        assert_eq!(state.playback.track_index, Some(2));
        assert!(!state.navigation.unplayed_contains(2), "playing reserved");

        assert_eq!(state.swap_queue_entry_down(), QueueMutationResult::Applied);
        assert_eq!(state.playlist.tracks()[2].display_name(), "c");
        assert_eq!(state.playback.track_index, Some(1));
        assert!(!state.navigation.unplayed_contains(1), "playing reserved");

        // Boundary refusals leave the queue untouched
        assert_eq!(
            state.swap_queue_entry_down(),
            QueueMutationResult::Unchanged
        );
        assert_eq!(state.playlist.cursor(), 2);
    }

    #[test]
    fn deleting_the_playing_queue_entry_stops_and_clears_playback_state() {
        let mut state = playing_queue_state();
        state.async_ops.begin_stream_acquisition(
            TrackLocation::url(url::Url::parse("https://example.com/live").unwrap()),
            7,
        );
        state.playback.elapsed = Duration::from_secs(12);
        state.playback.duration = Some(Duration::from_secs(90));
        state.persistence.last_track_position_ms = 12_000;

        assert_eq!(
            state.delete_queue_entry_at_cursor(),
            QueueMutationResult::PlayingTrackRemoved
        );

        assert_eq!(state.playlist.len(), 2);
        assert_eq!(state.playlist.cursor(), 1);
        assert_eq!(state.playback.status, crate::audio::PlayStatus::Stopped);
        assert_eq!(state.playback.track_index, None);
        assert_eq!(state.playback.elapsed, Duration::ZERO);
        assert_eq!(state.playback.duration, None);
        assert_eq!(state.persistence.last_track, None);
        assert_eq!(state.persistence.last_track_position_ms, 0);
        assert!(state.async_ops.stream_activity().is_none());
        assert!(
            state.navigation.unplayed_contains(1),
            "cleared playback must not reserve a stale index"
        );
    }

    #[test]
    fn clear_queue_empties_the_queue_but_keeps_playback_running() {
        let root = unique_temp_dir("state-clear-queue");
        let path = root.join("keep.mp3");
        fs::write(&path, b"audio").expect("fixture file");

        let mut state = AppState::default();
        state.extend_playlist([path.clone()]);
        state.playlist.select(0);
        state.playback.track_index = Some(0);
        state.playback.status = crate::audio::PlayStatus::Playing;

        state.clear_queue();

        assert!(state.playlist.is_empty());
        assert_eq!(state.navigation.history_len(), 0);
        assert!(
            path.exists(),
            "clearing the queue must never delete the backing file"
        );
        assert_eq!(
            state.playback.status,
            crate::audio::PlayStatus::Playing,
            "the running track finishes naturally"
        );
    }

    #[test]
    fn playlist_scroll_anchors_to_the_edge_being_approached() {
        let mut state = AppState::default();
        state.extend_playlist(
            (0..20)
                .map(|i| PathBuf::from(format!("/m/t{i:02}.mp3")))
                .collect::<Vec<_>>(),
        );
        state.playlist_viewport_height = 8;

        // Move down to the tail: the scroll window hugs the bottom (air below).
        state.move_playlist_to_bottom();
        assert_eq!(state.playlist.cursor(), 19);
        assert_eq!(state.playlist_scroll_offset, 12); // 20 - 8

        // One step up: the cursor climbs from the bottom row (relative 7) to
        // relative 6 while the window stays at the tail anchor.
        state.move_playlist_cursor_up();
        assert_eq!(state.playlist.cursor(), 18);
        assert_eq!(state.playlist_scroll_offset, 12);

        // Keep climbing until the cursor is within the top context band, then
        // the window recedes (cursor 13 => offset 11).
        while state.playlist.cursor() > 13 {
            state.move_playlist_cursor_up();
        }
        assert_eq!(state.playlist.cursor(), 13);
        assert_eq!(state.playlist_scroll_offset, 11);

        // Paging down advances the window while keeping the cursor visible.
        state.page_playlist_down();
        assert!(state.playlist.cursor() >= 13);
        assert!(state.playlist_scroll_offset + 8 > state.playlist.cursor());
    }

    #[test]
    fn playlist_home_end_page_navigation_reaches_both_edges() {
        let mut state = AppState::default();
        state.extend_playlist(
            (0..10)
                .map(|i| PathBuf::from(format!("/m/t{i:02}.mp3")))
                .collect::<Vec<_>>(),
        );
        state.playlist_viewport_height = 5;

        state.move_playlist_to_bottom();
        assert_eq!(state.playlist.cursor(), 9);
        assert_eq!(state.playlist_scroll_offset, 5);

        state.move_playlist_to_top();
        assert_eq!(state.playlist.cursor(), 0);
        assert_eq!(state.playlist_scroll_offset, 0);

        state.move_playlist_cursor_down();
        state.move_playlist_cursor_down();
        state.page_playlist_up();
        assert_eq!(
            state.playlist.cursor(),
            0,
            "paging up saturates at the head"
        );

        state.move_playlist_to_bottom();
        state.page_playlist_down();
        assert_eq!(state.playlist.cursor(), 9, "paging down clamps at the tail");
    }

    #[test]
    fn playlist_resize_is_normalized_before_following_navigation() {
        let mut state = AppState::default();
        state.extend_playlist(
            (0..20)
                .map(|i| PathBuf::from(format!("/m/t{i:02}.mp3")))
                .collect::<Vec<_>>(),
        );
        state.playlist_viewport_height = 8;
        state.move_playlist_to_bottom();
        assert_eq!(state.playlist_scroll_offset, 12);

        // A shorter frame must commit the visible window to state before the
        // next navigation command reads it.
        state.tick_frame(FrameMetrics::new(0, 4, 0, 0), Duration::ZERO);
        assert_eq!(state.playlist_viewport_height, 4);
        assert_eq!(state.playlist_scroll_offset, 16);

        state.move_playlist_cursor_up();
        assert_eq!(state.playlist.cursor(), 18);
        assert_eq!(state.playlist_scroll_offset, 16);
        state.move_playlist_cursor_down();
        assert_eq!(state.playlist.cursor(), 19);
        assert_eq!(state.playlist_scroll_offset, 16);
    }

    #[test]
    fn playlist_tick_handles_empty_and_degenerate_viewports_safely() {
        let mut state = AppState::default();
        state.extend_playlist([
            PathBuf::from("/m/one.mp3"),
            PathBuf::from("/m/two.mp3"),
            PathBuf::from("/m/three.mp3"),
        ]);
        state.playlist.select(2);
        state.playlist_scroll_offset = usize::MAX;

        state.tick_frame(FrameMetrics::new(0, 0, 0, 0), Duration::ZERO);

        assert_eq!(state.playlist_viewport_height, 1);
        assert_eq!(state.playlist_scroll_offset, 2);
        assert!(state.playlist_scroll_offset <= state.playlist.cursor());
        assert!(state.playlist.cursor() < state.playlist_scroll_offset + 1);

        state.playlist.clear();
        state.playlist_scroll_offset = usize::MAX;
        state.tick_frame(FrameMetrics::new(0, 0, 0, 0), Duration::ZERO);
        assert_eq!(state.playlist_scroll_offset, 0);
    }

    #[test]
    fn empty_playlist_scroll_commands_are_safe_no_ops() {
        let mut state = AppState::default();

        state.move_playlist_cursor_up();
        state.move_playlist_cursor_down();
        state.move_playlist_to_top();
        state.move_playlist_to_bottom();
        state.page_playlist_up();
        state.page_playlist_down();

        assert_eq!(state.playlist.cursor(), 0);
        assert_eq!(state.playlist_scroll_offset, 0);
    }

    #[test]
    fn default_playlist_name_skips_existing_numbers() {
        assert_eq!(
            default_playlist_name(&["Playlist 1".to_string(), "Playlist 2".to_string()]),
            "Playlist 3"
        );
    }

    #[test]
    fn default_playlist_name_starts_at_one_when_empty() {
        assert_eq!(default_playlist_name(&[]), "Playlist 1");
    }

    #[test]
    fn default_playlist_name_fills_the_first_gap() {
        assert_eq!(
            default_playlist_name(&["Playlist 2".to_string()]),
            "Playlist 1"
        );
    }

    #[test]
    fn next_playlist_name_uses_highest_suffix_plus_one() {
        assert_eq!(
            next_playlist_name(&[
                "Playlist 1".to_string(),
                "Playlist 2".to_string(),
                "Playlist 5".to_string()
            ]),
            "Playlist 6"
        );
    }

    #[test]
    fn next_playlist_name_starts_at_one_when_empty() {
        assert_eq!(next_playlist_name(&[]), "Playlist 1");
    }

    #[test]
    fn next_playlist_name_ignores_unrelated_names() {
        assert_eq!(
            next_playlist_name(&["Rock".to_string(), "Jazz".to_string()]),
            "Playlist 1"
        );
    }

    #[test]
    fn fresh_state_has_no_active_playlist_or_dialog() {
        let state = AppState::default();
        assert_eq!(state.active_playlist_name, None);
        assert_eq!(state.popup_dialog.dialog_mode_ref(), None);
        assert_eq!(state.popup_dialog.dialog_error(), None);
        assert_eq!(state.popup_dialog.dialog_input_ref(), None);
        assert_eq!(state.popup_dialog.dialog_cursor(), None);
        assert_eq!(state.popup_dialog.dialog_extension(), None);
    }

    #[test]
    fn modal_stack_allows_only_supported_nested_transitions() {
        let mut modal = PopupDialogState::default();
        assert!(matches!(modal.modal, ModalStack::Normal));

        modal.open_popup(Popup::Settings {
            tab: SettingsTab::General,
            focus: SettingsFocus::Content,
            draft: SettingsDraft::default(),
        });
        modal.open_dialog(DialogMode::SettingsEdit, "value".to_string(), None);
        assert!(matches!(modal.modal, ModalStack::DialogInPopup { .. }));

        modal.close_dialog();
        assert!(matches!(
            modal.modal,
            ModalStack::Popup(Popup::Settings { .. })
        ));

        modal.clear();
        modal.open_dialog(
            DialogMode::RenameFile {
                path: PathBuf::from("/music/song.mp3"),
                original_name: "song.mp3".to_string(),
                error: None,
            },
            "song".to_string(),
            Some(".mp3".to_string()),
        );
        modal.push_popup(Popup::RenameCollision {
            existing: PathBuf::from("/music/existing.mp3"),
            attempted: "existing.mp3".to_string(),
        });
        assert!(matches!(modal.modal, ModalStack::PopupInDialog { .. }));
        modal.close_modal();
        assert!(matches!(modal.modal, ModalStack::Dialog(_)));

        // A warning cannot be opened without its required underlying state.
        modal.clear();
        modal.open_popup(Popup::ConfirmOverwrite {
            name: "Mix".to_string(),
        });
        assert!(matches!(modal.modal, ModalStack::Normal));

        modal.open_search(SearchScope::Playlist);
        modal.set_alert("search failed".to_string());
        assert_eq!(modal.alert_message(), Some("search failed"));
        modal.dismiss_alert();
        assert!(matches!(modal.modal, ModalStack::Search(_)));
    }

    #[test]
    fn general_cursor_stops_on_the_sort_action_row() {
        // General has four settings rows plus one explicit action row.
        let mut field = SettingsField::GeneralBrowserDirectory;
        for _ in 0..12 {
            field = next_general_field(field, true);
            assert!(SettingsField::GENERAL_ALL.contains(&field));
        }
        assert_eq!(field, SettingsField::GeneralSortTracks);

        // Moving up from the action returns to the ordinary rows safely.
        let mut field = SettingsField::GeneralSortTracks;
        for _ in 0..12 {
            field = next_general_field(field, false);
            assert!(SettingsField::GENERAL_ALL.contains(&field));
        }
        assert_eq!(field, SettingsField::GeneralBrowserDirectory);

        // The metadata flag is intentionally ignored by the General cursor.
        let mut field = SettingsField::GeneralSortTracks;
        for _ in 0..3 {
            field = next_general_field(field, true);
        }
        assert_eq!(field, SettingsField::GeneralSortTracks);
    }

    #[test]
    fn typed_settings_fields_have_exhaustive_enabled_navigation() {
        let draft = SettingsDraft::default();
        let mut field = SettingsField::GENERAL_ALL[0];
        for _ in 0..SettingsField::GENERAL_ALL.len() {
            field = SettingsField::next_enabled(SettingsTab::General, field, true, &draft);
            assert!(SettingsField::GENERAL_ALL.contains(&field));
        }
        assert_eq!(
            SettingsField::GeneralSortTracks.activate(&mut draft.clone()),
            Some(SettingsField::GeneralSortTracks)
        );
    }

    #[test]
    fn typed_settings_navigation_covers_every_tab_and_appearance_column() {
        let mut draft = SettingsDraft::default();
        draft.theme_names = vec!["default".into(), "custom".into()];
        draft.outputs = vec![crate::audio::AudioOutput::session_default()];
        draft.now_playing_display.sort_by = crate::config::SortBy::Metadata;
        draft.playlist_columns.display_by = crate::config::SortBy::Metadata;

        let general = SettingsField::GENERAL_ALL.to_vec();
        let playback = SettingsField::PLAYBACK_ALL.to_vec();
        let keys = crate::config::KeySettingsRow::ALL
            .into_iter()
            .map(SettingsField::Keys)
            .collect::<Vec<_>>();
        let display = SettingsField::appearance_display_fields(&draft);
        let colors = crate::ui::theme::ThemeColorField::ALL
            .into_iter()
            .map(SettingsField::AppearanceColor)
            .collect::<Vec<_>>();
        let themes = (0..draft.theme_names.len())
            .map(SettingsField::AppearanceTheme)
            .collect::<Vec<_>>();
        let outputs = (0..draft.outputs.len())
            .map(SettingsField::SoundOutput)
            .collect::<Vec<_>>();

        for (tab, fields) in [
            (SettingsTab::General, general),
            (SettingsTab::Playback, playback),
            (SettingsTab::Keys, keys),
        ] {
            let mut current = fields[0];
            for expected in fields.iter().skip(1) {
                current = SettingsField::next_enabled(tab, current, true, &draft);
                assert_eq!(current, *expected);
            }
        }

        for (column, fields) in [
            (AppearanceColumn::Display, display),
            (AppearanceColumn::Themes, themes),
            (AppearanceColumn::Colors, colors),
        ] {
            draft.appearance_column = column;
            draft.set_active_field(SettingsTab::Appearance, fields[0]);
            let mut current = fields[0];
            for expected in fields.iter().skip(1) {
                current =
                    SettingsField::next_enabled(SettingsTab::Appearance, current, true, &draft);
                assert_eq!(current, *expected);
            }
            assert_eq!(draft.active_field(SettingsTab::Appearance), Some(fields[0]));
        }

        let current = outputs[0];
        assert_eq!(
            SettingsField::next_enabled(SettingsTab::Sound, current, true, &draft),
            current
        );
        draft.set_active_field(SettingsTab::Sound, current);
        assert_eq!(draft.active_field(SettingsTab::Sound), Some(current));
    }

    #[test]
    fn appearance_cursor_skips_disabled_metadata_and_playlist_title() {
        let mut field = AppearanceDisplayField::Border(crate::config::BorderType::Plain);
        for _ in 0..20 {
            field = next_appearance_display_field(field, true, false, true);
        }
        assert_eq!(
            field,
            AppearanceDisplayField::PlaylistMetadata(crate::config::SortMetadataField::TrackNumber),
            "Playlist Title is never a typed field"
        );

        field = next_appearance_display_field(
            AppearanceDisplayField::Border(crate::config::BorderType::Plain),
            true,
            false,
            false,
        );
        assert_eq!(
            field,
            AppearanceDisplayField::Border(crate::config::BorderType::Rounded),
            "border options remain selectable"
        );
        field = next_appearance_display_field(
            AppearanceDisplayField::NowPlayingSort(crate::config::SortBy::Metadata),
            true,
            false,
            false,
        );
        assert_eq!(
            field,
            AppearanceDisplayField::PlaylistSort(crate::config::SortBy::Filename),
            "disabled Now Playing rows are skipped"
        );
    }

    #[test]
    fn focus_ring_cycles_browser_and_playlist_when_lyrics_are_hidden() {
        assert_eq!(next_panel(Panel::Browser, false), Panel::Playlist);
        assert_eq!(next_panel(Panel::Playlist, false), Panel::Browser);
        assert_eq!(previous_panel(Panel::Browser, false), Panel::Playlist);
        assert_eq!(previous_panel(Panel::Playlist, false), Panel::Browser);
    }

    #[test]
    fn focus_ring_includes_lyrics_once_visible() {
        // Tab forward: Browser -> Playlist -> Lyrics -> Playlist.
        assert_eq!(next_panel(Panel::Browser, true), Panel::Playlist);
        assert_eq!(next_panel(Panel::Playlist, true), Panel::Lyrics);
        assert_eq!(next_panel(Panel::Lyrics, true), Panel::Playlist);

        // Shift+Tab backwards mirrors the ring.
        assert_eq!(previous_panel(Panel::Playlist, true), Panel::Browser);
        assert_eq!(previous_panel(Panel::Browser, true), Panel::Lyrics);
        assert_eq!(previous_panel(Panel::Lyrics, true), Panel::Playlist);
    }

    #[test]
    fn focus_ring_never_lands_on_lyrics_while_hidden() {
        assert_ne!(next_panel(Panel::Playlist, false), Panel::Lyrics);
        assert_ne!(previous_panel(Panel::Browser, false), Panel::Lyrics);
    }

    #[test]
    fn default_lyrics_state_is_hidden_and_empty() {
        let state = LyricsState::default();
        assert!(!state.visible);
        assert_eq!(state.scroll, 0);
        assert_eq!(state.track_index, None);
        assert_eq!(state.document, None);
        assert_eq!(state.error, None);
    }

    #[test]
    fn lyrics_layout_cache_reuses_the_same_document_and_width() {
        let mut lyrics = LyricsState::default();
        lyrics.set_document(Some(LyricsDocument::from_plain("abcdefg")));
        lyrics.ensure_layout_cache(3);

        let first = lyrics.layout_cache_for(3).expect("layout cache");
        let first_layout = &first.layout as *const _;
        let first_version = first.document_version;

        lyrics.ensure_layout_cache(3);

        let reused = lyrics.layout_cache_for(3).expect("layout cache");
        assert_eq!(&reused.layout as *const _, first_layout);
        assert_eq!(reused.document_version, first_version);
    }

    #[test]
    fn lyrics_layout_cache_invalidates_when_width_changes() {
        let mut lyrics = LyricsState::default();
        lyrics.set_document(Some(LyricsDocument::from_plain("abcdefg")));
        lyrics.ensure_layout_cache(3);
        let first_version = lyrics
            .layout_cache_for(3)
            .expect("layout cache")
            .document_version;

        lyrics.ensure_layout_cache(4);

        let changed = lyrics.layout_cache_for(4).expect("layout cache");
        assert_eq!(changed.document_version, first_version);
        assert_eq!(changed.width, 4);
        assert_ne!(changed.layout.total_rows, 3);
    }

    #[test]
    fn lyrics_layout_cache_invalidates_when_document_changes() {
        let mut lyrics = LyricsState::default();
        lyrics.set_document(Some(LyricsDocument::from_plain("abcdefg")));
        lyrics.ensure_layout_cache(4);
        let first_version = lyrics
            .layout_cache_for(4)
            .expect("layout cache")
            .document_version;

        lyrics.set_document(Some(LyricsDocument::from_plain("a\nb\nc")));
        lyrics.ensure_layout_cache(4);

        let changed = lyrics.layout_cache_for(4).expect("layout cache");
        assert_ne!(changed.document_version, first_version);
        assert_eq!(changed.layout.total_rows, 3);
        assert_eq!(changed.char_times.len(), 3);
    }

    #[test]
    fn settings_tabs_follow_the_playback_order_ring() {
        // Display order left to right, matching the visual tab strip.
        assert_eq!(
            SettingsTab::all(),
            [
                SettingsTab::General,
                SettingsTab::Appearance,
                SettingsTab::Keys,
                SettingsTab::Playback,
                SettingsTab::Sound,
            ]
        );
        // `next` wraps through the same order and `previous` mirrors it.
        assert_eq!(SettingsTab::General.next(), SettingsTab::Appearance);
        assert_eq!(SettingsTab::Keys.next(), SettingsTab::Playback);
        assert_eq!(SettingsTab::Playback.next(), SettingsTab::Sound);
        assert_eq!(SettingsTab::Sound.next(), SettingsTab::General);
        assert_eq!(SettingsTab::Playback.previous(), SettingsTab::Keys);
        assert_eq!(SettingsTab::General.previous(), SettingsTab::Sound);
    }

    #[test]
    fn draft_seeds_remote_lyrics_from_the_playback_config() {
        let mut config = crate::config::AppConfig::default();
        config.playback.remote_lyrics = true;
        let draft =
            SettingsDraft::from_state(&AppState::default(), &config, &std::path::PathBuf::new());
        assert!(draft.remote_lyrics);
        assert_eq!(draft.playback_field, SettingsField::PlaybackRemoteLyrics);
    }

    #[test]
    fn draft_seeds_gain_db_from_the_playback_config() {
        let mut config = crate::config::AppConfig::default();
        config.playback.gain_db = GainDb::try_from(-3.5).unwrap();
        let draft =
            SettingsDraft::from_state(&AppState::default(), &config, &std::path::PathBuf::new());
        assert_eq!(draft.gain_db, GainDb::try_from(-3.5).unwrap());
        assert_eq!(draft.gain_db_initial, GainDb::try_from(-3.5).unwrap());
    }

    #[test]
    fn tick_spinner_advances_the_frame_after_elapsed_time() {
        // Dot advances every 100 ms (10 FPS). Feeding more than one full
        // interval through `tick_spinner` is enough to guarantee the frame
        // advances even on a slow runner, and keeps the test deterministic
        // without sleeping.
        let mut state = AppState::default();
        let initial_frame = state.frame.spinner.frame_str().to_string();
        state.tick_spinner(std::time::Duration::from_millis(250));
        let next_frame = state.frame.spinner.frame_str().to_string();
        assert_ne!(
            initial_frame, next_frame,
            "tick_spinner must advance the global spinner phase"
        );
    }

    #[test]
    fn shared_frame_spinner_uses_the_dot_preset() {
        let state = AppState::default();
        let expected_frames: Vec<String> = SpinnerType::Dot
            .frames()
            .iter()
            .map(|frame| (*frame).to_string())
            .collect();

        assert_eq!(state.frame.spinner.frames(), expected_frames.as_slice());
        assert_eq!(state.frame.spinner.interval(), Duration::from_millis(100));
        assert_eq!(
            state.spinner_loading_line()[0].content.as_ref(),
            state.frame.spinner.frame_str()
        );
    }

    #[test]
    fn spinner_loading_line_contains_exactly_the_three_required_spans() {
        let state = AppState::default();
        let spans = state.spinner_loading_line();
        assert_eq!(spans.len(), 3, "spinner + space + label");
        assert_eq!(spans[1].content.as_ref(), " ");
        assert_eq!(spans[2].content.as_ref(), "Loading");
        // The first span is the current frame string and must be non-empty.
        assert!(
            !spans[0].content.as_ref().is_empty(),
            "the spinner frame must render at least one glyph"
        );
    }

    #[test]
    fn has_pending_effects_starts_false_and_tracks_the_counter() {
        let state = AppState::default();
        assert!(
            !state.has_pending_effects(),
            "a fresh app must report no in-flight effects"
        );
        state
            .async_ops
            .pending_effects
            .fetch_add(1, Ordering::Relaxed);
        assert!(
            state.has_pending_effects(),
            "an incremented counter must surface as in-flight"
        );
        state
            .async_ops
            .pending_effects
            .fetch_sub(1, Ordering::Relaxed);
        assert!(
            !state.has_pending_effects(),
            "decrementing back to zero must clear the flag"
        );
    }
}
