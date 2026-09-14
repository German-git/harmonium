//! Playlist naming dialogs (Save as, rename, new) applied to the app.
//!
//! Captures naming intent for worker-owned playlist operations while keeping
//! dialog transitions and conflict messages in the reducer.

use super::*;
use crate::app::effect::request_named_save_validation;

/// Dialog workflows operate on the state they mutate while keeping modal
/// transitions out of `App`'s command composition code.
pub(crate) struct DialogController<'app> {
    state: &'app mut AppState,
}

impl<'app> DialogController<'app> {
    pub(crate) fn new(state: &'app mut AppState) -> Self {
        Self { state }
    }
}

/// Insert `ch` into `buffer` at the character position `cursor` (clamped to
/// the current length) and advance the cursor by one.
///
/// Indices are tracked in characters, not bytes, so multibyte input never
/// lands between UTF-8 code units and the visible caret always sits on a
/// grapheme boundary. The cursor is clamped on the way in so a stale value
/// left over from a previous field cannot push the new character past the
/// end of the string.
pub(crate) fn insert_dialog_char(buffer: &mut String, cursor: &mut usize, ch: char) {
    let len = buffer.chars().count();
    let at = (*cursor).min(len);
    let byte_index = char_byte_index(buffer, at);
    buffer.insert(byte_index, ch);
    *cursor = at + 1;
}

/// Remove the character immediately before `cursor` (Backspace semantics)
/// and pull the cursor back by one when there is something to delete.
///
/// Calling this with the cursor at position 0 is a no-op, matching the
/// terminal behaviour where Backspace at the start of the line is ignored.
pub(crate) fn delete_dialog_char_before(buffer: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let at = *cursor - 1;
    let (start, end) = char_byte_range(buffer, at);
    buffer.replace_range(start..end, "");
    *cursor = at;
}

/// Remove the character at `cursor` (Delete semantics).
///
/// Out-of-range cursors are a no-op; if the cursor sits on the last
/// character this clears the tail without leaving the cursor dangling past
/// the end of the (now shorter) buffer.
pub(crate) fn delete_dialog_char_at(buffer: &mut String, cursor: usize) {
    let len = buffer.chars().count();
    if cursor >= len {
        return;
    }
    let (start, end) = char_byte_range(buffer, cursor);
    buffer.replace_range(start..end, "");
}

/// Return the byte offset where the `index`-th character starts.
///
/// `index` is clamped to the buffer length so out-of-range callers get a
/// safe "append at the end" position instead of panicking.
fn char_byte_index(buffer: &str, index: usize) -> usize {
    buffer
        .char_indices()
        .nth(index)
        .map(|(byte, _)| byte)
        .unwrap_or(buffer.len())
}

/// Return the byte range `[start, end)` occupied by the `index`-th
/// character, matching the same clamping rules as [`char_byte_index`].
fn char_byte_range(buffer: &str, index: usize) -> (usize, usize) {
    let len = buffer.chars().count();
    if index >= len {
        return (buffer.len(), buffer.len());
    }
    let mut iter = buffer.char_indices();
    let (start, _) = iter.nth(index).expect("in range");
    let end = iter.next().map(|(byte, _)| byte).unwrap_or(buffer.len());
    (start, end)
}

impl DialogController<'_> {
    pub(crate) fn handle_dialog_confirm(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mode = match self.state.popup_dialog.dialog_mode_ref() {
            Some(mode) => mode.clone(),
            None => return effects,
        };

        // File actions skip the playlist naming empty-name check: the rename
        // dialog validates its own trimmed input and the metadata editor may
        // legitimately save empty fields (which remove the tag)
        match mode {
            DialogMode::RenameFile { .. } => {
                return FileActionsController::new(self.state).commit_rename();
            }
            DialogMode::EditMetadata { .. } => {
                return FileActionsController::new(self.state).commit_metadata_save();
            }
            DialogMode::RenameStream { .. } => {
                return FileActionsController::new(self.state).commit_rename_stream();
            }
            _ => {}
        }

        let name = self
            .state
            .popup_dialog
            .dialog_input_ref()
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() {
            self.state
                .popup_dialog
                .set_dialog_error(Some("Name cannot be empty".to_string()));
            return effects;
        }

        match mode {
            DialogMode::SaveAs => {
                return request_named_save_validation(
                    &mut self.state,
                    PlaylistSaveAction::SaveAs,
                    name,
                );
            }
            DialogMode::NewPlaylist => {
                return request_named_save_validation(
                    &mut self.state,
                    PlaylistSaveAction::NewPlaylist,
                    name,
                );
            }
            DialogMode::RenamePlaying => {
                let old = self.state.active_playlist_name.clone();
                match old {
                    Some(old) => {
                        return start_playlist_rename(
                            &mut self.state,
                            PlaylistRenameAction::Playing,
                            old,
                            name,
                        );
                    }
                    None => {
                        return request_named_save_validation(
                            &mut self.state,
                            PlaylistSaveAction::RenamePlaying,
                            name,
                        );
                    }
                }
            }
            DialogMode::RenameSaved => {
                let old = match self.state.popup_dialog.active_popup_ref() {
                    Some(Popup::PlaylistManager { cursor, names }) => names.get(*cursor).cloned(),
                    _ => None,
                };
                if let Some(old) = old {
                    return start_playlist_rename(
                        &mut self.state,
                        PlaylistRenameAction::Saved,
                        old,
                        name,
                    );
                }
            }
            DialogMode::SettingsEdit => {
                // Settings field edits are handled via the settings commit path,
                // not the playlist naming flow. Reuse the same logic so Enter
                // while editing a settings field commits the field.
                return Vec::new();
            }
            DialogMode::AddStream { .. } => {
                // The popup validates its own URL with the `url` crate; an
                // invalid input is shown inside the dialog without closing
                // it. The resolution runs off the UI thread and posts back
                // through `AppEvent::StreamResolved`, which closes the popup
                // once the track is queued (or surfaces the resolver failure
                // inside the dialog on error).
                let url_input = self
                    .state
                    .popup_dialog
                    .dialog_input_ref()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let parsed = url::Url::parse(&url_input).or_else(|_| {
                    // Common typo: user typed `example.com/live` without the
                    // scheme. Prepend `https://` so the resolver gets a usable
                    // URL.
                    let with_scheme = format!("https://{url_input}");
                    url::Url::parse(&with_scheme)
                });
                let url = match parsed {
                    Ok(url) => url,
                    Err(parse_error) => {
                        // The current input supersedes any resolver that was
                        // still running, so its activity cannot outlive the
                        // invalid submission and keep a spinner active.
                        self.state.cancel_stream_resolution();
                        if let Some(DialogMode::AddStream { error, loading }) =
                            self.state.popup_dialog.dialog_mode_mut()
                        {
                            let detail = crate::net::normalize_diagnostic(
                                &parse_error.to_string(),
                                crate::error::MAX_WORKER_DIAGNOSTIC_CHARS,
                            );
                            *error = Some(format!("Invalid URL: {detail}"));
                            *loading = false;
                        }
                        return effects;
                    }
                };
                let (request_id, cancellation) = self.state.begin_stream_resolution();
                effects.push(Effect::ResolveStream {
                    request_id,
                    url,
                    cancellation,
                });
                return effects;
            }
            // The file actions return early above; these arms keep the match
            // exhaustive for the compiler
            DialogMode::RenameFile { .. }
            | DialogMode::EditMetadata { .. }
            | DialogMode::RenameStream { .. } => {
                unreachable!("handled before the naming flow")
            }
        }

        // Close the dialog and refresh the popup list when it is open.
        ModalRouter::close_dialog(self.state);
        effects
    }

    /// Perform the deferred overwrite after the ConfirmOverwrite warning.
    ///
    /// Reads the still-open naming dialog input, saves the playlist, then closes
    /// both the warning and the naming dialog. On failure it returns to the
    /// naming dialog with an error instead of closing.
    pub(crate) fn confirm_overwrite(&mut self) -> Vec<Effect> {
        let Some(Popup::ConfirmOverwrite { name }) = self.state.popup_dialog.active_popup_ref()
        else {
            return Vec::new();
        };
        let name = name.clone();
        let action = match self.state.popup_dialog.dialog_mode_ref() {
            Some(DialogMode::NewPlaylist) => PlaylistSaveAction::Overwrite { new_playlist: true },
            Some(DialogMode::SaveAs) => PlaylistSaveAction::Overwrite {
                new_playlist: false,
            },
            _ => return Vec::new(),
        };
        start_playlist_save(&mut self.state, action, name)
    }

    /// Perform the deferred delete after the ConfirmDelete warning.
    ///
    /// Reads the playlist name from the active popup, removes the file, and
    /// returns focus to the playlist manager popup with refreshed names. If
    /// the deleted playlist was the active one, the queue is cleared and a
    /// generated default name is assigned so the queue always has an owner.
    pub(crate) fn confirm_delete(&mut self) -> Vec<Effect> {
        start_playlist_delete(self.state)
    }

    /// Capture keystrokes for the naming dialog.
    pub(crate) fn handle_dialog_key_event(&mut self, key_event: KeyEvent) -> Vec<Effect> {
        if key_event.kind != KeyEventKind::Press {
            return Vec::new();
        }
        if self.state.async_ops.playlist_request.is_active() && key_event.code != KeyCode::Esc {
            return Vec::new();
        }
        if matches!(
            self.state.popup_dialog.dialog_mode_ref(),
            Some(DialogMode::EditMetadata { .. })
        ) {
            return self.handle_metadata_field_key(key_event);
        }
        match key_event.code {
            KeyCode::Enter
                if self.state.popup_dialog.dialog_mode_ref() == Some(&DialogMode::SettingsEdit) =>
            {
                Vec::new()
            }
            KeyCode::Enter => self.handle_dialog_confirm(),
            KeyCode::Esc => {
                self.state.async_ops.playlist_request.cancel();
                self.state.async_ops.file_rename_request.cancel();
                if self.state.popup_dialog.dialog_mode_ref() == Some(&DialogMode::SettingsEdit) {
                    self.state.async_ops.browser_validation_request.cancel();
                    self.state.async_ops.cancel_theme_requests();
                }
                let cancelling_stream = matches!(
                    self.state.popup_dialog.dialog_mode_ref(),
                    Some(DialogMode::AddStream { .. })
                );
                if cancelling_stream {
                    self.state.cancel_stream_resolution();
                    ModalRouter::close_modal(self.state);
                }
                ModalRouter::close_dialog(self.state);
                Vec::new()
            }
            KeyCode::Left => {
                let cursor = self.state.popup_dialog.dialog_cursor().unwrap_or(0);
                self.state
                    .popup_dialog
                    .set_dialog_cursor(cursor.saturating_sub(1));
                Vec::new()
            }
            KeyCode::Right => {
                let len = self
                    .state
                    .popup_dialog
                    .dialog_input_ref()
                    .unwrap_or_default()
                    .chars()
                    .count();
                let cursor = self.state.popup_dialog.dialog_cursor().unwrap_or(0);
                self.state
                    .popup_dialog
                    .set_dialog_cursor((cursor + 1).min(len));
                Vec::new()
            }
            KeyCode::Home => {
                self.state.popup_dialog.set_dialog_cursor(0);
                Vec::new()
            }
            KeyCode::End => {
                let len = self
                    .state
                    .popup_dialog
                    .dialog_input_ref()
                    .unwrap_or_default()
                    .chars()
                    .count();
                self.state.popup_dialog.set_dialog_cursor(len);
                Vec::new()
            }
            KeyCode::Backspace => {
                self.edit_dialog_buffer(|buffer, cursor| {
                    delete_dialog_char_before(buffer, cursor);
                });
                self.clear_dialog_errors();
                Vec::new()
            }
            KeyCode::Delete => {
                self.edit_dialog_buffer(|buffer, cursor| {
                    delete_dialog_char_at(buffer, *cursor);
                });
                self.clear_dialog_errors();
                Vec::new()
            }
            KeyCode::Char(c) if !key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.edit_dialog_buffer(|buffer, cursor| {
                    insert_dialog_char(buffer, cursor, c);
                });
                self.clear_dialog_errors();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn clear_dialog_errors(&mut self) {
        if let Some(DialogMode::RenameFile { error, .. }) =
            self.state.popup_dialog.dialog_mode_mut()
        {
            *error = None;
        }
        self.state.popup_dialog.set_dialog_error(None);
    }

    /// Capture keystrokes for the metadata editor form.
    pub(crate) fn handle_metadata_field_key(&mut self, key_event: KeyEvent) -> Vec<Effect> {
        let Some(DialogMode::EditMetadata {
            cursor, loading, ..
        }) = self.state.popup_dialog.dialog_mode_mut()
        else {
            return Vec::new();
        };
        let (cursor, loading) = (*cursor, *loading);

        if key_event.code == KeyCode::Esc {
            ModalRouter::close_dialog(self.state);
            return Vec::new();
        }
        if loading {
            return Vec::new();
        }

        match key_event.code {
            KeyCode::Enter => {
                self.commit_metadata_field(cursor);
                self.handle_dialog_confirm()
            }
            KeyCode::Left => {
                let cursor = self.state.popup_dialog.dialog_cursor().unwrap_or(0);
                self.state
                    .popup_dialog
                    .set_dialog_cursor(cursor.saturating_sub(1));
                Vec::new()
            }
            KeyCode::Right => {
                let len = self
                    .state
                    .popup_dialog
                    .dialog_input_ref()
                    .unwrap_or_default()
                    .chars()
                    .count();
                let cursor = self.state.popup_dialog.dialog_cursor().unwrap_or(0);
                self.state
                    .popup_dialog
                    .set_dialog_cursor((cursor + 1).min(len));
                Vec::new()
            }
            KeyCode::Home => {
                self.state.popup_dialog.set_dialog_cursor(0);
                Vec::new()
            }
            KeyCode::End => {
                let len = self
                    .state
                    .popup_dialog
                    .dialog_input_ref()
                    .unwrap_or_default()
                    .chars()
                    .count();
                self.state.popup_dialog.set_dialog_cursor(len);
                Vec::new()
            }
            KeyCode::Backspace => {
                self.edit_dialog_buffer(|buffer, cursor| {
                    delete_dialog_char_before(buffer, cursor);
                });
                Vec::new()
            }
            KeyCode::Delete => {
                self.edit_dialog_buffer(|buffer, cursor| {
                    delete_dialog_char_at(buffer, *cursor);
                });
                Vec::new()
            }
            KeyCode::Char(c) if !key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.edit_dialog_buffer(|buffer, cursor| {
                    insert_dialog_char(buffer, cursor, c);
                });
                Vec::new()
            }
            KeyCode::Up => {
                self.move_metadata_cursor(cursor.saturating_sub(1));
                Vec::new()
            }
            KeyCode::Down => {
                self.move_metadata_cursor(cursor.saturating_add(1).min(9));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn move_metadata_cursor(&mut self, next: usize) {
        let draft = self
            .state
            .popup_dialog
            .dialog_input_ref()
            .unwrap_or_default()
            .to_string();
        let next_value = {
            let Some(DialogMode::EditMetadata { cursor, fields, .. }) =
                self.state.popup_dialog.dialog_mode_mut()
            else {
                return;
            };
            let current = *cursor;
            if current == next {
                return;
            }
            fields[current] = draft;
            *cursor = next;
            fields[next].clone()
        };
        let cursor = next_value.chars().count();
        if let Some(input) = self.state.popup_dialog.dialog_input_mut() {
            *input = next_value;
        }
        self.state.popup_dialog.set_dialog_cursor(cursor);
    }

    fn edit_dialog_buffer<F>(&mut self, edit: F)
    where
        F: FnOnce(&mut String, &mut usize),
    {
        let mut buffer = self
            .state
            .popup_dialog
            .dialog_input_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut cursor = self.state.popup_dialog.dialog_cursor().unwrap_or(0);
        edit(&mut buffer, &mut cursor);
        if let Some(input) = self.state.popup_dialog.dialog_input_mut() {
            *input = buffer;
        }
        self.state.popup_dialog.set_dialog_cursor(cursor);
    }

    fn commit_metadata_field(&mut self, cursor: usize) {
        let draft = self
            .state
            .popup_dialog
            .dialog_input_ref()
            .unwrap_or_default()
            .to_string();
        if let Some(DialogMode::EditMetadata { fields, .. }) =
            self.state.popup_dialog.dialog_mode_mut()
        {
            fields[cursor] = draft;
        }
    }
}
