//! Remote lyrics resolution through the LRCLIB API.
//!
//! LRCLIB needs no API key and asks clients to identify themselves with a
//! User-Agent, which is the only contract enforced here. Every network or
//! parse failure degrades to `None` with a warning: lyrics must never break
//! playback or the UI thread, and this module runs entirely inside blocking
//! workers.
//!
//! Results are validated against the identity of the playing song, never
//! trusted blindly. Two tiers apply, in order: an exact release match
//! (artist, title and album) and a partial match (artist and title only).
//! In every tier both the artist and the title must agree with the song;
//! the album is the only field allowed to differ in the fallback tier.

use std::io::{self, Read};
use std::time::Duration;

use serde::Deserialize;

use super::{LyricsDocument, LyricsRequest};

/// LRCLIB lyrics search endpoint.
const SEARCH_URL: &str = "https://lrclib.net/api/search";

/// Honest client identification required by the LRCLIB API policy.
const USER_AGENT: &str = "harmonium/0.1.0 (terminal music player)";

/// Timeout for one API call; lyrics are nice to have, never worth hanging.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling for a response body read from the lyrics provider.
///
/// LRCLIB answers a search with a small array of results (timed metadata plus
/// a few lyric lines each), so a body larger than this is either a server
/// error page or an unbounded payload we should refuse rather than buffer.
const MAX_LYRIC_BODY_BYTES: u64 = 1024 * 1024;

/// Read at most `max` bytes from `reader`, erroring when the body overflows.
///
/// `reqwest`'s `.json()` would deserialize directly from the stream, but it
/// has no built-in size cap and would happily buffer an arbitrary response.
fn read_bounded(reader: &mut impl Read, max: u64) -> io::Result<Vec<u8>> {
    crate::net::read_bounded_body(reader, None, usize::try_from(max).unwrap_or(usize::MAX)).map_err(
        |error| match error {
            crate::net::BoundedBodyError::Read(error) => error,
            error => io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
        },
    )
}

/// One search result as returned by LRCLIB, only the fields we consume.
///
/// The API sends camelCase keys (`trackName`, `plainLyrics`,
/// `syncedLyrics`); without the rename the deserializer would silently
/// produce `None` fields and the app would report missing lyrics.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LrclibResult {
    /// Track title reported by the service.
    pub track_name: Option<String>,
    /// Artist name reported by the service.
    pub artist_name: Option<String>,
    /// Album name reported by the service, used for the exact-release tier.
    pub album_name: Option<String>,
    /// Untimed lyrics, `None` when unavailable.
    pub plain_lyrics: Option<String>,
    /// Timed (LRC) lyrics, `None` when unavailable.
    pub synced_lyrics: Option<String>,
}

/// The identity of the song used for searching and validating results.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SongIdentity {
    /// Canonical title: tagged title or file stem.
    title: String,
    /// Tagged artist when known; `None` when the tags were silent.
    artist: Option<String>,
    /// Tagged album when known; `None` when the tags were silent.
    album: Option<String>,
}

impl SongIdentity {
    /// Build the identity from a request, keeping only real values.
    fn from_request(req: &LyricsRequest) -> Self {
        Self {
            title: song_title(req),
            artist: req.artist.as_deref().and_then(known).map(str::to_string),
            album: req.album.as_deref().and_then(known).map(str::to_string),
        }
    }
}

/// LRCLIB backed provider used by the remote link of the source chain.
///
/// The blocking client is built once with the UA and timeout. Client
/// construction failure disables the provider entirely: resolution then
/// returns `None` without a panic.
pub struct LrcLibProvider {
    client: Option<reqwest::blocking::Client>,
}

impl LrcLibProvider {
    /// Build the provider with a configured blocking client.
    pub fn new() -> Self {
        let client = crate::net::build_finite_client(REQUEST_TIMEOUT, Some(USER_AGENT))
            .map_err(|error| tracing::warn!("cannot build lyrics HTTP client: {error}"))
            .ok();
        Self { client }
    }

    /// Search LRCLIB for the request, validating every accepted result.
    ///
    /// Attempt one queries with the full tagged identity (artist, title and
    /// album when known). When the tags lacked the artist, attempt two
    /// derives it from the file name (`Artist - Title`) and searches again;
    /// the tagged title stays authoritative in that iteration. Any payload
    /// whose artist or title disagrees with the query is rejected.
    pub fn resolve(&self, req: &LyricsRequest) -> Option<LyricsDocument> {
        let client = self.client.as_ref()?;
        let identity = SongIdentity::from_request(req);

        if let Some(document) = self.resolve_with(client, &identity) {
            return Some(document);
        }

        // Secondary iteration: the file name can only supply a better
        // artist when the tags said nothing, and only when it actually
        // differs from the identity already searched.
        if let Some(query) = fallback_query(&identity, req)
            && let Some(document) = self.resolve_with(client, &query)
        {
            return Some(document);
        }

        None
    }

    /// Run one search and return the validated best document, if any.
    fn resolve_with(
        &self,
        client: &reqwest::blocking::Client,
        identity: &SongIdentity,
    ) -> Option<LyricsDocument> {
        let results = search(client, identity)?;
        let result = select_result(&results, identity)?;
        result_to_document(result)
    }
}

impl Default for LrcLibProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical title for the identity: tagged title or the file stem.
fn song_title(req: &LyricsRequest) -> String {
    req.title
        .as_deref()
        .and_then(known)
        .map(str::to_string)
        .unwrap_or_else(|| stem_for(req))
}

/// File stem of the audio path, used to identify the song without tags.
fn stem_for(req: &LyricsRequest) -> String {
    req.audio_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Trimmed value when it carries real information, `None` for empty or
/// placeholder text such as the metadata `Unknown Artist` sentinel.
fn known(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case(crate::metadata::UNKNOWN_ARTIST)
        || trimmed.eq_ignore_ascii_case(crate::metadata::UNKNOWN_ALBUM)
        || trimmed.eq_ignore_ascii_case(crate::metadata::UNKNOWN_TITLE)
    {
        None
    } else {
        Some(trimmed)
    }
}

fn non_empty(value: &str) -> Option<&str> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.trim())
    }
}

/// Best-effort identity derived from the `Artist - Title` file name shape.
fn filename_identity(req: &LyricsRequest) -> Option<SongIdentity> {
    let stem = stem_for(req);
    if stem.is_empty() {
        return None;
    }
    let (artist, title) = stem.split_once(" - ")?;
    let artist = known(artist)?;
    let title = non_empty(title)?;
    Some(SongIdentity {
        title: title.to_string(),
        artist: Some(artist.to_string()),
        album: None,
    })
}

/// Identity for the secondary iteration, or `None` when the primary one
/// already used everything the file name could tell us.
fn fallback_query(identity: &SongIdentity, req: &LyricsRequest) -> Option<SongIdentity> {
    // A tagged artist is already enforced; the file name cannot add value.
    if identity.artist.is_some() {
        return None;
    }
    let filename = filename_identity(req)?;
    let tags_had_title = req.title.as_deref().and_then(known).is_some();
    let query = SongIdentity {
        // The tagged title is the song identity and wins over the file
        // name; the stem title is only a guess, so the file name may refine
        // it when the tags were silent.
        title: if tags_had_title {
            identity.title.clone()
        } else {
            filename.title
        },
        artist: filename.artist,
        album: None,
    };
    if query == *identity {
        None
    } else {
        Some(query)
    }
}

/// Run one LRCLIB search query for the given identity.
fn search(
    client: &reqwest::blocking::Client,
    identity: &SongIdentity,
) -> Option<Vec<LrclibResult>> {
    let mut url = reqwest::Url::parse(SEARCH_URL)
        .map_err(|error| tracing::warn!("invalid lyrics endpoint URL: {error}"))
        .ok()?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("track_name", &identity.title);
        if let Some(artist) = identity.artist.as_deref() {
            pairs.append_pair("artist_name", artist);
        }
        if let Some(album) = identity.album.as_deref() {
            pairs.append_pair("album_name", album);
        }
    }

    match client.get(url).send() {
        Ok(response) => {
            let mut response = match response.error_for_status() {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(
                        "lyrics provider returned an error status: {}",
                        crate::net::normalize_diagnostic(&error.to_string(), 256)
                    );
                    return None;
                }
            };
            // Cap the response body so a malformed or malicious endpoint cannot
            // hand us an unbounded payload. LRCLIB results are tiny (a few
            // lyrics plus metadata), so a generous ceiling is plenty.
            let body = match read_bounded(&mut response, MAX_LYRIC_BODY_BYTES) {
                Ok(body) => body,
                Err(error) => {
                    tracing::warn!("lyrics provider response too large or unreadable: {error}");
                    return None;
                }
            };
            match serde_json::from_slice::<Vec<LrclibResult>>(&body) {
                Ok(results) => Some(results),
                Err(error) => {
                    tracing::warn!("lyrics provider sent unreadable JSON: {error}");
                    None
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                "lyrics provider request failed: {}",
                crate::net::normalize_diagnostic(&error.to_string(), 256)
            );
            None
        }
    }
}

/// Pick the best validated result for the identity.
///
/// Only results whose artist and title agree with the song enter the
/// candidate pool (`title` always, `artist` when known). Inside that pool
/// the exact-release tier (tagged album equality) wins over the partial
/// artist+title tier; within a tier, synced lyrics beat plain ones and
/// the API's relevance order breaks the tie.
fn select_result<'a>(
    results: &'a [LrclibResult],
    identity: &SongIdentity,
) -> Option<&'a LrclibResult> {
    let matched: Vec<&LrclibResult> = results
        .iter()
        .filter(|result| {
            result
                .track_name
                .as_deref()
                .is_some_and(|name| equal_ci(name, &identity.title))
                && artist_matches(result, identity.artist.as_deref())
        })
        .collect();
    if matched.is_empty() {
        return None;
    }

    let pool: Vec<&LrclibResult> = match identity.album.as_deref() {
        Some(album) => {
            let exact: Vec<&LrclibResult> = matched
                .iter()
                .copied()
                .filter(|result| {
                    result
                        .album_name
                        .as_deref()
                        .is_some_and(|name| equal_ci(name, album))
                })
                .collect();
            if exact.is_empty() { matched } else { exact }
        }
        None => matched,
    };

    pool.iter()
        .copied()
        .find(|result| has_synced(result))
        .or_else(|| pool.iter().copied().find(|result| has_plain(result)))
}

/// Artist agreement: enforced when the song has one, otherwise any
/// non-empty artist is accepted so instrumental or blank rows stay out.
fn artist_matches(result: &LrclibResult, expected: Option<&str>) -> bool {
    match expected {
        Some(expected) => result
            .artist_name
            .as_deref()
            .is_some_and(|name| equal_ci(name, expected)),
        None => result
            .artist_name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty()),
    }
}

/// Case- and whitespace-insensitive equality for human-entered names.
fn equal_ci(left: &str, right: &str) -> bool {
    left.trim().to_lowercase() == right.trim().to_lowercase()
}

fn has_synced(result: &LrclibResult) -> bool {
    result
        .synced_lyrics
        .as_deref()
        .is_some_and(|lyrics| !lyrics.trim().is_empty())
}

fn has_plain(result: &LrclibResult) -> bool {
    result
        .plain_lyrics
        .as_deref()
        .is_some_and(|lyrics| !lyrics.trim().is_empty())
}

/// Convert a validated result into a document, synced lyrics preferred.
fn result_to_document(result: &LrclibResult) -> Option<LyricsDocument> {
    if has_synced(result) {
        return Some(super::parse_lrc(result.synced_lyrics.as_deref()?).into_document());
    }
    if has_plain(result) {
        return Some(LyricsDocument::from_plain(result.plain_lyrics.as_deref()?));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn result(
        track: &str,
        artist: &str,
        album: &str,
        synced: Option<&str>,
        plain: Option<&str>,
    ) -> LrclibResult {
        LrclibResult {
            track_name: Some(track.to_string()),
            artist_name: Some(artist.to_string()),
            album_name: Some(album.to_string()),
            synced_lyrics: synced.map(str::to_string),
            plain_lyrics: plain.map(str::to_string),
        }
    }

    fn identity(title: &str, artist: Option<&str>, album: Option<&str>) -> SongIdentity {
        SongIdentity {
            title: title.to_string(),
            artist: artist.map(str::to_string),
            album: album.map(str::to_string),
        }
    }

    fn request(path: &str) -> LyricsRequest {
        LyricsRequest {
            audio_path: PathBuf::from(path),
            title: None,
            artist: None,
            album: None,
        }
    }

    #[test]
    fn exact_album_release_beats_other_release_with_synced() {
        // The live version carries synced lyrics, yet the tagged album
        // "Ten" wins over it with only plain lyrics.
        let results = vec![
            result(
                "Black",
                "Pearl Jam",
                "Fila Forum, Milano",
                Some("[00:01.00]live line"),
                None,
            ),
            result("Black", "Pearl Jam", "Ten", None, Some("studio lyrics")),
        ];
        let selected = select_result(&results, &identity("Black", Some("Pearl Jam"), Some("Ten")))
            .expect("an exact release result must be picked");
        assert_eq!(selected.album_name.as_deref(), Some("Ten"));
        assert_eq!(selected.plain_lyrics.as_deref(), Some("studio lyrics"));
    }

    #[test]
    fn partial_artist_title_tier_wins_per_synced_when_album_unknown() {
        let results = vec![
            result("Black", "Pearl Jam", "Ten", Some("[00:01.00]studio"), None),
            result("Black", "Pearl Jam", "Live", None, Some("live")),
        ];
        let selected = select_result(&results, &identity("Black", Some("Pearl Jam"), None))
            .expect("an artist+title result must be picked");
        assert_eq!(selected.synced_lyrics.as_deref(), Some("[00:01.00]studio"));
    }

    #[test]
    fn album_tier_falls_back_to_partial_when_no_exact_release() {
        let results = vec![result(
            "Black",
            "Pearl Jam",
            "Fila Forum, Milano",
            Some("[00:01.00]live"),
            None,
        )];
        let selected = select_result(&results, &identity("Black", Some("Pearl Jam"), Some("Ten")))
            .expect("artist+title must still be honored without an exact release");
        assert_eq!(selected.album_name.as_deref(), Some("Fila Forum, Milano"));
    }

    #[test]
    fn mismatched_artist_is_rejected() {
        let results = vec![result("Black", "Metallica", "Ten", Some("l"), None)];
        assert_eq!(
            select_result(&results, &identity("Black", Some("Pearl Jam"), Some("Ten"))),
            None
        );
    }

    #[test]
    fn mismatched_title_is_rejected() {
        let results = vec![result("Yellow", "Coldplay", "Parachutes", Some("l"), None)];
        assert_eq!(
            select_result(
                &results,
                &identity("Black", Some("Coldplay"), Some("Parachutes"))
            ),
            None
        );
    }

    #[test]
    fn matching_ignores_case_and_whitespace() {
        let results = vec![result("  black ", "PEARL JAM", " ten ", Some("l"), None)];
        let selected = select_result(&results, &identity("Black", Some("Pearl Jam"), Some("Ten")))
            .expect("folding must accept differently cased names");
        assert_eq!(selected.artist_name.as_deref(), Some("PEARL JAM"));
    }

    #[test]
    fn unknown_artist_accepts_any_non_empty_artist() {
        let results = vec![result("Black", "Pearl Jam", "Ten", Some("l"), None)];
        assert!(select_result(&results, &identity("Black", None, None)).is_some());
    }

    #[test]
    fn synced_preferred_within_the_same_tier() {
        let results = vec![
            result("Black", "Pearl Jam", "Ten", None, Some("plain")),
            result("Black", "Pearl Jam", "Ten", Some("[00:01.00]synced"), None),
        ];
        let selected = select_result(&results, &identity("Black", Some("Pearl Jam"), Some("Ten")))
            .expect("a result must be picked");
        assert_eq!(selected.synced_lyrics.as_deref(), Some("[00:01.00]synced"));
    }

    #[test]
    fn empty_results_yield_nothing() {
        assert_eq!(
            select_result(&[], &identity("Black", Some("Pearl Jam"), None)),
            None
        );
        assert_eq!(
            select_result(
                &[LrclibResult {
                    track_name: Some("Black".to_string()),
                    artist_name: Some("Pearl Jam".to_string()),
                    album_name: None,
                    plain_lyrics: Some("   ".to_string()),
                    synced_lyrics: None,
                }],
                &identity("Black", Some("Pearl Jam"), None)
            ),
            None,
            "whitespace-only payloads count as unavailable"
        );
    }

    #[test]
    fn synced_payloads_are_parsed_with_real_timestamps() {
        let document = result_to_document(&result(
            "Black",
            "Pearl Jam",
            "Ten",
            Some("[00:01.50]hello"),
            None,
        ))
        .expect("synced payload becomes a document");
        assert_eq!(
            document.lines,
            vec![super::super::LyricsLine {
                timestamp_ms: Some(1500),
                text: "hello".to_string(),
                words: Vec::new(),
            }]
        );
    }

    #[test]
    fn plain_payloads_become_untimed_documents() {
        let document =
            result_to_document(&result("Black", "Pearl Jam", "Ten", None, Some("one\ntwo")))
                .expect("plain payload becomes a document");
        assert_eq!(document.text, "one\ntwo");
        assert!(document.lines.iter().all(|l| l.timestamp_ms.is_none()));
    }

    /// Regression: LRCLIB answers with camelCase keys. The struct must map
    /// them, otherwise deserialization yields all-`None` results and the
    /// app silently reports missing lyrics.
    #[test]
    fn api_response_deserializes_camel_case_lyrics() {
        let payload = r#"[
  {
    "id": 16832589,
    "name": "Black",
    "trackName": "Black",
    "artistName": "Pearl Jam",
    "albumName": "Fila Forum, Milano, Italy 17Se",
    "duration": 594.0,
    "instrumental": false,
    "plainLyrics": "Sheets of empty canvas",
    "syncedLyrics": "[00:12.68] Hey, hey, yeah, uh"
  },
  {
    "id": 21349384,
    "name": "Black",
    "trackName": "Black",
    "artistName": "Pearl Jam",
    "albumName": "Ten",
    "duration": 336.0,
    "instrumental": false,
    "plainLyrics": "Sheets of empty canvas",
    "syncedLyrics": "[00:12.68] Hey, hey, yeah, uh"
  }
]"#;
        let results: Vec<LrclibResult> =
            serde_json::from_str(payload).expect("camelCase API payload must deserialize");
        let selected = select_result(&results, &identity("Black", Some("Pearl Jam"), Some("Ten")))
            .expect("exact album release must win from the real payload");
        assert_eq!(selected.album_name.as_deref(), Some("Ten"));
        let document = result_to_document(selected).expect("selected endpoint result has lyrics");
        assert_eq!(document.lines[0].timestamp_ms, Some(12680));
    }

    #[test]
    fn song_title_prefers_the_tagged_title() {
        let mut req = request("/m/Some Song.mp3");
        req.title = Some("Proper Title".to_string());
        assert_eq!(song_title(&req), "Proper Title");
    }

    #[test]
    fn song_title_falls_back_to_the_stem_when_untagged() {
        assert_eq!(song_title(&request("/m/Some Song.mp3")), "Some Song");
    }

    #[test]
    fn song_title_ignores_unknown_placeholder() {
        let mut req = request("/m/Some Song.mp3");
        req.title = Some("Unknown Title".to_string());
        assert_eq!(song_title(&req), "Some Song");
    }

    #[test]
    fn unknown_placeholders_are_not_part_of_the_identity() {
        let mut req = request("/m/Pearl Jam - Black.mp3");
        req.artist = Some("Unknown Artist".to_string());
        req.album = Some("Unknown Album".to_string());
        let identity = SongIdentity::from_request(&req);
        assert_eq!(identity.artist, None);
        assert_eq!(identity.album, None);
    }

    #[test]
    fn filename_identity_parses_artist_dash_title() {
        let parsed = filename_identity(&request("/m/Pearl Jam - Black.mp3"))
            .expect("dash separated stem parses");
        assert_eq!(parsed.title, "Black");
        assert_eq!(parsed.artist.as_deref(), Some("Pearl Jam"));
    }

    #[test]
    fn filename_identity_needs_a_dash_separator() {
        assert_eq!(filename_identity(&request("/m/Black.mp3")), None);
    }

    #[test]
    fn fallback_query_keeps_the_tagged_title() {
        let mut req = request("/m/Pearl Jam - Black (Live).mp3");
        req.title = Some("Black".to_string());
        let identity = SongIdentity::from_request(&req);
        let query = fallback_query(&identity, &req).expect("derived artist adds value");
        assert_eq!(query.title, "Black");
        assert_eq!(query.artist.as_deref(), Some("Pearl Jam"));
    }

    #[test]
    fn fallback_query_uses_the_file_name_title_when_tags_are_silent() {
        let req = request("/m/Pearl Jam - Black.mp3");
        let identity = SongIdentity::from_request(&req);
        let query = fallback_query(&identity, &req).expect("stem identity can be refined");
        assert_eq!(query.title, "Black");
        assert_eq!(query.artist.as_deref(), Some("Pearl Jam"));
    }

    #[test]
    fn fallback_query_is_none_when_tags_have_the_artist() {
        let mut req = request("/m/Pearl Jam - Black.mp3");
        req.title = Some("Black".to_string());
        req.artist = Some("Pearl Jam".to_string());
        let identity = SongIdentity::from_request(&req);
        assert_eq!(fallback_query(&identity, &req), None);
    }

    #[test]
    fn read_bounded_reads_up_to_the_limit() {
        let mut data: &[u8] = b"hello world";
        assert_eq!(
            read_bounded(&mut data, MAX_LYRIC_BODY_BYTES).expect("fits"),
            b"hello world"
        );
    }

    #[test]
    fn read_bounded_rejects_an_oversized_body() {
        let mut data: &[u8] = b"0123456789";
        let error = read_bounded(&mut data, 5).expect_err("must reject oversized body");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
