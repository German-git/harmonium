//! Typed routing for application-owned modal input.
//!
//! This seam decides which authority owns a key. It does not mutate modal
//! text or perform capability work. Dialog and search controllers keep those
//! responsibilities after the router has classified the event.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::command::Command;
use crate::state::{AppState, DialogMode, Popup};

/// The authority that owns a key event after modal precedence is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModalAction {
    /// Dismiss the one-shot alert without routing the key any further.
    DismissAlert,
    /// Send a modal key binding through the normal command/effect controller.
    Command(Command),
    /// Route a help-popup command to the help controller.
    Help(Command),
    /// Route a settings-popup command to the settings controller.
    Settings(Command),
    /// Route a sort-confirmation command to the sort-confirmation controller.
    ConfirmSortTracks(Command),
    /// Let the active free-text dialog controller consume the raw key.
    Dialog(DialogAction),
    /// Let the search controller consume the raw key.
    Search(SearchAction),
    /// A modal owns the key but has no action for it.
    Consume,
    /// No modal owns the key, so normal input routing may proceed.
    Normal,
}

/// Dialog controller input selected by [`ModalRouter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogAction {
    /// Forward the raw key to the dialog controller.
    Key,
}

/// Search controller input selected by [`ModalRouter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchAction {
    /// Forward the raw key to the search controller.
    Key,
}

/// Single authority for modal precedence and modal key classification.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ModalRouter;

/// Resolution target for an Enter event on modal confirmation chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmDialogTarget {
    Overwrite,
    Delete,
    Dialog,
    None,
}

impl ModalRouter {
    pub(crate) fn open_popup(state: &mut AppState, popup: Popup) {
        state.popup_dialog.open_popup(popup);
    }

    pub(crate) fn open_search(state: &mut AppState, scope: crate::search::SearchScope) {
        state.popup_dialog.open_search(scope);
    }

    pub(crate) fn open_dialog(
        state: &mut AppState,
        mode: DialogMode,
        input: String,
        extension: Option<String>,
    ) {
        state.popup_dialog.open_dialog(mode, input, extension);
    }

    pub(crate) fn push_popup(state: &mut AppState, popup: Popup) {
        state.popup_dialog.push_popup(popup);
    }

    pub(crate) fn replace_popup(state: &mut AppState, popup: Popup) {
        state.popup_dialog.replace_popup(popup);
    }

    pub(crate) fn close_dialog(state: &mut AppState) {
        state.popup_dialog.close_dialog();
    }

    pub(crate) fn close_modal(state: &mut AppState) {
        state.popup_dialog.close_modal();
    }

    pub(crate) fn clear(state: &mut AppState) {
        state.popup_dialog.clear();
    }

    pub(crate) fn dismiss_alert(state: &mut AppState) {
        state.popup_dialog.dismiss_alert();
    }

    pub(crate) fn set_alert(state: &mut AppState, message: String) {
        state.popup_dialog.set_alert(message);
    }

    pub(crate) fn confirm_dialog(state: &AppState) -> ConfirmDialogTarget {
        match state.popup_dialog.active_popup_ref() {
            Some(Popup::ConfirmOverwrite { .. }) => ConfirmDialogTarget::Overwrite,
            Some(Popup::ConfirmDelete { .. }) => ConfirmDialogTarget::Delete,
            _ if state.popup_dialog.dialog_mode_ref().is_some() => ConfirmDialogTarget::Dialog,
            _ => ConfirmDialogTarget::None,
        }
    }

    /// Classify one key without mutating state or invoking a controller.
    pub(crate) fn route(state: &AppState, key: KeyEvent) -> ModalAction {
        // Alerts are layered over every other surface and intentionally dismiss
        // on any terminal event, including release events.
        if state.popup_dialog.alert_message().is_some() {
            return ModalAction::DismissAlert;
        }

        // Search has its own editing and result-navigation vocabulary. It wins
        // over any stale dialog mode that might remain in the state while the
        // search popup is active.
        if state.popup_dialog.search_ref().is_some() {
            return ModalAction::Search(SearchAction::Key);
        }

        // A free-text dialog owns the key before the chrome of its underlying
        // popup, except for the explicit overwrite and rename-collision
        // warnings that temporarily sit above it.
        if state.popup_dialog.dialog_mode_ref().is_some()
            && !matches!(
                state.popup_dialog.active_popup_ref(),
                Some(Popup::ConfirmOverwrite { .. }) | Some(Popup::RenameCollision { .. })
            )
        {
            return ModalAction::Dialog(DialogAction::Key);
        }

        match state.popup_dialog.active_popup_ref() {
            Some(popup) => popup_action(key, popup),
            None => ModalAction::Normal,
        }
    }
}

/// Resolve the command vocabulary owned by popup chrome.
fn popup_action(key: KeyEvent, popup: &Popup) -> ModalAction {
    if key.kind != KeyEventKind::Press {
        return ModalAction::Consume;
    }

    let command = match popup {
        Popup::ConfirmQuit => match (key.code, key.modifiers) {
            (KeyCode::Char('y') | KeyCode::Char('Y'), _) => Some(Command::ConfirmQuitYes),
            (KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc, _) => {
                Some(Command::CancelPopup)
            }
            _ => None,
        },
        Popup::ConfirmSortTracks { .. } => match (key.code, key.modifiers) {
            (KeyCode::Enter, _) => Some(Command::ConfirmDialog),
            (KeyCode::Esc, _) => Some(Command::CancelPopup),
            _ => None,
        },
        Popup::Help { .. } => match (key.code, key.modifiers) {
            (KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q'), _) => Some(Command::CancelPopup),
            (KeyCode::Char('h'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Command::ToggleHelp)
            }
            (KeyCode::Char('?'), _) => Some(Command::ToggleHelp),
            (KeyCode::Char('j') | KeyCode::Down, _) => Some(Command::CursorDown),
            (KeyCode::Char('k') | KeyCode::Up, _) => Some(Command::CursorUp),
            (KeyCode::PageDown, _) => Some(Command::PageDown),
            (KeyCode::PageUp, _) => Some(Command::PageUp),
            (KeyCode::Char('g') | KeyCode::Home, _) => Some(Command::CursorTop),
            (KeyCode::Char('G') | KeyCode::End, _) => Some(Command::CursorBottom),
            _ => None,
        },
        Popup::PlaylistManager { .. } => match (key.code, key.modifiers) {
            (KeyCode::Up | KeyCode::Char('k'), _) => Some(Command::MovePlaylistManagerUp),
            (KeyCode::Down | KeyCode::Char('j'), _) => Some(Command::MovePlaylistManagerDown),
            (KeyCode::Home | KeyCode::Char('g'), _) => Some(Command::MovePlaylistManagerTop),
            (KeyCode::End | KeyCode::Char('G'), _) => Some(Command::MovePlaylistManagerBottom),
            (KeyCode::PageUp, _) => Some(Command::MovePlaylistManagerPageUp),
            (KeyCode::PageDown, _) => Some(Command::MovePlaylistManagerPageDown),
            (KeyCode::Enter, _) | (KeyCode::Char('l'), _) => Some(Command::LoadPlaylist),
            (KeyCode::Esc, _) | (KeyCode::Char('p'), _) => Some(Command::ClosePopup),
            (KeyCode::Char('d'), _) => Some(Command::DeletePlaylist),
            (KeyCode::Char('r'), _) => Some(Command::RenamePlaylist),
            _ => None,
        },
        Popup::ConfirmOverwrite { .. } | Popup::ConfirmDelete { .. } => {
            match (key.code, key.modifiers) {
                (KeyCode::Enter, _) | (KeyCode::Char('y'), _) | (KeyCode::Char('Y'), _) => {
                    Some(Command::ConfirmDialog)
                }
                (KeyCode::Esc, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('c'), _) => {
                    Some(Command::CancelPopup)
                }
                _ => None,
            }
        }
        Popup::RenameCollision { .. } => match (key.code, key.modifiers) {
            (KeyCode::Enter, _) | (KeyCode::Esc, _) => Some(Command::CancelPopup),
            _ => None,
        },
        Popup::SearchQuery { .. } | Popup::SearchLoading { .. } | Popup::SearchResults { .. } => {
            None
        }
        Popup::Settings { .. } => match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => Some(Command::CancelPopup),
            (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::BackTab, _) => {
                Some(Command::FocusPreviousPanel)
            }
            (KeyCode::Tab, _) => Some(Command::FocusNextPanel),
            (KeyCode::Left | KeyCode::Char('h'), KeyModifiers::NONE) => {
                Some(Command::SettingsPreviousColumn)
            }
            (KeyCode::Right | KeyCode::Char('l'), KeyModifiers::NONE) => {
                Some(Command::SettingsNextColumn)
            }
            (KeyCode::Up | KeyCode::Char('k'), _) => Some(Command::CursorUp),
            (KeyCode::Down | KeyCode::Char('j'), _) => Some(Command::CursorDown),
            (KeyCode::Enter, _) => Some(Command::ConfirmDialog),
            _ => None,
        },
    };

    command.map_or(ModalAction::Consume, |command| match popup {
        Popup::ConfirmSortTracks { .. } => ModalAction::ConfirmSortTracks(command),
        Popup::Help { .. } => ModalAction::Help(command),
        Popup::Settings { .. } => ModalAction::Settings(command),
        _ => ModalAction::Command(command),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::search::SearchScope;
    use crate::state::{DialogMode, SettingsDraft, SettingsFocus, SettingsTab};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn settings_popup() -> Popup {
        Popup::Settings {
            tab: SettingsTab::General,
            focus: SettingsFocus::Content,
            draft: SettingsDraft::default(),
        }
    }

    fn popup_cases() -> Vec<(&'static str, Popup, KeyEvent, ModalAction)> {
        vec![
            (
                "quit confirmation",
                Popup::ConfirmQuit,
                key(KeyCode::Char('y')),
                ModalAction::Command(Command::ConfirmQuitYes),
            ),
            (
                "sort confirmation",
                Popup::ConfirmSortTracks {
                    draft: SettingsDraft::default(),
                },
                key(KeyCode::Enter),
                ModalAction::ConfirmSortTracks(Command::ConfirmDialog),
            ),
            (
                "help",
                Popup::Help { scroll: 0 },
                key(KeyCode::PageDown),
                ModalAction::Help(Command::PageDown),
            ),
            (
                "playlist manager",
                Popup::PlaylistManager {
                    cursor: 0,
                    names: vec![],
                },
                key(KeyCode::Char('r')),
                ModalAction::Command(Command::RenamePlaylist),
            ),
            (
                "playlist manager page up",
                Popup::PlaylistManager {
                    cursor: 3,
                    names: vec!["A".to_string(); 5],
                },
                key(KeyCode::PageUp),
                ModalAction::Command(Command::MovePlaylistManagerPageUp),
            ),
            (
                "playlist manager page down",
                Popup::PlaylistManager {
                    cursor: 0,
                    names: vec!["A".to_string(); 5],
                },
                key(KeyCode::PageDown),
                ModalAction::Command(Command::MovePlaylistManagerPageDown),
            ),
            (
                "overwrite confirmation",
                Popup::ConfirmOverwrite {
                    name: "Mix".to_string(),
                },
                key(KeyCode::Esc),
                ModalAction::Command(Command::CancelPopup),
            ),
            (
                "delete confirmation",
                Popup::ConfirmDelete {
                    name: "Mix".to_string(),
                    cursor: Some(0),
                },
                key(KeyCode::Char('n')),
                ModalAction::Command(Command::CancelPopup),
            ),
            (
                "rename collision",
                Popup::RenameCollision {
                    existing: PathBuf::from("/music/existing.mp3"),
                    attempted: "existing.mp3".to_string(),
                },
                key(KeyCode::Enter),
                ModalAction::Command(Command::CancelPopup),
            ),
            (
                "search query",
                Popup::SearchQuery {
                    scope: SearchScope::Browser,
                },
                key(KeyCode::Char('x')),
                ModalAction::Search(SearchAction::Key),
            ),
            (
                "search loading",
                Popup::SearchLoading {
                    scope: SearchScope::Playlist,
                    request_id: 1,
                },
                key(KeyCode::Esc),
                ModalAction::Search(SearchAction::Key),
            ),
            (
                "search results",
                Popup::SearchResults {
                    scope: SearchScope::Playlist,
                    request_id: 1,
                    results: vec![],
                    cursor: 0,
                },
                key(KeyCode::Down),
                ModalAction::Search(SearchAction::Key),
            ),
            (
                "search results h remains search-owned",
                Popup::SearchResults {
                    scope: SearchScope::Playlist,
                    request_id: 1,
                    results: vec![],
                    cursor: 0,
                },
                key(KeyCode::Char('h')),
                ModalAction::Search(SearchAction::Key),
            ),
            (
                "settings",
                settings_popup(),
                key(KeyCode::Tab),
                ModalAction::Settings(Command::FocusNextPanel),
            ),
        ]
    }

    #[test]
    fn every_popup_has_one_modal_authority() {
        for (name, popup, key, expected) in popup_cases() {
            let mut state = AppState::default();
            match popup {
                popup @ Popup::ConfirmOverwrite { .. } => {
                    state
                        .popup_dialog
                        .open_dialog(DialogMode::SaveAs, String::new(), None);
                    state.popup_dialog.push_popup(popup);
                }
                popup @ Popup::ConfirmDelete { .. } => {
                    state.popup_dialog.open_popup(Popup::PlaylistManager {
                        cursor: 0,
                        names: Vec::new(),
                    });
                    state.popup_dialog.push_popup(popup);
                }
                popup @ Popup::RenameCollision { .. } => {
                    state.popup_dialog.open_dialog(
                        DialogMode::RenameFile {
                            path: PathBuf::from("/music/song.mp3"),
                            original_name: "song.mp3".to_string(),
                            error: None,
                        },
                        "song".to_string(),
                        Some(".mp3".to_string()),
                    );
                    state.popup_dialog.push_popup(popup);
                }
                popup => state.popup_dialog.open_popup(popup),
            }
            assert_eq!(ModalRouter::route(&state, key), expected, "{name}");
        }
    }

    #[test]
    fn unknown_popup_keys_are_consumed_instead_of_reaching_input_mapper() {
        let mut state = AppState::default();
        state.popup_dialog.open_popup(Popup::Help { scroll: 0 });

        assert_eq!(
            ModalRouter::route(&state, key(KeyCode::Char(' '))),
            ModalAction::Consume
        );
    }

    #[test]
    fn every_dialog_mode_routes_to_the_dialog_controller() {
        let modes = [
            DialogMode::RenameSaved,
            DialogMode::RenamePlaying,
            DialogMode::SaveAs,
            DialogMode::NewPlaylist,
            DialogMode::SettingsEdit,
            DialogMode::RenameFile {
                path: PathBuf::from("/music/song.mp3"),
                original_name: "song.mp3".to_string(),
                error: None,
            },
            DialogMode::EditMetadata {
                path: PathBuf::from("/music/song.mp3"),
                fields: Box::default(),
                cursor: 0,
                error: None,
                loading: false,
            },
            DialogMode::RenameStream {
                url: url::Url::parse("https://example.test/live").expect("valid test URL"),
                original_title: "Live".to_string(),
                error: None,
            },
            DialogMode::AddStream {
                error: None,
                loading: false,
            },
        ];

        for mode in modes {
            let mut state = AppState::default();
            state.popup_dialog.open_dialog(mode, String::new(), None);
            assert_eq!(
                ModalRouter::route(&state, key(KeyCode::Char('x'))),
                ModalAction::Dialog(DialogAction::Key)
            );
        }
    }

    #[test]
    fn modal_precedence_is_alert_then_search_then_dialog_then_popup() {
        let mut state = AppState::default();
        state.popup_dialog.open_search(SearchScope::Browser);
        state.popup_dialog.set_alert("invalid".to_string());
        assert_eq!(
            ModalRouter::route(&state, key(KeyCode::Char('x'))),
            ModalAction::DismissAlert
        );

        state.popup_dialog.dismiss_alert();
        assert_eq!(
            ModalRouter::route(&state, key(KeyCode::Char('x'))),
            ModalAction::Search(SearchAction::Key)
        );

        state.popup_dialog.clear();
        state
            .popup_dialog
            .open_dialog(DialogMode::SaveAs, String::new(), None);
        assert_eq!(
            ModalRouter::route(&state, key(KeyCode::Char('x'))),
            ModalAction::Dialog(DialogAction::Key)
        );

        state.popup_dialog.clear();
        state
            .popup_dialog
            .open_dialog(DialogMode::SaveAs, String::new(), None);
        state.popup_dialog.push_popup(Popup::ConfirmOverwrite {
            name: "Mix".to_string(),
        });
        assert_eq!(
            ModalRouter::route(&state, key(KeyCode::Enter)),
            ModalAction::Command(Command::ConfirmDialog)
        );
    }

    #[test]
    fn escape_is_modal_owned_and_normal_escape_is_not() {
        let mut state = AppState::default();
        state.popup_dialog.open_popup(Popup::Help { scroll: 0 });
        assert_eq!(
            ModalRouter::route(&state, key(KeyCode::Esc)),
            ModalAction::Help(Command::CancelPopup)
        );

        state.popup_dialog.clear();
        assert_eq!(
            ModalRouter::route(&state, key(KeyCode::Esc)),
            ModalAction::Normal
        );
    }
}
