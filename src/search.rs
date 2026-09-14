//! Pure search matching and result construction for contextual search.

use std::path::{Path, PathBuf};

use crate::track::{Track, TrackLocation};

/// Search surface selected when the search popup opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// Search supported audio below the browser's current directory.
    Browser,
    /// Search the tracks currently held by the active playlist.
    Playlist,
}

/// One result shown by the search results popup.
///
/// `identity` is deliberately stable and is used again when the user accepts
/// the result. Browser paths use `TrackLocation::Local`; playlist results use
/// the source's typed identity, never a queue index that could become stale
/// after a sort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// Stable identity used to reveal the result on the main thread.
    pub identity: TrackLocation,
    /// Sanitizable display label for the result list.
    pub label: String,
}

/// Match a query against a candidate using case-insensitive partial matching.
///
/// A query without `*` is treated as a substring. When `*` is present it has
/// its usual glob meaning and the whole expression remains partial, so both
/// `flow*.flac` and `*.ogg` work against a filename or path.
pub fn wildcard_match(query: &str, candidate: &str) -> bool {
    let query = query.to_lowercase();
    let candidate = candidate.to_lowercase();
    let pattern = format!("*{query}*");
    let pattern: Vec<char> = pattern.chars().collect();
    let candidate: Vec<char> = candidate.chars().collect();

    let mut previous = vec![false; candidate.len() + 1];
    previous[0] = true;

    for pattern_char in pattern {
        let mut current = vec![false; candidate.len() + 1];
        if pattern_char == '*' {
            current[0] = previous[0];
            for index in 1..=candidate.len() {
                current[index] = current[index - 1] || previous[index];
            }
        } else {
            for index in 1..=candidate.len() {
                current[index] = previous[index - 1] && pattern_char == candidate[index - 1];
            }
        }
        previous = current;
    }

    previous[candidate.len()]
}

/// Build browser results from the paths returned by the recursive scanner.
pub fn search_browser_paths(
    query: &str,
    root: &Path,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Vec<SearchResult> {
    paths
        .into_iter()
        .filter(|path| wildcard_match(query, &path.to_string_lossy()))
        .map(|path| SearchResult {
            identity: TrackLocation::local(path.clone()),
            label: path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string(),
        })
        .collect()
}

/// Build playlist results by checking the filename and every useful metadata
/// field exposed by the track model.
pub fn search_playlist_tracks(query: &str, tracks: &[Track]) -> Vec<SearchResult> {
    tracks
        .iter()
        .filter(|track| track_matches(query, track))
        .map(|track| SearchResult {
            identity: track.track_location(),
            label: track.display_name().into_owned(),
        })
        .collect()
}

fn track_matches(query: &str, track: &Track) -> bool {
    if wildcard_match(query, &track.display_location())
        || wildcard_match(query, &track.display_name())
    {
        return true;
    }

    let Some(metadata) = track.metadata() else {
        return false;
    };

    [
        metadata.title.as_str(),
        metadata.artist.as_str(),
        metadata.album.as_str(),
        metadata.album_artist.as_deref().unwrap_or_default(),
        metadata.genre.as_deref().unwrap_or_default(),
        metadata.year.as_deref().unwrap_or_default(),
        metadata.composer.as_deref().unwrap_or_default(),
        metadata.comment.as_deref().unwrap_or_default(),
        metadata.codec.as_str(),
        metadata.format.as_str(),
    ]
    .into_iter()
    .any(|field| !field.is_empty() && wildcard_match(query, field))
        || metadata
            .track_number
            .map(|number| wildcard_match(query, &number.to_string()))
            .unwrap_or(false)
        || metadata
            .disc_number
            .map(|number| wildcard_match(query, &number.to_string()))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::metadata::TrackMetadata;
    use url::Url;

    fn metadata() -> TrackMetadata {
        TrackMetadata {
            title: "Flow State".to_string(),
            title_tagged: true,
            artist: "The Uppercase Band".to_string(),
            album: "Live Sessions".to_string(),
            album_artist: Some("The Uppercase Band".to_string()),
            track_number: Some(7),
            disc_number: Some(2),
            genre: Some("Ambient".to_string()),
            year: Some("2026".to_string()),
            composer: Some("Composer".to_string()),
            comment: Some("A note".to_string()),
            duration: Duration::from_secs(1),
            ..Default::default()
        }
    }

    #[test]
    fn wildcard_matching_is_case_insensitive_and_partial() {
        assert!(wildcard_match("FLOW", "ambient/Flow State.flac"));
        assert!(wildcard_match("flow*.FLAC", "Ambient/Flow State.flac"));
        assert!(wildcard_match("*.ogg", "Ambient/recording.OGG"));
        assert!(!wildcard_match("flow*.flac", "Ambient/Flow State.ogg"));
    }

    #[test]
    fn browser_results_keep_a_path_identity_and_relative_label() {
        let root = Path::new("/music");
        let results = search_browser_paths(
            "flow",
            root,
            [
                root.join("live/Flow State.flac"),
                root.join("other/song.ogg"),
            ],
        );

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].identity,
            TrackLocation::local("/music/live/Flow State.flac")
        );
        assert_eq!(results[0].label, "live/Flow State.flac");
    }

    #[test]
    fn playlist_results_match_filename_and_metadata() {
        let mut track = Track::local("/music/quiet.flac");
        track.set_metadata(metadata());
        let tracks = [track, Track::local("/music/no-match.mp3")];

        assert_eq!(search_playlist_tracks("upperCASE", &tracks).len(), 1);
        assert_eq!(search_playlist_tracks("7", &tracks).len(), 1);
        assert_eq!(search_playlist_tracks("quiet", &tracks).len(), 1);
        assert!(search_playlist_tracks("missing", &tracks).is_empty());
    }

    #[test]
    fn playlist_search_keeps_colliding_local_and_url_identities_distinct() {
        let text = "https://example.com/live";
        let tracks = [
            Track::local(text),
            Track::from_stream(
                Url::parse(text).expect("valid URL"),
                crate::stream::StreamKind::Http,
            ),
        ];

        let results = search_playlist_tracks("example.com", &tracks);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].identity, TrackLocation::local(text));
        assert_eq!(
            results[1].identity,
            TrackLocation::url(Url::parse(text).expect("valid URL"))
        );
        assert_ne!(results[0].identity, results[1].identity);
    }
}
