//! Search capability controller.
//!
//! Search owns only the query popup lifecycle and its result staleness gate.
//! The application remains the state owner and delegates these narrow
//! operations through the controller.

use super::*;

#[derive(Debug, Default)]
pub(crate) struct SearchController;

impl SearchController {
    pub(crate) fn start(&self, state: &mut AppState) -> Vec<Effect> {
        let Some(Popup::SearchQuery { scope }) = state.popup_dialog.active_popup_ref().cloned()
        else {
            return Vec::new();
        };

        let request_id = state.async_ops.next_search_request_id.saturating_add(1);
        state.async_ops.next_search_request_id = request_id;
        let query = state
            .popup_dialog
            .search_query()
            .unwrap_or_default()
            .to_string();
        let (root, tracks) = match scope {
            SearchScope::Browser => (state.browser.current_dir.clone(), Vec::new()),
            SearchScope::Playlist => (PathBuf::new(), state.playlist.tracks().to_vec()),
        };
        ModalRouter::replace_popup(state, Popup::SearchLoading { scope, request_id });

        vec![Effect::Search {
            request_id,
            scope,
            root,
            query,
            tracks,
        }]
    }

    pub(crate) fn close(&self, state: &mut AppState) {
        if matches!(
            state.popup_dialog.active_popup_ref(),
            Some(
                Popup::SearchQuery { .. }
                    | Popup::SearchLoading { .. }
                    | Popup::SearchResults { .. }
            )
        ) {
            ModalRouter::clear(state);
        }
    }

    pub(crate) fn apply_completed(
        &self,
        state: &mut AppState,
        request_id: u64,
        scope: SearchScope,
        results: Vec<SearchResult>,
        message: Option<crate::error::WorkerError>,
    ) {
        let current = matches!(
            state.popup_dialog.active_popup_ref().cloned(),
            Some(Popup::SearchLoading {
                scope: current_scope,
                request_id: current_id,
            }) if current_scope == scope && current_id == request_id
        );
        if !current {
            return;
        }
        if let Some(message) = message {
            push_notification(
                &mut state.notifications,
                format!("Search failed: {message}"),
                NOTIFICATIONS_CAP,
            );
        }
        ModalRouter::replace_popup(
            state,
            Popup::SearchResults {
                scope,
                request_id,
                results,
                cursor: 0,
            },
        );
    }

    pub(crate) fn move_cursor(&self, state: &mut AppState, down: bool) {
        let Some(Popup::SearchResults {
            results, cursor, ..
        }) = state.popup_dialog.active_popup_mut()
        else {
            return;
        };
        if results.is_empty() {
            *cursor = 0;
        } else if down {
            *cursor = (*cursor + 1).min(results.len() - 1);
        } else {
            *cursor = cursor.saturating_sub(1);
        }
    }

    pub(crate) fn reveal(&self, state: &mut AppState) -> Vec<Effect> {
        let Some(Popup::SearchResults {
            scope,
            results,
            cursor,
            ..
        }) = state.popup_dialog.active_popup_ref().cloned()
        else {
            return Vec::new();
        };
        let Some(result) = results.get(cursor) else {
            return Vec::new();
        };

        match scope {
            SearchScope::Browser => {
                let Some(path) = result.identity.as_path().map(Path::to_path_buf) else {
                    push_notification(
                        &mut state.notifications,
                        "The search result is not a local browser path".to_string(),
                        NOTIFICATIONS_CAP,
                    );
                    self.close(state);
                    return Vec::new();
                };
                let parent = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                let restore_cursor_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
                let effect = request_browser_directory(state, parent, restore_cursor_name);
                ModalRouter::clear(state);
                return vec![effect];
            }
            SearchScope::Playlist => {
                if let Some(index) = state
                    .playlist
                    .tracks()
                    .iter()
                    .position(|track| track.track_location() == result.identity)
                {
                    state.playlist.select(index);
                    state.reanchor_playlist_scroll(crate::browser_state::ScrollDirection::Down);
                } else {
                    push_notification(
                        &mut state.notifications,
                        "The track is not in this playlist".to_string(),
                        NOTIFICATIONS_CAP,
                    );
                }
            }
        }
        self.close(state);
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_completion_does_not_replace_the_visible_search() {
        let controller = SearchController;
        let mut state = AppState::default();
        ModalRouter::open_search(&mut state, SearchScope::Playlist);
        state.popup_dialog.replace_popup(Popup::SearchLoading {
            scope: SearchScope::Playlist,
            request_id: 2,
        });

        controller.apply_completed(
            &mut state,
            1,
            SearchScope::Playlist,
            Vec::new(),
            Some(crate::error::WorkerError::message("search", "stale")),
        );

        assert!(matches!(
            state.popup_dialog.active_popup_ref(),
            Some(Popup::SearchLoading {
                scope: SearchScope::Playlist,
                request_id: 2,
            })
        ));
        assert!(state.notifications.is_empty());
    }

    #[test]
    fn cursor_movement_is_clamped_for_empty_and_non_empty_results() {
        let controller = SearchController;
        let mut state = AppState::default();
        state.popup_dialog.open_search(SearchScope::Playlist);
        state.popup_dialog.replace_popup(Popup::SearchResults {
            scope: SearchScope::Playlist,
            request_id: 1,
            results: Vec::new(),
            cursor: 9,
        });
        controller.move_cursor(&mut state, true);
        assert!(matches!(
            state.popup_dialog.active_popup_ref(),
            Some(Popup::SearchResults { cursor: 0, .. })
        ));
    }
}
