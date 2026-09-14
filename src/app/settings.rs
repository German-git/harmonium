//! Pure helpers for the settings popup working copy.
//!
//! `App` stays the orchestrator: it owns state, holds the `SettingsDraft` and
//! decides *when* an edit commits. The logic here is limited to translating
//! between a `SettingsDraft` and individual editable values (colors, key
//! bindings) and to validating a binding against the hardcoded map. Keeping
//! these functions outside `app.rs` trims the god-object without splitting
//! `App` into ad-hoc controllers, which the analysis explicitly warns against.

use super::*;

use crate::state::SettingsDraft;

/// Settings operations receive only the state and application capabilities
/// required by the settings workflow.
pub(crate) struct SettingsController<'app> {
    state: &'app mut AppState,
    config: &'app mut AppConfig,
    config_dir: &'app Path,
    data_dir: &'app Path,
    keys: &'app mut KeysConfig,
    input_mapper: &'app mut InputMapper,
    border_type: &'app mut crate::config::BorderType,
    output_provider: &'app ArcOutputProvider,
}

impl<'app> SettingsController<'app> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        state: &'app mut AppState,
        config: &'app mut AppConfig,
        config_dir: &'app Path,
        data_dir: &'app Path,
        keys: &'app mut KeysConfig,
        input_mapper: &'app mut InputMapper,
        border_type: &'app mut crate::config::BorderType,
        output_provider: &'app ArcOutputProvider,
    ) -> Self {
        Self {
            state,
            config,
            config_dir,
            data_dir,
            keys,
            input_mapper,
            border_type,
            output_provider,
        }
    }
}

/// The global action a hardcoded binding belongs to, if editing a row
/// with the given input
///
/// Matching is delegated to the authoritative reserved-key policy in
/// `input.rs`, including wildcard and modifier-containing bindings. The field
/// that owns the lyrics fallback is exempted so the stock keymap can be
/// confirmed without being rejected.
pub(crate) fn hardcoded_global_action_for(
    input: &str,
    row: crate::config::KeySettingsRow,
) -> Option<&'static str> {
    let reserved = crate::input::reserved_global_action_for(input);
    let is_owner =
        row == crate::config::KeySettingsRow::Lyrics && reserved == Some("toggle lyrics");
    if is_owner {
        return None;
    }
    reserved
}

/// Value of the editable color field for the Appearance tab.
pub(crate) fn appearance_color_at(
    draft: &SettingsDraft,
    field: crate::ui::theme::ThemeColorField,
) -> &str {
    field.get(&draft.colors)
}

/// What would be the saved theme name for the currently selected theme.
///
/// Bundled themes (default, gruvbox) are written as `<name>-custom` so the
/// user's edits never overwrite the shipped palette; any other theme name is
/// reused for its natural file.
pub(crate) fn settings_theme_write_name(draft: &SettingsDraft) -> String {
    let theme_name = draft
        .theme_names
        .get(draft.selected_theme().unwrap_or(0))
        .cloned()
        .unwrap_or_else(|| "custom".to_string());
    let is_bundled = theme_name == "default" || theme_name == "gruvbox";
    if is_bundled {
        format!("{theme_name}-custom")
    } else {
        theme_name
    }
}

/// Set the editable color field for the Appearance tab.
pub(crate) fn appearance_set_color(
    draft: &mut SettingsDraft,
    field: crate::ui::theme::ThemeColorField,
    value: String,
) {
    field.set(&mut draft.colors, value);
}

/// Value of a Keys settings row.
pub(crate) fn keys_value_at(draft: &SettingsDraft, row: crate::config::KeySettingsRow) -> &str {
    row.get(&draft.keys_draft)
}

/// Set a Keys settings row.
pub(crate) fn keys_set_at(
    draft: &mut SettingsDraft,
    row: crate::config::KeySettingsRow,
    value: String,
) -> Result<(), crate::input::KeyChordError> {
    row.set(&mut draft.keys_draft, value)
}

impl SettingsController<'_> {
    fn push_notification(&mut self, message: String) {
        push_notification(&mut self.state.notifications, message, NOTIFICATIONS_CAP);
    }

    pub(crate) fn handle_settings_key(&mut self, command: Command) -> Vec<Effect> {
        // The alert takes priority: any key dismisses it without changing the
        // rest of the state.
        if self.state.popup_dialog.alert_message().is_some() {
            ModalRouter::dismiss_alert(self.state);
            return Vec::new();
        }

        // Editing a text field has its own confirm/cancel handling via the
        // dialog path; the command layer only sees cursor/tab/enter.
        let is_editing =
            self.state.popup_dialog.dialog_mode_ref() == Some(&DialogMode::SettingsEdit);

        match command {
            Command::CancelPopup => {
                if is_editing {
                    self.state.async_ops.browser_validation_request.cancel();
                    self.state.async_ops.cancel_theme_requests();
                    ModalRouter::close_dialog(self.state);
                    return Vec::new();
                }
                // Esc applies changes and closes the popup.
                return self.apply_settings_inner();
            }
            // Tab and Shift+Tab always switch tabs.
            Command::FocusNextPanel => {
                if is_editing {
                    return Vec::new();
                }
                if let Some(Popup::Settings { tab, .. }) =
                    self.state.popup_dialog.active_popup_mut()
                {
                    *tab = tab.next();
                }
                return Vec::new();
            }
            Command::FocusPreviousPanel => {
                if is_editing {
                    return Vec::new();
                }
                if let Some(Popup::Settings { tab, .. }) =
                    self.state.popup_dialog.active_popup_mut()
                {
                    *tab = tab.previous();
                }
                return Vec::new();
            }
            // Left/Right move between columns within the active tab.
            Command::SettingsNextColumn | Command::SettingsPreviousColumn => {
                if is_editing {
                    return Vec::new();
                }
                let forward = command == Command::SettingsNextColumn;
                if let Some(Popup::Settings { tab, draft, .. }) =
                    self.state.popup_dialog.active_popup_mut()
                {
                    match tab {
                        SettingsTab::Appearance => {
                            if forward {
                                draft.appearance_column = draft.appearance_column.next(true);
                            } else {
                                draft.appearance_column = draft.appearance_column.next(false);
                            }
                        }
                        // Sound is a single list; Left/Right do nothing.
                        SettingsTab::General | SettingsTab::Sound | SettingsTab::Keys => {}
                        // Playback: row 0 is a checkbox, row 1 the gain slider
                        // (0.5 dB steps), row 2 the crossfade length (5 s steps,
                        // clamped between Off and CROSSFADE_MAX_SECONDS so the
                        // arrow keys never wrap the indicator).
                        SettingsTab::Playback => match draft.active_field(SettingsTab::Playback) {
                            Some(crate::state::SettingsField::PlaybackGain) => {
                                draft.gain_db = draft.gain_db.stepped(forward);
                            }
                            Some(crate::state::SettingsField::PlaybackCrossfade) => {
                                draft.crossfade_seconds = crate::audio::playback::stepped_crossfade(
                                    draft.crossfade_seconds,
                                    forward,
                                );
                            }
                            _ => {}
                        },
                    }
                }
                return Vec::new();
            }
            Command::CursorUp => {
                if is_editing {
                    return Vec::new();
                }
                let mut theme_selection_moved = false;
                if let Some(Popup::Settings { tab, draft, .. }) =
                    self.state.popup_dialog.active_popup_mut()
                {
                    if let Some(current) = draft.active_field(*tab) {
                        let next =
                            crate::state::SettingsField::next_enabled(*tab, current, false, draft);
                        theme_selection_moved = matches!(
                            (current, next),
                            (
                                crate::state::SettingsField::AppearanceTheme(_),
                                crate::state::SettingsField::AppearanceTheme(_)
                            ) if current != next
                        );
                        draft.set_active_field(*tab, next);
                    }
                }
                return if theme_selection_moved {
                    self.request_selected_theme_colors().into_iter().collect()
                } else {
                    Vec::new()
                };
            }
            Command::CursorDown => {
                if is_editing {
                    return Vec::new();
                }
                let mut theme_selection_moved = false;
                if let Some(Popup::Settings { tab, draft, .. }) =
                    self.state.popup_dialog.active_popup_mut()
                {
                    if let Some(current) = draft.active_field(*tab) {
                        let next =
                            crate::state::SettingsField::next_enabled(*tab, current, true, draft);
                        theme_selection_moved = matches!(
                            (current, next),
                            (
                                crate::state::SettingsField::AppearanceTheme(_),
                                crate::state::SettingsField::AppearanceTheme(_)
                            ) if current != next
                        );
                        draft.set_active_field(*tab, next);
                    }
                }
                return if theme_selection_moved {
                    self.request_selected_theme_colors().into_iter().collect()
                } else {
                    Vec::new()
                };
            }
            Command::ConfirmDialog => {
                if is_editing {
                    return self.commit_settings_edit();
                }
                // When not editing, start editing the focused field or apply.
                let (tab, field) = match self.state.popup_dialog.active_popup_ref() {
                    Some(Popup::Settings { tab, draft, .. }) => (*tab, draft.active_field(*tab)),
                    _ => return Vec::new(),
                };
                match tab {
                    SettingsTab::General => {
                        if field == Some(crate::state::SettingsField::GeneralBrowserDirectory) {
                            // Edit the browser directory.
                            let initial = match self.state.popup_dialog.active_popup_ref() {
                                Some(Popup::Settings { draft, .. }) => {
                                    draft.browser_directory.clone()
                                }
                                _ => String::new(),
                            };
                            ModalRouter::open_dialog(
                                self.state,
                                DialogMode::SettingsEdit,
                                initial,
                                None,
                            );
                            return Vec::new();
                        }
                        if field == Some(crate::state::SettingsField::GeneralConfirmQuit) {
                            if let Some(Popup::Settings { draft, .. }) =
                                self.state.popup_dialog.active_popup_mut()
                            {
                                draft.confirm_quit = !draft.confirm_quit;
                                self.state.confirm_quit = draft.confirm_quit;
                            }
                            return self.persist_runtime_state();
                        }
                        if field == Some(crate::state::SettingsField::GeneralResumePreviousTrack) {
                            if let Some(Popup::Settings { draft, .. }) =
                                self.state.popup_dialog.active_popup_mut()
                            {
                                draft.resume_previous_track = !draft.resume_previous_track;
                                self.state.persistence.resume_previous_track =
                                    draft.resume_previous_track;
                            }
                            return self.persist_runtime_state();
                        }
                        if field == Some(crate::state::SettingsField::GeneralShowHidden) {
                            if let Some(Popup::Settings { draft, .. }) =
                                self.state.popup_dialog.active_popup_mut()
                            {
                                draft.show_hidden = !draft.show_hidden;
                                self.state.browser.show_hidden = draft.show_hidden;
                            }
                            return self.persist_runtime_state();
                        }
                        // The reorder is an explicit action, not a setting.
                        if field == Some(crate::state::SettingsField::GeneralSortTracks) {
                            let draft = match self.state.popup_dialog.active_popup_ref() {
                                Some(Popup::Settings { draft, .. }) => draft.clone(),
                                _ => return Vec::new(),
                            };
                            self.state
                                .popup_dialog
                                .open_popup(Popup::ConfirmSortTracks { draft });
                            return Vec::new();
                        }
                    }
                    SettingsTab::Appearance => match field {
                        Some(crate::state::SettingsField::AppearanceDisplay(display)) => {
                            use crate::config::SortMetadataField;
                            if let Some(Popup::Settings { draft, .. }) =
                                self.state.popup_dialog.active_popup_mut()
                            {
                                match display {
                                    crate::state::AppearanceDisplayField::Border(border) => {
                                        draft.border_type = border;
                                        *self.border_type = border;
                                        self.config.ui.border_type = border;
                                    }
                                    crate::state::AppearanceDisplayField::NowPlayingSort(sort) => {
                                        draft.now_playing_display.sort_by = sort;
                                    }
                                    crate::state::AppearanceDisplayField::NowPlayingMetadata(
                                        metadata,
                                    ) => match metadata {
                                        SortMetadataField::TrackNumber => {
                                            draft.now_playing_display.metadata_track_number =
                                                !draft.now_playing_display.metadata_track_number;
                                        }
                                        SortMetadataField::Artist => {
                                            draft.now_playing_display.metadata_artist =
                                                !draft.now_playing_display.metadata_artist;
                                        }
                                        SortMetadataField::Album => {
                                            draft.now_playing_display.metadata_album =
                                                !draft.now_playing_display.metadata_album;
                                        }
                                        SortMetadataField::Title => {
                                            draft.now_playing_display.metadata_title =
                                                !draft.now_playing_display.metadata_title;
                                        }
                                    },
                                    crate::state::AppearanceDisplayField::PlaylistSort(sort) => {
                                        draft.playlist_columns.display_by = sort;
                                    }
                                    crate::state::AppearanceDisplayField::PlaylistMetadata(
                                        metadata,
                                    ) => match metadata {
                                        SortMetadataField::Artist => {
                                            draft.playlist_columns.metadata_artist =
                                                !draft.playlist_columns.metadata_artist;
                                        }
                                        SortMetadataField::Album => {
                                            draft.playlist_columns.metadata_album =
                                                !draft.playlist_columns.metadata_album;
                                        }
                                        SortMetadataField::TrackNumber => {
                                            draft.playlist_columns.metadata_track_number =
                                                !draft.playlist_columns.metadata_track_number;
                                        }
                                        SortMetadataField::Title => {}
                                    },
                                }
                            }
                            return Vec::new();
                        }
                        Some(crate::state::SettingsField::AppearanceTheme(_)) => {
                            let selected = match self.state.popup_dialog.active_popup_ref() {
                                Some(Popup::Settings { draft, .. }) => draft
                                    .selected_theme()
                                    .and_then(|index| draft.theme_names.get(index).cloned()),
                                _ => None,
                            };
                            return selected
                                .map(|name| self.apply_theme_name(name))
                                .unwrap_or_default();
                        }
                        Some(crate::state::SettingsField::AppearanceColor(color)) => {
                            let initial = match self.state.popup_dialog.active_popup_ref() {
                                Some(Popup::Settings { draft, .. }) => {
                                    settings::appearance_color_at(draft, color).to_string()
                                }
                                _ => return Vec::new(),
                            };
                            ModalRouter::open_dialog(
                                self.state,
                                DialogMode::SettingsEdit,
                                initial,
                                None,
                            );
                            return Vec::new();
                        }
                        _ => return Vec::new(),
                    },
                    SettingsTab::Sound => {
                        return self.apply_selected_output();
                    }
                    SettingsTab::Keys => {
                        let row = match self.state.popup_dialog.active_popup_ref() {
                            Some(Popup::Settings { draft, .. }) => {
                                let Some(row) = draft.key_settings_row() else {
                                    return Vec::new();
                                };
                                row
                            }
                            _ => return Vec::new(),
                        };
                        let initial = match self.state.popup_dialog.active_popup_ref() {
                            Some(Popup::Settings { draft, .. }) => {
                                settings::keys_value_at(draft, row).to_string()
                            }
                            _ => return Vec::new(),
                        };
                        ModalRouter::open_dialog(
                            self.state,
                            DialogMode::SettingsEdit,
                            initial,
                            None,
                        );
                        return Vec::new();
                    }
                    SettingsTab::Playback => {
                        // Row 0: Enter/Space flips the Remote lyrics checkbox.
                        // Row 1 (gain slider) is adjusted with Left/Right, so
                        // Enter does nothing there. Changes stay in the draft
                        // until Esc applies the whole settings popup.
                        if let Some(Popup::Settings { draft, .. }) =
                            self.state.popup_dialog.active_popup_mut()
                            && draft.active_field(SettingsTab::Playback)
                                == Some(crate::state::SettingsField::PlaybackRemoteLyrics)
                        {
                            draft.remote_lyrics = !draft.remote_lyrics;
                        }
                        return Vec::new();
                    }
                }
                // Fallback: if Enter was not used for a field, apply everything.
                return self.apply_settings_inner();
            }
            _ => {}
        }
        Vec::new()
    }

    pub(crate) fn commit_settings_edit(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let raw_input = self
            .state
            .popup_dialog
            .dialog_input_ref()
            .unwrap_or_default()
            .to_string();
        let input = raw_input.trim().to_string();
        let (tab, field) = match self.state.popup_dialog.active_popup_ref() {
            Some(Popup::Settings { tab, draft, .. }) => (*tab, draft.active_field(*tab)),
            _ => {
                ModalRouter::close_dialog(self.state);
                return Vec::new();
            }
        };
        match tab {
            SettingsTab::General => {
                if field == Some(crate::state::SettingsField::GeneralBrowserDirectory) {
                    let previous_dir = self
                        .state
                        .browser
                        .current_dir
                        .to_string_lossy()
                        .into_owned();
                    if input.is_empty() {
                        // Restore the previous directory and keep the editor open
                        // so the input can be corrected from its last character.
                        if let Some(Popup::Settings { draft, .. }) =
                            self.state.popup_dialog.active_popup_mut()
                        {
                            draft.browser_directory = previous_dir;
                        }
                        self.state.popup_dialog.set_dialog_input(raw_input);
                        self.state.popup_dialog.set_dialog_error(None);
                        self.state
                            .popup_dialog
                            .set_alert("Browser directory cannot be empty".to_string());
                        return Vec::new();
                    }
                    let request_id = self.state.async_ops.browser_validation_request.begin();
                    effects.push(Effect::ValidateBrowserDirectory {
                        request_id,
                        path: PathBuf::from(&input),
                        validation: BrowserDirectoryValidation::Settings {
                            input,
                            previous_dir: PathBuf::from(previous_dir),
                        },
                    });
                    return effects;
                }
            }
            SettingsTab::Appearance => {
                let Some(crate::state::SettingsField::AppearanceColor(color)) = field else {
                    return Vec::new();
                };
                if let Some(Popup::Settings { draft, .. }) =
                    self.state.popup_dialog.active_popup_mut()
                {
                    settings::appearance_set_color(draft, color, input);
                }
                let (write_name, colors, base_colors) =
                    match self.state.popup_dialog.active_popup_mut() {
                        Some(Popup::Settings { draft, .. }) => (
                            settings::settings_theme_write_name(draft),
                            draft.colors.clone(),
                            draft
                                .loaded_theme_palette
                                .as_ref()
                                .filter(|(name, _)| {
                                    draft
                                        .theme_names
                                        .get(draft.selected_theme().unwrap_or(0))
                                        .is_some_and(|selected| selected == name)
                                })
                                .map(|(_, colors)| colors.clone()),
                        ),
                        _ => (
                            String::new(),
                            crate::ui::theme::ThemeColors::default(),
                            None,
                        ),
                    };
                let Some(base_colors) = base_colors else {
                    self.state
                        .popup_dialog
                        .set_dialog_error(Some("Theme is still loading".to_string()));
                    return effects;
                };
                if write_name.is_empty() || colors == base_colors {
                    // No effective change: do not create a custom file.
                    ModalRouter::close_dialog(self.state);
                    return Vec::new();
                }
                if let Err(error) = crate::ui::theme::Theme::try_from_colors(colors.clone()) {
                    self.state
                        .popup_dialog
                        .set_dialog_error(Some(format!("Invalid theme color: {error}")));
                    return effects;
                }
                let Some((request_id, visit_id)) = self.state.async_ops.begin_theme_save_request()
                else {
                    return effects;
                };
                self.state.popup_dialog.set_dialog_error(None);
                effects.push(Effect::SaveTheme {
                    request_id,
                    visit_id,
                    themes_dir: self.config_dir.join("themes"),
                    name: write_name,
                    colors,
                });
                return effects;
            }
            SettingsTab::Keys => {
                // Detect conflicts before committing the binding.
                let Some(crate::state::SettingsField::Keys(row)) = field else {
                    return Vec::new();
                };
                if input.is_empty() {
                    ModalRouter::close_dialog(self.state);
                    self.state
                        .popup_dialog
                        .set_alert("Key binding cannot be empty".to_string());
                    return Vec::new();
                }
                // Detect duplicates with other bindings.
                let draft_snapshot = match self.state.popup_dialog.active_popup_ref() {
                    Some(Popup::Settings { draft, .. }) => draft.clone(),
                    _ => return Vec::new(),
                };
                let mut other_values = Vec::new();
                for other_row in crate::config::KeySettingsRow::ALL {
                    if other_row == row {
                        continue;
                    }
                    other_values
                        .push(settings::keys_value_at(&draft_snapshot, other_row).to_string());
                }
                if other_values.iter().any(|v| v == &input) {
                    ModalRouter::close_dialog(self.state);
                    ModalRouter::set_alert(
                        self.state,
                        format!("Key '{input}' conflicts with another action"),
                    );
                    return Vec::new();
                }
                // A configurable binding that shadows a hardcoded global action
                // (help, settings, artwork toggle, lyrics, speed map, focus
                // cycle) would make that action unreachable. Refuse it clearly
                // instead of silently losing a feature the user never expected
                // to move. The field being edited is exempted: `lyrics` hosting
                // `shift+L` is the default, not a conflict.
                if let Some(action) = settings::hardcoded_global_action_for(input.as_str(), row) {
                    ModalRouter::close_dialog(self.state);
                    ModalRouter::set_alert(
                        self.state,
                        format!("Key '{input}' is reserved for '{action}' and cannot be remapped"),
                    );
                    return Vec::new();
                }
                if let Some(Popup::Settings { draft, .. }) =
                    self.state.popup_dialog.active_popup_mut()
                {
                    if settings::keys_set_at(draft, row, input.clone()).is_err() {
                        ModalRouter::close_dialog(self.state);
                        ModalRouter::set_alert(
                            self.state,
                            format!("Invalid key binding '{input}'"),
                        );
                        return Vec::new();
                    }
                }
                let keys = match self.state.popup_dialog.active_popup_ref() {
                    Some(Popup::Settings { draft, .. }) => draft.keys_draft.clone(),
                    _ => KeysConfig::default(),
                };
                self.config.keys = keys.clone();
                *self.keys = keys;
                *self.input_mapper = InputMapper::from_config(self.keys);
                return vec![self.save_config_effect()];
            }
            SettingsTab::Sound => {
                // The Sound tab has no text fields.
            }
            SettingsTab::Playback => {
                // The Playback tab has no text fields.
            }
        }
        ModalRouter::close_dialog(self.state);
        effects
    }

    fn save_config_effect(&mut self) -> Effect {
        let request_id = self.state.async_ops.begin_config_save();
        Effect::SaveConfig {
            request_id,
            config: self.config.clone(),
            config_dir: self.config_dir.to_path_buf(),
        }
    }

    fn persist_runtime_state_inner(&mut self, persist_browser_directory: bool) -> Vec<Effect> {
        if self.data_dir.as_os_str().is_empty() {
            return Vec::new();
        }
        let persisted = PersistedState {
            repeat_mode: self.state.playback_mode.repeat(),
            shuffle: self.state.playback_mode.shuffle(),
            last_playlist: self.state.active_playlist_name.clone(),
            last_track_path: self
                .state
                .persistence
                .last_track
                .as_ref()
                .map(crate::track::TrackLocation::to_persisted),
            last_track_position_ms: self.state.persistence.last_track_position_ms,
        };
        let state_request_id = self.state.async_ops.begin_runtime_state_save();
        let state_effect = Effect::SaveRuntimeState {
            request_id: state_request_id,
            state: persisted,
            data_dir: self.data_dir.to_path_buf(),
        };

        self.config.general.volume_percent = self.state.playback.volume_percent;
        self.config.general.confirm_quit = self.state.confirm_quit;
        self.config.general.resume_previous_track = self.state.persistence.resume_previous_track;
        self.config.general.artwork_visible = self.state.artwork.is_visible();
        self.config.general.show_hidden = self.state.browser.show_hidden;
        self.config.general.playlist_columns = self.state.playlist_columns.clone();
        self.config.general.now_playing_display = self.state.now_playing_display.clone();
        self.config.ui.border_type = *self.border_type;
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

    fn persist_runtime_state(&mut self) -> Vec<Effect> {
        self.persist_runtime_state_inner(true)
    }

    fn apply_theme_name(&mut self, name: String) -> Vec<Effect> {
        self.config.ui.theme = name.clone();
        let Some((request_id, visit_id)) = self.state.async_ops.begin_theme_request() else {
            return vec![self.save_config_effect()];
        };
        vec![
            Effect::LoadTheme {
                request_id,
                visit_id,
                themes_dir: self.config_dir.join("themes"),
                name,
                include_names: false,
                purpose: ThemeLoadPurpose::Apply,
            },
            self.save_config_effect(),
        ]
    }

    fn request_selected_theme_colors(&mut self) -> Option<Effect> {
        let name = {
            let Some(Popup::Settings { draft, .. }) = self.state.popup_dialog.active_popup_mut()
            else {
                return None;
            };
            let Some(name) = draft
                .selected_theme()
                .and_then(|index| draft.theme_names.get(index).cloned())
            else {
                return None;
            };
            if let Some((cached_name, cached_colors)) = &draft.loaded_theme_palette
                && cached_name == &name
            {
                draft.colors = cached_colors.clone();
                return None;
            }
            name
        };
        let (request_id, visit_id) = self.state.async_ops.begin_theme_request()?;
        Some(Effect::LoadTheme {
            request_id,
            visit_id,
            themes_dir: self.config_dir.join("themes"),
            name,
            include_names: false,
            purpose: ThemeLoadPurpose::SettingsPreview,
        })
    }

    /// Apply the currently highlighted output and persist the selection.
    pub(crate) fn apply_selected_output(&mut self) -> Vec<Effect> {
        let selected = match self.state.popup_dialog.active_popup_ref() {
            Some(Popup::Settings { draft, .. }) => draft
                .selected_output()
                .and_then(|index| draft.outputs.get(index).cloned()),
            _ => None,
        };
        let Some(selected) = selected else {
            return Vec::new();
        };
        if let Err(error) = self.output_provider.accept_selection(&selected.id) {
            self.state
                .popup_dialog
                .set_alert(format!("Could not set output: {error}"));
            return Vec::new();
        }
        let target = OutputTarget::from(&selected);
        self.config.sound.output_sink_id = selected.id;
        self.push_notification(format!("Output set to {}", selected.name));
        // Route the live stream to the chosen PipeWire sink node.
        vec![
            self.save_config_effect(),
            Effect::Audio(AudioCommand::SetOutput(target)),
        ]
    }
    pub(crate) fn apply_settings_inner(&mut self) -> Vec<Effect> {
        let draft = match self.state.popup_dialog.active_popup_ref() {
            Some(Popup::Settings { draft, .. }) => draft.clone(),
            _ => return Vec::new(),
        };
        let mut effects = Vec::new();
        let selected_output = draft
            .selected_output()
            .and_then(|index| draft.outputs.get(index))
            .cloned()
            .unwrap_or_else(AudioOutput::session_default);
        let unrelated_settings_changed = draft.confirm_quit != self.state.confirm_quit
            || draft.resume_previous_track != self.state.persistence.resume_previous_track
            || draft.show_hidden != self.state.browser.show_hidden
            || draft.playlist_columns != draft.playlist_columns_initial
            || crate::playlist::sorter::sort_config_effective_changed(
                &draft.now_playing_display_initial,
                &draft.now_playing_display,
            )
            || draft.keys_draft != self.config.keys
            || draft.remote_lyrics != self.config.playback.remote_lyrics
            || crate::audio::playback::gain_changed(self.config.playback.gain_db, draft.gain_db)
            || draft.crossfade_seconds != self.config.playback.crossfade_seconds
            || draft.border_type != *self.border_type
            || selected_output.id != self.config.sound.output_sink_id;

        // Apply the hidden-file filter before dispatching any re-listing.
        let hidden_changed = self.state.browser.show_hidden != draft.show_hidden;
        self.state.confirm_quit = draft.confirm_quit;
        self.state.persistence.resume_previous_track = draft.resume_previous_track;
        self.state.browser.show_hidden = draft.show_hidden;
        *self.border_type = draft.border_type;
        self.config.ui.border_type = draft.border_type;

        // Validate and apply the browser directory. Re-list only when the
        // directory truly changes; re-reading the current directory wastes I/O.
        let empty_dir = draft.browser_directory.trim().is_empty();
        let target_dir = PathBuf::from(&draft.browser_directory);
        let dir_changed = !empty_dir && target_dir != self.state.browser.current_dir;
        let mut dir_refreshed = false;

        if empty_dir {
            self.push_notification(
                "Browser directory cannot be empty — keeping previous".to_string(),
            );
        } else if dir_changed {
            effects.push(request_browser_directory(&mut self.state, target_dir, None));
            dir_refreshed = true;
        }

        // If the hidden-file filter changed without moving directories, refresh
        // once. A directory change above already used the current `show_hidden`.
        if hidden_changed && !dir_refreshed {
            let current_dir = self.state.browser.current_dir.clone();
            effects.push(request_browser_directory(
                &mut self.state,
                current_dir,
                None,
            ));
        }

        // Playlist columns only affect presentation and the explicit reorder
        // action. Closing Settings persists the presentation config but never
        // changes queue order implicitly.
        self.config.general.playlist_columns = draft.playlist_columns.clone();
        self.state.playlist_columns = draft.playlist_columns.clone();

        // Now playing display: persist when effectively changed; the band just
        // uses it for labelling, so no queue mutation is needed.
        if crate::playlist::sorter::sort_config_effective_changed(
            &draft.now_playing_display_initial,
            &draft.now_playing_display,
        ) {
            self.config.general.now_playing_display = draft.now_playing_display.clone();
            self.state.now_playing_display = draft.now_playing_display.clone();
        } else {
            self.state.now_playing_display = draft.now_playing_display_initial.clone();
        }

        // Reload the key mapper. Defer the config save until the end so one
        // settings edit performs one write to `config.toml`.
        self.config.keys = draft.keys_draft.clone();
        *self.keys = draft.keys_draft.clone();
        *self.input_mapper = InputMapper::from_config(self.keys);

        // Persist the remote-lyrics toggle and preamp gain, forwarding them to
        // shared services only when their values actually change.
        let remote_changed = self.config.playback.remote_lyrics != draft.remote_lyrics;
        self.config.playback.remote_lyrics = draft.remote_lyrics;
        if remote_changed {
            effects.push(Effect::SetLyricsRemote(draft.remote_lyrics));
        }
        let gain_changed =
            crate::audio::playback::gain_changed(self.config.playback.gain_db, draft.gain_db);
        self.config.playback.gain_db = draft.gain_db;
        if gain_changed {
            effects.push(Effect::Audio(AudioCommand::SetGain(draft.gain_db)));
        }
        let crossfade_changed = self.config.playback.crossfade_seconds != draft.crossfade_seconds;
        self.config.playback.crossfade_seconds = draft.crossfade_seconds;
        if crossfade_changed {
            effects.push(Effect::Audio(AudioCommand::SetCrossfade(
                draft.crossfade_seconds,
            )));
        }

        // Sound: persist the stable selection and route through the audio worker.
        let output_changed = selected_output.id != self.config.sound.output_sink_id;
        let mut output_error = None;
        if output_changed {
            if let Err(error) = self.output_provider.accept_selection(&selected_output.id) {
                output_error = Some(format!("Could not set output: {error}"));
            } else {
                let target = OutputTarget::from(&selected_output);
                self.config.sound.output_sink_id = selected_output.id;
                // Only rebuild the output stream when the destination
                // actually changed. Re-sending SetOutput with the same
                // node tears down and reopens the PipeWire stream.
                effects.push(Effect::Audio(AudioCommand::SetOutput(target)));
            }
        }

        // Appearance: closing Settings with Esc does not apply or save the
        // theme. Only two paths apply it: Enter on a theme name in the list
        // (`apply_theme_name`), or saving the editor with the `-custom` suffix
        // (also through `apply_theme_name`).

        // Save unrelated settings immediately. A changed browser directory is
        // persisted separately by the accepted async listing.
        if dir_refreshed && unrelated_settings_changed {
            effects.extend(self.persist_runtime_state_inner(false));
        } else if !dir_refreshed {
            effects.extend(self.persist_runtime_state());
        }

        ModalRouter::clear(self.state);
        if let Some(error) = output_error {
            ModalRouter::set_alert(self.state, error);
        }
        self.push_notification("Settings saved".to_string());
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SettingsDraft;

    fn draft() -> SettingsDraft {
        SettingsDraft::default()
    }

    #[test]
    fn reserved_keys_are_rejected_outside_the_owning_slot() {
        for binding in crate::input::RESERVED {
            assert_eq!(
                hardcoded_global_action_for(
                    binding.settings_key,
                    crate::config::KeySettingsRow::Quit
                ),
                Some(binding.action),
                "{} must be rejected as reserved",
                binding.settings_key
            );
        }

        assert_eq!(
            hardcoded_global_action_for("shift+L", crate::config::KeySettingsRow::Lyrics),
            None
        );
        assert_eq!(
            hardcoded_global_action_for("h", crate::config::KeySettingsRow::PlayPause),
            None
        );
        assert_eq!(
            hardcoded_global_action_for("ctrl+h", crate::config::KeySettingsRow::PlayPause),
            Some("toggle help")
        );
        assert_eq!(
            hardcoded_global_action_for("shift+S", crate::config::KeySettingsRow::PlayPause),
            Some("open Add Stream")
        );
        assert_eq!(
            hardcoded_global_action_for("shift+C", crate::config::KeySettingsRow::PlayPause),
            Some("open settings")
        );
        assert_eq!(
            hardcoded_global_action_for("c", crate::config::KeySettingsRow::PlayPause),
            None
        );
        assert_eq!(
            hardcoded_global_action_for("f", crate::config::KeySettingsRow::PlayPause),
            None
        );
    }

    #[test]
    fn color_at_and_set_round_trip_every_slot() {
        let mut d = draft();
        for (index, field) in crate::ui::theme::ThemeColorField::ALL
            .into_iter()
            .enumerate()
        {
            let value = format!("#{:02x}00ff", index * 7);
            appearance_set_color(&mut d, field, value.clone());
            assert_eq!(appearance_color_at(&d, field), value);
        }
        assert_eq!(crate::ui::theme::ThemeColorField::from_index(99), None);
    }

    #[test]
    fn lyrics_color_slots_are_editable_at_the_end_of_the_list() {
        let mut d = draft();
        appearance_set_color(
            &mut d,
            crate::ui::theme::ThemeColorField::LyricsText,
            "#b8bb26".into(),
        );
        appearance_set_color(
            &mut d,
            crate::ui::theme::ThemeColorField::LyricsHighlight,
            "magenta".into(),
        );
        appearance_set_color(
            &mut d,
            crate::ui::theme::ThemeColorField::LyricsBackground,
            "blue".into(),
        );
        appearance_set_color(
            &mut d,
            crate::ui::theme::ThemeColorField::LyricsBorder,
            "indexed:9".into(),
        );
        appearance_set_color(
            &mut d,
            crate::ui::theme::ThemeColorField::LyricsBorderFocused,
            "magenta".into(),
        );
        assert_eq!(d.colors.lyrics_text, "#b8bb26");
        assert_eq!(d.colors.lyrics_highlight, "magenta");
        assert_eq!(d.colors.lyrics_background, "blue");
        assert_eq!(d.colors.lyrics_border, "indexed:9");
        assert_eq!(d.colors.lyrics_border_focused, "magenta");
        assert_eq!(
            appearance_color_at(&d, crate::ui::theme::ThemeColorField::LyricsText),
            "#b8bb26"
        );
        assert_eq!(
            appearance_color_at(&d, crate::ui::theme::ThemeColorField::LyricsBorder),
            "indexed:9"
        );
        assert_eq!(
            appearance_color_at(&d, crate::ui::theme::ThemeColorField::LyricsBorderFocused),
            "magenta"
        );
    }

    #[test]
    fn color_cursor_clamps_at_the_last_theme_field() {
        let mut app = crate::app::App::new();
        let mut d = draft();
        d.appearance_column = crate::state::AppearanceColumn::Colors;
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: SettingsTab::Appearance,
            focus: crate::state::SettingsFocus::Content,
            draft: d,
        });

        // Walk well past the end of the list: the cursor must stop on the
        // last editable field (the lyrics_border row), never beyond it.
        for _ in 0..(crate::ui::theme::ThemeColorField::ALL.len() + 10) {
            app.handle_settings_key(Command::CursorDown);
        }
        let Some(Popup::Settings { draft, .. }) = app.active_popup_ref() else {
            panic!("settings popup must still be open");
        };
        assert_eq!(
            draft.appearance_color_field,
            crate::ui::theme::ThemeColorField::LyricsBorderFocused
        );
    }

    #[test]
    fn keys_cursor_clamps_at_both_canonical_boundaries() {
        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: SettingsTab::Keys,
            focus: crate::state::SettingsFocus::Content,
            draft: draft(),
        });

        for _ in 0..(crate::config::KeySettingsRow::ALL.len() + 10) {
            app.handle_settings_key(Command::CursorDown);
        }
        let Some(Popup::Settings { draft, .. }) = app.active_popup_ref() else {
            panic!("settings popup must still be open");
        };
        assert_eq!(draft.keys_field, crate::config::KeySettingsRow::Lyrics);

        for _ in 0..(crate::config::KeySettingsRow::ALL.len() + 10) {
            app.handle_settings_key(Command::CursorUp);
        }
        let Some(Popup::Settings { draft, .. }) = app.active_popup_ref() else {
            panic!("settings popup must still be open");
        };
        assert_eq!(draft.keys_field, crate::config::KeySettingsRow::Quit);
    }

    #[test]
    fn bundled_theme_is_written_as_custom() {
        let mut d = draft();
        d.theme_names = vec!["default".into(), "gruvbox".into(), "mine".into()];
        d.appearance_theme_field = crate::state::SettingsField::AppearanceTheme(2);
        assert_eq!(settings_theme_write_name(&d), "mine");
        d.appearance_theme_field = crate::state::SettingsField::AppearanceTheme(0);
        assert_eq!(settings_theme_write_name(&d), "default-custom");
    }

    #[test]
    fn key_fields_round_trip_through_the_draft() {
        let mut d = draft();
        keys_set_at(&mut d, crate::config::KeySettingsRow::Next, "j".into()).unwrap();
        assert_eq!(keys_value_at(&d, crate::config::KeySettingsRow::Next), "j");
    }

    #[test]
    fn invalid_key_edit_is_rejected_at_the_settings_boundary() {
        let mut d = draft();
        assert!(
            keys_set_at(
                &mut d,
                crate::config::KeySettingsRow::Next,
                "not_a_real_key".into()
            )
            .is_err()
        );
        assert_eq!(keys_value_at(&d, crate::config::KeySettingsRow::Next), "n");
    }

    #[test]
    fn gain_change_policy_treats_exact_values_as_unchanged() {
        let gain = crate::audio::GainDb::try_from(3.0).unwrap();
        assert!(!crate::audio::playback::gain_changed(gain, gain));
    }

    #[test]
    fn gain_change_policy_has_no_unrepresentable_float_noise() {
        let gain = crate::audio::GainDb::try_from(3.0).unwrap();
        assert!(!crate::audio::playback::gain_changed(gain, gain));
    }

    #[test]
    fn gain_change_policy_detects_a_meaningful_slider_change() {
        let previous = crate::audio::GainDb::try_from(3.0).unwrap();
        let current = crate::audio::GainDb::try_from(3.5).unwrap();
        assert!(crate::audio::playback::gain_changed(previous, current));
    }

    #[test]
    fn gain_noise_does_not_trigger_unrelated_persistence() {
        let root = crate::test_support::unique_temp_dir("settings-gain-noise");
        let mut app = crate::app::App::new();
        app.set_config_paths(
            crate::config::AppConfig::default(),
            root.to_path_buf(),
            root.to_path_buf(),
        );

        let themes_dir = root.to_path_buf();
        let mut settings_draft = SettingsDraft::from_state(app.state(), &app.config, &themes_dir);
        settings_draft.browser_directory = root.join("browser").display().to_string();
        settings_draft.gain_db = crate::audio::GainDb::default();
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: SettingsTab::Playback,
            focus: crate::state::SettingsFocus::Content,
            draft: settings_draft,
        });

        let effects = app.handle_settings_key(Command::CancelPopup);

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Audio(AudioCommand::SetGain(_))))
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::SaveRuntimeState { .. }))
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::SaveConfig { .. }))
        );
    }

    #[test]
    fn gain_noise_does_not_mask_unrelated_settings_persistence() {
        let root = crate::test_support::unique_temp_dir("settings-gain-policy");
        let mut app = crate::app::App::new();
        app.set_config_paths(
            crate::config::AppConfig::default(),
            root.to_path_buf(),
            root.to_path_buf(),
        );

        let mut settings_draft = SettingsDraft::default();
        settings_draft.browser_directory = root.join("browser").display().to_string();
        settings_draft.gain_db = crate::audio::GainDb::default();
        settings_draft.remote_lyrics = true;
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: SettingsTab::Playback,
            focus: crate::state::SettingsFocus::Content,
            draft: settings_draft,
        });

        let effects = app.handle_settings_key(Command::CancelPopup);

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::SetLyricsRemote(true)))
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Audio(AudioCommand::SetGain(_))))
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::SaveRuntimeState { .. }))
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::SaveConfig { .. }))
        );
    }

    #[test]
    fn border_selection_updates_runtime_immediately_and_persists_on_save() {
        let root = crate::test_support::unique_temp_dir("settings-border-type");
        let mut app = crate::app::App::new();
        app.set_config_paths(
            crate::config::AppConfig::default(),
            root.to_path_buf(),
            root.to_path_buf(),
        );
        let settings_draft = SettingsDraft {
            appearance_column: crate::state::AppearanceColumn::Display,
            appearance_display_field: crate::state::AppearanceDisplayField::Border(
                crate::config::BorderType::Rounded,
            ),
            ..SettingsDraft::default()
        };
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: SettingsTab::Appearance,
            focus: crate::state::SettingsFocus::Content,
            draft: settings_draft,
        });

        app.handle_settings_key(Command::ConfirmDialog);
        assert_eq!(app.border_type(), crate::config::BorderType::Rounded);

        let effects = app.handle_settings_key(Command::CancelPopup);
        let services = crate::runtime::AppServices::new().expect("services construction");
        app.execute_effects(effects, &services);
        let mut config_saved = false;
        let mut state_saved = false;
        for _ in 0..2 {
            let event = services
                .events()
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("persistence completion");
            let operation_id = match event {
                crate::event::AppEvent::ConfigSaved { operation_id, .. } => {
                    config_saved = true;
                    operation_id
                }
                crate::event::AppEvent::RuntimeStateSaved {
                    operation_id,
                    result: Ok(true),
                    ..
                } => {
                    state_saved = true;
                    operation_id
                }
                other => panic!("expected persistence completion, got {other:?}"),
            };
            services.release_operation_event(Some(operation_id));
        }
        assert!(config_saved);
        assert!(state_saved);
        assert!(app.active_popup().is_none());
        assert_eq!(
            crate::config::AppConfig::load(&root).ui.border_type,
            crate::config::BorderType::Rounded
        );
        services.shutdown();
    }
}
