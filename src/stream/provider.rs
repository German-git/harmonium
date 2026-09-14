//! Provider trait shared by every concrete streaming backend.
//!
//! Each provider answers two questions for a URL it can handle:
//!
//! - *What is this resource?* — full [`ResolvedStream`] metadata (title,
//!   station, codec, …) so the playlist entry carries something better than
//!   the bare hostname fallback.
//! - *How do I open it for playback?* — a [`StreamReader`] that owns the
//!   transport (HTTP body, process pipe, …) and yields bytes on demand.
//!
//! Detection and playback are deliberately split: metadata resolution must
//! not require a live network round-trip that the playback layer would also
//! pay for. When a body-bearing probe is unavoidable, providers may return an
//! opaque one-shot transport so playback can consume that same response.
//!
//! To add a new backend (Spotify, SoundCloud, Jellyfin, …), implement
//! [`StreamProvider`] for it, append the new provider to
//! [`crate::stream::resolver_with_defaults`], and the rest of the system
//! picks it up automatically through [`crate::stream::classify`].

use std::fmt;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::stream::source::StreamKind;
use url::Url;

/// Cooperative cancellation shared by one stream acquisition and its reader.
///
/// Providers remain synchronous, so the token is checked at every boundary
/// where a reader can safely stop. The owner may replace the token for a newer
/// source without affecting the identity of an older detached acquisition.
#[derive(Clone, Debug)]
pub struct StreamCancellation {
    cancelled: Arc<AtomicBool>,
    #[cfg(test)]
    active_http_requests: Arc<AtomicUsize>,
}

impl PartialEq for StreamCancellation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }
}

impl Eq for StreamCancellation {}

impl StreamCancellation {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            active_http_requests: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn begin_http_request(&self) -> ActiveHttpRequestGuard {
        self.active_http_requests.fetch_add(1, Ordering::AcqRel);
        ActiveHttpRequestGuard(self.active_http_requests.clone())
    }

    #[cfg(test)]
    pub(crate) fn active_http_requests(&self) -> usize {
        self.active_http_requests.load(Ordering::Acquire)
    }
}

/// A one-shot transport prepared while resolving stream metadata.
///
/// The concrete response stays inside its provider implementation. This
/// opaque handoff lets playback consume a response without making reqwest part
/// of the application or provider APIs.
pub struct PreparedStream {
    inner: Arc<Mutex<Option<Box<dyn PreparedStreamHandle>>>>,
}

pub(crate) trait PreparedStreamHandle: Send {
    fn open(
        self: Box<Self>,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError>;
}

impl PreparedStream {
    pub(crate) fn new(handle: Box<dyn PreparedStreamHandle>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(handle))),
        }
    }

    /// Consume the prepared transport once. A cloned handle shares the same
    /// one-shot response; later playback attempts fall back to a fresh open.
    pub(crate) fn open(
        &self,
        cancellation: &StreamCancellation,
    ) -> Option<Result<StreamReader, StreamError>> {
        let handle = match self.inner.lock() {
            Ok(mut handle) => handle.take(),
            Err(_) => {
                return Some(Err(StreamError::Other(
                    "prepared stream handle was poisoned".into(),
                )));
            }
        }?;
        Some(handle.open(cancellation))
    }
}

impl Clone for PreparedStream {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl fmt::Debug for PreparedStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedStream").finish_non_exhaustive()
    }
}

impl PartialEq for PreparedStream {
    fn eq(&self, _other: &Self) -> bool {
        // The transport is runtime state, not part of stream identity.
        true
    }
}

impl Eq for PreparedStream {}

#[cfg(test)]
pub(crate) struct ActiveHttpRequestGuard(Arc<AtomicUsize>);

#[cfg(test)]
impl Drop for ActiveHttpRequestGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Marker trait for byte streams that satisfy both [`Read`] and [`Seek`].
///
/// The blanket impl keeps [`StreamReader`] construction free of concrete
/// reader types: any `Read + Seek` source works (Cursor,
/// `SeekableHttpReader`, in-memory test doubles).
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek + ?Sized> ReadSeek for T {}

/// Metadata extracted for a stream by its provider.
///
/// `None` on a field means "the provider did not surface that information";
/// the fallback chain (provider -> ICY -> URL -> hostname) layers
/// progressively looser defaults on top. `None` is the right choice for
/// "live radio": we genuinely do not know the duration, and the M3U8
/// serializer encodes it as `-1` from a missing value, matching the spec.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedStream {
    /// Display title that should become the EXTINF label. Mandatory in the
    /// sense that callers fall back to the URL hostname when this stays
    /// empty, but the field itself is optional so a provider can leave it
    /// blank and let the fallback take over.
    pub title: Option<String>,
    /// Reported duration, only meaningful for finite (non-live) sources.
    pub duration: Option<std::time::Duration>,
    /// Stream content type when the transport was probed (e.g. `audio/mpeg`).
    pub content_type: Option<String>,
    /// Reported bitrate in bits per second, when the provider knows it.
    pub bitrate: Option<u32>,
    /// Codec / container label (`MP3`, `AAC`, `OGG`, …).
    pub codec: Option<String>,
    /// Station name (radio-style providers: Icecast, SHOUTcast, Radio
    /// Browser). Independent of `title` because the title is the song or
    /// show currently playing and the station is the broadcaster.
    pub station: Option<String>,
    /// Genre / tags from the source metadata.
    pub genre: Option<String>,
    /// Logo / favicon URL when the provider has one.
    pub logo_url: Option<Url>,
    /// Homepage URL when the provider has one.
    pub homepage: Option<Url>,
    /// Station country, when the provider surfaces it (Radio Browser only).
    pub country: Option<String>,
    /// Station language, when the provider surfaces it.
    pub language: Option<String>,
    /// One-shot transport that can be reused by playback when resolution
    /// already received a body-bearing response.
    pub(crate) prepared: Option<PreparedStream>,
}

/// What a provider hands back to the audio layer.
///
/// The provider owns the transport (HTTP response body, process pipe, …)
/// and the audio engine consumes it through one common `Read + Seek` value.
/// In-memory HLS materialization is adapted through `Cursor<Vec<u8>>`, while
/// HTTP and live HLS retain their lazy seekable readers. The optional length
/// is preserved for decoder configuration and `SeekFrom::End` callers.
pub struct StreamReader {
    inner: Box<dyn ReadSeek + Send + Sync>,
    content_length: Option<u64>,
}

impl fmt::Debug for StreamReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamReader")
            .field("content_length", &self.content_length)
            .finish_non_exhaustive()
    }
}

impl StreamReader {
    /// Wrap a seekable reader. The audio engine consumes it directly, so no
    /// upfront buffering happens on the network path.
    pub fn seekable(reader: Box<dyn ReadSeek + Send + Sync>) -> Self {
        Self::seekable_with_content_length(reader, None)
    }

    /// Wrap a seekable reader and retain the total length reported by its
    /// source, when known.
    pub fn seekable_with_content_length(
        reader: Box<dyn ReadSeek + Send + Sync>,
        content_length: Option<u64>,
    ) -> Self {
        Self {
            inner: reader,
            content_length,
        }
    }

    /// Adapt a pre-buffered `Cursor<Vec<u8>>` to the common reader. The HLS
    /// demuxer uses this path because its segment fetcher has no `Seek`.
    pub fn buffered(cursor: Cursor<Vec<u8>>) -> Self {
        let content_length = Some(cursor.get_ref().len() as u64);
        Self::seekable_with_content_length(Box::new(cursor), content_length)
    }

    /// Total length reported by the source, when known.
    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// Move this reader behind a bounded producer so subsequent reads never
    /// perform blocking transport I/O on the consumer thread.
    pub(crate) fn progressive(
        self,
        cancellation: &StreamCancellation,
    ) -> Result<Self, StreamError> {
        if cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        let Self {
            inner,
            content_length,
        } = self;
        let reader = crate::stream::progressive::ProgressiveReader::spawn_with_content_length(
            inner,
            cancellation,
            content_length,
        )
        .map_err(StreamError::Io)?;
        Ok(Self {
            inner: Box::new(reader),
            content_length,
        })
    }
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Failures that can happen while resolving a stream.
#[derive(Debug, Error)]
pub enum StreamError {
    /// The owning playback request was superseded or shut down.
    #[error("stream operation was cancelled")]
    Cancelled,
    /// The URL itself is malformed or uses an unsupported scheme.
    #[error("invalid stream URL: {0}")]
    InvalidUrl(String),
    /// A network round-trip failed (DNS, TLS, timeout, status code).
    #[error("network error for {}: {message}", crate::net::safe_url(url))]
    Network {
        /// URL the provider was contacting.
        url: Url,
        /// Human readable detail; safe to surface to the user.
        message: String,
    },
    /// The server replied but the payload was not a stream we can play.
    #[error(
        "unsupported stream response for {}: {message}",
        crate::net::safe_url(url)
    )]
    Unsupported {
        /// URL that returned an unsupported response.
        url: Url,
        /// Why the response was rejected (wrong content type, missing body, …).
        message: String,
    },
    /// The external resolver tool (currently `yt-dlp`) failed.
    #[error(
        "external resolver failed for {}: {message}",
        crate::net::safe_url(url)
    )]
    External {
        /// URL passed to the external tool.
        url: Url,
        /// Captured stderr or status detail.
        message: String,
    },
    /// The external resolver exceeded its wall-clock budget.
    #[error(
        "external resolver timed out for {} after {timeout_ms} ms",
        crate::net::safe_url(url)
    )]
    ExternalTimeout { url: Url, timeout_ms: u64 },
    /// The external resolver attempted to produce more output than allowed.
    #[error(
        "external resolver {stream} output exceeded {limit} byte limit for {}",
        crate::net::safe_url(url)
    )]
    ExternalOutputLimit {
        url: Url,
        stream: &'static str,
        limit: usize,
    },
    /// The external tool we rely on is not installed.
    #[error("missing required tool: {0}")]
    MissingTool(String),
    /// IO failure while reading the stream body for playback.
    #[error("stream IO error: {0}")]
    Io(#[from] io::Error),
    /// Any other failure, with a free-form message. Kept last so typed
    /// variants stay useful for the common cases.
    #[error("stream error: {0}")]
    Other(String),
}

/// Resolves metadata for a URL and opens a reader for playback.
///
/// The trait is intentionally object-safe: the resolver holds a `Vec<Box<dyn
/// StreamProvider>>` and iterates it. New providers only need to satisfy the
/// two-method shape, no associated types, no generics.
pub trait StreamProvider: Send + Sync {
    /// True when this provider is the canonical resolver for `url`.
    ///
    /// The check is cheap (host string, path prefix, scheme) and must not
    /// touch the network — it runs on every keystroke of the Add Stream
    /// popup so the UI can label the URL while the user types.
    fn can_handle(&self, url: &Url) -> bool;

    /// Kind this provider handles, used to surface a stable label in the
    /// playlist row.
    fn kind(&self) -> StreamKind;

    /// Resolve metadata for `url`.
    ///
    /// Implementations may return a [`ResolvedStream`] with only some fields
    /// filled in; the resolver layers fallbacks (ICY, URL hostname) on top
    /// of whatever the provider returned.
    fn resolve(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<ResolvedStream, StreamError>;

    /// Open a reader for playback.
    ///
    /// For most providers this means GETting the URL and wrapping the
    /// response body. For YouTube it means spawning `yt-dlp` with the
    /// `--get-url` style flags and handing back the process stdout. The
    /// reader is consumed lazily by rodio, so the body is not buffered into
    /// memory up front.
    fn open_reader(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError>;

    /// Open playback using a response prepared during metadata resolution.
    /// Providers without a reusable response retain their normal open path.
    fn open_reader_with_prepared(
        &self,
        url: &Url,
        prepared: Option<&PreparedStream>,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError> {
        let _ = prepared;
        self.open_reader(url, cancellation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test for the [`ResolvedStream`] builder path so future fields
    /// stay optional and easy to set in tests.
    #[test]
    fn resolved_stream_default_is_empty() {
        let resolved = ResolvedStream::default();
        assert_eq!(resolved.title, None);
        assert_eq!(resolved.duration, None);
        assert_eq!(resolved.bitrate, None);
        assert_eq!(resolved.station, None);
    }

    #[test]
    fn stream_reader_wraps_any_send_reader() {
        let bytes: Vec<u8> = b"abc".to_vec();
        let cursor = Cursor::new(bytes);
        let mut reader = StreamReader::buffered(cursor);
        let mut buf = [0u8; 3];
        reader.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"abc");
        assert_eq!(reader.content_length(), Some(3));
    }

    #[test]
    fn stream_reader_buffered_reader_supports_seek_from_end() {
        let mut reader = StreamReader::buffered(Cursor::new(b"hello".to_vec()));
        assert_eq!(reader.content_length(), Some(5));
        assert_eq!(reader.seek(SeekFrom::End(-2)).expect("seek"), 3);
        let mut buf = [0u8; 5];
        let count = reader.read(&mut buf).expect("read");
        assert_eq!(&buf[..count], b"lo");
    }

    #[test]
    fn stream_reader_seekable_reader_retains_length_and_seeking() {
        let mut reader = StreamReader::seekable_with_content_length(
            Box::new(Cursor::new(b"hello".to_vec())),
            Some(5),
        );
        assert_eq!(reader.content_length(), Some(5));
        assert_eq!(reader.seek(SeekFrom::Start(1)).expect("seek"), 1);
        let mut buf = [0u8; 5];
        let count = reader.read(&mut buf).expect("read");
        assert_eq!(&buf[..count], b"ello");
    }

    #[test]
    fn stream_reader_is_read_seek_send_sync() {
        fn assert_bounds<T: Read + Seek + Send + Sync>() {}
        assert_bounds::<StreamReader>();
    }
}
