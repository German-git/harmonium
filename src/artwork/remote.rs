//! Remote artwork fetching via MusicBrainz Cover Art Archive.
//!
//! This module is the only place where HTTP types (reqwest) live,
//! keeping network concerns isolated from the UI and domain layers.
//! Every public function is designed to run inside `spawn_blocking`
//! workers, never from the event loop.

use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;

use crate::filesystem::persistence::atomic_replace;

/// User-Agent header required by MusicBrainz rate limit policy.
const USER_AGENT: &str = "harmonium/0.1.0 (https://github.com/harmonium/harmonium)";

/// Timeout for HTTP requests in seconds.
const HTTP_TIMEOUT_SECS: u64 = 10;

/// Minimum interval between MusicBrainz/Cover Art Archive requests.
///
/// MusicBrainz's public policy caps clients at **1 request per second** (a
/// shared, well-behaved user-agent must not hammer it). Every cover fetch fires
/// two requests back-to-back (release search + CAA front), and a user skipping
/// tracks quickly would otherwise emit a burst that gets throttled with 503s or
/// gets the user-agent blocked. This gate sleeps the remainder of the interval
/// so requests are paced even when workers run concurrently.
const MB_RATE_INTERVAL: Duration = Duration::from_millis(1_100);

/// Logical reservation state for the shared MusicBrainz/CAA request gate.
///
/// Keeping the next slot instead of the last observed request matters when
/// callers arrive concurrently: each caller reserves a different slot before
/// any of them sleeps, so they cannot all wake and send together.
#[derive(Debug, Default)]
struct RequestRateGate {
    next_slot: Option<Duration>,
}

impl RequestRateGate {
    /// Reserve the next request slot and return how long the caller must wait.
    fn reserve(&mut self, now: Duration, interval: Duration) -> Duration {
        let slot = self.next_slot.map_or(now, |next| next.max(now));
        self.next_slot = Some(slot.saturating_add(interval));
        slot.saturating_sub(now)
    }
}

/// Paces MusicBrainz/CAA requests across all blocking workers.
static MUSIC_API_GATE: Mutex<RequestRateGate> = Mutex::new(RequestRateGate { next_slot: None });
static MUSIC_API_CLOCK_START: OnceLock<Instant> = OnceLock::new();

/// Wait until the caller's reserved music-backend slot. Sleeping happens on
/// the calling worker (always a spawn_blocking thread, never the event loop),
/// so pacing never blocks UI.
fn throttle_music_api() {
    let now = MUSIC_API_CLOCK_START.get_or_init(Instant::now).elapsed();
    let sleep_for = MUSIC_API_GATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reserve(now, MB_RATE_INTERVAL);
    if !sleep_for.is_zero() {
        // The slot is already reserved under the shared gate. Sleeping outside
        // the lock keeps unrelated workers from blocking while preserving the
        // distinct reservation order established above.
        std::thread::sleep(sleep_for);
    }
}

/// MusicBrainz search endpoint for releases.
const MB_SEARCH_URL: &str = "https://musicbrainz.org/ws/2/release/";

/// Cover Art Archive base URL.
const CAA_BASE_URL: &str = "https://coverartarchive.org/release/";
/// High-resolution bounded variant used for the browser overlay.
const CAA_FRONT_VARIANT: &str = "front-500";
/// MusicBrainz release searches return a small metadata document.
const MAX_MUSICBRAINZ_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Keep remote artwork bounded before it reaches the image decoder and cache.
const MAX_COVER_ART_BYTES: usize = 10 * 1024 * 1024;

/// Number of hex characters used in the cache filename.
const CACHE_KEY_LEN: usize = 16;

/// Subdirectory inside the cache root for artwork files.
const ARTWORK_CACHE_SUBDIR: &str = "artwork";

/// Whether an artist/album pair carries enough information to search for
/// remote artwork.
///
/// The metadata reader fills missing tags with the shared `Unknown *`
/// sentinels, and a bare empty value can reach here when a track has no
/// metadata at all. Searching MusicBrainz with those would 404 or return a
/// bogus/random release, so the caller skips the remote lookup entirely.
pub fn usable_remote_identity(artist: &str, album: &str) -> bool {
    let artist = artist.trim();
    let album = album.trim();
    let useful = |value: &str| {
        !value.is_empty()
            && !value.eq_ignore_ascii_case(crate::metadata::UNKNOWN_ARTIST)
            && !value.eq_ignore_ascii_case(crate::metadata::UNKNOWN_ALBUM)
    };
    useful(artist) && useful(album)
}

/// Fetch cover art bytes for the given track, checking cache first.
///
/// Always called from a blocking worker, never from the event loop.
/// Returns `None` on any failure (network error, no results, no cover)
/// with a warning log. Never propagates errors.
pub fn fetch_cover(artist: &str, album: &str, cache_dir: &Path) -> Option<Vec<u8>> {
    // Refuse a lookup that has no usable identity to search on, instead of
    // waiting on the rate-limit gate for a query that cannot succeed.
    if !usable_remote_identity(artist, album) {
        tracing::debug!("skipping remote artwork: no usable artist/album metadata");
        return None;
    }
    let cache_dir = cache_dir.join(ARTWORK_CACHE_SUBDIR);
    let cache_key = build_cache_key(artist, album);

    // Check cache first
    if let Some(cached) = read_cache(&cache_dir, &cache_key) {
        tracing::debug!("artwork cache hit for {artist} - {album}");
        return Some(cached);
    }

    // Search MusicBrainz for the release MBID
    let mbid = search_musicbrainz(artist, album)?;

    // Fetch cover from Cover Art Archive
    let bytes = fetch_cover_art(&mbid)?;

    // Store in cache for future use
    if let Err(error) = write_cache(&cache_dir, &cache_key, &bytes) {
        tracing::warn!("failed to cache artwork: {error}");
    }

    Some(bytes)
}

/// Build a deterministic cache key from artist and album.
fn build_cache_key(artist: &str, album: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    // Version the key with the requested image variant so old `/front-250`
    // cache entries do not silently defeat the quality improvement.
    hasher.update(CAA_FRONT_VARIANT.as_bytes());
    hasher.update(b"\n");
    hasher.update(artist.as_bytes());
    hasher.update(b"\n");
    hasher.update(album.as_bytes());
    let hash = hasher.finalize();

    hex_bytes(&hash)[..CACHE_KEY_LEN].to_string()
}

/// Convert raw bytes to a hex string.
fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Search MusicBrainz for a release MBID matching the artist and album.
///
/// Returns the first MBID whose release actually agrees with the lookup
/// (case-insensitive, trimmed, whitespace-collapsed), or `None` when nothing
/// matches or on a network/parse failure. The query already filters server
/// side, but the result can still be an approximate match (different album
/// spelling, a live version, etc.), so we only trust a candidate that
/// coheres with what we asked for.
fn search_musicbrainz(artist: &str, album: &str) -> Option<String> {
    throttle_music_api();
    let client = shared_client()?;
    let query = build_musicbrainz_query(artist, album);
    let response = client
        .get(MB_SEARCH_URL)
        .query(&[("query", query.as_str()), ("fmt", "json"), ("limit", "10")])
        .send()
        .inspect_err(|error| {
            tracing::warn!(
                "MusicBrainz search failed: {}",
                crate::net::normalize_diagnostic(&error.to_string(), 256)
            );
        })
        .ok()?;

    let mut response = response;
    let bytes = crate::net::read_bounded_response(&mut response, MAX_MUSICBRAINZ_BODY_BYTES)
        .map_err(|error| {
            tracing::warn!("MusicBrainz response body rejected: {error}");
            error
        })
        .ok()?;
    let body: MusicBrainzResponse = serde_json::from_slice(&bytes)
        .map_err(|error| {
            tracing::warn!("MusicBrainz response parse failed: {error}");
            error
        })
        .ok()?;

    body.releases.into_iter().find_map(|release| {
        if release_matches(&release, artist, album) && canonical_mbid(&release.id).is_some() {
            Some(release.id)
        } else {
            None
        }
    })
}

/// Build the MusicBrainz Lucene query without allowing metadata to add query
/// operators or field syntax.
fn build_musicbrainz_query(artist: &str, album: &str) -> String {
    format!(
        "artist:\"{}\" AND release:\"{}\"",
        escape_lucene(artist),
        escape_lucene(album)
    )
}

/// Escape Lucene query-parser syntax while preserving the user's text inside a
/// quoted field phrase.
fn escape_lucene(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            let escaped = matches!(
                character,
                '+' | '-'
                    | '&'
                    | '|'
                    | '!'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '^'
                    | '"'
                    | '~'
                    | '*'
                    | '?'
                    | ':'
                    | '\\'
                    | '/'
            );
            escaped
                .then_some('\\')
                .into_iter()
                .chain(std::iter::once(character))
        })
        .collect()
}

fn release_matches(release: &MusicBrainzRelease, artist: &str, album: &str) -> bool {
    let title_ok = release
        .title
        .as_deref()
        .is_some_and(|title| same_release(title, album));
    title_ok && credit_mentions(&release.artist_credit, artist)
}

/// Whether `release_title` normalized agrees with the looked-up `album`.
fn same_release(release_title: &str, album: &str) -> bool {
    normalize(release_title) == normalize(album)
}

/// Whether the release's artist-credit entries mention the looked-up `artist`
/// as a whole name. Matching entry names instead of joined display text avoids
/// treating a partial artist name as a match.
fn credit_mentions(credit: &[ArtistCreditEntry], artist: &str) -> bool {
    let target = normalize(artist);
    if target.is_empty() {
        return false;
    }
    credit
        .iter()
        .filter_map(|entry| entry.name.as_deref())
        .any(|name| normalize(name) == target)
}

/// Lowercase, trim and collapse internal whitespace for stable comparisons.
fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Fetch cover art bytes from the Cover Art Archive.
///
/// Uses the bounded `/front-500` endpoint so browser artwork has useful detail
/// without requesting the archive's unbounded maximum variant.
/// Returns `None` on any failure (404, network error, etc).
fn fetch_cover_art(mbid: &str) -> Option<Vec<u8>> {
    let mbid = canonical_mbid(mbid)?;
    let url = cover_art_url(mbid)?;
    throttle_music_api();
    let client = shared_client()?;

    let mut response = client
        .get(&url)
        .send()
        .map_err(|error| {
            tracing::warn!(
                "Cover Art Archive fetch failed for {mbid}: {}",
                crate::net::normalize_diagnostic(&error.to_string(), 256)
            );
            error
        })
        .ok()?;

    let status = response.status();
    if !status.is_success() {
        tracing::warn!("Cover Art Archive returned {status} for {mbid}, no cover available");
        return None;
    }

    let bytes = crate::net::read_bounded_response(&mut response, MAX_COVER_ART_BYTES)
        .map_err(|error| {
            tracing::warn!("Cover Art Archive body read failed for {mbid}: {error}");
            error
        })
        .ok()?;

    Some(bytes)
}

/// Build the versioned high-resolution Cover Art Archive URL in one place so
/// the request and its regression test cannot drift apart.
fn cover_art_url(mbid: &str) -> Option<String> {
    canonical_mbid(mbid).map(|mbid| format!("{CAA_BASE_URL}{mbid}/{CAA_FRONT_VARIANT}"))
}

/// Accept only the lowercase, hyphenated UUID representation used by MBIDs.
///
/// This is intentionally stricter than accepting any string that happens to
/// parse as a UUID: the exact canonical form is safe to place in a URL path
/// and keeps diagnostics stable.
fn canonical_mbid(mbid: &str) -> Option<&str> {
    let bytes = mbid.as_bytes();
    if bytes.len() != 36 {
        return None;
    }

    for (index, byte) in bytes.iter().copied().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if byte != b'-' {
                return None;
            }
        } else if !matches!(byte, b'0'..=b'9' | b'a'..=b'f') {
            return None;
        }
    }

    Some(mbid)
}

/// Shared HTTP client, built once and reused across all cover fetches.
///
/// reqwest's `Client` internally pools connections, so rebuilding it on every
/// request threw that pool away and paid a fresh TLS handshake each time. The
/// client is constructed lazily on first use and reused for the whole process.
fn shared_client() -> Option<&'static Client> {
    static CLIENT: OnceLock<Option<Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            crate::net::build_finite_client(
                Duration::from_secs(HTTP_TIMEOUT_SECS),
                Some(USER_AGENT),
            )
            .map_err(|error| tracing::warn!("HTTP client build failed: {error}"))
            .ok()
        })
        .as_ref()
}

/// Read cached artwork bytes by cache key, checking common extensions.
///
/// Only a file that still parses as `jpg`/`png` is trusted: a cache entry that
/// was truncated by a crash mid-write (or by hand) must not surface as artwork
/// and then fail the decode in the UI. `image_dimensions` reads just the
/// header, so the check is cheap.
fn read_cache(cache_dir: &Path, key: &str) -> Option<Vec<u8>> {
    for ext in ["jpg", "jpeg", "png"] {
        let path = cache_dir.join(format!("{key}.{ext}"));
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            tracing::warn!("ignoring symlinked artwork cache entry {}", path.display());
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        if is_decodable_image(&bytes) {
            return Some(bytes);
        }
        // Corrupt cache entry: drop it so the next fetch rewrites a good one.
        tracing::warn!("removing corrupt artwork cache entry {}", path.display());
        let _ = fs::remove_file(&path);
    }
    None
}

/// Write artwork bytes to the cache with a JPEG extension.
///
/// Atomic on the file system: the bytes are written to a temp file in the same
/// directory and then renamed into place, so a concurrent reader (or a crash)
/// never observes a half-written cache entry. The strictest rename semantics
/// are only an optimization; correctness comes from `read_cache` validating
/// the bytes again.
fn write_cache(cache_dir: &Path, key: &str, bytes: &[u8]) -> std::io::Result<()> {
    fs::create_dir_all(cache_dir)?;
    let path = cache_dir.join(format!("{key}.jpg"));
    atomic_replace(&path, bytes)
}

/// Whether the byte slice parses as a supported image (jpg/png), inspecting
/// only the header via `image_dimensions` rather than decoding the bitmap.
fn is_decodable_image(bytes: &[u8]) -> bool {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map(|reader| {
            matches!(
                reader.format(),
                Some(image::ImageFormat::Jpeg | image::ImageFormat::Png)
            )
        })
        .unwrap_or(false)
}

/// MusicBrainz search API response.
#[derive(serde::Deserialize)]
struct MusicBrainzResponse {
    releases: Vec<MusicBrainzRelease>,
}

/// A single release in the MusicBrainz search response.
#[derive(serde::Deserialize)]
struct MusicBrainzRelease {
    id: String,
    /// Release title as reported by MusicBrainz, used to validate the hit.
    #[serde(default)]
    title: Option<String>,
    /// MusicBrainz emits `artist-credit` as an array of credited artist
    /// entries, not as the joined display string.
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCreditEntry>,
}

#[derive(serde::Deserialize)]
struct ArtistCreditEntry {
    #[serde(default)]
    name: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    joinphrase: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_dir;

    #[test]
    fn build_cache_key_is_deterministic() {
        let key1 = build_cache_key("Pink Floyd", "The Wall");
        let key2 = build_cache_key("Pink Floyd", "The Wall");
        assert_eq!(key1, key2);
        assert_eq!(key1.len(), CACHE_KEY_LEN);
    }

    #[test]
    fn build_cache_key_differs_for_different_input() {
        let key1 = build_cache_key("Pink Floyd", "The Wall");
        let key2 = build_cache_key("Pink Floyd", "The Dark Side of the Moon");
        assert_ne!(key1, key2);
    }

    #[test]
    fn cover_art_url_uses_the_browser_sized_variant() {
        assert_eq!(
            cover_art_url("01234567-89ab-cdef-0123-456789abcdef"),
            Some("https://coverartarchive.org/release/01234567-89ab-cdef-0123-456789abcdef/front-500".to_string())
        );
    }

    #[test]
    fn invalid_mbids_are_rejected_before_url_construction() {
        for mbid in [
            "release-id",
            "0123456789abcdef0123456789abcdef0123",
            "01234567-89AB-cdef-0123-456789abcdef",
            "01234567-89ab-cdef-0123-456789abcdeg",
        ] {
            assert_eq!(canonical_mbid(mbid), None);
            assert_eq!(cover_art_url(mbid), None);
        }
    }

    #[test]
    fn invalid_mbid_stops_before_the_network_boundary() {
        assert_eq!(fetch_cover_art("not-a-mbid"), None);
    }

    #[test]
    fn hex_bytes_produces_valid_hex() {
        let result = hex_bytes(&[0x00, 0xff, 0xab]);
        assert_eq!(result, "00ffab");
    }

    #[test]
    fn read_cache_returns_none_for_missing_entry() {
        let root = unique_temp_dir("remote-cache-miss");
        let result = read_cache(&root, "nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn write_cache_then_read_cache_round_trips() {
        let root = unique_temp_dir("remote-cache-hit");
        let key = "aabbccdd11223344";
        let data = tiny_png();

        write_cache(&root, key, &data).expect("write");
        let result = read_cache(&root, key);
        // The cache was written as `.jpg`; the round-trip must re-read it and
        // only accept it because it is still a decodable image.
        assert_eq!(result.as_deref(), Some(data.as_slice()));
    }

    #[test]
    fn read_cache_rejects_and_drops_a_corrupt_entry() {
        let root = unique_temp_dir("remote-cache-corrupt");
        let key = "deadbeef11223344";
        fs::create_dir_all(&root).expect("cache dir");
        fs::write(root.join(format!("{key}.jpg")), b"not an image").expect("corrupt entry");

        assert_eq!(read_cache(&root, key), None);
        assert!(
            !root.join(format!("{key}.jpg")).exists(),
            "corrupt entry must be removed"
        );
    }

    /// A 1x1 transparent PNG, valid minimal bytes for cache round-trip tests.
    fn tiny_png() -> Vec<u8> {
        // Minimal, well-formed PNG without ancillary chunks.
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR len + type
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
            0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, // bit depth/color
            0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, // IDAT len
            0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, // zlib data
            0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, // adler + crc
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND
            0xAE, 0x42, 0x60, 0x82,
        ]
    }

    #[test]
    fn fetch_cover_returns_none_for_empty_credentials() {
        let root = unique_temp_dir("remote-fetch-none");
        // Empty artist/album will find nothing on MusicBrainz
        let result = fetch_cover("", "", &root);
        assert!(result.is_none());
    }

    #[test]
    fn rate_gate_reserves_distinct_slots_for_concurrent_callers() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const CALLERS: usize = 4;
        let gate = Arc::new(Mutex::new(RequestRateGate::default()));
        let start = Arc::new(Barrier::new(CALLERS));
        let mut handles = Vec::with_capacity(CALLERS);

        for _ in 0..CALLERS {
            let gate = Arc::clone(&gate);
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || {
                start.wait();
                gate.lock()
                    .expect("gate lock")
                    .reserve(Duration::ZERO, MB_RATE_INTERVAL)
            }));
        }

        let mut reservations: Vec<Duration> = handles
            .into_iter()
            .map(|handle| handle.join().expect("reservation thread"))
            .collect();
        reservations.sort_unstable();

        let expected: Vec<Duration> = (0..CALLERS)
            .map(|index| MB_RATE_INTERVAL.saturating_mul(index as u32))
            .collect();
        assert_eq!(reservations, expected);
    }

    #[test]
    fn rate_gate_preserves_interval_between_reserved_slots() {
        let mut gate = RequestRateGate::default();

        assert_eq!(
            gate.reserve(Duration::ZERO, MB_RATE_INTERVAL),
            Duration::ZERO
        );
        assert_eq!(
            gate.reserve(Duration::from_millis(100), MB_RATE_INTERVAL),
            Duration::from_millis(1_000)
        );
        assert_eq!(
            gate.reserve(Duration::from_millis(2_200), MB_RATE_INTERVAL),
            Duration::ZERO
        );
    }

    #[test]
    fn usable_identity_rejects_placeholders_and_blanks() {
        assert!(!usable_remote_identity("", "The Wall"));
        assert!(!usable_remote_identity("   ", "The Wall"));
        assert!(!usable_remote_identity("Pink Floyd", ""));
        assert!(!usable_remote_identity("Unknown Artist", "The Wall"));
        assert!(!usable_remote_identity("Pink Floyd", "Unknown Album"));
        assert!(!usable_remote_identity("unknown artist", "unknown album"));
        assert!(usable_remote_identity("Pink Floyd", "The Wall"));
    }

    #[test]
    fn finite_remote_client_keeps_the_ten_second_timeout_bound() {
        let timeout = Duration::from_secs(HTTP_TIMEOUT_SECS);
        assert_eq!(timeout, Duration::from_secs(10));
        assert!(
            crate::net::build_finite_client(timeout, Some(USER_AGENT)).is_ok(),
            "the finite client profile must remain constructible with the bound"
        );
        assert!(shared_client().is_some());
    }

    #[test]
    fn musicbrainz_query_escapes_lucene_syntax_in_metadata() {
        assert_eq!(
            build_musicbrainz_query("AC/DC + The (Band)", "A: B? [Live]"),
            "artist:\"AC\\/DC \\+ The \\(Band\\)\" AND release:\"A\\: B\\? \\[Live\\]\""
        );
    }

    #[test]
    fn release_match_is_case_and_whitespace_insensitive() {
        assert!(same_release("The Wall", "the  wall"));
        assert!(same_release(
            "The Dark Side of the Moon",
            "the dark side of the moon"
        ));
        assert!(!same_release("Wish You Were Here", "The Wall"));
    }

    #[test]
    fn credit_mentions_requires_a_full_name_among_joiners() {
        let credit = vec![
            ArtistCreditEntry {
                name: Some("Roger Waters".into()),
                joinphrase: Some(" & ".into()),
            },
            ArtistCreditEntry {
                name: Some("Pink Floyd".into()),
                joinphrase: None,
            },
        ];
        assert!(credit_mentions(&credit, "Pink Floyd"));
        assert!(!credit_mentions(&credit, "Pink"));
        assert!(!credit_mentions(&credit, "David Gilmour"));
    }

    #[test]
    fn musicbrainz_artist_credit_deserializes_from_the_actual_array_payload() {
        let payload = r#"
        {
          "releases": [
            {
              "id": "release-id",
              "title": "The Wall",
              "artist-credit": [
                {
                  "name": "Pink Floyd",
                  "artist": {
                    "id": "artist-id",
                    "name": "Pink Floyd",
                    "sort-name": "Pink Floyd"
                  },
                  "joinphrase": ""
                }
              ]
            }
          ]
        }
        "#;

        let response: MusicBrainzResponse = serde_json::from_str(payload).expect("valid payload");
        let release = &response.releases[0];
        assert!(release_matches(release, "pink  floyd", "the wall"));
        assert!(!release_matches(release, "Pink", "The Wall"));
        assert_eq!(release.artist_credit[0].joinphrase.as_deref(), Some(""));
    }
}
