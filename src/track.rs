//! Playlist track model identified by its source.
//!
//! A [`Track`] is the single queue entry that drives both the audio engine
//! and the M3U8 persistence layer. It used to assume every entry was a file
//! on disk; the streaming layer added a [`TrackSource`] enum that lets the
//! same struct represent either a local file or a remote stream (YouTube,
//! Radio Browser, Icecast, SHOUTcast, plain HTTP/HTTPS).
//!
//! The split mirrors the spec's "what is this URL?" vs "how do I play it?"
//! principle:
//!
//! - [`TrackSource::Local`] keeps the historical path-based behaviour: the
//!   M3U8 entry is the path, the audio engine opens the file, the rename
//!   shortcut moves the file on disk and rewrites every saved playlist.
//! - [`TrackSource::Stream`] stores the canonical URL of the resource so
//!   playlists round-trip through `.m3u8` cleanly. Resolving a direct media
//!   URL (especially for YouTube) happens at playback time, never at parse
//!   time, so the persisted playlist never holds an expiring URL.
//!
//! Metadata behaves identically for both kinds: the lofty snapshot for a
//! local file or the resolver output for a stream both attach in place and
//! become the display label once they arrive.

use std::borrow::Cow;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};

use crate::metadata::TrackMetadata;
use crate::stream::{StreamKind, TrackSource};

/// A filesystem path normalized without consulting the filesystem.
///
/// This type is the boundary for local track paths. Its constructor removes
/// only lexical `.` and `..` components, so missing files and symlink spellings
/// remain valid and stable identities.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrackPath(PathBuf);

/// Prefix for the lossless local-path encoding used by new persistence data.
/// The payload is the normalized path's raw bytes encoded as lowercase hex.
const ENCODED_LOCAL_PATH_PREFIX: &str = "harmonium-local-v2:";

/// Prefix emitted by the previous URL-looking-local compatibility escape.
const LEGACY_ESCAPED_LOCAL_PATH_PREFIX: &str = "harmonium-local-v1:";

impl TrackPath {
    /// Normalize a path lexically without following symlinks or requiring it
    /// to exist.
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        // `.` is the one canonical lexical spelling for an empty/current-
        // directory path. Keep it consistent with paths whose components
        // collapse away, such as `a/..`.
        if path.as_os_str().is_empty() {
            return Self(PathBuf::from("."));
        }
        if !path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Self(path);
        }

        let mut normalized = PathBuf::new();
        let mut normal_components = 0usize;
        let mut absolute = false;

        for component in path.components() {
            match component {
                Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
                Component::RootDir => {
                    normalized.push(component.as_os_str());
                    absolute = true;
                }
                Component::CurDir => {}
                Component::Normal(part) => {
                    normalized.push(part);
                    normal_components += 1;
                }
                Component::ParentDir if normal_components > 0 => {
                    normalized.pop();
                    normal_components -= 1;
                }
                Component::ParentDir if !absolute => normalized.push(component.as_os_str()),
                Component::ParentDir => {}
            }
        }

        if normalized.as_os_str().is_empty() && !absolute {
            normalized.push(".");
        }

        Self(normalized)
    }

    /// Borrow the normalized path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TrackPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl Deref for TrackPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_path()
    }
}

/// Stable identity for a queued resource.
///
/// Unlike a printable location string, this value keeps filesystem paths and
/// stream URLs distinct. Runtime state uses it as the sole last-track
/// identity; conversion to a persisted string happens only at the state-file
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TrackLocation {
    /// A resource backed by a local filesystem path.
    Local(TrackPath),
    /// A resource backed by a remote URL.
    Url(url::Url),
}

impl TrackLocation {
    /// Construct a local track identity without interpreting the value as a URL.
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self::Local(TrackPath::new(path))
    }

    /// Construct a URL track identity from an already parsed URL.
    pub fn url(url: url::Url) -> Self {
        Self::Url(url)
    }

    /// Decode the compatibility string used by `state.toml` and M3U.
    ///
    /// New local values use a strict, lossless byte encoding. Invalid values
    /// carrying the new prefix remain ordinary local paths, which prevents a
    /// literal path from being truncated or reclassified. The v1 marker is
    /// retained as an opaque local-path prefix because its old URL-looking
    /// escape is indistinguishable from a literal path with the same prefix.
    /// Empty legacy values are treated as missing. Values that are not valid
    /// URLs remain local paths, preserving the historical path behavior while
    /// making malformed legacy URLs harmless non-matches during startup.
    pub fn from_persisted(value: &str) -> Option<Self> {
        if value.is_empty() {
            return None;
        }
        if value.starts_with(ENCODED_LOCAL_PATH_PREFIX) {
            return Some(Self::local(
                decode_encoded_local_path(value).unwrap_or_else(|| PathBuf::from(value)),
            ));
        }
        if value.starts_with(LEGACY_ESCAPED_LOCAL_PATH_PREFIX) {
            return Some(Self::local(value));
        }
        if let Some(path) = Self::persisted_local_path(value) {
            return Some(Self::local(path));
        }
        match url::Url::parse(value) {
            Ok(url) => Some(Self::url(url)),
            Err(_) => Some(Self::local(value)),
        }
    }

    /// Encode the identity in the lossless string format used by `state.toml`
    /// and M3U body lines.
    pub fn to_persisted(&self) -> String {
        match self {
            Self::Local(path) => encode_local_path(path),
            Self::Url(url) => url.as_str().to_string(),
        }
    }

    /// Return a local path encoded in a persistence value.
    ///
    /// M3U parsing uses this before stream classification so a serialized
    /// local path remains local on reload. Invalid v2 values are deliberately
    /// not decoded and are therefore treated as ordinary compatibility paths.
    /// The ambiguous v1 marker is intentionally excluded: stripping it could
    /// silently change a literal legacy path into a different identity.
    pub(crate) fn persisted_local_path(value: &str) -> Option<PathBuf> {
        decode_encoded_local_path(value)
    }

    /// Resolve a relative local identity against an explicit boundary base.
    /// No filesystem access or canonicalization is performed.
    pub fn resolve_relative(&self, base: &Path) -> Self {
        match self {
            Self::Local(path) if !path.is_absolute() => Self::local(base.join(path.as_path())),
            _ => self.clone(),
        }
    }

    /// Compare two identities after applying the same explicit relative-path
    /// base. This is used only at boundaries that own that base, such as
    /// restoring state into a playlist loaded from its M3U directory.
    pub fn equivalent_with_base(&self, other: &Self, base: &Path) -> bool {
        self.resolve_relative(base) == other.resolve_relative(base)
    }

    /// Borrow the path when this identity is local.
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path),
            Self::Url(_) => None,
        }
    }

    /// Borrow the URL when this identity is remote.
    pub fn as_url(&self) -> Option<&url::Url> {
        match self {
            Self::Local(_) => None,
            Self::Url(url) => Some(url),
        }
    }

    /// Convert the identity to text for a user-facing label or diagnostic.
    pub fn display_location(&self) -> Cow<'_, str> {
        match self {
            Self::Local(path) => path.to_string_lossy(),
            Self::Url(url) => Cow::Borrowed(url.as_str()),
        }
    }
}

/// Anchor a relative path at the process working directory without consulting
/// the filesystem. Direct additions and browser actions share this boundary
/// so their local identities cannot diverge.
pub(crate) fn resolve_process_relative_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    std::env::current_dir()
        .map(|base| base.join(&path))
        .unwrap_or(path)
}

#[cfg(unix)]
fn encode_local_path(path: &TrackPath) -> String {
    use std::os::unix::ffi::OsStrExt;

    encode_local_path_bytes(path.as_path().as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn encode_local_path(path: &TrackPath) -> String {
    let value = path.as_path().to_string_lossy();
    encode_local_path_bytes(value.as_bytes())
}

fn encode_local_path_bytes(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(ENCODED_LOCAL_PATH_PREFIX.len() + bytes.len() * 2);
    encoded.push_str(ENCODED_LOCAL_PATH_PREFIX);
    for byte in bytes {
        use std::fmt::Write;

        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn decode_encoded_local_path(value: &str) -> Option<PathBuf> {
    let payload = value.strip_prefix(ENCODED_LOCAL_PATH_PREFIX)?;
    if payload.is_empty() || !payload.len().is_multiple_of(2) {
        return None;
    }

    let mut bytes = Vec::with_capacity(payload.len() / 2);
    for pair in payload.as_bytes().chunks_exact(2) {
        let high = decode_hex_digit(pair[0])?;
        let low = decode_hex_digit(pair[1])?;
        bytes.push(high << 4 | low);
    }

    if bytes.contains(&0) {
        return None;
    }
    path_from_bytes(bytes)
}

fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> Option<PathBuf> {
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// A single queued track identified by its [`TrackSource`].
///
/// The struct stores exactly two pieces of state: the source that decides
/// how to open the resource, and the metadata snapshot that arrived
/// asynchronously (lofty for local files, the streaming resolver for
/// remote sources).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// Local file or remote stream this track represents.
    source: TrackSource,
    /// Extracted snapshot, absent until the worker delivers it.
    metadata: Option<TrackMetadata>,
}

impl Track {
    /// Build a track from a normalized local path, initially without metadata.
    fn from_path(path: TrackPath) -> Self {
        Self {
            source: TrackSource::Local(path),
            metadata: None,
        }
    }

    /// Build a track from a local file path, normalizing its identity without
    /// filesystem access.
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self::from_path(TrackPath::new(path))
    }

    /// Build a track from a stream URL and the detected [`StreamKind`].
    ///
    /// Most call sites do not need this directly; the streaming layer adds
    /// tracks via [`crate::stream::StreamResolver`] which fills in both the
    /// kind and the metadata.
    pub fn from_stream(url: url::Url, kind: StreamKind) -> Self {
        Self {
            source: TrackSource::Stream {
                url,
                kind,
                prepared: None,
            },
            metadata: None,
        }
    }

    /// Attach the response prepared while resolving this stream.
    pub(crate) fn attach_prepared_stream(
        &mut self,
        prepared: Option<crate::stream::provider::PreparedStream>,
    ) {
        self.source.attach_prepared(prepared);
    }

    /// Filesystem path for local tracks. `None` for streams.
    ///
    /// Code paths that need a printable identifier (log message or status
    /// footer) should reach for [`Self::display_location`]. Serialization
    /// should use [`TrackLocation::to_persisted`] at its boundary.
    pub fn path(&self) -> Option<&Path> {
        self.source.path()
    }

    /// Typed stable identity for the underlying resource.
    pub fn track_location(&self) -> TrackLocation {
        self.source.track_location()
    }

    /// Convert the source location to text for presentation only.
    pub fn display_location(&self) -> Cow<'_, str> {
        self.source.display_location()
    }

    /// Borrow the underlying [`TrackSource`].
    pub fn source(&self) -> &TrackSource {
        &self.source
    }

    /// True when the source is a local file.
    pub fn is_local(&self) -> bool {
        self.source.is_local()
    }

    /// True when the source is a remote stream of any kind.
    pub fn is_stream(&self) -> bool {
        self.source.is_stream()
    }

    /// Detected kind when the source is a stream, `None` for local files.
    pub fn stream_kind(&self) -> Option<StreamKind> {
        self.source.stream_kind()
    }

    /// Extracted snapshot, absent until the background workers deliver it.
    pub fn metadata(&self) -> Option<&TrackMetadata> {
        self.metadata.as_ref()
    }

    /// Attach or replace the extracted snapshot for this track.
    pub fn set_metadata(&mut self, metadata: TrackMetadata) {
        self.metadata = Some(metadata);
    }

    /// Best available display name for the queue and the popup.
    ///
    /// The tagged title wins when present, otherwise we fall back to a
    /// source-appropriate label:
    ///
    /// - Local tracks: the file stem, then the full path when there is no
    ///   stem. Matches the historical behaviour so the UI does not shift
    ///   for existing playlists.
    /// - Streams: the URL hostname, then the URL itself when there is no
    ///   host. Mirrors the M3U8 fallback the spec requires.
    ///
    /// `Cow` lets callers borrow the already-owned title when metadata is
    /// present, avoiding a clone per render; the fallbacks allocate only
    /// when there is no tag to borrow.
    pub fn display_name(&self) -> Cow<'_, str> {
        if let Some(metadata) = &self.metadata
            && metadata.title_tagged
            && !metadata.title.is_empty()
        {
            return Cow::Borrowed(&metadata.title);
        }

        match &self.source {
            TrackSource::Local(path) => match path.file_stem() {
                Some(stem) => Cow::Owned(stem.to_string_lossy().into_owned()),
                None => Cow::Owned(path.to_string_lossy().into_owned()),
            },
            TrackSource::Stream { url, .. } => match url.host_str() {
                Some(host) => Cow::Owned(host.to_string()),
                None => Cow::Owned(url.as_str().to_string()),
            },
        }
    }

    /// Rebuild a local track at a new location, keeping its metadata.
    ///
    /// Renaming never loses tags: the metadata attached to the old path is
    /// exactly what the renamed file still carries, so the snapshot rides
    /// along instead of being re-extracted.
    ///
    /// Stream tracks cannot be renamed on disk. The caller is expected to
    /// know which kind it holds; this method keeps the path-based rename
    /// path simple for the existing local-file shortcut.
    pub fn rename_to(self, new_path: PathBuf) -> Track {
        match self.source {
            TrackSource::Local(_) => Track {
                source: TrackSource::local(new_path),
                metadata: self.metadata,
            },
            // Streams cannot be renamed on disk; the caller should have
            // routed to a metadata/title edit instead. Returning self here
            // is a no-op rather than a panic, so a stale shortcut binding
            // never crashes the app.
            other => Track {
                source: other,
                metadata: self.metadata,
            },
        }
    }

    /// Replace the display title in the metadata snapshot.
    ///
    /// Used by the rename and edit-metadata shortcuts on stream tracks,
    /// where the only mutable surface is the EXTINF label in the M3U8
    /// document. Local files keep their lofty-driven titles; this helper
    /// exists so the streaming flow does not have to touch the lofty
    /// snapshot directly.
    pub fn set_title(&mut self, title: impl Into<String>) {
        let title = title.into();
        let metadata = self.metadata.get_or_insert_with(TrackMetadata::default);
        metadata.title = title;
        metadata.title_tagged = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use url::Url;

    fn sample_metadata(title: &str) -> TrackMetadata {
        TrackMetadata {
            title: title.to_string(),
            title_tagged: true,
            artist: "Artist".to_string(),
            album: "Album".to_string(),
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

    #[test]
    fn local_track_path_round_trips() {
        let track = Track::local("/tmp/a/b.flac");
        assert_eq!(track.path(), Some(Path::new("/tmp/a/b.flac")));
        assert_eq!(track.display_location(), "/tmp/a/b.flac");
        assert_eq!(
            track.track_location(),
            TrackLocation::local("/tmp/a/b.flac")
        );
        assert!(track.is_local());
        assert!(!track.is_stream());
        assert_eq!(track.stream_kind(), None);
    }

    #[test]
    fn local_paths_are_normalized_lexically_without_filesystem_access() {
        let track = Track::local("a/./b/../c");

        assert_eq!(track.path(), Some(Path::new("a/c")));
        assert_eq!(Track::local("a/./b").path(), Some(Path::new("a/b")));
        assert_eq!(Track::local("/a/./b/../c").path(), Some(Path::new("/a/c")));
        assert_eq!(Track::local("/a/..").path(), Some(Path::new("/")));
        assert_eq!(Track::local("a/..").path(), Some(Path::new(".")));
        assert_eq!(
            Track::local("../a/../../b").path(),
            Some(Path::new("../../b"))
        );
    }

    #[test]
    fn empty_local_paths_use_the_current_directory_representation() {
        assert_eq!(Track::local("").path(), Some(Path::new(".")));
        assert_eq!(Track::local("./").path(), Some(Path::new(".")));
        assert_eq!(Track::local("a/..").path(), Some(Path::new(".")));
    }

    #[cfg(windows)]
    #[test]
    fn local_paths_preserve_windows_prefixes_while_normalizing_components() {
        assert_eq!(
            Track::local(r"C:\a\.\b\..\c").path(),
            Some(Path::new(r"C:\a\c"))
        );
    }

    #[test]
    fn nonexistent_local_paths_keep_their_lexical_identity() {
        let path = "/harmonium/path-that-does-not-exist/a/../song.flac";

        assert_eq!(
            Track::local(path).path(),
            Some(Path::new("/harmonium/path-that-does-not-exist/song.flac"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_path_normalization_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = crate::test_support::TestTempDir::new("track-path-symlink");
        std::fs::create_dir(temp.path().join("real")).expect("real directory");
        symlink(temp.path().join("real"), temp.path().join("link")).expect("symlink");

        let input = temp.path().join("link/song.mp3");
        let track = Track::local(&input);

        assert_eq!(track.path(), Some(input.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_local_paths_are_normalized_without_lossy_conversion() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(
            b"/music/a/../"
                .iter()
                .copied()
                .chain([0xff, b'.', b'm', b'p', b'3'])
                .collect(),
        );
        let expected = OsString::from_vec(
            b"/music/"
                .iter()
                .copied()
                .chain([0xff, b'.', b'm', b'p', b'3'])
                .collect(),
        );

        assert_eq!(Track::local(raw).path(), Some(Path::new(&expected)));
    }

    #[test]
    fn local_track_and_source_birth_points_share_the_same_identity() {
        let input = "music/./album/../song.flac";
        let expected = TrackLocation::local("music/song.flac");

        assert_eq!(Track::local(input).track_location(), expected);
        assert_eq!(
            TrackSource::local(input).track_location(),
            TrackLocation::local("music/song.flac")
        );
    }

    #[test]
    fn stream_track_hides_the_path() {
        let url = Url::parse("https://radio.example.com/live").expect("valid url");
        let track = Track::from_stream(url.clone(), StreamKind::Http);

        assert!(track.is_stream());
        assert!(!track.is_local());
        assert_eq!(track.path(), None);
        assert_eq!(track.display_location(), url.as_str());
        assert_eq!(track.track_location(), TrackLocation::url(url.clone()));
        assert_eq!(track.stream_kind(), Some(StreamKind::Http));
    }

    #[test]
    fn track_location_round_trips_local_paths_and_urls_without_crossing_kinds() {
        let local = TrackLocation::local("/music/song.mp3");
        let url =
            TrackLocation::url(Url::parse("https://radio.example.com/live").expect("valid URL"));

        assert_eq!(local.as_path(), Some(Path::new("/music/song.mp3")));
        assert_eq!(local.as_url(), None);
        assert_eq!(
            TrackLocation::from_persisted(&local.to_persisted()),
            Some(local.clone())
        );
        assert_eq!(url.as_path(), None);
        assert_eq!(
            url.as_url().map(Url::as_str),
            Some("https://radio.example.com/live")
        );
        assert_eq!(
            TrackLocation::from_persisted(&url.to_persisted()),
            Some(url.clone())
        );
        assert_eq!(TrackLocation::from_persisted(""), None);
        assert_eq!(
            TrackLocation::from_persisted("not a URL"),
            Some(TrackLocation::local("not a URL"))
        );
        assert_eq!(
            TrackLocation::from_persisted("/music/./album/../song.mp3"),
            Some(TrackLocation::local("/music/song.mp3"))
        );
    }

    #[test]
    fn url_looking_local_identity_uses_an_explicit_persistence_escape() {
        let local = TrackLocation::local("https://example.com/live");
        let persisted = local.to_persisted();

        assert_eq!(
            persisted,
            "harmonium-local-v2:68747470733a2f2f6578616d706c652e636f6d2f6c697665"
        );
        assert_eq!(TrackLocation::from_persisted(&persisted), Some(local));
        assert!(matches!(
            TrackLocation::from_persisted("https://example.com/live"),
            Some(TrackLocation::Url(_))
        ));
    }

    #[test]
    fn invalid_new_escape_values_remain_literal_local_paths() {
        for value in [
            "harmonium-local-v2:",
            "harmonium-local-v2:0",
            "harmonium-local-v2:gg",
            "harmonium-local-v2:00",
        ] {
            assert_eq!(
                TrackLocation::from_persisted(value),
                Some(TrackLocation::local(value)),
                "invalid escape must not be stripped: {value}"
            );
        }
    }

    #[test]
    fn legacy_v1_values_are_opaque_local_paths_to_avoid_ambiguous_loss() {
        assert_eq!(
            TrackLocation::from_persisted("harmonium-local-v1:https://example.com/live"),
            Some(TrackLocation::local(
                "harmonium-local-v1:https://example.com/live"
            ))
        );
        let literal = "harmonium-local-v1:ordinary/song.mp3";
        assert_eq!(
            TrackLocation::from_persisted(literal),
            Some(TrackLocation::local(literal))
        );
    }

    #[test]
    fn relative_local_identities_can_be_compared_at_the_playlist_boundary() {
        let saved = TrackLocation::local("song.mp3");
        let loaded = TrackLocation::local("/playlists/song.mp3");

        assert!(saved.equivalent_with_base(&loaded, Path::new("/playlists")));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_local_identity_round_trips_through_persistence() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(
            b"/music/"
                .iter()
                .copied()
                .chain([0xff, b'.', b'm', b'p', b'3'])
                .collect(),
        ));
        let location = TrackLocation::local(path);
        let persisted = location.to_persisted();

        assert!(persisted.starts_with("harmonium-local-v2:"));
        assert_eq!(TrackLocation::from_persisted(&persisted), Some(location));
    }

    #[test]
    fn local_and_url_identities_do_not_collide_when_their_text_matches() {
        let text = "https://example.com/live";
        let local = Track::local(text);
        let stream = Track::from_stream(Url::parse(text).expect("valid URL"), StreamKind::Http);

        assert_ne!(local.track_location(), stream.track_location());
        assert_eq!(local.display_location(), stream.display_location());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_local_identity_is_preserved_while_display_is_lossy() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(
            b"/music/".iter().copied().chain([0xff]).collect(),
        ));
        let track = Track::local(path.clone());

        assert_eq!(track.track_location(), TrackLocation::local(path));
        assert!(track.display_location().contains('\u{fffd}'));
    }

    #[test]
    fn display_name_uses_the_file_stem_without_metadata() {
        let track = Track::local("/home/user/Music/Rush - Tom Sawyer.mp3");

        assert_eq!(track.display_name(), "Rush - Tom Sawyer");
    }

    #[test]
    fn display_name_falls_back_to_the_full_path_without_a_stem() {
        let track = Track::local("/");

        assert_eq!(track.display_name(), "/");
    }

    #[test]
    fn display_name_falls_back_to_hostname_for_streams() {
        let url = Url::parse("https://radio.example.com/live.mp3").expect("valid url");
        let track = Track::from_stream(url, StreamKind::Http);

        assert_eq!(track.display_name(), "radio.example.com");
    }

    #[test]
    fn display_name_falls_back_to_full_url_when_host_missing() {
        let url = Url::parse("data:audio/mpeg;base64,").expect("valid url");
        let track = Track::from_stream(url.clone(), StreamKind::Http);

        assert_eq!(track.display_name(), url.as_str());
    }

    #[test]
    fn attached_metadata_becomes_the_display_name() {
        let mut track = Track::local("/m/untitled.mp3");
        assert_eq!(track.metadata(), None);

        track.set_metadata(sample_metadata("Real Title"));

        assert_eq!(
            track.metadata().map(|meta| meta.title.as_str()),
            Some("Real Title")
        );
        assert_eq!(track.display_name(), "Real Title");
    }

    #[test]
    fn replacing_metadata_overwrites_the_previous_snapshot() {
        let mut track = Track::local("/m/song.flac");
        track.set_metadata(sample_metadata("First"));

        track.set_metadata(sample_metadata("Second"));

        assert_eq!(track.display_name(), "Second");
    }

    #[test]
    fn rename_to_updates_the_path_and_rides_metadata_along() {
        let mut track = Track::local("/music/old-name.mp3");
        track.set_metadata(sample_metadata("Real Title"));
        let snapshot = track.metadata().cloned();

        let renamed = track.rename_to(PathBuf::from("/music/new-name.mp3"));

        assert_eq!(renamed.path(), Some(Path::new("/music/new-name.mp3")));
        assert_eq!(renamed.display_name(), "Real Title");
        assert_eq!(
            renamed.metadata(),
            snapshot.as_ref(),
            "snapshot must ride along"
        );
    }

    #[test]
    fn rename_to_keeps_an_untagged_track_untagged() {
        let track = Track::local("/music/plain.flac");

        let renamed = track.rename_to(PathBuf::from("/music/renamed.flac"));

        assert_eq!(renamed.path(), Some(Path::new("/music/renamed.flac")));
        assert_eq!(renamed.metadata(), None);
    }

    #[test]
    fn rename_to_is_a_noop_for_stream_tracks() {
        let url = Url::parse("https://radio.example.com/live").expect("valid url");
        let track = Track::from_stream(url.clone(), StreamKind::Http);

        let renamed = track.rename_to(PathBuf::from("/not/used"));

        // A stream cannot be renamed: source identity stays intact so the
        // caller notices the noop instead of corrupting the URL.
        assert_eq!(renamed.track_location(), TrackLocation::url(url));
    }

    #[test]
    fn set_title_updates_the_metadata_snapshot() {
        let mut track = Track::local("/music/song.mp3");

        track.set_title("My Favourite Radio");

        let metadata = track.metadata().expect("metadata attached");
        assert_eq!(metadata.title, "My Favourite Radio");
        assert!(metadata.title_tagged);
        assert_eq!(track.display_name(), "My Favourite Radio");
    }

    #[test]
    fn set_title_works_on_stream_tracks_without_metadata() {
        let url = Url::parse("https://radio.example.com/live").expect("valid url");
        let mut track = Track::from_stream(url, StreamKind::Http);

        track.set_title("My Radio");

        assert_eq!(track.display_name(), "My Radio");
    }
}
