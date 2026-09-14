//! Saved-playlist and queue capability controller.
//!
//! The controller owns playlist-specific transitions while `App` continues to
//! own the state, store, command vocabulary, and effect execution boundary.

use super::*;

#[derive(Debug, Default)]
pub(crate) struct PlaylistController;

impl PlaylistController {
    pub(crate) fn autosave_active(&self, state: &AppState) -> Vec<Effect> {
        match &state.active_playlist_name {
            Some(name) => vec![Effect::SaveActivePlaylist {
                name: name.clone(),
                contents: crate::playlist::render_m3u(&state.playlist),
            }],
            None => Vec::new(),
        }
    }

    pub(crate) fn load_metadata_for_queue(&self, state: &AppState) -> Vec<Effect> {
        let paths: Vec<PathBuf> = state
            .playlist
            .tracks()
            .iter()
            .filter(|track| track.is_local())
            .filter_map(|track| track.path().map(Path::to_path_buf))
            .collect();
        if paths.is_empty() {
            Vec::new()
        } else {
            vec![Effect::LoadMetadata(paths)]
        }
    }

    pub(crate) fn open_manager(&self, state: &mut AppState) {
        ModalRouter::open_popup(
            state,
            Popup::PlaylistManager {
                cursor: 0,
                names: Vec::new(),
            },
        );
    }

    pub(crate) fn load_selected(&self, state: &mut AppState) -> Vec<Effect> {
        let selected = match state.popup_dialog.active_popup_ref() {
            Some(Popup::PlaylistManager { cursor, names }) => names.get(*cursor).cloned(),
            _ => None,
        };
        let Some(name) = selected else {
            return Vec::new();
        };

        if state.active_playlist_name.as_deref() == Some(name.as_str()) {
            ModalRouter::clear(state);
            return Vec::new();
        }

        let Some(request_id) = state.async_ops.playlist_request.try_begin() else {
            return Vec::new();
        };

        vec![Effect::LoadPlaylistNamed { request_id, name }]
    }

    pub(crate) fn begin_delete(&self, state: &mut AppState) {
        let (selected_name, source_cursor) = match state.popup_dialog.active_popup_ref() {
            Some(Popup::PlaylistManager { cursor, names }) => {
                (names.get(*cursor).cloned(), Some(*cursor))
            }
            _ => (state.active_playlist_name.clone(), None),
        };
        if let Some(name) = selected_name {
            ModalRouter::push_popup(
                state,
                Popup::ConfirmDelete {
                    name,
                    cursor: source_cursor,
                },
            );
        }
    }

    pub(crate) fn begin_rename(&self, state: &mut AppState) {
        let mode = match state.popup_dialog.active_popup_ref() {
            Some(Popup::PlaylistManager { cursor, names }) => {
                names.get(*cursor).map(|_| DialogMode::RenameSaved)
            }
            _ => match &state.active_playlist_name {
                Some(_) => Some(DialogMode::RenamePlaying),
                None => Some(DialogMode::SaveAs),
            },
        };
        if let Some(mode) = mode {
            ModalRouter::open_dialog(state, mode, String::new(), None);
        }
    }

    pub(crate) fn begin_save_as(&self, state: &mut AppState) {
        let default = state.active_playlist_name.clone().unwrap_or_default();
        ModalRouter::open_dialog(state, DialogMode::SaveAs, default, None);
    }

    pub(crate) fn begin_new(&self, state: &mut AppState) {
        ModalRouter::open_dialog(state, DialogMode::NewPlaylist, String::new(), None);
    }

    pub(crate) fn move_manager_up(&self, state: &mut AppState) {
        state.popup_dialog.move_manager_up();
    }

    pub(crate) fn move_manager_down(&self, state: &mut AppState) {
        state.popup_dialog.move_manager_down();
    }

    pub(crate) fn move_manager_top(&self, state: &mut AppState) {
        state.popup_dialog.move_manager_top();
    }

    pub(crate) fn move_manager_bottom(&self, state: &mut AppState) {
        state.popup_dialog.move_manager_bottom();
    }

    pub(crate) fn page_manager_up(&self, state: &mut AppState) {
        state.popup_dialog.page_manager_up();
    }

    pub(crate) fn page_manager_down(&self, state: &mut AppState) {
        state.popup_dialog.page_manager_down();
    }
}
