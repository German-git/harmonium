//! Identification of the resource behind a queued track.
//!
//! [`TrackSource`] is the single field that decides whether a queue entry
//! points at a file on disk or at a remote stream. Everything else in the
//! crate — the M3U8 layer, the audio engine, the rename/edit shortcuts —
//! branches on this enum rather than on lexical URL checks scattered around.
//!
//! The split mirrors the spec's "what is this URL?" vs "how do I play it?"
//! principle:
//!
//! - Detection (`url_detect`) classifies a URL into a [`StreamKind`].
//! - The provider layer resolves metadata and an openable transport.
//! - The audio layer only sees an opaque reader handed back by the provider.
//!
//! For streams, the URL stored in the playlist is the **original** URL (the
//! YouTube page, the Radio Browser station page, the Icecast mountpoint). The
//! provider re-resolves it on playback, so a temporary media URL never
//! leaks into the persisted `.m3u8`.

use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

use url::Url;

/// Classification of a stream URL used by the resolver and surfaced in the
/// playlist entry so the UI can label the source honestly.
///
/// `Http` covers every plain HTTP/HTTPS stream that is not a recognised
/// platform: Icecast and SHOUTcast servers fall in here too because the
/// transport is the same and the metadata they expose is a strict subset of
/// the generic HTTP case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKind {
    /// Public YouTube watch / share URL.
    YouTube,
    /// Radio Browser station page or direct stream URL.
    RadioBrowser,
    /// Generic HTTP/HTTPS stream (Icecast, SHOUTcast, raw MP3/AAC/OGG/Opus).
    Http,
}

impl StreamKind {
    /// Short label suitable for the playlist row and the status footer.
    pub fn label(self) -> &'static str {
        match self {
            StreamKind::YouTube => "YouTube",
            StreamKind::RadioBrowser => "Radio Browser",
            StreamKind::Http => "Stream",
        }
    }
}

impl fmt::Display for StreamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Identity of a queued track.
///
/// `Local` is a real file on disk: the player opens it through the rodio
/// decoder exactly as it did before streams existed.
///
/// `Stream` carries the URL the user typed (or that was parsed out of an
/// `.m3u8`) plus the detected kind and, when the user followed a tracking
/// redirect, the original URL the playlist should keep on disk. The audio
/// layer never has to know which provider a stream came from: it just asks
/// the source for a reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSource {
    /// A real audio file on the local filesystem.
    Local(crate::track::TrackPath),
    /// A remote stream identified by its canonical URL.
    Stream {
        /// Canonical URL of the resource. For YouTube this is the watch URL;
        /// for Radio Browser it is the station stream URL; for raw HTTP
        /// streams it is the mountpoint. Always safe to round-trip through
        /// `.m3u8`.
        url: Url,
        /// Detected kind, used to pick the provider at playback time.
        kind: StreamKind,
        /// Runtime-only response prepared during metadata resolution.
        prepared: Option<crate::stream::provider::PreparedStream>,
    },
}

impl TrackSource {
    /// Build a local-file source from any path-like value.
    pub fn local(path: impl Into<PathBuf>) -> Self {
        TrackSource::Local(crate::track::TrackPath::new(path))
    }

    /// Build a stream source from a parsed URL and detected kind.
    pub fn stream(url: Url, kind: StreamKind) -> Self {
        TrackSource::Stream {
            url,
            kind,
            prepared: None,
        }
    }

    /// Attach a one-shot response for the next playback open.
    pub(crate) fn attach_prepared(
        &mut self,
        prepared: Option<crate::stream::provider::PreparedStream>,
    ) {
        if let TrackSource::Stream { prepared: slot, .. } = self {
            *slot = prepared;
        }
    }

    /// Borrow the runtime-only response prepared during resolution.
    pub(crate) fn prepared(&self) -> Option<&crate::stream::provider::PreparedStream> {
        match self {
            TrackSource::Stream { prepared, .. } => prepared.as_ref(),
            TrackSource::Local(_) => None,
        }
    }

    /// True when the source points at the local filesystem.
    pub fn is_local(&self) -> bool {
        matches!(self, TrackSource::Local(_))
    }

    /// True when the source is a remote stream of any kind.
    pub fn is_stream(&self) -> bool {
        matches!(self, TrackSource::Stream { .. })
    }

    /// Stream kind, if the source is a stream. `None` for local files.
    pub fn stream_kind(&self) -> Option<StreamKind> {
        match self {
            TrackSource::Local(_) => None,
            TrackSource::Stream { kind, .. } => Some(*kind),
        }
    }

    /// Filesystem path, only meaningful for local sources.
    ///
    /// Returns `None` for streams so the call site has to acknowledge that a
    /// stream has no path.
    pub fn path(&self) -> Option<&Path> {
        match self {
            TrackSource::Local(p) => Some(p.as_path()),
            TrackSource::Stream { .. } => None,
        }
    }

    /// Typed stable identity of this source.
    pub fn track_location(&self) -> crate::track::TrackLocation {
        match self {
            TrackSource::Local(path) => crate::track::TrackLocation::Local(path.clone()),
            TrackSource::Stream { url, .. } => crate::track::TrackLocation::url(url.clone()),
        }
    }

    /// Convert this source location to text for presentation only.
    pub fn display_location(&self) -> Cow<'_, str> {
        match self {
            TrackSource::Local(p) => p.to_string_lossy(),
            TrackSource::Stream { url, .. } => Cow::Borrowed(url.as_str()),
        }
    }
}

impl fmt::Display for TrackSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrackSource::Local(p) => write!(f, "{}", p.display()),
            TrackSource::Stream { url, kind, .. } => write!(f, "{kind}: {url}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_source_exposes_its_path() {
        let source = TrackSource::local("/music/song.mp3");
        assert!(source.is_local());
        assert!(!source.is_stream());
        assert_eq!(source.path(), Some(Path::new("/music/song.mp3")));
        assert_eq!(source.display_location(), "/music/song.mp3");
        assert_eq!(
            source.track_location(),
            crate::track::TrackLocation::local("/music/song.mp3")
        );
        assert_eq!(source.stream_kind(), None);
    }

    #[test]
    fn stream_source_hides_the_path_and_carries_the_url() {
        let url = Url::parse("https://radio.example.com/live").expect("valid url");
        let source = TrackSource::stream(url.clone(), StreamKind::Http);

        assert!(source.is_stream());
        assert!(!source.is_local());
        assert_eq!(source.path(), None);
        assert_eq!(source.display_location(), url.as_str());
        assert_eq!(
            source.track_location(),
            crate::track::TrackLocation::url(url.clone())
        );
        assert_eq!(source.stream_kind(), Some(StreamKind::Http));
    }

    #[test]
    fn stream_kind_label_is_stable() {
        assert_eq!(StreamKind::YouTube.label(), "YouTube");
        assert_eq!(StreamKind::RadioBrowser.label(), "Radio Browser");
        assert_eq!(StreamKind::Http.label(), "Stream");
    }
}
