//! Playback queue with selection cursor and pure ordering operations.
//!
//! Invariants kept by every transition:
//!
//! - `cursor` is always a valid index while the queue is non empty, and
//!   exactly zero when it is empty.
//! - Removals and swaps never leave the cursor pointing at a different
//!   track than the one it pointed at before, whenever that track survives.
//! - The queue only ever references paths, so removing an entry can never
//!   touch the underlying file.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::metadata::TrackMetadata;
use crate::track::{Track, TrackLocation};

/// Ordered queue of tracks waiting for playback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Playlist {
    tracks: Vec<Track>,
    cursor: usize,
}

impl Playlist {
    /// Create an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of queued tracks.
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Queued tracks in playback order.
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Index of the currently selected entry.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Append tracks preserving order, keeping the cursor where it was.
    ///
    /// Returns how many entries landed, matching the feedback vocabulary
    /// used by the status notifications.
    pub fn extend<I>(&mut self, tracks: I) -> usize
    where
        I: IntoIterator<Item = Track>,
    {
        let before = self.tracks.len();
        self.tracks.extend(tracks);
        self.tracks.len() - before
    }

    /// Attach extracted metadata to every entry backed by `path`.
    ///
    /// Returns how many entries were updated, which can exceed one when a
    /// file was queued more than once. Stream entries are never touched:
    /// their metadata arrives through the streaming resolver at queue time,
    /// so a lofty snapshot has nothing to apply.
    pub fn apply_metadata(&mut self, path: &Path, metadata: TrackMetadata) -> usize {
        let target = TrackLocation::local(path);
        let mut updated = 0;
        for track in &mut self.tracks {
            if track.track_location() == target {
                track.set_metadata(metadata.clone());
                updated += 1;
            }
        }
        updated
    }

    /// Rebuild the queue with every entry backed by `old` moved to `new`.
    ///
    /// A pure transform mirroring [`Self::apply_metadata`]: positions and the
    /// selection cursor are preserved, the snapshot rides along via
    /// [`Track::rename_to`], and duplicated queue entries are all updated so
    /// playback and navigation indices keep pointing at the renamed file.
    /// Stream entries are left untouched because they have no path to rename.
    pub fn rewrite_path(&self, old: &Path, new: &Path) -> Playlist {
        let old = TrackLocation::local(old);
        let tracks = self
            .tracks
            .iter()
            .map(|t| {
                if t.track_location() == old {
                    t.clone().rename_to(new.to_path_buf())
                } else {
                    t.clone()
                }
            })
            .collect();
        Playlist {
            tracks,
            cursor: self.cursor,
        }
    }

    /// Attach a whole batch of extracted metadata in a single pass.
    ///
    /// The batch is indexed by path once (a `HashMap` lookup per entry) so
    /// applying `k` metadata results against an `n`-entry queue is O(n + k)
    /// instead of the O(n·k) of calling [`Self::apply_metadata`] per item.
    /// When the batch holds several entries for the same path, the last one
    /// wins, mirroring the overwrite behaviour a caller would get applying
    /// them in order. Stream entries are skipped because their metadata
    /// already arrived through the streaming resolver.
    pub fn apply_metadata_batch(&mut self, batch: &[(PathBuf, TrackMetadata)]) -> usize {
        if batch.is_empty() || self.tracks.is_empty() {
            return 0;
        }
        let by_path: HashMap<TrackLocation, &TrackMetadata> = batch
            .iter()
            .map(|(path, meta)| (TrackLocation::local(path), meta))
            .collect();
        let mut updated = 0;
        for track in &mut self.tracks {
            let location = track.track_location();
            let Some(meta) = by_path.get(&location) else {
                continue;
            };
            track.set_metadata((*meta).clone());
            updated += 1;
        }
        updated
    }

    /// The selected track, if any.
    pub fn current(&self) -> Option<&Track> {
        self.tracks.get(self.cursor)
    }

    /// Mutable accessor for the selected track, used by flows that need
    /// to patch the in-memory snapshot (e.g. the stream rename shortcut
    /// updating its EXTINF title without touching disk).
    pub fn current_mut(&mut self) -> Option<&mut Track> {
        self.tracks.get_mut(self.cursor)
    }

    /// Index following the cursor while another entry exists.
    ///
    /// Pure peek support so phase 4 playback wiring decides its own moment
    /// to advance.
    pub fn next_index(&self) -> Option<usize> {
        let next = self.cursor.checked_add(1)?;
        (next < self.tracks.len()).then_some(next)
    }

    /// Move the cursor to `index`, clamped into the valid range.
    pub fn select(&mut self, index: usize) {
        self.cursor = index.min(self.tracks.len().saturating_sub(1));
    }

    /// Advance to [`Self::next_index`], reporting whether an entry follows.
    pub fn advance(&mut self) -> bool {
        match self.next_index() {
            Some(next) => {
                self.cursor = next;
                true
            }
            None => false,
        }
    }

    /// Move the selection cursor up, saturating at the top.
    pub fn move_cursor_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the selection cursor down, saturating at the last entry.
    pub fn move_cursor_down(&mut self) {
        if let Some(last) = self.tracks.len().checked_sub(1) {
            self.cursor = self.cursor.saturating_add(1).min(last);
        }
    }

    /// Jump the selection to the first queued entry.
    pub fn goto_top(&mut self) {
        self.cursor = 0;
    }

    /// Jump the selection to the last queued entry, or stay put when empty.
    pub fn goto_bottom(&mut self) {
        self.cursor = self.tracks.len().saturating_sub(1);
    }

    /// Page the selection up by `step` rows, saturating at the head.
    pub fn page_up(&mut self, step: usize) {
        self.cursor = self.cursor.saturating_sub(step);
    }

    /// Page the selection down by `step` rows, clamping at the tail.
    pub fn page_down(&mut self, step: usize) {
        if let Some(last) = self.tracks.len().checked_sub(1) {
            self.cursor = self.cursor.saturating_add(step).min(last);
        }
    }

    /// Remove every entry whose index appears in `indices`.
    ///
    /// Duplicated, unsorted or out of range indices are tolerated because
    /// callers feed straight from UI selection state. The cursor keeps
    /// pointing at its track when it survives, otherwise at the successor
    /// position clamped to the new tail, which keeps the invariant that the
    /// cursor indexes valid entries only.
    pub fn remove_selected(&mut self, indices: &[usize]) -> usize {
        if indices.is_empty() || self.tracks.is_empty() {
            return 0;
        }

        // Mark valid original positions once so duplicates and out-of-range
        // indices cost only O(k), without making the retain pass O(n*k).
        let mut removed_at = vec![false; self.tracks.len()];
        for &index in indices {
            if let Some(mark) = removed_at.get_mut(index) {
                *mark = true;
            }
        }

        // Retain performs the only queue pass. The original index lets the
        // same pass count removals before the cursor for its remapping.
        let cursor = self.cursor;
        let mut original_index = 0;
        let mut removed = 0;
        let mut removed_before_cursor = 0;
        self.tracks.retain(|_| {
            let index = original_index;
            original_index += 1;
            if !removed_at[index] {
                return true;
            }
            removed += 1;
            if index < cursor {
                removed_before_cursor += 1;
            }
            false
        });
        if removed == 0 {
            return 0;
        }

        self.cursor = cursor.saturating_sub(removed_before_cursor);
        self.clamp_cursor();
        removed
    }

    /// Remove the currently selected entry, returning it.
    pub fn remove_current(&mut self) -> Option<Track> {
        if self.tracks.is_empty() {
            return None;
        }
        let removed = self.tracks.remove(self.cursor);
        self.clamp_cursor();
        Some(removed)
    }

    /// Swap the entry at `index` with its predecessor.
    ///
    /// Returns false at the top boundary or for out of range indices. The
    /// cursor rides along whenever it pointed at one of the swapped rows,
    /// so the selection visually follows the moved track.
    pub fn swap_up(&mut self, index: usize) -> bool {
        if index == 0 || index >= self.tracks.len() {
            return false;
        }
        self.tracks.swap(index - 1, index);
        match self.cursor {
            position if position == index => self.cursor = index - 1,
            position if position + 1 == index => self.cursor = index,
            _ => {}
        }
        true
    }

    /// Swap the entry at `index` with its successor, mirroring [`Self::swap_up`].
    pub fn swap_down(&mut self, index: usize) -> bool {
        match index.checked_add(1) {
            Some(next) if next < self.tracks.len() => self.swap_up(next),
            _ => false,
        }
    }

    /// Drop every entry without touching any backing file.
    pub fn clear(&mut self) {
        self.tracks.clear();
        self.cursor = 0;
    }

    /// Keep the cursor inside the surviving range after destructive ops.
    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.tracks.len().saturating_sub(1));
    }

    /// Reorder the queue by `key`, keeping the cursor on the track it was
    /// pointing at (it moves with its entry) so the selection does not jump
    /// to a different song after a sort.
    pub fn sort_tracks_by<F, K>(&mut self, mut key: F)
    where
        F: FnMut(&Track) -> K,
        K: Ord,
    {
        // Use the typed source identity so local paths and stream URLs cannot
        // collide merely because their printable forms happen to match.
        let cursor_location = self.tracks.get(self.cursor).map(Track::track_location);
        self.tracks.sort_by_cached_key(|track| key(track));
        if let Some(location) = cursor_location
            && let Some(new_index) = self
                .tracks
                .iter()
                .position(|t| t.track_location() == location)
        {
            self.cursor = new_index;
            self.clamp_cursor();
        }
    }

    /// Reorder the queue with a direct track comparator, preserving the
    /// selected track by its typed source identity.
    pub fn sort_tracks_by_compare<F>(&mut self, mut compare: F)
    where
        F: FnMut(&Track, &Track) -> Ordering,
    {
        let cursor_location = self.tracks.get(self.cursor).map(Track::track_location);
        self.tracks.sort_by(|left, right| compare(left, right));
        if let Some(location) = cursor_location
            && let Some(new_index) = self
                .tracks
                .iter()
                .position(|track| track.track_location() == location)
        {
            self.cursor = new_index;
            self.clamp_cursor();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn track(path: &str) -> Track {
        Track::local(path)
    }

    #[test]
    fn sort_reanchors_a_colliding_local_path_and_stream_by_typed_identity() {
        let url = url::Url::parse("https://example.com/live").expect("valid URL");
        let mut playlist = Playlist::new();
        playlist.extend([
            Track::local(url.as_str()),
            Track::from_stream(url.clone(), crate::stream::StreamKind::Http),
        ]);
        playlist.select(1);

        playlist.sort_tracks_by(|track| track.is_local());

        assert_eq!(playlist.cursor(), 0);
        assert_eq!(
            playlist.current().expect("selected track").track_location(),
            crate::track::TrackLocation::url(url)
        );
    }

    /// Queue holding /a /b /c with the cursor on /b.
    fn fixture() -> Playlist {
        let mut playlist = Playlist::new();
        playlist.extend([track("/a.mp3"), track("/b.mp3"), track("/c.mp3")]);
        playlist.select(1);
        playlist
    }

    fn display_names(playlist: &Playlist) -> Vec<String> {
        playlist
            .tracks()
            .iter()
            .map(|item| item.display_name().into_owned())
            .collect()
    }

    #[test]
    fn empty_queue_stays_safe_for_every_operation() {
        let mut playlist = Playlist::new();

        assert!(playlist.is_empty());
        assert_eq!(playlist.current(), None);
        assert_eq!(playlist.next_index(), None);
        assert!(!playlist.advance());
        assert_eq!(playlist.remove_current(), None);
        assert_eq!(playlist.remove_selected(&[0, 1]), 0);

        playlist.select(99);
        assert_eq!(playlist.cursor(), 0);
    }

    #[test]
    fn extend_preserves_order_and_reports_the_count() {
        let mut playlist = Playlist::new();

        assert_eq!(playlist.extend([track("/1"), track("/2"), track("/3")]), 3);
        assert_eq!(playlist.extend([]), 0);
        assert_eq!(
            display_names(&playlist),
            ["1", "2", "3"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn current_and_next_walk_playback_order() {
        let mut playlist = fixture();

        assert_eq!(
            playlist.current().map(Track::display_name),
            Some("b".into())
        );
        assert_eq!(playlist.next_index(), Some(2));

        assert!(playlist.advance());
        assert_eq!(playlist.cursor(), 2);
        assert!(!playlist.advance(), "the tail has no successor");
        assert_eq!(playlist.next_index(), None);
    }

    #[test]
    fn select_clamps_out_of_range_requests() {
        let mut playlist = fixture();

        playlist.select(999);
        assert_eq!(playlist.cursor(), 2);

        playlist.select(0);
        assert_eq!(playlist.cursor(), 0);
    }

    #[test]
    fn remove_current_keeps_the_queue_consistent() {
        let mut playlist = fixture();

        let removed = playlist.remove_current().expect("entry exists");
        assert_eq!(removed.display_name(), "b");
        assert_eq!(playlist.len(), 2);
        assert_eq!(playlist.cursor(), 1, "successor slid into place");
        assert_eq!(
            playlist.current().map(Track::display_name),
            Some("c".into())
        );

        playlist.remove_current();
        playlist.remove_current();
        playlist.remove_current();
        assert!(playlist.is_empty());
        assert_eq!(playlist.cursor(), 0);
    }

    #[test]
    fn remove_selected_handles_unsorted_and_duplicated_indices() {
        let mut playlist = Playlist::new();
        playlist.extend([
            track("/a"),
            track("/b"),
            track("/c"),
            track("/d"),
            track("/e"),
        ]);

        // 9 is out of range and the second 3 collapses into the first
        assert_eq!(playlist.remove_selected(&[3, 0, 3, 9]), 2);

        assert_eq!(
            display_names(&playlist),
            ["b", "c", "e"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(playlist.cursor(), 0);
    }

    #[test]
    fn remove_selected_ignores_when_no_index_is_in_range() {
        let mut playlist = fixture();
        let before = playlist.clone();

        assert_eq!(playlist.remove_selected(&[3, 99, usize::MAX, 99]), 0);
        assert_eq!(playlist, before);
    }

    #[test]
    fn remove_selected_preserves_survivor_metadata_and_order() {
        let mut first = track("/first.mp3");
        first.set_metadata(TrackMetadata {
            title: "First tagged".into(),
            title_tagged: true,
            ..TrackMetadata::default()
        });
        let mut third = track("/third.mp3");
        third.set_metadata(TrackMetadata {
            title: "Third tagged".into(),
            title_tagged: true,
            ..TrackMetadata::default()
        });

        let mut playlist = Playlist::new();
        playlist.extend([first, track("/removed.mp3"), third]);
        playlist.select(2);

        assert_eq!(playlist.remove_selected(&[99, 1, 1]), 1);
        assert_eq!(playlist.cursor(), 1);
        assert_eq!(playlist.tracks()[0].path(), Some(Path::new("/first.mp3")));
        assert_eq!(playlist.tracks()[0].display_name(), "First tagged");
        assert_eq!(
            playlist.tracks()[0].metadata().unwrap().title,
            "First tagged"
        );
        assert_eq!(playlist.tracks()[1].path(), Some(Path::new("/third.mp3")));
        assert_eq!(playlist.tracks()[1].display_name(), "Third tagged");
        assert_eq!(
            playlist.tracks()[1].metadata().unwrap().title,
            "Third tagged"
        );
    }

    #[test]
    fn remove_selected_handles_large_unsorted_duplicate_selection_linearly() {
        const TRACK_COUNT: usize = 10_000;
        let mut playlist = Playlist::new();
        playlist.extend((0..TRACK_COUNT).map(|index| track(&format!("/track-{index:05}.mp3"))));
        playlist.select(TRACK_COUNT - 1);

        // Reverse and forward copies exercise unsorted input and duplicate
        // indices; the final values exercise both ordinary and huge misses.
        let mut indices = Vec::with_capacity(TRACK_COUNT + 3);
        indices.extend((0..TRACK_COUNT).step_by(2).rev());
        indices.extend((0..TRACK_COUNT).step_by(2));
        indices.extend([TRACK_COUNT, TRACK_COUNT + 1, usize::MAX]);

        assert_eq!(playlist.remove_selected(&indices), TRACK_COUNT / 2);
        assert_eq!(playlist.len(), TRACK_COUNT / 2);
        assert_eq!(playlist.tracks()[0].display_location(), "/track-00001.mp3");
        assert_eq!(
            playlist.tracks().last().unwrap().display_location(),
            "/track-09999.mp3"
        );
        assert_eq!(playlist.cursor(), TRACK_COUNT / 2 - 1);
        assert_eq!(
            playlist.current().unwrap().display_location(),
            "/track-09999.mp3"
        );
    }

    #[test]
    fn removing_the_current_entry_selects_its_successor_position() {
        let mut playlist = Playlist::new();
        playlist.extend([track("/a"), track("/b"), track("/c")]);
        playlist.select(1);

        playlist.remove_selected(&[1]);

        assert_eq!(playlist.cursor(), 1);
        assert_eq!(
            playlist.current().map(Track::display_name),
            Some("c".into())
        );
    }

    #[test]
    fn removing_entries_before_the_cursor_shifts_it_left() {
        let mut playlist = Playlist::new();
        playlist.extend([track("/a"), track("/b"), track("/c")]);
        playlist.select(2);

        playlist.remove_selected(&[0]);

        assert_eq!(playlist.cursor(), 1);
        assert_eq!(
            playlist.current().map(Track::display_name),
            Some("c".into())
        );
    }

    #[test]
    fn removing_the_tail_clamps_the_cursor_to_the_new_last_entry() {
        let mut playlist = Playlist::new();
        playlist.extend([track("/a"), track("/b"), track("/c")]);
        playlist.select(2);

        playlist.remove_selected(&[2]);

        assert_eq!(playlist.cursor(), 1);
        assert_eq!(
            playlist.current().map(Track::display_name),
            Some("b".into())
        );
    }

    #[test]
    fn swaps_move_entries_and_carry_the_cursor_along() {
        let mut playlist = fixture();

        // Cursor sits on b at index 1, swapping up must follow the track
        assert!(playlist.swap_up(1));
        assert_eq!(playlist.cursor(), 0);
        assert_eq!(
            display_names(&playlist),
            ["b", "a", "c"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );

        // Swapping it back down drags the cursor along to its track again
        assert!(playlist.swap_down(0));
        assert_eq!(
            display_names(&playlist),
            ["a", "b", "c"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(playlist.cursor(), 1);

        // Boundaries refuse without panicking
        assert!(!playlist.swap_up(0));
        assert!(!playlist.swap_down(playlist.len() - 1));
        assert!(!playlist.swap_up(999));
        assert!(!playlist.swap_down(usize::MAX));
    }

    #[test]
    fn cursor_moves_saturate_at_both_boundaries_and_stay_empty_safe() {
        let mut playlist = fixture();

        playlist.move_cursor_up();
        playlist.move_cursor_up();
        assert_eq!(playlist.cursor(), 0, "the top absorbs extra moves");

        playlist.move_cursor_down();
        assert_eq!(playlist.cursor(), 1);

        playlist.select(2);
        playlist.move_cursor_down();
        playlist.move_cursor_down();
        assert_eq!(playlist.cursor(), 2, "the tail absorbs extra moves");

        let mut empty = Playlist::new();
        empty.move_cursor_up();
        empty.move_cursor_down();
        assert_eq!(empty.cursor(), 0);
    }

    #[test]
    fn clear_empties_everything_but_touches_no_files() {
        let root = crate::test_support::unique_temp_dir("playlist-clear");
        let path = root.join("keep.wav");
        std::fs::write(&path, b"data").expect("fixture");

        let mut playlist = Playlist::new();
        playlist.extend([Track::local(&path)]);
        playlist.clear();

        assert!(playlist.is_empty());
        assert_eq!(playlist.cursor(), 0);
        assert!(path.exists(), "clearing must never delete files");
    }

    #[test]
    fn metadata_attaches_to_every_entry_of_a_path() {
        let mut playlist = Playlist::new();
        playlist.extend([track("/dup.mp3"), track("/other.mp3"), track("/dup.mp3")]);

        let meta = TrackMetadata {
            title: "T".into(),
            title_tagged: true,
            artist: "A".into(),
            album: "L".into(),
            track_number: Some(7),
            duration: Duration::from_secs(180),
            bitrate: Some(320),
            sample_rate: Some(44100),
            codec: "MP3".into(),
            format: "MP3".into(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        };

        assert_eq!(
            playlist.apply_metadata(Path::new("/album/../dup.mp3"), meta.clone()),
            2
        );
        assert_eq!(playlist.tracks()[0].metadata(), Some(&meta));
        assert_eq!(playlist.tracks()[1].metadata(), None);
    }

    #[test]
    fn apply_metadata_batch_matches_every_path_in_one_pass() {
        let mut playlist = Playlist::new();
        playlist.extend([track("/a.mp3"), track("/b.mp3"), track("/a.mp3")]);

        let meta_a = TrackMetadata {
            title: "A".into(),
            title_tagged: true,
            artist: String::new(),
            album: String::new(),
            track_number: None,
            duration: Duration::from_secs(1),
            bitrate: None,
            sample_rate: None,
            codec: String::new(),
            format: String::new(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        };
        let meta_b = TrackMetadata {
            title: "B".into(),
            title_tagged: true,
            ..meta_a.clone()
        };
        let batch = vec![
            (PathBuf::from("/album/../a.mp3"), meta_a.clone()),
            (PathBuf::from("/b.mp3"), meta_b),
        ];

        assert_eq!(playlist.apply_metadata_batch(&batch), 3);
        assert_eq!(playlist.tracks()[0].metadata(), Some(&meta_a));
        assert_eq!(
            playlist.tracks()[1].metadata().map(|m| m.title.as_str()),
            Some("B")
        );
        assert_eq!(playlist.tracks()[2].metadata(), Some(&meta_a));
    }

    #[test]
    fn apply_metadata_accepts_borrowed_paths_from_events() {
        let mut playlist = Playlist::new();
        playlist.extend([track("/x.flac")]);

        let path: PathBuf = "/x.flac".into();
        let meta = TrackMetadata {
            title: "X".into(),
            title_tagged: true,
            artist: String::new(),
            album: String::new(),
            track_number: None,
            duration: Duration::from_secs(1),
            bitrate: None,
            sample_rate: None,
            codec: String::new(),
            format: String::new(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        };

        assert_eq!(playlist.apply_metadata(&path, meta), 1);
    }

    #[test]
    fn rewrite_path_renames_matching_entries_and_keeps_the_cursor() {
        let mut playlist = fixture();
        playlist.extend([Track::local("/d.mp3")]);
        playlist.select(1);

        let rewritten = playlist.rewrite_path(Path::new("/b.mp3"), Path::new("/b-renamed.mp3"));

        let paths: Vec<PathBuf> = rewritten
            .tracks()
            .iter()
            .filter_map(|t| t.path().map(Path::to_path_buf))
            .collect();
        assert_eq!(
            paths,
            ["/a.mp3", "/b-renamed.mp3", "/c.mp3", "/d.mp3"]
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(rewritten.cursor(), 1, "the selection stays on its track");
        // The original queue is untouched, the rename is a pure transform
        assert_eq!(playlist.tracks()[1].path(), Some(Path::new("/b.mp3")));
    }

    #[test]
    fn rewrite_path_updates_every_duplicate_entry() {
        let mut playlist = Playlist::new();
        playlist.extend([track("/dup.mp3"), track("/other.mp3"), track("/dup.mp3")]);

        let rewritten = playlist.rewrite_path(Path::new("/dup.mp3"), Path::new("/renamed.mp3"));

        let paths: Vec<PathBuf> = rewritten
            .tracks()
            .iter()
            .filter_map(|t| t.path().map(Path::to_path_buf))
            .collect();
        assert_eq!(
            paths,
            ["/renamed.mp3", "/other.mp3", "/renamed.mp3"]
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn path_matching_normalizes_both_track_and_argument_sides() {
        let mut playlist = Playlist::new();
        playlist.extend([track("/music/album/../song.mp3")]);

        let rewritten = playlist.rewrite_path(
            Path::new("/music/./song.mp3"),
            Path::new("/music/renamed.mp3"),
        );

        assert_eq!(
            rewritten.tracks()[0].path(),
            Some(Path::new("/music/renamed.mp3"))
        );
    }

    #[test]
    fn rewrite_path_keeps_metadata_and_leaves_unmatched_tracks_alone() {
        let mut playlist = Playlist::new();
        playlist.extend([track("/a.mp3"), track("/b.mp3")]);
        let meta = TrackMetadata {
            title: "Renamed Song".into(),
            title_tagged: true,
            artist: String::new(),
            album: String::new(),
            track_number: None,
            duration: Duration::from_secs(60),
            bitrate: None,
            sample_rate: None,
            codec: String::new(),
            format: String::new(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        };
        playlist.apply_metadata(Path::new("/a.mp3"), meta.clone());

        let rewritten = playlist.rewrite_path(Path::new("/a.mp3"), Path::new("/a-new.mp3"));

        assert_eq!(rewritten.tracks()[0].metadata(), Some(&meta));
        assert_eq!(rewritten.tracks()[1].metadata(), None);
        assert_eq!(rewritten.tracks()[1].path(), Some(Path::new("/b.mp3")));
    }
}
