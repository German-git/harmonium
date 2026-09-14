//! Central application command vocabulary.

/// Commands currently wired into the application.
///
/// Future command groups are reserved and must not be bound until their
/// feature phases land:
///
/// - library: open, refresh, toggle hidden files
/// - playlist management: sort, save, load
/// - ui: cycle themes, toggle lyrics panels
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Request application exit, honoring the confirm quit preference.
    Quit,
    /// Positive answer to the active confirmation dialog.
    ConfirmQuitYes,
    /// Dismiss the active popup or dialog without acting.
    CancelPopup,
    /// Move focus to the next panel.
    FocusNextPanel,
    /// Move focus to the previous panel.
    FocusPreviousPanel,

    // Browser navigation, active only while the browser panel is focused
    /// Move the browser cursor one entry up.
    CursorUp,
    /// Move the browser cursor one entry down.
    CursorDown,
    /// Jump the browser cursor to the first entry.
    CursorTop,
    /// Jump the browser cursor to the last entry.
    CursorBottom,
    /// Page the browser view one screen up.
    PageUp,
    /// Page the browser view one screen down.
    PageDown,
    /// Open the parent directory of the browser location.
    ParentDir,
    /// Activate the browser cursor entry: directories are opened while a
    /// regular file is offered as a single add, mirroring Termusic.
    EnterSelected,
    /// Toggle a mark on the browser cursor entry for batch queueing.
    ///
    /// HARMONIUM EXTENSION: Termusic v0.13.2 has no multi-select marks.
    ToggleMark,
    /// Queue every marked entry, or the cursor entry when nothing is marked.
    ///
    /// HARMONIUM EXTENSION: replaces the Termusic `a` library add-root
    /// binding which has no counterpart in this player.
    AddSelected,

    // File actions on the cursor entry (browser) or the current track
    // (playlist). Both resolve their target from the focused panel at
    // dispatch time, matching the AddSelected cursor/mark precedent.
    /// Rename the audio file under the browser cursor or the playlist cursor.
    RenameFile,
    /// Edit the tag metadata of the file under the browser or playlist cursor.
    EditMetadata,
    /// Open contextual search for the focused browser or playlist panel.
    OpenSearch,
    /// Open the "Add Stream" popup so the user can paste an HTTP(S) URL and
    /// queue it as a remote stream. Streams join the same playlist model as
    /// local files and resolve at playback time through the streaming
    /// resolver (YouTube, Radio Browser, Icecast/SHOUTcast, plain HTTP).
    AddStream,

    // Playback control following the audited Termusic v0.13.2 keymap
    /// Toggle between pause and resume for the active track.
    ///
    /// Space never starts playback from a stopped state so the command can
    /// never double as a play-selection shortcut.
    TogglePause,
    /// Advance to the next queue entry when one exists.
    NextTrack,
    /// Restart the current track deep into it, otherwise step back one
    /// queue entry.
    PreviousTrack,
    /// Raise output volume by one step.
    VolumeUp,
    /// Lower output volume by one step.
    VolumeDown,
    /// Increase the playback speed by one step.
    SpeedUp,
    /// Decrease the playback speed by one step.
    SpeedDown,
    /// Reset the playback speed to the default 1.0x.
    SpeedReset,
    /// Seek forward inside the playing track with an adaptive step.
    SeekForward,
    /// Seek backward inside the playing track with an adaptive step.
    SeekBackward,
    /// Play the entry under the playlist selection cursor.
    PlaySelected,
    /// Cycle the repeat mode through off, track and all.
    ///
    /// HARMONIUM EXTENSION: the audited Termusic map spends `r` on
    /// shuffling the visible list and keeps loop on another key, but those
    /// features do not exist in this player and its single LoopMode cannot
    /// express our two axis model, so `m` owns the playback order cycle
    /// here. The help surfaces describe this divergence honestly instead
    /// of claiming Termusic parity.
    CycleRepeat,
    /// Toggle the shuffle axis on or off.
    ///
    /// HARMONIUM EXTENSION: same reasoning as [`Command::CycleRepeat`],
    /// `s` is unclaimed in this player and becomes the shuffle switch.
    ToggleShuffle,

    // Playlist management, active only while the playlist panel is focused
    /// Swap the selected queue entry with its upper neighbour.
    SwapSelectedUp,
    /// Swap the selected queue entry with its lower neighbour.
    SwapSelectedDown,
    /// Remove the selected queue entry, never the underlying file.
    DeleteQueueEntry,
    /// Remove every queued entry, never the underlying files.
    ClearQueue,

    // Named-playlist management (T4)
    /// Open the saved-playlist manager popup.
    OpenPlaylistManager,
    /// Load the playlist selected in the manager popup into the queue.
    LoadPlaylist,
    /// Delete the selected saved playlist, or the playing playlist if closed.
    DeletePlaylist,
    /// Start renaming a saved or the playing playlist.
    RenamePlaylist,
    /// Start saving the current queue under a new name.
    SaveAsPlaylist,
    /// Start creating a brand new empty playlist and switch to it.
    NewPlaylist,
    /// Confirm the in-progress naming dialog with the typed text.
    ConfirmDialog,
    /// Move the cursor up in the playlist manager popup.
    MovePlaylistManagerUp,
    /// Move the cursor down in the playlist manager popup.
    MovePlaylistManagerDown,
    /// Jump to the first saved playlist in the manager popup.
    MovePlaylistManagerTop,
    /// Jump to the last saved playlist in the manager popup.
    MovePlaylistManagerBottom,
    /// Page up in the playlist manager popup.
    MovePlaylistManagerPageUp,
    /// Page down in the playlist manager popup.
    MovePlaylistManagerPageDown,
    /// Close the active popup without performing its action.
    ClosePopup,

    // Interface affordances
    /// Open the help popup, or close it when it is already on screen.
    ToggleHelp,
    /// Toggle artwork cell visibility.
    ToggleArtwork,
    /// Toggle the lyrics panel replacing the browser while it is visible.
    ToggleLyrics,
    /// Open the full-window settings popup.
    OpenSettings,
    /// Move to the next column inside the active settings tab.
    SettingsNextColumn,
    /// Move to the previous column inside the active settings tab.
    SettingsPreviousColumn,
}

/// Compatibility-normalized command groups used by the application reducer.
/// The public `Command` vocabulary remains accepted at input boundaries while
/// the reducer receives target-carrying groups instead of one flat match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandGroup {
    Lifecycle(LifecycleCommand),
    Navigation(NavigationCommand),
    File(FileCommand),
    Playback(PlaybackCommand),
    Queue(QueueCommand),
    Playlist(PlaylistCommand),
    Ui(UiCommand),
}

/// Runtime target captured before the reducer dispatches a grouped command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelTarget {
    Browser,
    Playlist,
    Lyrics,
}

/// Input context needed to build a target-carrying command group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandContext {
    pub active_panel: PanelTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleCommand {
    Quit,
    ConfirmQuitYes,
    CancelPopup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationCommand {
    FocusNextPanel,
    FocusPreviousPanel,
    CursorUp { target: PanelTarget },
    CursorDown { target: PanelTarget },
    CursorTop { target: PanelTarget },
    CursorBottom { target: PanelTarget },
    PageUp { target: PanelTarget },
    PageDown { target: PanelTarget },
    ParentDir { target: PanelTarget },
    EnterSelected { target: PanelTarget },
    ToggleMark { target: PanelTarget },
    AddSelected { target: PanelTarget },
    OpenSearch { target: PanelTarget },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCommand {
    RenameFile { target: PanelTarget },
    EditMetadata { target: PanelTarget },
    AddStream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackCommand {
    TogglePause,
    NextTrack,
    PreviousTrack,
    VolumeUp,
    VolumeDown,
    SpeedUp,
    SpeedDown,
    SpeedReset,
    SeekForward,
    SeekBackward,
    PlaySelected { target: PanelTarget },
    CycleRepeat,
    ToggleShuffle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueCommand {
    SwapSelectedUp { target: PanelTarget },
    SwapSelectedDown { target: PanelTarget },
    DeleteQueueEntry { target: PanelTarget },
    ClearQueue { target: PanelTarget },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistCommand {
    OpenPlaylistManager,
    LoadPlaylist,
    DeletePlaylist,
    RenamePlaylist,
    SaveAsPlaylist,
    NewPlaylist,
    ConfirmDialog,
    MovePlaylistManagerUp,
    MovePlaylistManagerDown,
    MovePlaylistManagerTop,
    MovePlaylistManagerBottom,
    MovePlaylistManagerPageUp,
    MovePlaylistManagerPageDown,
    ClosePopup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    ToggleHelp,
    ToggleArtwork,
    ToggleLyrics,
    OpenSettings,
    SettingsNextColumn,
    SettingsPreviousColumn,
}

impl Command {
    /// Convert legacy flat input into the grouped reducer vocabulary.
    pub fn into_grouped(self, context: CommandContext) -> CommandGroup {
        use Command::*;
        let target = context.active_panel;
        match self {
            Quit => CommandGroup::Lifecycle(LifecycleCommand::Quit),
            ConfirmQuitYes => CommandGroup::Lifecycle(LifecycleCommand::ConfirmQuitYes),
            CancelPopup => CommandGroup::Lifecycle(LifecycleCommand::CancelPopup),
            FocusNextPanel => CommandGroup::Navigation(NavigationCommand::FocusNextPanel),
            FocusPreviousPanel => CommandGroup::Navigation(NavigationCommand::FocusPreviousPanel),
            CursorUp => CommandGroup::Navigation(NavigationCommand::CursorUp { target }),
            CursorDown => CommandGroup::Navigation(NavigationCommand::CursorDown { target }),
            CursorTop => CommandGroup::Navigation(NavigationCommand::CursorTop { target }),
            CursorBottom => CommandGroup::Navigation(NavigationCommand::CursorBottom { target }),
            PageUp => CommandGroup::Navigation(NavigationCommand::PageUp { target }),
            PageDown => CommandGroup::Navigation(NavigationCommand::PageDown { target }),
            ParentDir => CommandGroup::Navigation(NavigationCommand::ParentDir { target }),
            EnterSelected => CommandGroup::Navigation(NavigationCommand::EnterSelected { target }),
            ToggleMark => CommandGroup::Navigation(NavigationCommand::ToggleMark { target }),
            AddSelected => CommandGroup::Navigation(NavigationCommand::AddSelected { target }),
            OpenSearch => CommandGroup::Navigation(NavigationCommand::OpenSearch { target }),
            RenameFile => CommandGroup::File(FileCommand::RenameFile { target }),
            EditMetadata => CommandGroup::File(FileCommand::EditMetadata { target }),
            AddStream => CommandGroup::File(FileCommand::AddStream),
            TogglePause => CommandGroup::Playback(PlaybackCommand::TogglePause),
            NextTrack => CommandGroup::Playback(PlaybackCommand::NextTrack),
            PreviousTrack => CommandGroup::Playback(PlaybackCommand::PreviousTrack),
            VolumeUp => CommandGroup::Playback(PlaybackCommand::VolumeUp),
            VolumeDown => CommandGroup::Playback(PlaybackCommand::VolumeDown),
            SpeedUp => CommandGroup::Playback(PlaybackCommand::SpeedUp),
            SpeedDown => CommandGroup::Playback(PlaybackCommand::SpeedDown),
            SpeedReset => CommandGroup::Playback(PlaybackCommand::SpeedReset),
            SeekForward => CommandGroup::Playback(PlaybackCommand::SeekForward),
            SeekBackward => CommandGroup::Playback(PlaybackCommand::SeekBackward),
            PlaySelected => CommandGroup::Playback(PlaybackCommand::PlaySelected { target }),
            CycleRepeat => CommandGroup::Playback(PlaybackCommand::CycleRepeat),
            ToggleShuffle => CommandGroup::Playback(PlaybackCommand::ToggleShuffle),
            SwapSelectedUp => CommandGroup::Queue(QueueCommand::SwapSelectedUp { target }),
            SwapSelectedDown => CommandGroup::Queue(QueueCommand::SwapSelectedDown { target }),
            DeleteQueueEntry => CommandGroup::Queue(QueueCommand::DeleteQueueEntry { target }),
            ClearQueue => CommandGroup::Queue(QueueCommand::ClearQueue { target }),
            OpenPlaylistManager => CommandGroup::Playlist(PlaylistCommand::OpenPlaylistManager),
            LoadPlaylist => CommandGroup::Playlist(PlaylistCommand::LoadPlaylist),
            DeletePlaylist => CommandGroup::Playlist(PlaylistCommand::DeletePlaylist),
            RenamePlaylist => CommandGroup::Playlist(PlaylistCommand::RenamePlaylist),
            SaveAsPlaylist => CommandGroup::Playlist(PlaylistCommand::SaveAsPlaylist),
            NewPlaylist => CommandGroup::Playlist(PlaylistCommand::NewPlaylist),
            ConfirmDialog => CommandGroup::Playlist(PlaylistCommand::ConfirmDialog),
            MovePlaylistManagerUp => CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerUp),
            MovePlaylistManagerDown => {
                CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerDown)
            }
            MovePlaylistManagerTop => {
                CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerTop)
            }
            MovePlaylistManagerBottom => {
                CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerBottom)
            }
            MovePlaylistManagerPageUp => {
                CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerPageUp)
            }
            MovePlaylistManagerPageDown => {
                CommandGroup::Playlist(PlaylistCommand::MovePlaylistManagerPageDown)
            }
            ClosePopup => CommandGroup::Playlist(PlaylistCommand::ClosePopup),
            ToggleHelp => CommandGroup::Ui(UiCommand::ToggleHelp),
            ToggleArtwork => CommandGroup::Ui(UiCommand::ToggleArtwork),
            ToggleLyrics => CommandGroup::Ui(UiCommand::ToggleLyrics),
            OpenSettings => CommandGroup::Ui(UiCommand::OpenSettings),
            SettingsNextColumn => CommandGroup::Ui(UiCommand::SettingsNextColumn),
            SettingsPreviousColumn => CommandGroup::Ui(UiCommand::SettingsPreviousColumn),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards against accidental variant duplication while the set grows.
    #[test]
    fn wired_command_set_is_explicit() {
        fn assert_wired(command: Command) {
            match command {
                Command::Quit
                | Command::ConfirmQuitYes
                | Command::CancelPopup
                | Command::FocusNextPanel
                | Command::FocusPreviousPanel
                | Command::CursorUp
                | Command::CursorDown
                | Command::CursorTop
                | Command::CursorBottom
                | Command::PageUp
                | Command::PageDown
                | Command::ParentDir
                | Command::EnterSelected
                | Command::ToggleMark
                | Command::AddSelected
                | Command::RenameFile
                | Command::EditMetadata
                | Command::OpenSearch
                | Command::AddStream
                | Command::TogglePause
                | Command::NextTrack
                | Command::PreviousTrack
                | Command::VolumeUp
                | Command::VolumeDown
                | Command::SpeedUp
                | Command::SpeedDown
                | Command::SpeedReset
                | Command::SeekForward
                | Command::SeekBackward
                | Command::PlaySelected
                | Command::CycleRepeat
                | Command::ToggleShuffle
                | Command::SwapSelectedUp
                | Command::SwapSelectedDown
                | Command::DeleteQueueEntry
                | Command::ClearQueue
                | Command::ToggleHelp
                | Command::ToggleArtwork
                | Command::ToggleLyrics
                | Command::OpenPlaylistManager
                | Command::LoadPlaylist
                | Command::DeletePlaylist
                | Command::RenamePlaylist
                | Command::SaveAsPlaylist
                | Command::NewPlaylist
                | Command::ConfirmDialog
                | Command::MovePlaylistManagerUp
                | Command::MovePlaylistManagerDown
                | Command::MovePlaylistManagerTop
                | Command::MovePlaylistManagerBottom
                | Command::MovePlaylistManagerPageUp
                | Command::MovePlaylistManagerPageDown
                | Command::ClosePopup
                | Command::OpenSettings
                | Command::SettingsNextColumn
                | Command::SettingsPreviousColumn => {}
            }
        }

        assert_wired(Command::AddStream);
    }

    #[test]
    fn legacy_commands_normalize_into_grouped_reducer_commands() {
        let context = CommandContext {
            active_panel: PanelTarget::Playlist,
        };
        assert_eq!(
            Command::VolumeUp.into_grouped(context),
            CommandGroup::Playback(PlaybackCommand::VolumeUp)
        );
        assert_eq!(
            Command::RenamePlaylist.into_grouped(context),
            CommandGroup::Playlist(PlaylistCommand::RenamePlaylist)
        );
        assert_eq!(
            Command::CursorDown.into_grouped(context),
            CommandGroup::Navigation(NavigationCommand::CursorDown {
                target: PanelTarget::Playlist
            })
        );
    }
}
