//! Playlist reordering and column labels.
//!
//! Two strategies are supported: alphabetical by file name, or by selected
//! metadata fields (artist / album / track number / title) joined in a
//! canonical order. A
//! field marked for sorting is dropped when it is missing or holds a literal
//! "Unknown", and a track with no usable metadata falls back to its file
//! name, so a mixed library still sorts predictably.

use crate::config::{PlaylistColumnsConfig, SortBy, SortMetadataField, SortTracksConfig};
use crate::playlist::Playlist;
use crate::track::Track;
use std::cmp::Ordering;
#[cfg(test)]
use std::path::Path;

/// Shared read-only view used by the sorter for the legacy Now Playing
/// configuration and the new Playlist columns configuration.
pub trait ColumnConfig {
    fn display_by(&self) -> SortBy;
    fn field_enabled(&self, field: SortMetadataField) -> bool;
}

impl ColumnConfig for SortTracksConfig {
    fn display_by(&self) -> SortBy {
        self.sort_by
    }

    fn field_enabled(&self, field: SortMetadataField) -> bool {
        field.is_enabled(self)
    }
}

impl ColumnConfig for PlaylistColumnsConfig {
    fn display_by(&self) -> SortBy {
        self.display_by
    }

    fn field_enabled(&self, field: SortMetadataField) -> bool {
        field.is_enabled_playlist(self)
    }
}

/// Whether the effective ordering configuration changed between two states.
///
/// The metadata sub-options only matter when `sort_by` is `Metadata`. So
/// switching Filename→Filename (even after toggling sub-options in between)
/// produces no effective change, matching the user's expectation that sub
/// option edits are ignored unless Metadata is on.
pub fn sort_config_effective_changed<C: ColumnConfig>(initial: &C, final_: &C) -> bool {
    effective_ordering_key(initial) != effective_ordering_key(final_)
}

/// Canonical descriptor of the ordering a config actually applies.
fn effective_ordering_key<C: ColumnConfig>(config: &C) -> String {
    if config.display_by() == SortBy::Filename {
        return "filename".to_string();
    }
    let fields: Vec<_> = SortMetadataField::ORDER
        .into_iter()
        .filter(|field| config.field_enabled(*field))
        .map(SortMetadataField::key)
        .collect();
    format!("metadata:{}", fields.join(","))
}

/// Compare two tracks under the configured ordering strategy.
///
/// Ordering is intentionally independent from display labels. Text uses a
/// case-insensitive natural comparison followed by a case-sensitive natural
/// tie-break. Metadata fields are visited in the canonical Artist, Album,
/// Track Number, Title order; unusable fields are omitted and the filename is
/// used when no usable metadata remains and as the final tie-break.
pub fn compare_tracks<C: ColumnConfig>(left: &Track, right: &Track, config: &C) -> Ordering {
    match config.display_by() {
        SortBy::Filename => compare_text(&file_name(left), &file_name(right)),
        SortBy::Metadata => compare_metadata_tracks(left, right, config),
    }
}

fn compare_text(left: &str, right: &str) -> Ordering {
    natord::compare_ignore_case(left, right).then_with(|| natord::compare(left, right))
}

#[derive(Debug, Clone, Copy)]
enum MetadataSortValue<'a> {
    Text(&'a str),
    Number(u32),
}

fn compare_metadata_tracks<C: ColumnConfig>(left: &Track, right: &Track, config: &C) -> Ordering {
    let left_name = file_name(left);
    let right_name = file_name(right);
    let left_values = metadata_sort_values(left, config)
        .unwrap_or_else(|| vec![MetadataSortValue::Text(&left_name)]);
    let right_values = metadata_sort_values(right, config)
        .unwrap_or_else(|| vec![MetadataSortValue::Text(&right_name)]);

    let metadata_order = left_values
        .iter()
        .zip(&right_values)
        .map(|(left, right)| compare_metadata_value(*left, *right))
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or_else(|| left_values.len().cmp(&right_values.len()));
    if metadata_order != Ordering::Equal {
        return metadata_order;
    }

    compare_text(&file_name(left), &file_name(right))
}

fn compare_metadata_value(left: MetadataSortValue<'_>, right: MetadataSortValue<'_>) -> Ordering {
    match (left, right) {
        (MetadataSortValue::Text(left), MetadataSortValue::Text(right)) => {
            compare_text(left, right)
        }
        (MetadataSortValue::Number(left), MetadataSortValue::Number(right)) => left.cmp(&right),
        (MetadataSortValue::Number(left), MetadataSortValue::Text(right)) => {
            compare_text(&left.to_string(), right)
        }
        (MetadataSortValue::Text(left), MetadataSortValue::Number(right)) => {
            compare_text(left, &right.to_string())
        }
    }
}

/// Human readable track label used by playlist rows, honouring the Playlist
/// columns display strategy and its canonical metadata order.
///
/// Filename shows the file name; Metadata shows the enabled fields joined in
/// order, dropping empty/"Unknown" values, and falls back to the file name
/// when no usable field remains.
///
/// Stream tracks displayed under the Filename strategy append the EXTINF
/// title in parentheses after the URL when the resolver populated it, so the
/// queue row reads `https://... - (Lofi Girl)` instead of a bare URL. The
/// suffix is dropped when the title is empty or only the derived (untagged)
/// fallback is available, so freshly added streams with no resolver output
/// yet stay clean.
pub fn display_label<C: ColumnConfig>(track: &Track, config: &C) -> String {
    display_label_with_metadata_order(track, config, &SortMetadataField::ORDER)
}

/// Human readable label used by the Now Playing band.
///
/// Now Playing has its own display configuration and intentionally retains its
/// established Track Number, Artist, Album, Title order. Playlist rows use
/// [`display_label`] instead.
pub fn now_playing_display_label(track: &Track, config: &SortTracksConfig) -> String {
    const NOW_PLAYING_METADATA_ORDER: [SortMetadataField; 4] = [
        SortMetadataField::TrackNumber,
        SortMetadataField::Artist,
        SortMetadataField::Album,
        SortMetadataField::Title,
    ];
    display_label_with_metadata_order(track, config, &NOW_PLAYING_METADATA_ORDER)
}

fn display_label_with_metadata_order<C: ColumnConfig>(
    track: &Track,
    config: &C,
    metadata_order: &[SortMetadataField],
) -> String {
    if config.display_by() == SortBy::Metadata {
        let label = metadata_display_key(track, config, metadata_order);
        if !label.is_empty() {
            return label;
        }
    }
    let name = file_name(track);
    if track.is_stream()
        && let Some(meta) = track.metadata()
        && meta.title_tagged
        && !meta.title.is_empty()
    {
        return format!("{name} - ({})", meta.title);
    }
    name
}

/// The track's file name (stem), used for the Filename strategy and as the
/// fallback when no metadata field is usable.
///
/// Stream tracks have no file stem, so this function falls back to the
/// source display location (URL or path) — keeps the sort deterministic when a
/// playlist mixes local files with streams.
fn file_name(track: &Track) -> String {
    track
        .path()
        .and_then(|p| p.file_stem())
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| track.display_location().into_owned())
}

/// Join the enabled metadata fields (excluding "Unknown"/empty) into one
/// display string, or return an empty string when nothing is usable.
fn metadata_display_key(
    track: &Track,
    config: &impl ColumnConfig,
    metadata_order: &[SortMetadataField],
) -> String {
    // Streams under Metadata mirror the sort fallback: see
    // `stream_should_fallback_to_filename`. Keeping display and sort in lock
    // step prevents the playlist row from showing one label while the
    // ordering key uses another.
    if stream_should_fallback_to_filename(track, config) {
        return String::new();
    }

    let Some(meta) = track.metadata() else {
        return String::new();
    };

    let mut parts: Vec<String> = Vec::new();
    for &field in metadata_order {
        if !config.field_enabled(field) {
            continue;
        }
        match field {
            SortMetadataField::Artist => push_if_usable(&mut parts, &meta.artist),
            SortMetadataField::Album => push_if_usable(&mut parts, &meta.album),
            SortMetadataField::TrackNumber => {
                if let Some(number) = meta.track_number {
                    parts.push(number.to_string());
                }
            }
            SortMetadataField::Title => push_if_usable(&mut parts, &meta.title),
        }
    }

    if parts.is_empty() {
        String::new()
    } else {
        parts.join(" - ")
    }
}

/// True when a stream must fall back to its Filename for ordering, even
/// though Metadata is the configured strategy.
///
/// Streams only carry Title as reliable metadata (artist and album are
/// usually empty, track number is never populated by the resolver). If the
/// user did not include Title in the sort fields, or the resolver never
/// attached a tagged, non-empty Title, sorting by the remaining fields
/// would either be empty (degenerate "all equal" ordering) or carry
/// meaningless garbage. Falling back to the URL pins streams to a stable,
/// deterministic position instead.
fn stream_should_fallback_to_filename(track: &Track, config: &impl ColumnConfig) -> bool {
    if !track.is_stream() {
        return false;
    }
    if !config.field_enabled(SortMetadataField::Title) {
        return true;
    }
    !matches!(track.metadata(), Some(meta) if meta.title_tagged && !meta.title.is_empty())
}

/// Collect metadata values in the canonical ordering priority, omitting
/// missing and "Unknown" values. `None` means the filename is the ordering
/// value for this track.
fn metadata_sort_values<'a>(
    track: &'a Track,
    config: &impl ColumnConfig,
) -> Option<Vec<MetadataSortValue<'a>>> {
    if stream_should_fallback_to_filename(track, config) {
        return None;
    }

    let Some(meta) = track.metadata() else {
        return None;
    };

    let mut values = Vec::new();
    for field in SortMetadataField::ORDER {
        if !config.field_enabled(field) {
            continue;
        }
        match field {
            SortMetadataField::Artist => {
                if let Some(value) = usable_metadata_value(&meta.artist) {
                    values.push(MetadataSortValue::Text(value));
                }
            }
            SortMetadataField::Album => {
                if let Some(value) = usable_metadata_value(&meta.album) {
                    values.push(MetadataSortValue::Text(value));
                }
            }
            SortMetadataField::TrackNumber => {
                if let Some(number) = meta.track_number {
                    values.push(MetadataSortValue::Number(number));
                }
            }
            SortMetadataField::Title => {
                if let Some(value) = usable_metadata_value(&meta.title) {
                    values.push(MetadataSortValue::Text(value));
                }
            }
        }
    }

    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

/// Unusable metadata labels the reader can emit when a tag is absent.
///
/// The reader reports a missing artist/album/title as the shared `UNKNOWN_*`
/// sentinels instead of an empty string, so dropping only the bare "Unknown"
/// would still pollute labels and sort keys with those values. The check
/// compares against the single source of truth, so a rename in the metadata
/// module propagates here automatically.
fn is_unusable_metadata_label(value: &str) -> bool {
    value.eq_ignore_ascii_case("unknown")
        || value.eq_ignore_ascii_case(crate::metadata::UNKNOWN_ARTIST)
        || value.eq_ignore_ascii_case(crate::metadata::UNKNOWN_ALBUM)
        || value.eq_ignore_ascii_case(crate::metadata::UNKNOWN_TITLE)
}

/// Append `value` unless it is empty or one of the reader's "Unknown"
/// placeholders (case-insensitive and trimmed), so untagged fields never
/// pollute the ordering key or the displayed label.
fn push_if_usable(parts: &mut Vec<String>, value: &str) {
    if let Some(value) = usable_metadata_value(value) {
        parts.push(value.to_string());
    }
}

fn usable_metadata_value(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty() && !is_unusable_metadata_label(value)).then_some(value)
}

/// Reorder a playlist in place by the configured strategy.
///
/// The selection cursor follows its track so a sort never redirects the
/// highlight to a different song.
pub fn sort_playlist<C: ColumnConfig>(playlist: &mut Playlist, config: &C) {
    playlist.sort_tracks_by_compare(|left, right| compare_tracks(left, right, config));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::TrackMetadata;
    use std::path::PathBuf;
    use std::time::Duration;

    fn sort_cfg(sort_by: SortBy) -> SortTracksConfig {
        SortTracksConfig {
            sort_by,
            ..SortTracksConfig::default()
        }
    }

    fn meta(artist: &str, album: &str, title: &str) -> TrackMetadata {
        TrackMetadata {
            title: title.to_string(),
            title_tagged: true,
            artist: artist.to_string(),
            album: album.to_string(),
            track_number: None,
            duration: Duration::from_secs(60),
            bitrate: None,
            sample_rate: None,
            codec: "MP3".to_string(),
            format: "MP3".to_string(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        }
    }

    fn named_track(path: &str, metadata: Option<TrackMetadata>) -> Track {
        let mut track = Track::local(path);
        if let Some(meta) = metadata {
            track.set_metadata(meta);
        }
        track
    }

    fn playlist(tracks: Vec<Track>) -> Playlist {
        let mut playlist = Playlist::new();
        playlist.extend(tracks);
        playlist
    }

    #[test]
    fn filename_sort_orders_by_display_name() {
        let mut cfg = sort_cfg(SortBy::Filename);
        cfg.metadata_artist = true; // ignored under Filename
        let mut pl = playlist(vec![
            named_track("/z.mp3", None),
            named_track("/a.mp3", None),
            named_track("/m.mp3", None),
        ]);

        sort_playlist(&mut pl, &cfg);

        assert_eq!(
            pl.tracks()
                .iter()
                .map(|t| t.display_name().into_owned())
                .collect::<Vec<_>>(),
            vec!["a", "m", "z"]
        );
    }

    #[test]
    fn filename_sort_uses_natural_case_insensitive_order_with_tie_breaking() {
        let cfg = sort_cfg(SortBy::Filename);
        let mut pl = playlist(vec![
            named_track("/track 10.mp3", None),
            named_track("/track 2.mp3", None),
            named_track("/track 1.mp3", None),
            named_track("/track 2.mp3", None),
            named_track("/Track 2.mp3", None),
        ]);

        sort_playlist(&mut pl, &cfg);

        let names: Vec<_> = pl
            .tracks()
            .iter()
            .map(|track| track.display_name().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["track 1", "Track 2", "track 2", "track 2", "track 10"]
        );
    }

    #[test]
    fn exact_comparator_ties_keep_their_input_order() {
        let cfg = sort_cfg(SortBy::Filename);
        let mut pl = playlist(vec![
            stream_track("https://example.com/live", Some("First")),
            stream_track("https://example.com/live", Some("Second")),
        ]);

        sort_playlist(&mut pl, &cfg);

        let titles: Vec<_> = pl
            .tracks()
            .iter()
            .map(|track| track.metadata().unwrap().title.as_str())
            .collect();
        assert_eq!(titles, vec!["First", "Second"]);
    }

    #[test]
    fn metadata_sort_orders_by_enabled_fields() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_album = true;
        cfg.metadata_title = true;
        let mut pl = playlist(vec![
            named_track(
                "/2.mp3",
                Some(meta("Rush", "Moving Pictures", "Tom Sawyer")),
            ),
            named_track("/1.mp3", Some(meta("Rush", "2112", "Overture"))),
            named_track(
                "/3.mp3",
                Some(meta("Alphaville", "Forever Young", "Forever Young")),
            ),
        ]);

        sort_playlist(&mut pl, &cfg);

        let paths: Vec<_> = pl
            .tracks()
            .iter()
            .map(|t| t.path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            paths,
            vec!["/3.mp3", "/1.mp3", "/2.mp3"],
            "tracks sorted by artist-album-title"
        );
    }

    #[test]
    fn metadata_sort_orders_artist_album_track_number_and_title() {
        fn track(path: &str, artist: &str, album: &str, number: u32, title: &str) -> Track {
            let mut m = meta(artist, album, title);
            m.track_number = Some(number);
            named_track(path, Some(m))
        }
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_album = true;
        cfg.metadata_track_number = true;
        cfg.metadata_title = true;
        let mut pl = playlist(vec![
            track("/artist-z.mp3", "Z", "A", 1, "A"),
            track("/track-two.mp3", "Same", "Z", 2, "A"),
            track("/artist-a.mp3", "A", "Z", 99, "Z"),
            track("/track-one.mp3", "Same", "Z", 1, "Z"),
            track("/album-a.mp3", "Same", "A", 99, "Z"),
        ]);

        sort_playlist(&mut pl, &cfg);

        let paths: Vec<_> = pl
            .tracks()
            .iter()
            .map(|t| t.path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/artist-a.mp3",
                "/album-a.mp3",
                "/track-one.mp3",
                "/track-two.mp3",
                "/artist-z.mp3",
            ],
            "metadata fields must sort artist, album, track number, then title"
        );
    }

    #[test]
    fn track_numbers_over_999_sort_numerically_not_lexically() {
        fn track(path: &str, number: u32) -> Track {
            let mut m = meta("S", "O", "T");
            m.track_number = Some(number);
            named_track(path, Some(m))
        }
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_track_number = true;

        // Regression fixture: numeric comparison must not depend on padding or
        // on the largest number present in the playlist.
        let mut pl = playlist(vec![
            track("/b.mp3", 1000),
            track("/a.mp3", 999),
            track("/c.mp3", 10000),
        ]);

        sort_playlist(&mut pl, &cfg);

        // Assert the actual queue order rather than recalculating a standalone
        // key that has no playlist context.
        let paths: Vec<_> = pl
            .tracks()
            .iter()
            .map(|t| t.path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["/a.mp3", "/b.mp3", "/c.mp3"]);
    }

    #[test]
    fn metadata_sort_naturally_orders_pearl_jam_backspacer_tracks() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_album = true;
        cfg.metadata_track_number = true;
        cfg.metadata_title = true;
        let tracks = (1..=11)
            .rev()
            .map(|number| {
                let mut metadata = meta("Pearl Jam", "Backspacer", &format!("Track {number}"));
                metadata.track_number = Some(number);
                named_track(&format!("/music/{number}.mp3"), Some(metadata))
            })
            .collect::<Vec<_>>();
        let mut pl = playlist(tracks);

        sort_playlist(&mut pl, &cfg);

        let numbers: Vec<_> = pl
            .tracks()
            .iter()
            .map(|track| track.metadata().unwrap().track_number.unwrap())
            .collect();
        assert_eq!(numbers, (1..=11).collect::<Vec<_>>());
    }

    #[test]
    fn metadata_text_fields_use_natural_comparison() {
        for (field, first, second) in [
            (SortMetadataField::Artist, "Artist 10", "Artist 2"),
            (SortMetadataField::Album, "Album 10", "Album 2"),
            (SortMetadataField::Title, "Title 10", "Title 2"),
        ] {
            let mut cfg = sort_cfg(SortBy::Metadata);
            match field {
                SortMetadataField::Artist => cfg.metadata_artist = true,
                SortMetadataField::Album => cfg.metadata_album = true,
                SortMetadataField::TrackNumber => cfg.metadata_track_number = true,
                SortMetadataField::Title => cfg.metadata_title = true,
            }
            let left = named_track("/left.mp3", Some(meta(first, first, first)));
            let right = named_track("/right.mp3", Some(meta(second, second, second)));

            assert_eq!(compare_tracks(&right, &left, &cfg), Ordering::Less);
        }
    }

    #[test]
    fn effective_change_ignores_sub_options_under_filename() {
        let initial = sort_cfg(SortBy::Filename);
        let mut final_cfg = sort_cfg(SortBy::Filename);
        // Sub-option edits while staying on Filename are NOT an effective change.
        final_cfg.metadata_artist = true;
        final_cfg.metadata_title = true;
        assert!(!sort_config_effective_changed(&initial, &final_cfg));

        // Turning Metadata on IS a change.
        let mut metadata = sort_cfg(SortBy::Metadata);
        metadata.metadata_artist = true;
        assert!(sort_config_effective_changed(&initial, &metadata));

        // Metadata on -> different sub-options is a change.
        let mut other = metadata.clone();
        other.metadata_title = true;
        assert!(sort_config_effective_changed(&metadata, &other));

        // Metadata on -> Metadata on with same options is NOT a change.
        assert!(!sort_config_effective_changed(&metadata, &metadata.clone()));

        // Metadata on -> Filename (dropping Metadata) IS a change.
        assert!(sort_config_effective_changed(
            &metadata,
            &sort_cfg(SortBy::Filename)
        ));

        let mut all_metadata = sort_cfg(SortBy::Metadata);
        all_metadata.metadata_artist = true;
        all_metadata.metadata_album = true;
        all_metadata.metadata_track_number = true;
        all_metadata.metadata_title = true;
        assert_eq!(
            effective_ordering_key(&all_metadata),
            "metadata:artist,album,track_number,title"
        );
    }

    #[test]
    fn unknown_or_missing_field_is_omitted() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_album = true;
        let mut pl = playlist(vec![
            named_track("/b.mp3", Some(meta("Unknown", "Album B", "T"))),
            named_track("/a.mp3", Some(meta("Artist A", "Album A", "T"))),
        ]);

        sort_playlist(&mut pl, &cfg);

        // The "Unknown" artist is dropped; /b sorts by its album.
        let paths: Vec<_> = pl
            .tracks()
            .iter()
            .map(|t| t.path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["/b.mp3", "/a.mp3"]);
    }

    #[test]
    fn track_without_metadata_falls_back_to_filename() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        let mut pl = playlist(vec![
            named_track("/zz.mp3", Some(meta("Zed", "Album", "T"))),
            named_track("/aa.mp3", None), // no metadata -> filename fallback
        ]);

        sort_playlist(&mut pl, &cfg);

        let paths: Vec<_> = pl
            .tracks()
            .iter()
            .map(|t| t.path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["/aa.mp3", "/zz.mp3"]);
    }

    fn stream_track(url: &str, title: Option<&str>) -> Track {
        let mut track = Track::from_stream(
            url::Url::parse(url).expect("valid url"),
            crate::stream::StreamKind::Http,
        );
        if let Some(title) = title {
            let mut m = meta("", "", title);
            m.artist.clear();
            m.album.clear();
            track.set_metadata(m);
        }
        track
    }

    #[test]
    fn stream_filename_display_appends_extinf_title_when_present() {
        let cfg = sort_cfg(SortBy::Filename);
        let track = stream_track(
            "https://www.youtube.com/watch?v=S_MOd40zlYU",
            Some("Lofi Girl"),
        );
        assert_eq!(
            display_label(&track, &cfg),
            "https://www.youtube.com/watch?v=S_MOd40zlYU - (Lofi Girl)"
        );
    }

    #[test]
    fn stream_filename_display_keeps_bare_url_when_resolver_has_not_run() {
        let cfg = sort_cfg(SortBy::Filename);
        let track = stream_track("https://www.youtube.com/watch?v=abc", None);
        assert_eq!(
            display_label(&track, &cfg),
            "https://www.youtube.com/watch?v=abc"
        );
    }

    #[test]
    fn stream_filename_display_drops_title_when_only_untagged_fallback_is_present() {
        // title_tagged=false must not produce the parenthesised suffix: that
        // means the metadata is just a default empty record, not a real
        // resolver-supplied EXTINF title.
        let cfg = sort_cfg(SortBy::Filename);
        let mut track = stream_track("https://www.youtube.com/watch?v=abc", None);
        track.set_metadata(TrackMetadata {
            title: String::new(),
            title_tagged: false,
            ..meta("", "", "")
        });
        assert_eq!(
            display_label(&track, &cfg),
            "https://www.youtube.com/watch?v=abc"
        );
    }

    /// Streams under Metadata fall back to their Filename (URL) for the
    /// sort key whenever Title is not part of the selected fields, even if
    /// other fields (artist/album) are populated. Streams only carry Title
    /// as reliable metadata so sorting on the rest would either collapse
    /// to "all equal" or carry meaningless empty strings.
    #[test]
    fn stream_sort_uses_filename_when_title_not_selected_under_metadata() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_album = true; // Title intentionally NOT selected
        let track_with_album = {
            let mut t = stream_track("https://www.youtube.com/watch?v=abc", Some("Lofi Girl"));
            // Force a populated album even though streams usually have none,
            // to prove that Album is still ignored when Title is not selected.
            let mut m = meta("Channel", "Lofi Album", "Lofi Girl");
            m.title_tagged = true;
            t.set_metadata(m);
            t
        };
        let other = stream_track("https://www.youtube.com/watch?v=def", Some("Other"));
        assert_eq!(
            compare_tracks(&track_with_album, &other, &cfg),
            Ordering::Less,
            "stream with Title not selected must sort by Filename, not by Album"
        );
    }

    /// Streams whose resolver never attached a tagged, non-empty Title
    /// also fall back to Filename even when Title IS selected, so the
    /// ordering key is the URL rather than an empty or untagged string.
    #[test]
    fn stream_sort_uses_filename_when_title_selected_but_resolver_has_not_run() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_title = true;
        let track = stream_track("https://www.youtube.com/watch?v=abc", None);
        let other = stream_track("https://www.youtube.com/watch?v=def", None);
        assert_eq!(
            compare_tracks(&track, &other, &cfg),
            Ordering::Less,
            "stream without metadata must sort by Filename even when Title is selected"
        );
        assert_eq!(
            display_label(&track, &cfg),
            "https://www.youtube.com/watch?v=abc",
            "display mirrors sort for unresolved streams"
        );
    }

    /// A stream with a real resolver-supplied Title sorts and displays
    /// using Title under Metadata, even with other Metadata fields unset
    /// (the typical case for YouTube streams).
    #[test]
    fn stream_sort_uses_title_when_resolver_supplied_it() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_title = true;
        let track = stream_track("https://www.youtube.com/watch?v=abc", Some("Lofi Girl"));
        let other = stream_track("https://www.youtube.com/watch?v=def", Some("Lofi Girl 10"));
        assert_eq!(
            compare_tracks(&track, &other, &cfg),
            Ordering::Less,
            "the resolver-supplied title drives the sort naturally"
        );
    }

    /// Local tracks already obey rule 1: sort uses whichever fields exist
    /// and falls back to Filename when nothing usable remains. Locking in
    /// the contract so a future change to the stream branch does not
    /// accidentally weaken the local-track branch.
    #[test]
    fn local_sort_uses_existing_metadata_and_falls_back_when_unusable() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_album = true;
        cfg.metadata_title = true;
        // Local track with one usable field (Album).
        let partial = named_track(
            "/audio/song.mp3",
            Some(meta("Unknown", "Real Album", "Unknown")),
        );
        let album_only = named_track(
            "/audio/song.mp3",
            Some(meta("Unknown", "Zebra Album", "Unknown")),
        );
        assert_eq!(
            compare_tracks(&partial, &album_only, &cfg),
            Ordering::Less,
            "tracks with at least one usable field sort by it"
        );
        // Local track with no usable metadata at all.
        let bare = named_track("/audio/song.mp3", None);
        // Local track whose metadata only holds Unknown placeholders.
        let unknown = named_track(
            "/audio/song.mp3",
            Some(meta("Unknown", "Unknown", "Unknown")),
        );
        assert_eq!(
            compare_tracks(&bare, &unknown, &cfg),
            Ordering::Equal,
            "tracks without metadata and with only Unknown fields fall back to Filename"
        );
    }

    #[test]
    fn sort_keeps_the_cursor_on_its_track() {
        let mut pl = playlist(vec![
            named_track("/c.mp3", None),
            named_track("/a.mp3", None),
            named_track("/b.mp3", None),
        ]);
        pl.select(0); // cursor on /c
        sort_playlist(&mut pl, &sort_cfg(SortBy::Filename));

        let cursor_path = pl.current().unwrap().path().map(Path::to_path_buf);
        assert_eq!(cursor_path, Some(PathBuf::from("/c.mp3")));
        assert_eq!(pl.cursor(), 2, "cursor follows its track to the tail");
    }

    #[test]
    fn display_label_uses_filename_by_default() {
        let cfg = sort_cfg(SortBy::Filename);
        // Filename strategy always shows the file name, even with metadata.
        let track = named_track("/audio/song.mp3", Some(meta("Artist", "Album", "Title")));
        assert_eq!(display_label(&track, &cfg), "song");
        // Without metadata, Filename label is the file stem too.
        let plain = named_track("/audio/song.mp3", None);
        assert_eq!(display_label(&plain, &cfg), "song");
    }

    #[test]
    fn display_label_joins_selected_metadata_fields() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_title = true;
        let track = named_track(
            "/audio/song.mp3",
            Some(meta("Rush", "Signals", "Tom Sawyer")),
        );
        assert_eq!(display_label(&track, &cfg), "Rush - Tom Sawyer");

        // Unknown artist is omitted; the label uses the remaining fields.
        let cfg2 = SortTracksConfig {
            sort_by: SortBy::Metadata,
            metadata_artist: true,
            metadata_album: true,
            ..Default::default()
        };
        let unknown = named_track("/x.mp3", Some(meta("Unknown", "Album", "T")));
        assert_eq!(display_label(&unknown, &cfg2), "Album");
    }

    #[test]
    fn playlist_columns_metadata_always_includes_title() {
        let mut metadata = meta("Artist", "Album", "Title");
        metadata.track_number = Some(7);
        let track = named_track("/music/song.mp3", Some(metadata));
        let config = PlaylistColumnsConfig {
            display_by: SortBy::Metadata,
            ..PlaylistColumnsConfig::default()
        };

        assert_eq!(display_label(&track, &config), "Title");
    }

    #[test]
    fn display_label_falls_back_to_filename_when_no_field_is_usable() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_album = true;
        // All marked fields are "Unknown" -> filename fallback.
        let track = named_track("/audio/thing.mp3", Some(meta("Unknown", "Unknown", "T")));
        assert_eq!(display_label(&track, &cfg), "thing");
    }

    #[test]
    fn display_label_joins_artist_album_track_number_and_title() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_album = true;
        cfg.metadata_track_number = true;
        cfg.metadata_title = true;
        let mut m = meta("Some Artist", "Ten", "Black");
        m.track_number = Some(3);
        let track = named_track("/m/song.mp3", Some(m));
        assert_eq!(
            display_label(&track, &cfg),
            "Some Artist - Ten - 3 - Black",
            "metadata labels must follow artist, album, track number, title order"
        );
    }

    #[test]
    fn now_playing_display_keeps_its_separate_field_order() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_album = true;
        cfg.metadata_track_number = true;
        cfg.metadata_title = true;
        let mut m = meta("Some Artist", "Ten", "Black");
        m.track_number = Some(3);
        let track = named_track("/m/song.mp3", Some(m));

        assert_eq!(
            now_playing_display_label(&track, &cfg),
            "3 - Some Artist - Ten - Black"
        );
    }

    #[test]
    fn display_label_drops_unknown_artist_sentinel() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_title = true;
        // The reader reports a missing artist as "Unknown Artist", not as a
        // bare "Unknown"; the label must still fall back to the remaining
        // fields instead of showing the placeholder.
        let track = named_track(
            "/audio/song.mp3",
            Some(meta("Unknown Artist", "Ten", "Black")),
        );
        assert_eq!(display_label(&track, &cfg), "Black");
    }

    #[test]
    fn display_label_drops_unknown_album_sentinel() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_album = true;
        cfg.metadata_title = true;
        // Missing album + missing title both arrive as sentinels; the label
        // then uses the artist alone instead of falling back to the file name.
        let track = named_track(
            "/audio/song.mp3",
            Some(meta("Rush", "Unknown Album", "Unknown Title")),
        );
        assert_eq!(display_label(&track, &cfg), "Rush");
    }

    #[test]
    fn metadata_comparison_uses_remaining_fields_for_unknown_artist() {
        let mut cfg = sort_cfg(SortBy::Metadata);
        cfg.metadata_artist = true;
        cfg.metadata_album = true;
        cfg.metadata_title = true;
        // Both tracks carry the "Unknown Artist" sentinel, so the order
        // must come from album/title rather than from the placeholder.
        let mut pl = playlist(vec![
            named_track("/b.mp3", Some(meta("Unknown Artist", "Ten", "Black"))),
            named_track("/a.mp3", Some(meta("Unknown Artist", "Acme", "Sea"))),
        ]);

        sort_playlist(&mut pl, &cfg);

        let paths: Vec<_> = pl
            .tracks()
            .iter()
            .map(|track| track.path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["/a.mp3", "/b.mp3"]);
    }
}
