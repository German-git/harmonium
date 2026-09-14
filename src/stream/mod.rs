//! Streaming support layered on top of the local-file playlist model.
//!
//! The player used to assume every queued track was a file on disk. Streams
//! (YouTube, Radio Browser, Icecast, SHOUTcast, plain HTTP/HTTPS radios) enter
//! the same playlist through a single [`TrackSource`] enum, so the rest of the
//! application keeps a uniform queue: the M3U8 layer, the audio engine, the
//! rename/edit shortcuts and the popup dialogs only branch on the source kind.
//!
//! Layout:
//!
//! - [`source`] defines [`TrackSource`] and the [`StreamKind`] taxonomy. This
//!   is the only data type the rest of the app ever sees; everything else in
//!   this module is an implementation detail of the resolution pipeline.
//! - [`provider`] declares the [`StreamProvider`](provider::StreamProvider)
//!   trait that every concrete resolver implements, together with the
//!   [`ResolvedStream`](provider::ResolvedStream) value object and the typed
//!   [`StreamError`](provider::StreamError).
//! - [`resolver`] orchestrates the providers: it picks the first one that
//!   claims the URL, runs it, and falls back to a synthetic entry built from
//!   the URL itself when resolution fails entirely.
//! - [`http`], [`youtube`], [`radio_browser`], [`hls`] are the concrete
//!   providers. New ones (Spotify, SoundCloud, etc.) drop into the same
//!   trait without touching the rest of the codebase.
//! - [`url_detect`] classifies a URL into a [`StreamKind`] so the UI and the
//!   resolver can pick the right provider without each module repeating the
//!   same `if url.host_str() == ...` ladder.
//!
//! The detection / metadata / playback split is intentional: every provider
//! answers two questions ("what is this URL?" and "how do I play it?") with
//! the same trait, but the metadata half never depends on the audio backend
//! and can be unit tested without spinning up rodio.

pub mod hls;
pub mod http;
pub mod progressive;
pub mod provider;
pub mod radio_browser;
pub mod resolver;
pub mod seekable;
pub mod source;
pub mod url_detect;
pub mod youtube;

pub use progressive::ProgressiveReader;
pub use provider::{
    ReadSeek, ResolvedStream, StreamCancellation, StreamError, StreamProvider, StreamReader,
};
pub use resolver::{StreamResolver, resolver_with_defaults};
pub use seekable::SeekableHttpReader;
pub use source::{StreamKind, TrackSource};
pub use url_detect::classify;

/// Lightweight identifier returned when a playlist cursor points at a
/// stream. Keeps the rename / edit-metadata shortcuts on streams honest
/// about what they can mutate (the title) and what they must not touch
/// (the URL itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamTarget {
    /// Canonical URL of the stream as it sits in the playlist.
    pub url: url::Url,
    /// Detected kind, used to label the stream source in the popup.
    pub kind: StreamKind,
}
