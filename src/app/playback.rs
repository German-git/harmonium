//! Playback lifecycle controller.
//!
//! The controller keeps worker snapshots and terminal playback transitions on
//! the application boundary. `App` remains the state owner and continues to
//! expose the established public event-application API.

use super::*;
use crate::playlist::navigation::{SelectionAction, auto_advance_action};

/// Playback operations run against the state and playback configuration they
/// need without inheriting the whole application coordinator.
pub(crate) struct PlaybackController<'app> {
    state: &'app mut AppState,
    crossfade_seconds: crate::audio::CrossfadeSeconds,
}

impl<'app> PlaybackController<'app> {
    pub(crate) fn new(
        state: &'app mut AppState,
        crossfade_seconds: crate::audio::CrossfadeSeconds,
    ) -> Self {
        Self {
            state,
            crossfade_seconds,
        }
    }

    fn push_notification(&mut self, message: String) {
        push_notification(&mut self.state.notifications, message, NOTIFICATIONS_CAP);
    }

    fn remap_stale_snapshot_index(
        &self,
        snapshot: crate::audio::PlaybackSnapshot,
    ) -> crate::audio::PlaybackSnapshot {
        let Some(index) = snapshot.track_index else {
            return snapshot;
        };
        let Some(playing_location) = self.state.persistence.last_track.as_ref() else {
            return snapshot;
        };
        if self
            .state
            .playlist
            .tracks()
            .get(index)
            .is_some_and(|track| track.track_location() == *playing_location)
        {
            return snapshot;
        }
        if let Some(actual) = self
            .state
            .playlist
            .tracks()
            .iter()
            .position(|track| track.track_location() == *playing_location)
        {
            let mut snapshot = snapshot;
            snapshot.track_index = Some(actual);
            return snapshot;
        }
        snapshot
    }

    fn remap_stale_track_index(&self, track_index: usize) -> Option<usize> {
        let Some(playing_location) = &self.state.persistence.last_track else {
            return Some(track_index);
        };
        if self.state.playlist.is_empty() {
            return Some(track_index);
        }
        if self
            .state
            .playlist
            .tracks()
            .get(track_index)
            .is_some_and(|track| track.track_location() == *playing_location)
        {
            return Some(track_index);
        }
        self.state
            .playlist
            .tracks()
            .iter()
            .position(|track| track.track_location() == *playing_location)
    }

    fn arm_crossfade_next(&self) -> Option<Effect> {
        if !self.crossfade_seconds.is_enabled() {
            return None;
        }
        let len = self.state.playlist.len();
        if len == 0 {
            return None;
        }
        let current = self.state.playback.track_index?;
        let mut navigation = self.state.navigation.clone();
        match auto_advance_action(&self.state.playback_mode, current, len, &mut navigation) {
            SelectionAction::Play(index) => {
                let track = self.state.playlist.tracks().get(index)?;
                if !track.is_local() {
                    return None;
                }
                Some(Effect::Audio(AudioCommand::PreloadNext {
                    source: track.source().clone(),
                    track_index: index,
                }))
            }
            SelectionAction::Stop => None,
        }
    }

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

    pub(crate) fn next_track(&mut self, effects: &mut Vec<Effect>) {
        if self.state.playlist.is_empty() {
            self.push_notification("Queue is empty".to_string());
            return;
        }

        let mode = self.state.playback_mode;
        let cursor = self.state.playlist.cursor();
        let len = self.state.playlist.len();
        match manual_next_action(&mode, cursor, len, &mut self.state.navigation) {
            SelectionAction::Play(index) => {
                self.state.playlist.select(index);
                effects.extend(self.begin_current_track());
            }
            SelectionAction::Stop => {
                self.push_notification("No next track".to_string());
            }
        }
    }

    pub(crate) fn previous_track(&mut self, effects: &mut Vec<Effect>) {
        if self.state.playlist.is_empty() {
            self.push_notification("Queue is empty".to_string());
            return;
        }

        if previous_restarts_track(self.state.playback.elapsed) {
            self.state.playback.elapsed = Duration::ZERO;
            effects.push(Effect::Audio(AudioCommand::SeekTo(Duration::ZERO)));
            return;
        }

        let cursor = self.state.playlist.cursor();
        match manual_previous_target(&mut self.state.navigation, cursor) {
            Some(index) => {
                self.state.playlist.select(index);
                effects.extend(self.begin_current_track());
            }
            None => {
                self.state.playback.elapsed = Duration::ZERO;
                effects.push(Effect::Audio(AudioCommand::SeekTo(Duration::ZERO)));
            }
        }
    }

    pub(crate) fn seek(&mut self, backward: bool, effects: &mut Vec<Effect>) {
        if self.state.playback.track_index.is_none() {
            return;
        }
        let amount = seek_step(self.state.playback.duration);
        effects.push(Effect::Audio(AudioCommand::SeekBy {
            forward: !backward,
            amount,
        }));
    }

    pub(crate) fn play_selected(&mut self, effects: &mut Vec<Effect>) {
        if self.state.playlist.is_empty() {
            self.push_notification("Queue is empty".to_string());
            return;
        }
        effects.extend(self.begin_current_track());
    }

    pub(crate) fn begin_current_track(&mut self) -> Vec<Effect> {
        let Some(track) = self.state.playlist.current() else {
            return Vec::new();
        };
        let index = self.state.playlist.cursor();
        let track_source = track.source().clone();
        let track_is_stream = track.is_stream();
        let track_path = track.path().map(Path::to_path_buf);
        let track_location = track.track_location();
        let metadata = track.metadata().cloned();
        let generation = if track_is_stream {
            self.state.async_ops.next_playback_generation = self
                .state
                .async_ops
                .next_playback_generation
                .wrapping_add(1);
            Some(self.state.async_ops.next_playback_generation)
        } else {
            None
        };

        if track_is_stream {
            let generation = generation.expect("stream playback must have a generation");
            self.state
                .async_ops
                .begin_stream_acquisition(track_location.clone(), generation);
        } else {
            // A local Play supersedes any stream acquisition represented by the
            // UI. The worker cancels the provider task separately; clearing the
            // identity here prevents a late SourceReady/SourceFailed event for
            // that stream from affecting the local track's state.
            self.state.async_ops.cancel_stream_activity();
        }

        self.state.navigation.record_started(index);
        self.state.playback.track_index = Some(index);
        self.state.playback.status = PlayStatus::Playing;
        self.state.playback.elapsed = Duration::ZERO;
        self.state.playback.duration = metadata.as_ref().map(|meta| meta.duration);
        self.state.persistence.last_track = Some(track_location);
        self.state.persistence.last_track_position_ms = 0;

        let mut effects = vec![Effect::Audio(AudioCommand::Play {
            source: track_source,
            track_index: index,
            generation,
        })];
        if self.state.artwork.is_enabled() {
            self.state.artwork.loading = true;
            effects.push(Effect::LoadArtwork {
                track_index: index,
                path: track_path.clone(),
                metadata: metadata.clone(),
                source_config: self.state.artwork.source_config,
                cache_dir: self.state.artwork.cache_dir.clone(),
            });
        }
        if let Some(track_path) = track_path {
            if self.state.lyrics.visible {
                let (request, display_title) =
                    App::lyrics_request_for(&track_path, metadata.as_ref());
                self.prepare_lyrics_request(index, request, display_title, &mut effects);
            }
            if let Some(arm) = self.arm_crossfade_next() {
                effects.push(arm);
            }
        }
        effects
    }

    pub(crate) fn apply_progress(&mut self, snapshot: crate::audio::PlaybackSnapshot) {
        let snapshot = self.remap_stale_snapshot_index(snapshot);
        self.state.playback.apply_snapshot(snapshot);
        self.state.persistence.last_track_position_ms =
            self.state.playback.elapsed.as_millis() as u64;
    }

    pub(crate) fn apply_track_ended(&mut self, track_index: usize) -> Vec<Effect> {
        let Some(track_index) = self.remap_stale_track_index(track_index) else {
            tracing::info!("ignoring track end for a no-longer-queued track");
            return Vec::new();
        };
        if self.state.playback.track_index != Some(track_index) {
            return Vec::new();
        }

        let mode = self.state.playback_mode;
        let current = track_index;
        let len = self.state.playlist.len();
        let mut effects = Vec::new();
        match auto_advance_action(&mode, current, len, &mut self.state.navigation) {
            SelectionAction::Play(index) => {
                self.state.playlist.select(index);
                effects = self.begin_current_track();
            }
            SelectionAction::Stop => {
                tracing::info!("queue finished, playback stopped");
                self.state.playback.status = PlayStatus::Stopped;
                self.state.playback.elapsed = Duration::ZERO;
            }
        }
        effects
    }

    pub(crate) fn apply_crossfade_completed(
        &mut self,
        track_index: usize,
        path: PathBuf,
        elapsed: Duration,
    ) -> Vec<Effect> {
        let completed_location = crate::track::TrackLocation::local(&path);
        let track_index = self
            .state
            .playlist
            .tracks()
            .iter()
            .position(|track| track.track_location() == completed_location)
            .unwrap_or(track_index);
        let Some(track) = self.state.playlist.tracks().get(track_index) else {
            tracing::warn!("crossfade completed for unknown track {path:?}, ignoring");
            return Vec::new();
        };
        if !track.is_local() {
            tracing::debug!("ignoring crossfade completion for stream track");
            return Vec::new();
        }
        tracing::info!(track_index, "app adopted crossfade-completed track");
        let track_path = track.path().map(PathBuf::from);
        let track_location = track.track_location();
        let metadata = track.metadata().cloned();
        let _ = track;

        self.state.playlist.select(track_index);
        self.state.navigation.record_started(track_index);
        self.state.playback.track_index = Some(track_index);
        self.state.playback.status = PlayStatus::Playing;
        self.state.playback.elapsed = elapsed;
        self.state.playback.duration = metadata.as_ref().map(|meta| meta.duration);
        self.state.persistence.last_track = Some(track_location);
        self.state.persistence.last_track_position_ms = elapsed.as_millis() as u64;

        let mut effects = Vec::new();
        if self.state.artwork.is_enabled() {
            self.state.artwork.loading = true;
            effects.push(Effect::LoadArtwork {
                track_index,
                path: track_path.clone(),
                metadata: metadata.clone(),
                source_config: self.state.artwork.source_config,
                cache_dir: self.state.artwork.cache_dir.clone(),
            });
        }
        if let Some(path) = track_path {
            if self.state.lyrics.visible {
                let (request, display_title) = App::lyrics_request_for(&path, metadata.as_ref());
                self.prepare_lyrics_request(track_index, request, display_title, &mut effects);
            }
            if let Some(arm) = self.arm_crossfade_next() {
                effects.push(arm);
            }
        }
        effects
    }

    pub(crate) fn apply_source_ready(&mut self, url: String) {
        self.apply_source_ready_with_generation(None, url);
    }

    pub(crate) fn apply_source_ready_with_generation(
        &mut self,
        generation: Option<u64>,
        url: String,
    ) {
        let Some(actual) = url::Url::parse(&url)
            .ok()
            .map(crate::track::TrackLocation::url)
        else {
            tracing::debug!(
                actual = %crate::net::safe_location(&url),
                "ignoring SourceReady with an invalid URL"
            );
            return;
        };
        if !self
            .state
            .async_ops
            .accept_stream_acquisition(&actual, generation)
        {
            tracing::debug!(
                actual = %crate::net::safe_location(&url),
                ?generation,
                "ignoring SourceReady for a no-longer-active stream"
            );
            return;
        }
    }

    pub(crate) fn apply_source_failed(&mut self, url: String) {
        self.apply_source_failed_with_generation(None, url);
    }

    pub(crate) fn apply_source_failed_with_generation(
        &mut self,
        generation: Option<u64>,
        url: String,
    ) {
        let Some(actual) = url::Url::parse(&url)
            .ok()
            .map(crate::track::TrackLocation::url)
        else {
            tracing::debug!(
                actual = %crate::net::safe_location(&url),
                "ignoring SourceFailed with an invalid URL"
            );
            return;
        };
        if !self
            .state
            .async_ops
            .accept_stream_acquisition(&actual, generation)
        {
            tracing::debug!(
                actual = %crate::net::safe_location(&url),
                ?generation,
                "ignoring SourceFailed for a no-longer-active stream"
            );
            return;
        }
    }

    pub(crate) fn apply_artwork_loaded(
        &mut self,
        track_index: usize,
        artwork: Option<ArtworkProtocol>,
    ) {
        if self.state.playback.track_index != Some(track_index) {
            return;
        }
        self.state.artwork.loading = false;
        self.state.artwork.set_artwork(track_index, artwork);
    }

    pub(crate) fn apply_artwork_resizes(&mut self) {
        self.state.artwork.apply_pending_resizes();
    }

    pub(crate) fn apply_lyrics_loaded(&mut self, track_index: usize, outcome: LoadOutcome) {
        if self.state.playback.track_index != Some(track_index) {
            return;
        }
        if let Some(path) = &outcome.saved_path {
            tracing::info!(saved = %path.display(), "lyrics cached for the track");
        }
        let lyrics = &mut self.state.lyrics;
        lyrics.loading = false;
        lyrics.track_index = Some(track_index);
        lyrics.set_document(outcome.document);
        lyrics.origin = outcome.origin;
        lyrics.error = outcome.error;
        lyrics.scroll = 0;
    }
}
