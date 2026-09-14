//! Orchestration of the file-level rename and metadata edit actions.
//!
//! These are the only file actions in the app: they resolve a single target
//! (the browser cursor entry or the current playlist track), validate the
//! user input and dispatch the background effects. Disk IO itself lives in
//! the `Effect` workers, so everything here stays pure and unit testable.

use std::path::PathBuf;

use super::*;
use crate::filesystem::safety::validate_rename_name;

/// File actions operate on the state they mutate without inheriting the whole
/// application coordinator.
pub(crate) struct FileActionsController<'app> {
    state: &'app mut AppState,
}

impl<'app> FileActionsController<'app> {
    pub(crate) fn new(state: &'app mut AppState) -> Self {
        Self { state }
    }
}

/// Split a file name into the editable base and the locked extension.
///
/// The base is everything before the *last* `.` in the name, the extension
/// is the trailing run including the dot. Names with no extension return an
/// empty extension suffix. Dot-files (`.hidden`) are returned as the base
/// alone, without any extension, because their first character is the
/// leading dot rather than a separator. The split is purely textual so it
/// matches what the renderer shows and what the user expects to see when
/// they open the rename popup.
///
/// Returns `(base, Option<extension_with_dot>)`.
pub(crate) fn split_file_name(name: &str) -> (String, Option<String>) {
    let trimmed = name.trim_end_matches(['/', '\\']);
    if let Some((stem, ext)) = trimmed.rsplit_once('.')
        && !stem.is_empty()
        && !ext.is_empty()
    {
        return (stem.to_string(), Some(format!(".{ext}")));
    }
    (trimmed.to_string(), None)
}

impl FileActionsController<'_> {
    fn push_notification(&mut self, message: String) {
        push_notification(&mut self.state.notifications, message, NOTIFICATIONS_CAP);
    }

    /// Resolve the single file a file action should affect.
    ///
    /// From the browser this is the cursor entry and only when it is a
    /// supported audio file (marked entries are deliberately ignored: these
    /// actions are cursor-only by spec). From the playlist it is the current
    /// track, which is always audio by construction. Returns `None` when
    /// there is nothing to act on, or when the target is a stream track
    /// (which has no file path on disk to rename or tag).
    pub(crate) fn resolve_file_action_target(state: &AppState) -> Option<PathBuf> {
        match state.active_panel {
            Panel::Browser => {
                let entry = state.browser.entries.get(state.browser.cursor())?;
                if entry.kind != EntryKind::File || !is_supported_audio(&entry.path) {
                    return None;
                }
                Some(crate::track::resolve_process_relative_path(
                    entry.path.clone(),
                ))
            }
            Panel::Playlist => state
                .playlist
                .current()
                .and_then(|track| track.path().map(Path::to_path_buf)),
            Panel::Lyrics => None,
        }
    }

    /// Resolve the current playlist track as a stream when one is on top.
    ///
    /// Mirrors [`Self::resolve_file_action_target`] but returns the stream
    /// URL (and detected kind) for the rename / edit-metadata shortcuts
    /// that have to update the EXTINF label rather than touch the disk.
    /// Returning `None` for local tracks keeps the two flows separate and
    /// avoids silent no-ops on the wrong kind.
    pub(crate) fn resolve_stream_action_target(
        state: &AppState,
    ) -> Option<crate::stream::StreamTarget> {
        if state.active_panel != Panel::Playlist {
            return None;
        }
        let track = state.playlist.current()?;
        let url = match track.source() {
            crate::stream::TrackSource::Stream { url, .. } => url.clone(),
            crate::stream::TrackSource::Local(_) => return None,
        };
        let kind = track
            .stream_kind()
            .unwrap_or(crate::stream::StreamKind::Http);
        Some(crate::stream::StreamTarget { url, kind })
    }

    /// Open the rename dialog for a stream track.
    ///
    /// The dialog reuses the same free-text input buffer as the local
    /// rename popup, but the commit path dispatches
    /// [`Effect::UpdateStreamExtinf`] so every saved playlist picks up
    /// the new EXTINF label instead of moving a file. Preloaded with the
    /// track's current display name so the user can edit in place.
    pub(crate) fn open_stream_rename_dialog(
        &mut self,
        url: url::Url,
        kind: crate::stream::StreamKind,
    ) {
        let _ = kind;
        let original_title = self
            .state
            .playlist
            .current()
            .map(|track| track.display_name().into_owned())
            .unwrap_or_default();
        ModalRouter::open_dialog(
            self.state,
            DialogMode::RenameStream {
                url,
                original_title: original_title.clone(),
                error: None,
            },
            original_title,
            None,
        );
    }

    /// Commit the open stream-rename dialog: validate, then dispatch the
    /// title update across every saved playlist.
    ///
    /// Streams have no on-disk file to move: the rename is a pure
    /// metadata rewrite that updates the EXTINF label in saved `.m3u8`
    /// documents and the in-memory snapshot used by the UI. A blank
    /// title keeps the dialog open with a validation error so the user
    /// can fix the typo without losing the URL.
    pub(crate) fn commit_rename_stream(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(DialogMode::RenameStream {
            url,
            original_title,
            ..
        }) = self.state.popup_dialog.dialog_mode_ref().cloned()
        else {
            return effects;
        };
        let trimmed = self
            .state
            .popup_dialog
            .dialog_input_ref()
            .unwrap_or_default()
            .trim()
            .to_string();
        if trimmed.is_empty() {
            if let Some(DialogMode::RenameStream { error, .. }) =
                self.state.popup_dialog.dialog_mode_mut()
            {
                *error = Some("Stream title cannot be empty".to_string());
            }
            return effects;
        }
        if trimmed == original_title {
            // No-op close so the user does not pay a disk write for an
            // unchanged title.
            self.close_dialog();
            return effects;
        }
        // Update the in-memory snapshot so the playlist row reflects the
        // rename immediately, before the disk pass lands.
        if let Some(track) = self.state.playlist.current_mut() {
            track.set_title(&trimmed);
        }
        self.close_dialog();
        effects.push(Effect::UpdateStreamExtinf {
            url,
            new_title: trimmed,
        });
        effects
    }

    /// Commit the open rename dialog: validate, then dispatch the disk work.
    ///
    /// The reducer performs only textual validation. Collision, symlink,
    /// canonicalization and no-overwrite checks are owned by the worker so the
    /// dialog never makes a filesystem decision from stale UI-thread state.
    pub(crate) fn commit_rename(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(DialogMode::RenameFile {
            path,
            original_name,
            ..
        }) = self.state.popup_dialog.dialog_mode_ref()
        else {
            return effects;
        };
        let path = path.clone();
        let original_name = original_name.clone();
        // The locked extension stays attached to whatever base name the user
        // typed; commit assembles the final file name by joining them so the
        // extension can never be dropped or rewritten from this dialog.
        let extension = self
            .state
            .popup_dialog
            .dialog_extension()
            .clone()
            .unwrap_or_default();

        let trimmed_base = self
            .state
            .popup_dialog
            .dialog_input_ref()
            .unwrap_or_default()
            .trim()
            .to_string();
        if trimmed_base.is_empty() {
            self.set_rename_error("The file name cannot be empty".to_string());
            return effects;
        }
        let name = format!("{trimmed_base}{extension}");
        if name == original_name {
            // Nothing changed: close without touching the filesystem
            self.close_dialog();
            return effects;
        }

        if let Err(error) = validate_rename_name(&name) {
            self.set_rename_error(error.to_string());
            return effects;
        }

        let request_id = self.state.async_ops.file_rename_request.begin();
        self.close_dialog();
        effects.push(Effect::RenameFileOnDisk {
            request_id,
            from: path,
            new_name: name,
            browser_dir: self.state.browser.current_dir.clone(),
        });
        effects
    }

    /// Apply a typed worker result while preserving the existing collision and
    /// validation dialog UX.
    pub(crate) fn apply_rename_result_completed(
        &mut self,
        from: PathBuf,
        to: PathBuf,
        result: RenameFileResult,
    ) -> Vec<Effect> {
        match result {
            RenameFileResult::Success { refresh_browser } => {
                self.apply_rename_completed(from, to, true, None, refresh_browser)
            }
            RenameFileResult::Conflict => {
                let attempted = to
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.restore_rename_dialog(&from, &to);
                ModalRouter::push_popup(
                    self.state,
                    Popup::RenameCollision {
                        existing: to,
                        attempted,
                    },
                );
                Vec::new()
            }
            RenameFileResult::Rejected(message) => {
                self.restore_rename_dialog(&from, &to);
                self.set_rename_error(message.to_string());
                Vec::new()
            }
            RenameFileResult::Failed(message) => {
                self.push_notification(format!("Rename failed: {message}"));
                Vec::new()
            }
        }
    }

    fn restore_rename_dialog(&mut self, from: &Path, to: &Path) {
        let original_name = from
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let attempted_name = to
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (base, extension) = split_file_name(&attempted_name);
        ModalRouter::open_dialog(
            self.state,
            DialogMode::RenameFile {
                path: from.to_path_buf(),
                original_name,
                error: None,
            },
            base,
            extension,
        );
    }

    /// Commit the open metadata editor: dispatch the tag write effect.
    ///
    /// The form has already committed every field via Enter; this final
    /// confirmation closes the dialog and emits the write effect. The
    /// completion event reports the outcome (a failure keeps the old tags,
    /// since the file was never touched).
    pub(crate) fn commit_metadata_save(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(DialogMode::EditMetadata { path, fields, .. }) =
            self.state.popup_dialog.dialog_mode_ref()
        else {
            return effects;
        };
        let path = path.clone();
        let fields = (**fields).clone();

        self.close_dialog();
        effects.push(Effect::EditMetadataWrite { path, fields });
        effects
    }

    /// Store a validation error inside the open rename dialog.
    fn set_rename_error(&mut self, message: String) {
        if let Some(DialogMode::RenameFile { error, .. }) =
            self.state.popup_dialog.dialog_mode_mut()
        {
            *error = Some(message);
        }
    }

    /// Close the active dialog and reset its shared input state.
    pub(crate) fn close_dialog(&mut self) {
        if matches!(
            self.state.popup_dialog.dialog_mode_ref(),
            Some(DialogMode::AddStream { .. })
        ) {
            self.state.cancel_stream_resolution();
        }
        ModalRouter::clear(self.state);
    }

    /// Apply a finished rename to the in-memory queue and the browser.
    pub(crate) fn apply_rename_completed(
        &mut self,
        from: PathBuf,
        to: PathBuf,
        ok: bool,
        conflict: Option<PathBuf>,
        refresh_browser: bool,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if !ok {
            match conflict {
                Some(existing) => {
                    let attempted = to
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ModalRouter::push_popup(
                        self.state,
                        Popup::RenameCollision {
                            existing,
                            attempted,
                        },
                    );
                }
                None => self.push_notification("Rename failed".to_string()),
            }
            return Vec::new();
        }

        self.state.playlist = self.state.playlist.rewrite_path(&from, &to);

        let from_location = TrackLocation::local(from.clone());
        let resume_matches_local =
            self.state.persistence.last_track.as_ref() == Some(&from_location);
        if resume_matches_local {
            self.state.persistence.last_track = Some(TrackLocation::local(to.clone()));
        }

        if refresh_browser {
            let current = self.state.browser.current_dir.clone();
            effects.push(request_browser_directory(&mut self.state, current, None));
        }

        let name = to
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| to.to_string_lossy().into_owned());
        self.push_notification(format!("Renamed to {name}"));

        let needs_extinf_update = self
            .state
            .playlist
            .tracks()
            .iter()
            .find(|track| track.track_location() == TrackLocation::local(&to))
            .and_then(|track| track.metadata())
            .is_none_or(|meta| !meta.title_tagged);
        if needs_extinf_update
            && let Some(new_name) = to
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        {
            effects.push(Effect::UpdateExtinfTitle {
                path: to,
                new_title: new_name,
            });
        }

        effects
    }

    /// Apply the outcome of a metadata write to the in-memory tracks.
    pub(crate) fn apply_metadata_write_completed(
        &mut self,
        path: PathBuf,
        result: crate::error::WorkerResult<()>,
        fields: [String; 10],
    ) -> Vec<Effect> {
        if let Err(error) = result {
            self.push_notification(format!("Could not save tags: {error}"));
            return Vec::new();
        }
        self.push_notification("Tags updated".to_string());

        let mut effects = vec![Effect::LoadMetadata(vec![path.clone()])];
        let new_title = fields[0].trim();
        if !new_title.is_empty() {
            effects.push(Effect::UpdateExtinfTitle {
                path,
                new_title: new_title.to_string(),
            });
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KeysConfig;
    use crate::playlist::PlaylistStore;
    use std::fs;

    fn app_with_rename_dialog(path: PathBuf, original_name: &str, input: &str) -> App {
        let (base, extension) = split_file_name(input);
        let mut app = App::from_config_and_store(
            KeysConfig::default(),
            PlaylistStore::for_dir(PathBuf::new()),
        );
        app.state.popup_dialog.open_dialog(
            DialogMode::RenameFile {
                path,
                original_name: original_name.to_string(),
                error: None,
            },
            base,
            extension,
        );
        app
    }

    #[test]
    fn split_file_name_separates_the_locked_extension() {
        assert_eq!(
            split_file_name("song.mp3"),
            ("song".to_string(), Some(".mp3".to_string()))
        );
        assert_eq!(
            split_file_name("track.flac"),
            ("track".to_string(), Some(".flac".to_string()))
        );
        assert_eq!(
            split_file_name("song.wav"),
            ("song".to_string(), Some(".wav".to_string()))
        );
        // Names with multiple dots only lock the trailing extension
        assert_eq!(
            split_file_name("my.song.ogg"),
            ("my.song".to_string(), Some(".ogg".to_string()))
        );
        // Names without an extension stay whole
        assert_eq!(split_file_name("song"), ("song".to_string(), None));
        // Dot-files are NOT treated as such (their leading dot is the name)
        assert_eq!(split_file_name(".hidden"), (".hidden".to_string(), None));
        // A trailing dot without an extension leaves the whole string whole
        // (the extension is empty so we cannot meaningfully lock it).
        assert_eq!(split_file_name("song."), ("song.".to_string(), None));
    }

    #[test]
    fn rename_file_locked_extension_is_preserved_on_commit() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("song.mp3"), b"audio").expect("fixture");
        let source = dir.path().join("song.mp3");

        // The base name changes, but the locked .mp3 extension is restored
        // when the rename hits disk
        let mut app = app_with_rename_dialog(source.clone(), "song.mp3", "track.mp3");
        let effects = app.commit_rename();
        assert_eq!(
            effects,
            vec![Effect::RenameFileOnDisk {
                request_id: 1,
                from: source.clone(),
                new_name: "track.mp3".to_string(),
                browser_dir: PathBuf::from("."),
            }],
            "the commit appends the locked extension"
        );
    }

    #[test]
    fn rename_file_locked_extension_cannot_be_edited() {
        let app = app_with_rename_dialog(PathBuf::from("/music/song.wav"), "song.wav", "song.wav");
        assert_eq!(
            app.state().popup_dialog.dialog_input_value(),
            "song",
            "base name only"
        );
        assert_eq!(
            app.state().popup_dialog.dialog_extension(),
            Some(".wav"),
            "extension stays locked"
        );
        // The cursor sits inside the editable buffer at the end of the base
        assert_eq!(
            app.state().popup_dialog.dialog_cursor().unwrap_or(0),
            "song".chars().count(),
            "cursor never crosses into the locked extension"
        );
    }

    #[test]
    fn commit_rename_rejects_an_empty_name_without_an_effect() {
        let mut app = app_with_rename_dialog(PathBuf::from("/music/song.wav"), "song.wav", "   ");

        let effects = app.commit_rename();

        assert!(effects.is_empty(), "no disk work may be dispatched");
        let Some(DialogMode::RenameFile { error, .. }) = app.state().popup_dialog.dialog_mode_ref()
        else {
            panic!("the dialog must stay open");
        };
        assert!(
            error.as_deref().is_some_and(|message| !message.is_empty()),
            "the dialog must show a validation error"
        );
    }

    #[test]
    fn commit_rename_rejects_path_syntax_without_filesystem_validation() {
        let mut app = app_with_rename_dialog(
            PathBuf::from("/missing/source/song.wav"),
            "song.wav",
            "../escape.wav",
        );

        let effects = app.commit_rename();

        assert!(effects.is_empty());
        assert!(matches!(
            app.state().popup_dialog.dialog_mode_ref(),
            Some(DialogMode::RenameFile {
                error: Some(message),
                ..
            }) if message.contains("path separators")
        ));
    }

    #[test]
    fn commit_rename_reports_a_collision_without_overwriting() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("song.wav"), b"audio").expect("source fixture");
        fs::write(dir.path().join("solo.wav"), b"audio").expect("collision fixture");
        let source = dir.path().join("song.wav");

        // The user typed the base "solo" with the .wav extension locked. The
        // reducer must dispatch intent; the worker owns the collision check.
        let mut app = app_with_rename_dialog(source, "song.wav", "solo.wav");
        let effects = app.commit_rename();

        assert!(matches!(
            effects.as_slice(),
            [Effect::RenameFileOnDisk {
                request_id: 1,
                new_name,
                ..
            }] if new_name == "solo.wav"
        ));
        assert!(app.state().popup_dialog.active_popup_ref().is_none());
        assert!(app.state().popup_dialog.dialog_mode_ref().is_none());
        assert!(dir.path().join("song.wav").exists(), "source untouched");
        assert!(dir.path().join("solo.wav").exists(), "target untouched");
    }

    #[test]
    fn commit_rename_dispatches_the_rename_effect_for_a_valid_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("song.wav"), b"audio").expect("fixture");
        let source = dir.path().join("song.wav");

        // The user typed the base "hit" and the helper seeds the locked .wav
        // extension; the commit must rebuild the on-disk name by appending it.
        let mut app = app_with_rename_dialog(source.clone(), "song.wav", "hit.wav");
        assert_eq!(app.state().popup_dialog.dialog_extension(), Some(".wav"));
        assert_eq!(
            app.state().popup_dialog.dialog_input_value(),
            "hit",
            "only the base is editable"
        );
        let effects = app.commit_rename();

        assert_eq!(
            effects,
            vec![Effect::RenameFileOnDisk {
                request_id: 1,
                from: source.clone(),
                new_name: "hit.wav".to_string(),
                browser_dir: PathBuf::from("."),
            }]
        );
        assert!(
            app.state().popup_dialog.dialog_mode_ref().is_none(),
            "dialog closes on success"
        );
        assert!(
            dir.path().join("song.wav").exists(),
            "the effect is async: the file is not renamed here"
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_rename_rejects_an_unchanged_name_as_a_no_op() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("song.wav"), b"audio").expect("fixture");

        let mut app = app_with_rename_dialog(dir.path().join("song.wav"), "song.wav", "song.wav");
        let effects = app.commit_rename();

        assert!(effects.is_empty());
        assert!(
            app.state().popup_dialog.dialog_mode_ref().is_none(),
            "unchanged name just closes"
        );
        assert!(dir.path().join("song.wav").exists());
    }

    #[test]
    fn commit_metadata_save_dispatches_the_write_effect_and_closes() {
        let path = PathBuf::from("/music/song.wav");
        let mut fields: [String; 10] = Default::default();
        fields[0] = "Tom Sawyer".to_string();
        let mut app = App::new();
        app.state.popup_dialog.open_dialog(
            DialogMode::EditMetadata {
                path: path.clone(),
                fields: Box::new(fields.clone()),
                cursor: 0,
                error: None,
                loading: false,
            },
            "Tom Sawyer".to_string(),
            None,
        );

        let effects = app.commit_metadata_save();

        assert_eq!(effects, vec![Effect::EditMetadataWrite { path, fields }]);
        assert!(
            app.state().popup_dialog.dialog_mode_ref().is_none(),
            "dialog closes on save"
        );
    }

    #[test]
    fn resolve_target_is_cursor_only_and_skips_non_audio_entries() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("song.wav"), b"audio").expect("audio fixture");
        fs::write(dir.path().join("notes.txt"), b"text").expect("text fixture");

        let mut app = App::from_config_and_store(
            KeysConfig::default(),
            PlaylistStore::for_dir(PathBuf::new()),
        );
        app.state_mut().change_browser_dir(
            dir.path().to_path_buf(),
            crate::filesystem::read_sorted_entries(dir.path(), false).expect("listing"),
        );

        // The browser sorts directories first, then files
        let notes = app
            .state()
            .browser
            .entries
            .iter()
            .position(|entry| entry.name == "notes.txt")
            .expect("notes entry");
        app.state_mut().browser.set_cursor(notes);
        assert_eq!(
            app.resolve_file_action_target(),
            None,
            "a text file must not be a rename target"
        );

        let song = app
            .state()
            .browser
            .entries
            .iter()
            .position(|entry| entry.name == "song.wav")
            .expect("song entry");
        app.state_mut().browser.set_cursor(song);
        assert_eq!(
            app.resolve_file_action_target(),
            Some(dir.path().join("song.wav"))
        );

        // From the playlist the current track is the target
        app.state_mut().active_panel = Panel::Playlist;
        app.state_mut()
            .extend_playlist([dir.path().join("song.wav")]);
        app.state_mut().playlist.select(0);
        assert_eq!(
            app.resolve_file_action_target(),
            Some(dir.path().join("song.wav"))
        );
    }

    #[test]
    fn relative_browser_rename_uses_the_same_absolute_identity_as_direct_add() {
        let base = std::env::current_dir().expect("working directory");
        let old = base.join("browser-relative-song.wav");
        let new = base.join("browser-relative-renamed.wav");
        let mut app = App::new();
        app.state_mut().active_panel = Panel::Browser;
        app.state_mut().change_browser_dir(
            PathBuf::from("."),
            vec![crate::filesystem::FileEntry::new(
                "browser-relative-song.wav",
                PathBuf::from("./browser-relative-song.wav"),
                crate::filesystem::EntryKind::File,
            )],
        );
        app.state_mut().extend_playlist([old.clone()]);
        app.state_mut().browser.set_cursor(0);

        app.handle_command(Command::RenameFile);

        assert!(matches!(
            app.state().popup_dialog.dialog_mode_ref(),
            Some(DialogMode::RenameFile { path, .. }) if path == &old
        ));

        let effects = app.apply_rename_completed(old, new.clone(), true, None);

        assert_eq!(
            app.state().playlist.tracks()[0].path(),
            Some(new.as_path()),
            "the queue must follow a browser rename resolved at the direct-add boundary"
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::UpdateExtinfTitle { path, .. } if path == &new
        )));
    }

    /// A stream track resolves through `resolve_stream_action_target` and
    /// never through `resolve_file_action_target`. The rename shortcut
    /// uses this to decide between editing the EXTINF label and editing a
    /// file on disk.
    #[test]
    fn stream_target_resolves_only_when_playlist_cursor_is_a_stream() {
        let mut app = App::from_config_and_store(
            KeysConfig::default(),
            PlaylistStore::for_dir(PathBuf::new()),
        );
        app.state_mut().active_panel = Panel::Playlist;
        // No track queued: nothing resolves to anything.
        assert_eq!(app.resolve_stream_action_target(), None);
        assert_eq!(app.resolve_file_action_target(), None);

        // Add a local track: only the file path resolver fires.
        app.state_mut()
            .extend_playlist([PathBuf::from("/music/song.mp3")]);
        app.state_mut().playlist.select(0);
        assert_eq!(app.resolve_stream_action_target(), None);
        assert_eq!(
            app.resolve_file_action_target(),
            Some(PathBuf::from("/music/song.mp3"))
        );

        // Queue a stream track: only the stream resolver fires.
        let url = url::Url::parse("https://radio.example.com/live").expect("valid url");
        let stream = crate::track::Track::from_stream(url.clone(), crate::stream::StreamKind::Http);
        app.state_mut().extend_playlist_tracks([stream]);
        app.state_mut().playlist.select(1);
        let stream_target = app
            .resolve_stream_action_target()
            .expect("stream target resolves");
        assert_eq!(stream_target.url, url);
        assert_eq!(stream_target.kind, crate::stream::StreamKind::Http);
        assert_eq!(app.resolve_file_action_target(), None);
    }

    /// Committing a stream rename updates the in-memory title, dispatches
    /// the EXTINF rewrite effect, and closes the dialog. An empty title
    /// keeps the dialog open with a validation error and never dispatches.
    #[test]
    fn commit_rename_stream_updates_in_memory_title_and_dispatches() {
        let url = url::Url::parse("https://radio.example.com/live").expect("valid url");
        let mut app = App::from_config_and_store(
            KeysConfig::default(),
            PlaylistStore::for_dir(PathBuf::new()),
        );
        app.state_mut().active_panel = Panel::Playlist;
        app.state_mut()
            .extend_playlist_tracks([crate::track::Track::from_stream(
                url.clone(),
                crate::stream::StreamKind::Http,
            )]);
        app.state_mut().playlist.select(0);
        app.open_stream_rename_dialog(url.clone(), crate::stream::StreamKind::Http);
        // Preloaded with the placeholder hostname (no metadata yet).
        assert_eq!(
            app.state().popup_dialog.dialog_input_ref(),
            Some("radio.example.com")
        );
        assert_eq!(
            app.state().popup_dialog.dialog_cursor().unwrap_or(0),
            "radio.example.com".chars().count()
        );

        // Type a new title and commit.
        app.state_mut()
            .popup_dialog
            .set_dialog_input("My Favourite Radio".to_string());
        let effects = app.commit_rename_stream();
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            crate::app::Effect::UpdateStreamExtinf {
                url: e_url,
                new_title,
            } => {
                assert_eq!(*e_url, url);
                assert_eq!(new_title, "My Favourite Radio");
            }
            other => panic!("expected UpdateStreamExtinf, got {other:?}"),
        }
        assert!(app.state().popup_dialog.dialog_mode_ref().is_none());
        // The in-memory playlist row reflects the new title immediately.
        assert_eq!(
            app.state().playlist.tracks()[0].display_name(),
            "My Favourite Radio"
        );
    }

    /// An empty title never dispatches and surfaces an error inside the
    /// dialog so the user can fix the typo without losing the URL.
    #[test]
    fn commit_rename_stream_rejects_an_empty_title() {
        let url = url::Url::parse("https://radio.example.com/live").expect("valid url");
        let mut app = App::from_config_and_store(
            KeysConfig::default(),
            PlaylistStore::for_dir(PathBuf::new()),
        );
        app.state_mut().active_panel = Panel::Playlist;
        app.state_mut()
            .extend_playlist_tracks([crate::track::Track::from_stream(
                url.clone(),
                crate::stream::StreamKind::Http,
            )]);
        app.state_mut().playlist.select(0);
        app.open_stream_rename_dialog(url.clone(), crate::stream::StreamKind::Http);
        app.state_mut()
            .popup_dialog
            .set_dialog_input("   ".to_string());

        let effects = app.commit_rename_stream();
        assert!(
            effects.is_empty(),
            "an empty title must not dispatch anything"
        );
        match app.state().popup_dialog.dialog_mode_ref() {
            Some(DialogMode::RenameStream { error, .. }) => {
                assert!(error.is_some(), "the dialog shows the validation error");
            }
            other => panic!("expected RenameStream dialog, got {other:?}"),
        }
    }
}
