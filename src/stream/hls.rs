//! Native HLS (HTTP Live Streaming) demuxer for the streaming layer.
//!
//! YouTube Live URLs (`https://www.youtube.com/live/<id>`) and most radio
//! streams delivered over plain HTTP today respond with an
//! `application/vnd.apple.mpegurl` playlist rather than raw audio bytes.
//! Before this provider existed, [`crate::stream::http::HttpProvider`] passed
//! the playlist body straight to the symphonia decoder, which choked on the
//! `#EXTM3U` text. We now sniff the response, parse it as either a master
//! playlist (multiple variants) or a media playlist (a sequence of
//! `.ts`/`.m4s` segments), and hand the audio engine a continuous byte
//! stream.
//!
//! ## Crate choice
//!
//! We use [`hls_m3u8`] 0.7 (`sile/hls_m3u8`). It is a parsing-only crate
//! with a single tiny transitive dependency (`stable-vec`), which keeps the
//! audio path fully synchronous — the player talks to the network
//! exclusively through `reqwest::blocking` and we did not want an async
//! runtime here. Alternatives considered:
//!
//! - `m3u8-rs` is also sync-friendly but less actively maintained.
//! - `hls` (the tokio-based one) requires an async runtime and would force
//!   the audio worker onto tokio, which is the opposite of what the spec
//!   asks for.
//!
//! `hls_m3u8` exposes `MediaPlaylist`, `MasterPlaylist` and `MediaSegment`
//! directly through `TryFrom<&str>`. We use it as the structural backbone
//! and layer our own segment fetcher on top.
//!
//! ## Live streams
//!
//! VOD playlists carry `#EXT-X-ENDLIST`. Once we have consumed every
//! segment the reader hits EOF exactly like a finite stream and the audio
//! engine advances the queue as usual.
//!
//! Live playlists never carry `EXT-X-ENDLIST`. We pre-fetch the first useful
//! segment, retain the rest of the initial snapshot as pending work, then
//! refetch the manifest as that work drains. The refetch is paced by the
//! longest segment duration in the playlist, capped at 10 seconds, so the
//! audio engine never busy-loops on a quiet broadcaster. The reader only
//! returns EOF when the upstream server genuinely stops producing new
//! segments — the audio engine treats that as a stream-end condition, which
//! is exactly the same behaviour we already have for Icecast/SHOUTcast feeds
//! that stop sending data.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use hls_m3u8::{MasterPlaylist, MediaPlaylist};
use reqwest::blocking::Response;
use url::Url;

use crate::stream::provider::{
    ResolvedStream, StreamCancellation, StreamError, StreamProvider, StreamReader,
};
use crate::stream::source::StreamKind;

/// Maximum wall-clock time the demuxer will spend refetching segments for
/// a live stream before declaring the upstream stalled and returning EOF.
///
/// Eight seconds bounds a live refetch stall so the audio worker has a
/// predictable upper limit before the reader reports end-of-stream.
const LIVE_STALL_TIMEOUT: Duration = Duration::from_secs(8);

/// Default refresh interval for live manifests when the playlist does not
/// declare a target duration. YouTube Live, Twitch and most radio
/// platforms advertise a target duration around 4–10 s. This is the cap
/// used by the live strategy in all cases.
///
/// Cap on the refresh interval for live manifests so a buggy server
/// declaring `#EXT-X-TARGETDURATION:600` does not stall us for minutes.
const MAX_LIVE_REFRESH: Duration = Duration::from_secs(10);

/// Hard cap on the retained unread buffer for live playlists.
///
/// Consumed bytes are compacted from the front before more segments are
/// fetched, so this remains a bounded sliding window rather than a cumulative
/// lifetime limit.
const MAX_LIVE_BUFFER_BYTES: usize = 32 * 1024 * 1024;
/// Minimum consumed prefix worth compacting during normal reads.
///
/// The half-buffer check below makes compaction amortized linear for large
/// buffers; this threshold avoids repeatedly copying tiny buffers while a
/// decoder consumes them in small reads.
const LIVE_BUFFER_COMPACTION_THRESHOLD: usize = 64 * 1024;
/// Maximum size of a materialized HLS manifest.
pub(crate) const MAX_HLS_MANIFEST_BYTES: usize = 2 * 1024 * 1024;

/// Demuxer for HLS streams. Never reached through the resolver directly —
/// [`crate::stream::http::HttpProvider`] sniffs the `Content-Type` and the
/// `.m3u8` extension on the response and dispatches here.
#[derive(Debug, Default, Clone)]
pub struct HlsProvider {
    _private: (),
}

impl HlsProvider {
    /// Construct a provider ready to handle HLS responses.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl StreamProvider for HlsProvider {
    fn can_handle(&self, _url: &Url) -> bool {
        // The HLS provider is dispatched from inside `HttpProvider` after
        // the manifest body has already been fetched, not from the
        // resolver (which works on URL host only). Returning `false` keeps
        // the resolver chain honest — the canonical HTTP provider still
        // claims every URL the HLS demuxer ends up consuming.
        false
    }

    fn kind(&self) -> StreamKind {
        StreamKind::Http
    }

    fn resolve(
        &self,
        _url: &Url,
        _cancellation: &StreamCancellation,
    ) -> Result<ResolvedStream, StreamError> {
        // HLS endpoints do not expose standalone metadata; the playlist
        // URL is the only stable identifier. The resolver's hostname
        // fallback already covers the "no metadata at all" case, so
        // returning an empty record is the right thing.
        Ok(ResolvedStream::default())
    }

    fn open_reader(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError> {
        // `HlsProvider::can_handle` always returns false, so the resolver
        // never reaches this method directly. Keep the implementation valid
        // for callers that invoke the provider explicitly, though.
        stream_reader_from_hls(build_reader(url, cancellation)?, url, cancellation)
    }
}

/// Configuration that decides whether the reader is finite or live and
/// (for live) carries the state used to refetch the manifest on demand.
#[derive(Debug)]
enum Mode {
    /// A finite playlist: every segment has been prefetched into the
    /// buffer. When the buffer drains we are done.
    Vod,
    /// A live playlist: refetch the manifest when the buffer drains, pull
    /// every segment whose sequence number is newer than what we already
    /// have, and never return EOF until the upstream genuinely stops
    /// producing new segments.
    Live {
        /// URL of the media playlist to refetch.
        playlist_url: Url,
        /// Sequence number of the next segment we still need to fetch.
        next_sequence: usize,
        /// Earliest time we should consider refetching the manifest.
        /// Pacing refetches by the longest segment duration keeps a noisy
        /// upstream from making us churn on the network.
        next_refresh_at: Instant,
        /// Refresh interval for the live manifest.
        refresh_interval: Duration,
    },
}

/// Concrete reader handed to the audio engine.
///
/// Holds a prefetched byte buffer, the list of segment URLs still to
/// consume, and — for live streams — enough state to refetch the manifest
/// when the buffer drains.
#[derive(Debug)]
pub struct HlsReader {
    mode: Mode,
    cancellation: StreamCancellation,
    /// Segment URLs still pending (sequence numbers already known).
    pending: VecDeque<Url>,
    /// The segment currently being copied into the bounded reader buffer.
    /// Keeping the response here lets each producer read make partial
    /// progress instead of materializing one whole segment.
    segment: Option<(Url, Response)>,
    /// Buffer of bytes already pulled from the network.
    buffer: Vec<u8>,
    /// Absolute stream offset represented by `buffer[0]`.
    buffer_start: u64,
    /// Position inside `buffer`.
    pos: usize,
    /// Wall-clock budget for waiting on a live stream whose upstream
    /// genuinely stopped producing new segments. Bounded so a stalled
    /// radio does not pin the audio thread forever.
    stall_timeout: Duration,
}

/// A seek operation that cannot be satisfied by a live HLS byte stream.
///
/// The error is stored inside `io::Error` because `Read + Seek` is the
/// boundary required by rodio. Keeping a concrete error value here prevents
/// an unsupported decoder seek from being mistaken for a successful no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsSeekError {
    /// A live playlist has no stable end position.
    LiveEndPositionUnknown,
}

/// Typed interruption returned when a live HLS read is superseded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsReadError {
    /// The owning playback request was replaced or shut down.
    Cancelled,
}

impl std::fmt::Display for HlsReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("live HLS read was cancelled"),
        }
    }
}

impl std::error::Error for HlsReadError {}

impl std::fmt::Display for HlsSeekError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LiveEndPositionUnknown => {
                f.write_str("live HLS stream does not support seeking from the end")
            }
        }
    }
}

impl std::error::Error for HlsSeekError {}

/// Outcome of sniffing a manifest URL. The dispatch in `HttpProvider`
/// calls [`build_reader`] once the response body is known to be a
/// playlist.
pub(crate) fn build_reader(
    url: &Url,
    cancellation: &StreamCancellation,
) -> Result<HlsReader, StreamError> {
    let (body, final_url) = fetch_bytes(url, cancellation)?;
    build_reader_from_body(&body, &final_url, cancellation)
}

/// Build an [`HlsReader`] from an already-fetched manifest body.
///
/// Exposed at `pub(crate)` so the dispatch inside
/// [`crate::stream::http`] can hand the body straight over without paying
/// for a second network round-trip.
pub(crate) fn build_reader_from_body(
    body: &[u8],
    playlist_url: &Url,
    cancellation: &StreamCancellation,
) -> Result<HlsReader, StreamError> {
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    if body.len() > MAX_HLS_MANIFEST_BYTES {
        return Err(StreamError::ExternalOutputLimit {
            url: playlist_url.clone(),
            stream: "HLS manifest",
            limit: MAX_HLS_MANIFEST_BYTES,
        });
    }
    // The manifest is text; reject binary payloads early so a
    // misconfigured server cannot push us onto the parse path.
    let text = std::str::from_utf8(body).map_err(|error| StreamError::External {
        url: playlist_url.clone(),
        message: format!("HLS manifest is not valid UTF-8: {error}"),
    })?;

    // Try the media playlist first because that is the common case (a
    // single variant already selected by `yt-dlp`, a single-mountpoint
    // radio). Fall back to the master playlist when the body is not a
    // media playlist — the spec allows interleaved unknown tags, so we
    // cannot rely on a single header line.
    if let Ok(media) = MediaPlaylist::try_from(text) {
        return HlsReader::new_media(media, playlist_url.clone(), cancellation);
    }

    match MasterPlaylist::try_from(text) {
        Ok(master) => {
            // Pick the highest-bandwidth audio-only variant, then refetch
            // that variant's media playlist. We do this here (rather than
            // refetching inside `new_media`) so callers can hand us a
            // master playlist URL and still get a single reader.
            let media_url = pick_audio_variant(&master, playlist_url)?;
            let (body, final_media_url) = fetch_bytes(&media_url, cancellation)?;
            let text = std::str::from_utf8(&body).map_err(|error| StreamError::External {
                url: final_media_url.clone(),
                message: format!("HLS media playlist is not valid UTF-8: {error}"),
            })?;
            let media = MediaPlaylist::try_from(text).map_err(|error| StreamError::External {
                url: final_media_url.clone(),
                message: format!("failed to parse HLS media playlist: {error}"),
            })?;
            HlsReader::new_media(media, final_media_url, cancellation)
        }
        Err(error) => Err(StreamError::External {
            url: playlist_url.clone(),
            message: format!("HLS manifest is neither a media nor master playlist: {error}"),
        }),
    }
}

/// Put every HLS reader behind the bounded producer path. VOD and live
/// playlists use the same adapter; the reader itself only retains the current
/// segment, while `ProgressiveReader` owns the bounded read-ahead window.
pub(crate) fn stream_reader_from_hls(
    reader: HlsReader,
    _url: &Url,
    cancellation: &StreamCancellation,
) -> Result<StreamReader, StreamError> {
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    StreamReader::seekable(Box::new(reader)).progressive(cancellation)
}

/// Build a reader from a parsed media playlist. Shared between the
/// direct-from-URL path and the master-playlist refetch path so the
/// segment handling stays in one place.
impl HlsReader {
    fn is_live(&self) -> bool {
        matches!(&self.mode, Mode::Live { .. })
    }

    fn new_media(
        media: MediaPlaylist<'_>,
        playlist_url: Url,
        cancellation: &StreamCancellation,
    ) -> Result<Self, StreamError> {
        if cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        let segments: Vec<Url> = media
            .segments
            .iter()
            .map(|(_, segment)| playlist_url.join(segment.uri().as_ref()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| StreamError::External {
                url: playlist_url.clone(),
                message: format!("HLS segment URI failed to resolve: {error}"),
            })?;

        let is_live = !media.has_end_list && !segments.is_empty();
        let mode = if !is_live {
            Mode::Vod
        } else {
            let refresh_interval = media.target_duration.min(MAX_LIVE_REFRESH);
            let next_sequence = media
                .segments
                .iter()
                .last()
                .map_or(media.media_sequence, |(_, segment)| {
                    segment.number().saturating_add(1)
                });
            Mode::Live {
                playlist_url,
                next_sequence,
                next_refresh_at: Instant::now() + refresh_interval,
                refresh_interval,
            }
        };

        // Leave every segment to the long-lived producer. The reader starts
        // with an empty buffer, so opening a VOD never transfers segment I/O
        // onto the audio worker or materializes the playlist in memory.
        let pending = segments.into_iter().collect();
        let buffer = Vec::new();

        let mut reader = Self {
            mode,
            cancellation: cancellation.clone(),
            pending,
            segment: None,
            buffer,
            buffer_start: 0,
            pos: 0,
            stall_timeout: LIVE_STALL_TIMEOUT,
        };
        // Preserve low-latency live startup: one partial segment is opened
        // before the reader is handed to the bounded producer. VOD remains
        // completely deferred and therefore never materializes at open time.
        if reader.is_live() {
            loop {
                match reader.append_segment_chunk(MAX_LIVE_BUFFER_BYTES)? {
                    Some(bytes) if bytes > 0 => break,
                    Some(_) => continue,
                    None => break,
                }
            }
        }
        Ok(reader)
    }

    fn compact_consumed(&mut self, before_append: bool) {
        if self.pos == 0 {
            return;
        }

        let consumed = self.pos;
        let unread = self.buffer.len() - consumed;
        if !before_append
            && (self.is_live()
                && (consumed < LIVE_BUFFER_COMPACTION_THRESHOLD || consumed < unread))
        {
            return;
        }

        self.buffer.copy_within(consumed.., 0);
        self.buffer.truncate(unread);
        self.buffer_start += consumed as u64;
        self.pos = 0;
    }

    /// Copy one bounded chunk from the current segment. `Some(0)` means that
    /// a segment ended (or was skipped); `None` means that there are no
    /// segments left to start. A positive value is deliberately returned as
    /// soon as it is read so the outer progressive producer can apply its own
    /// backpressure before another network read.
    fn append_segment_chunk(&mut self, cap: usize) -> Result<Option<usize>, StreamError> {
        if self.segment.is_none() {
            let Some(url) = self.pending.pop_front() else {
                return Ok(None);
            };
            let response = crate::stream::http::get_response(&url, &self.cancellation)?;
            self.segment = Some((url, response));
        }

        let remaining = cap.saturating_sub(self.buffer.len());
        if remaining == 0 {
            // The consumer has not made room yet. Keep the response open so
            // the next refill resumes at the exact byte where this segment
            // stopped instead of dropping its remainder.
            return Ok(Some(0));
        }

        let read_limit = if remaining == usize::MAX {
            16 * 1024
        } else {
            remaining.min(16 * 1024)
        };
        let mut chunk = vec![0u8; read_limit];
        let url = self
            .segment
            .as_ref()
            .map(|(url, _)| url.clone())
            .ok_or_else(|| StreamError::Other("HLS segment response disappeared".into()))?;
        let read = {
            let Some((_, response)) = self.segment.as_mut() else {
                return Err(StreamError::Other(
                    "HLS segment response disappeared".into(),
                ));
            };
            response
                .read(&mut chunk)
                .map_err(|error| StreamError::Network {
                    url: url.clone(),
                    message: error.to_string(),
                })?
        };
        if read == 0 {
            tracing::warn!(
                "HLS segment was empty; skipping {}",
                crate::net::safe_url(&url)
            );
            self.segment = None;
            return Ok(Some(0));
        }

        let appended = read.min(remaining);
        self.buffer.extend_from_slice(&chunk[..appended]);
        Ok(Some(appended))
    }

    fn refill_vod(&mut self) -> Result<bool, StreamError> {
        loop {
            self.compact_consumed(true);
            match self.append_segment_chunk(usize::MAX) {
                Ok(Some(bytes)) if bytes > 0 => return Ok(true),
                Ok(Some(_)) => continue,
                Ok(None) => return Ok(false),
                Err(StreamError::Cancelled) => return Err(StreamError::Cancelled),
                Err(error) => {
                    tracing::warn!("HLS VOD segment failed; skipping remainder: {error}");
                    self.segment = None;
                }
            }
        }
    }

    /// Refill the prefetch buffer using the live strategy.
    ///
    /// Returns `Ok(true)` when new bytes were appended, `Ok(false)` when
    /// the upstream genuinely stopped producing new segments (so the
    /// reader can surface EOF). Returns `Err` only on a hard failure.
    ///
    /// Each call performs at most one manifest refetch; subsequent calls
    /// (triggered by `read()` once the new bytes are drained) will fetch
    /// again. This bounds the audio thread's blocking time on any
    /// single refill to one network round-trip plus the segment fetches
    /// already known about.
    fn refill_live(&mut self) -> Result<bool, StreamError> {
        if self.cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        let mut did_refetch = false;
        loop {
            if self.cancellation.is_cancelled() {
                return Err(StreamError::Cancelled);
            }
            // Step 1: drain any pending segment URLs that we already know
            // about but have not yet fetched.
            if !self.pending.is_empty() || self.segment.is_some() {
                // Appending is the one place where stale consumed bytes can
                // consume the live cap, so compact the prefix before fetching
                // the next queued segment. Normal reads use the amortized
                // threshold/half-buffer path instead.
                self.compact_consumed(true);
                loop {
                    if self.buffer.len() >= MAX_LIVE_BUFFER_BYTES {
                        break;
                    }
                    match self.append_segment_chunk(MAX_LIVE_BUFFER_BYTES) {
                        Ok(Some(bytes)) if bytes > 0 => return Ok(true),
                        Ok(Some(_)) => continue,
                        Ok(None) => break,
                        Err(StreamError::Cancelled) => return Err(StreamError::Cancelled),
                        Err(error) => {
                            tracing::warn!("HLS segment failed; skipping remainder: {error}");
                            self.segment = None;
                        }
                    }
                }
            }

            // Step 2: pending is empty (or every pending segment returned
            // nothing). Refetch the manifest once per call and queue any
            // new segments for the next loop iteration.
            if did_refetch {
                return Ok(false);
            }
            did_refetch = true;

            let (playlist_url, next_sequence, refresh_due) = match &mut self.mode {
                Mode::Live {
                    playlist_url,
                    next_sequence,
                    next_refresh_at,
                    refresh_interval,
                    ..
                } => {
                    let now = Instant::now();
                    let due = now >= *next_refresh_at;
                    if due {
                        *next_refresh_at = now + *refresh_interval;
                    }
                    (playlist_url.clone(), *next_sequence, due)
                }
                Mode::Vod => {
                    return Err(StreamError::Other(
                        "refill_live called on a finite (VOD) HLS reader".into(),
                    ));
                }
            };

            if !refresh_due {
                // Manifest is fresh; nothing more to do this pass.
                return Ok(false);
            }

            // Refetch the manifest body. Treat any failure as "no new
            // content" so the audio thread does not tear down the stream on
            // a transient network blip.
            let (manifest, manifest_url) = match fetch_bytes(&playlist_url, &self.cancellation) {
                Ok(result) => result,
                Err(StreamError::Cancelled) => return Err(StreamError::Cancelled),
                Err(error) => {
                    tracing::warn!(
                        "HLS live manifest refetch failed for {}: {error}",
                        crate::net::safe_url(&playlist_url)
                    );
                    return Ok(false);
                }
            };

            let text = match std::str::from_utf8(&manifest) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!("HLS live manifest is not UTF-8: {error}");
                    return Ok(false);
                }
            };

            let media = match MediaPlaylist::try_from(text) {
                Ok(media) => media,
                Err(error) => {
                    tracing::warn!("HLS live manifest parse failed: {error}");
                    return Ok(false);
                }
            };

            // Queue every segment whose sequence number is newer than what
            // we have already buffered. The next loop iteration (still
            // inside this call) drains them.
            for (_, segment) in media.segments.iter() {
                if segment.number() < next_sequence {
                    continue;
                }
                let absolute = match manifest_url.join(segment.uri().as_ref()) {
                    Ok(url) => url,
                    Err(error) => {
                        tracing::warn!(
                            "HLS live segment URI failed to resolve: {error}; skipping segment"
                        );
                        continue;
                    }
                };
                self.pending.push_back(absolute);
            }

            // Update the next-sequence cursor so the next refill skips
            // segments we already have. We use the highest number we have
            // observed in this manifest rather than counting segments
            // because live playlists can drop old segments between refetches.
            if let Some((_, last)) = media.segments.iter().last()
                && let Mode::Live { next_sequence, .. } = &mut self.mode
            {
                *next_sequence = last.number() + 1;
            }
        }
    }
}

impl Read for HlsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.cancellation.is_cancelled() {
            return Err(cancelled_io_error());
        }

        self.compact_consumed(false);

        // Drain whatever we have buffered.
        if self.pos < self.buffer.len() {
            let remaining = self.buffer.len() - self.pos;
            let take = buf.len().min(remaining);
            buf[..take].copy_from_slice(&self.buffer[self.pos..self.pos + take]);
            self.pos += take;
            return Ok(take);
        }

        // Buffer empty: pull the next segment for VOD, or refetch the
        // manifest for live.
        match &mut self.mode {
            Mode::Vod => match self.refill_vod() {
                Ok(true) => self.read(buf),
                Ok(false) => Ok(0),
                Err(StreamError::Cancelled) => Err(cancelled_io_error()),
                Err(error) => Err(io::Error::other(error.to_string())),
            },
            Mode::Live { .. } => {
                // Live streams must block on the audio thread until new
                // bytes arrive — returning `Ok(0)` here signals EOF to
                // the symphonia decoder and tears down playback even
                // though the upstream is still producing segments. We
                // cap the wait at `stall_timeout` so a genuinely stalled
                // radio does not pin the worker forever; only after that
                // budget elapses do we surface `Ok(0)` to the decoder.
                let deadline = Instant::now() + self.stall_timeout;
                loop {
                    if self.cancellation.is_cancelled() {
                        return Err(cancelled_io_error());
                    }
                    let next_refresh_at = match &self.mode {
                        Mode::Live {
                            next_refresh_at, ..
                        } => *next_refresh_at,
                        Mode::Vod => unreachable!("matched Mode::Live above"),
                    };
                    let sleep_until = next_refresh_at.min(deadline);
                    let now = Instant::now();
                    if now < sleep_until {
                        let remaining = sleep_until - now;
                        std::thread::sleep(remaining.min(CANCELLATION_POLL));
                        continue;
                    }
                    match self.refill_live() {
                        Ok(true) => return self.read(buf),
                        Ok(false) if Instant::now() >= deadline => return Ok(0),
                        Ok(false) => continue,
                        Err(StreamError::Cancelled) => return Err(cancelled_io_error()),
                        Err(error) => return Err(io::Error::other(error.to_string())),
                    }
                }
            }
        }
    }
}

impl Seek for HlsReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let current = self.buffer_start + self.pos as u64;
        let target = match pos {
            SeekFrom::Start(target) => target,
            SeekFrom::Current(delta) => {
                let target = i128::from(current) + i128::from(delta);
                if target < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "seek before start of HLS stream",
                    ));
                }
                u64::try_from(target).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "HLS seek position overflowed")
                })?
            }
            SeekFrom::End(delta) => {
                let end = match &self.mode {
                    Mode::Vod => self.buffer_start + self.buffer.len() as u64,
                    Mode::Live { .. } => {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            HlsSeekError::LiveEndPositionUnknown,
                        ));
                    }
                };
                let target = i128::from(end) + i128::from(delta);
                if target < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "seek before start of HLS stream",
                    ));
                }
                u64::try_from(target).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "HLS seek position overflowed")
                })?
            }
        };

        let buffer_end = self.buffer_start + self.buffer.len() as u64;
        if target < self.buffer_start {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "HLS seek position is no longer retained",
            ));
        }
        if target <= buffer_end {
            self.pos = (target - self.buffer_start) as usize;
            return Ok(target);
        }

        let mut remaining = target - current;
        let mut discarded = [0u8; 16 * 1024];
        while remaining > 0 {
            let take = remaining.min(discarded.len() as u64) as usize;
            let read = self.read(&mut discarded[..take])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "HLS stream ended before the requested seek position",
                ));
            }
            remaining -= read as u64;
        }
        Ok(self.buffer_start + self.pos as u64)
    }
}

/// Polling cadence used while a live HLS read waits for a refresh or network
/// operation to observe its cancellation token.
const CANCELLATION_POLL: Duration = Duration::from_millis(10);

/// Fetch the manifest body as bytes.
fn fetch_bytes(
    url: &Url,
    cancellation: &StreamCancellation,
) -> Result<(Vec<u8>, Url), StreamError> {
    let response = crate::stream::http::get_response(url, cancellation)?;
    let final_url = response.url().clone();
    let body = crate::stream::http::read_bounded_response(
        response,
        &final_url,
        cancellation,
        MAX_HLS_MANIFEST_BYTES,
    )?;
    Ok((body, final_url))
}

fn cancelled_io_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, HlsReadError::Cancelled)
}

/// Identify the variant with the highest bandwidth whose CODECS advertise
/// audio (`mp4a`) and no video. Falls back to the highest-bandwidth
/// variant when every available rendition is muxed, which is the common
/// case for YouTube Live and a few radio CDNs.
fn pick_audio_variant(master: &MasterPlaylist, base: &Url) -> Result<Url, StreamError> {
    let mut best: Option<(u64, Url)> = None;
    let mut fallback: Option<(u64, Url)> = None;

    for variant in &master.variant_streams {
        let uri = match variant {
            hls_m3u8::tags::VariantStream::ExtXIFrame { uri, .. } => uri,
            hls_m3u8::tags::VariantStream::ExtXStreamInf { uri, .. } => uri,
        };
        let absolute = base
            .join(uri.as_ref())
            .map_err(|error| StreamError::External {
                url: base.clone(),
                message: format!("HLS master playlist contains a malformed variant URI: {error}"),
            })?;
        let bandwidth = variant.bandwidth();

        // Track the highest-bandwidth variant as a fallback so we never
        // bail out when every variant happens to advertise both audio
        // and video codecs.
        if fallback.as_ref().is_none_or(|(b, _)| bandwidth > *b) {
            fallback = Some((bandwidth, absolute.clone()));
        }

        let Some(codecs) = variant.codecs() else {
            continue;
        };
        // We want an audio-bearing rendition with no video references so
        // symphonia does not have to discard video frames it never
        // asked for.
        let has_audio = codecs.iter().any(|c| c.contains("mp4a"));
        let has_video = codecs.iter().any(|c| {
            c.starts_with("avc1")
                || c.starts_with("avc3")
                || c.starts_with("hvc1")
                || c.starts_with("hev1")
                || c.starts_with("vp09")
                || c.starts_with("vp9")
        });
        if has_audio && !has_video && best.as_ref().is_none_or(|(b, _)| bandwidth > *b) {
            best = Some((bandwidth, absolute));
        }
    }

    best.or(fallback)
        .map(|(_, url)| url)
        .ok_or_else(|| StreamError::External {
            url: base.clone(),
            message: "HLS master playlist carries no variants".into(),
        })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::test_support::{ScriptedHttpResponse, ScriptedHttpServer};

    fn sample_media() -> &'static str {
        "#EXTM3U\n\
         #EXT-X-VERSION:3\n\
         #EXT-X-TARGETDURATION:10\n\
         #EXTINF:9.5,\n\
         segment0.ts\n\
         #EXTINF:9.0,\n\
         segment1.ts\n\
         #EXT-X-ENDLIST\n"
    }

    fn sample_master() -> &'static str {
        "#EXTM3U\n\
         #EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.5\"\n\
         audio.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=240000,CODECS=\"avc1.42e00a,mp4a.40.2\"\n\
         muxed.m3u8\n\
         #EXT-X-STREAM-INF:BANDWIDTH=320000,CODECS=\"mp4a.40.2\"\n\
         audio-high.m3u8\n"
    }

    fn sample_live() -> &'static str {
        "#EXTM3U\n\
         #EXT-X-VERSION:3\n\
         #EXT-X-TARGETDURATION:6\n\
         #EXT-X-MEDIA-SEQUENCE:100\n\
         #EXTINF:5.5,\n\
         live-100.ts\n\
         #EXTINF:5.5,\n\
         live-101.ts\n"
    }

    fn url(s: &str) -> Url {
        Url::parse(s).expect("fixture url")
    }

    fn cancellation() -> StreamCancellation {
        StreamCancellation::new()
    }

    #[test]
    fn media_playlist_parses_segment_uris() {
        let playlist = MediaPlaylist::try_from(sample_media()).expect("parse");
        let uris: Vec<&str> = playlist
            .segments
            .iter()
            .map(|(_, segment)| segment.uri().as_ref())
            .collect();
        assert_eq!(uris, vec!["segment0.ts", "segment1.ts"]);
        assert!(playlist.has_end_list);
    }

    #[test]
    fn master_playlist_variant_picker_prefers_audio_only_highest_bandwidth() {
        let master = MasterPlaylist::try_from(sample_master()).expect("parse");
        let base = url("https://radio.example.com/live/");
        let picked = pick_audio_variant(&master, &base).expect("variant");
        // The audio-only variants are `audio.m3u8` (64 kbps) and
        // `audio-high.m3u8` (320 kbps). The muxed 240 kbps variant is
        // skipped because its CODECS contain `avc1`.
        assert_eq!(
            picked.as_str(),
            "https://radio.example.com/live/audio-high.m3u8"
        );
    }

    #[test]
    fn master_playlist_falls_back_to_highest_bandwidth_when_audio_only_missing() {
        // Both variants are muxed, so the picker must fall back to the
        // highest-bandwidth muxed rendition rather than returning an
        // error.
        let playlist = "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"avc1.42e00a,mp4a.40.2\"\n\
             lo.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=512000,CODECS=\"avc1.42e00a,mp4a.40.2\"\n\
             hi.m3u8\n";
        let master = MasterPlaylist::try_from(playlist).expect("parse");
        let base = url("https://radio.example.com/");
        let picked = pick_audio_variant(&master, &base).expect("variant");
        assert_eq!(picked.as_str(), "https://radio.example.com/hi.m3u8");
    }

    #[test]
    fn live_playlist_parses_without_end_list() {
        let playlist = MediaPlaylist::try_from(sample_live()).expect("parse");
        assert!(!playlist.has_end_list);
        let uris: Vec<&str> = playlist
            .segments
            .iter()
            .map(|(_, segment)| segment.uri().as_ref())
            .collect();
        assert_eq!(uris, vec!["live-100.ts", "live-101.ts"]);
        assert_eq!(playlist.media_sequence, 100);
    }

    #[test]
    fn build_reader_rejects_non_hls_body() {
        let url = url("https://radio.example.com/index.m3u8");
        // Garbage that is neither a master nor a media playlist must
        // surface as a clean `StreamError::External` so the audio layer
        // can render a notification.
        let error = build_reader_from_body(b"# not a manifest", &url, &cancellation())
            .expect_err("rejects");
        assert!(matches!(error, StreamError::External { .. }));
    }

    #[test]
    fn build_reader_fetches_a_delayed_chunked_manifest_and_segment() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::chunked(
                200,
                [sample_media().as_bytes().to_vec()],
                Duration::from_millis(1),
            )
            .with_header("Content-Type", "application/vnd.apple.mpegurl"),
            ScriptedHttpResponse::fixed(200, b"segment bytes"),
            ScriptedHttpResponse::fixed(200, b"second segment"),
        ]);
        let playlist_url = url(&server.endpoint("playlist.m3u8"));
        let mut reader = build_reader(&playlist_url, &cancellation()).expect("fetch HLS fixture");
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("read HLS fixture");

        assert_eq!(bytes, b"segment bytessecond segment");
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn redirected_manifest_resolves_relative_segments_from_the_final_url() {
        let server = ScriptedHttpServer::new(std::iter::empty());
        server.push_response(
            ScriptedHttpResponse::fixed(302, Vec::new())
                .with_header("Location", server.endpoint("nested/playlist.m3u8")),
        );
        server.push_response(ScriptedHttpResponse::fixed(200, sample_media().as_bytes()));
        server.push_response(ScriptedHttpResponse::fixed(200, b"segment bytes"));
        server.push_response(ScriptedHttpResponse::fixed(200, b"second segment"));

        let playlist_url = url(&server.endpoint("playlist.m3u8"));
        let mut reader = build_reader(&playlist_url, &cancellation()).expect("redirected HLS");
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("read redirected HLS");

        assert_eq!(bytes, b"segment bytessecond segment");
        let requests = server.requests();
        assert!(
            requests
                .iter()
                .any(|request| request.contains("/nested/segment0.ts"))
        );
        assert!(
            requests
                .iter()
                .any(|request| request.contains("/nested/segment1.ts"))
        );
    }

    #[test]
    fn build_reader_rejects_non_utf8_body() {
        let url = url("https://radio.example.com/index.m3u8");
        let bytes = [0xFFu8, 0x00, 0xFF];
        let error = build_reader_from_body(&bytes, &url, &cancellation()).expect_err("rejects");
        assert!(matches!(error, StreamError::External { .. }));
    }

    #[test]
    fn build_reader_rejects_an_oversized_manifest_before_parsing() {
        let url = url("https://radio.example.com/index.m3u8");
        let body = vec![b' '; MAX_HLS_MANIFEST_BYTES + 1];
        let error = build_reader_from_body(&body, &url, &cancellation())
            .expect_err("rejects oversized body");
        assert!(matches!(
            error,
            StreamError::ExternalOutputLimit {
                stream: "HLS manifest",
                ..
            }
        ));
    }

    /// Smoke test that the live strategy records the playlist's target
    /// duration as its refresh interval, capped at 10 s.
    #[test]
    fn live_strategy_caps_refresh_interval_at_ten_seconds() {
        let playlist = "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:600\n\
             #EXT-X-MEDIA-SEQUENCE:1\n\
             #EXTINF:5.0,\n\
             a.ts\n";
        let parsed = MediaPlaylist::try_from(playlist).expect("parse");
        let interval = parsed.target_duration.min(MAX_LIVE_REFRESH);
        assert_eq!(interval, MAX_LIVE_REFRESH);
        assert!(!parsed.has_end_list);
    }

    /// The live reader signals EOF only when the upstream genuinely
    /// stops producing new content. We exercise that contract by
    /// constructing a reader whose `pending` list is empty and whose
    /// `next_refresh_at` is in the past; with a 100 ms `stall_timeout`,
    /// `read` must block until the timeout elapses and then return
    /// `Ok(0)` because the refetch keeps surfacing `Ok(false)`. The
    /// earlier instant-EOF behaviour would silently tear down playback
    /// for any live stream that simply had no new bytes at the moment
    /// of the read.
    #[test]
    fn live_reader_signals_eof_only_after_refetch_returns_no_new_bytes() {
        let mut reader = HlsReader {
            mode: Mode::Live {
                playlist_url: url("data:text/plain,"),
                next_sequence: 0,
                next_refresh_at: Instant::now() - Duration::from_secs(1),
                refresh_interval: Duration::from_secs(1),
            },
            cancellation: cancellation(),
            pending: VecDeque::new(),
            segment: None,
            buffer: Vec::new(),
            buffer_start: 0,
            pos: 0,
            stall_timeout: Duration::from_millis(100),
        };
        let mut sink = [0u8; 16];
        // After up to 100 ms of waiting for the upstream to produce
        // new segments (which it never will, because the data: URL
        // cannot be parsed as a manifest), the reader surfaces EOF.
        let outcome = reader.read(&mut sink);
        assert!(
            matches!(outcome, Ok(0)),
            "live reader must surface Ok(0) when upstream stops after stall timeout"
        );
    }

    /// Companion to the stall-timeout test: while the upstream is still
    /// producing new segments, the live reader must block on `read` and
    /// never return `Ok(0)` prematurely. We exercise that contract by
    /// pointing the playlist URL at a non-existent host so every refill
    /// fails, and by picking a `stall_timeout` long enough to assert
    /// that the call returns *after* waiting (i.e. it actually blocks
    /// rather than answering immediately).
    #[test]
    fn live_reader_blocks_until_stall_timeout_when_refill_keeps_failing() {
        use std::time::Instant as StdInstant;
        let started = StdInstant::now();
        let mut reader = HlsReader {
            mode: Mode::Live {
                playlist_url: url("http://127.0.0.1:1/no-such-host.m3u8"),
                next_sequence: 0,
                next_refresh_at: Instant::now() - Duration::from_secs(1),
                refresh_interval: Duration::from_millis(50),
            },
            cancellation: cancellation(),
            pending: VecDeque::new(),
            segment: None,
            buffer: Vec::new(),
            buffer_start: 0,
            pos: 0,
            stall_timeout: Duration::from_millis(120),
        };
        let mut sink = [0u8; 16];
        let outcome = reader.read(&mut sink);
        let elapsed = started.elapsed();
        assert!(
            matches!(outcome, Ok(0)),
            "live reader must surface Ok(0) once the stall timeout elapses"
        );
        assert!(
            elapsed >= Duration::from_millis(100),
            "live reader must block until the stall timeout, not return early (elapsed: {elapsed:?})"
        );
    }

    #[test]
    fn live_reader_returns_typed_cancellation_during_delayed_manifest_refresh() {
        let server = ScriptedHttpServer::new([ScriptedHttpResponse::chunked(
            200,
            [b"#EXTM3U\n".as_slice()],
            Duration::from_secs(2),
        )]);
        let token = cancellation();
        let mut reader = HlsReader {
            mode: Mode::Live {
                playlist_url: url(&server.endpoint("live.m3u8")),
                next_sequence: 0,
                next_refresh_at: Instant::now() - Duration::from_secs(1),
                refresh_interval: Duration::from_secs(1),
            },
            cancellation: token.clone(),
            pending: VecDeque::new(),
            segment: None,
            buffer: Vec::new(),
            buffer_start: 0,
            pos: 0,
            stall_timeout: Duration::from_secs(5),
        };
        let started = Instant::now();
        let task = std::thread::spawn(move || {
            let mut bytes = [0u8; 16];
            reader.read(&mut bytes)
        });
        std::thread::sleep(Duration::from_millis(50));
        token.cancel();
        let error = task
            .join()
            .expect("live refresh task must join")
            .expect_err("cancellation must interrupt the read");

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "live refresh cancellation exceeded the bounded delay: {:?}",
            started.elapsed()
        );
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<HlsReadError>())
                .is_some_and(|error| *error == HlsReadError::Cancelled)
        );
    }

    /// `HlsReader` satisfies the bounds the audio engine requires:
    /// `Send` (so the worker thread can own it) and `Read` (so the
    /// decoder can pull bytes).
    #[test]
    fn hls_reader_satisfies_stream_reader_bounds() {
        fn assert_send<T: Send>() {}
        assert_send::<HlsReader>();
        fn assert_read_seek<T: Read + Seek>() {}
        assert_read_seek::<HlsReader>();
    }

    #[test]
    fn live_reader_delivers_initial_segment_before_waiting_for_manifest_eof() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, sample_live().as_bytes())
                .with_header("Content-Type", "application/vnd.apple.mpegurl"),
            ScriptedHttpResponse::fixed(200, b"youtube live segment"),
            ScriptedHttpResponse::chunked(
                200,
                [b"youtube second ".as_slice(), b"segment".as_slice()],
                Duration::from_secs(2),
            ),
        ]);
        let playlist_url = url(&server.endpoint("youtube-live.m3u8"));
        let started = Instant::now();
        let token = cancellation();
        let reader = build_reader(&playlist_url, &token).expect("fetch live HLS fixture");
        let mut reader =
            stream_reader_from_hls(reader, &playlist_url, &token).expect("wrap live reader");
        let mut bytes = [0u8; 20];
        std::io::Read::read_exact(&mut reader, &mut bytes).expect("read initial segment");

        assert_eq!(&bytes, b"youtube live segment");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "live startup must not wait for EOF"
        );
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn live_initial_snapshot_does_not_duplicate_on_first_refresh() {
        let refreshed = "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:1\n\
             #EXT-X-MEDIA-SEQUENCE:100\n\
             #EXTINF:1.0,\n\
             live-100.ts\n\
             #EXTINF:1.0,\n\
             live-101.ts\n\
             #EXTINF:1.0,\n\
             live-102.ts\n";
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, sample_live().as_bytes()),
            ScriptedHttpResponse::fixed(200, b"initial-100"),
            ScriptedHttpResponse::fixed(200, b"initial-101"),
            ScriptedHttpResponse::fixed(200, refreshed.as_bytes()),
            ScriptedHttpResponse::fixed(200, b"refreshed-102"),
        ]);
        let playlist_url = url(&server.endpoint("youtube-live.m3u8"));
        let mut reader =
            build_reader(&playlist_url, &cancellation()).expect("fetch live HLS fixture");
        if let Mode::Live {
            next_refresh_at, ..
        } = &mut reader.mode
        {
            *next_refresh_at = Instant::now() - Duration::from_secs(1);
        }

        let mut initial = [0u8; 11];
        reader.read_exact(&mut initial).expect("read first segment");
        assert_eq!(&initial, b"initial-100");

        let mut pending = [0u8; 11];
        reader
            .read_exact(&mut pending)
            .expect("read pending segment");
        assert_eq!(&pending, b"initial-101");

        let mut refreshed_bytes = [0u8; 13];
        reader
            .read_exact(&mut refreshed_bytes)
            .expect("read refreshed segment");
        assert_eq!(&refreshed_bytes, b"refreshed-102");

        let requests = server.requests();
        assert_eq!(requests.len(), 5);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("/live-100.ts"))
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("/live-101.ts"))
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("/live-102.ts"))
                .count(),
            1
        );
    }

    #[test]
    fn live_reader_compacts_only_after_threshold_and_half_buffer() {
        let mut reader = HlsReader {
            mode: Mode::Live {
                playlist_url: url("data:text/plain,"),
                next_sequence: 0,
                next_refresh_at: Instant::now(),
                refresh_interval: Duration::from_secs(1),
            },
            cancellation: cancellation(),
            pending: VecDeque::new(),
            segment: None,
            buffer: vec![b'x'; LIVE_BUFFER_COMPACTION_THRESHOLD * 2],
            buffer_start: 0,
            pos: LIVE_BUFFER_COMPACTION_THRESHOLD - 1,
            stall_timeout: Duration::from_millis(1),
        };

        reader.compact_consumed(false);
        assert_eq!(reader.buffer_start, 0);
        assert_eq!(reader.pos, LIVE_BUFFER_COMPACTION_THRESHOLD - 1);

        reader.pos += 1;
        reader.compact_consumed(false);
        assert_eq!(reader.buffer_start, LIVE_BUFFER_COMPACTION_THRESHOLD as u64);
        assert_eq!(reader.pos, 0);
        assert_eq!(reader.buffer.len(), LIVE_BUFFER_COMPACTION_THRESHOLD);
    }

    #[test]
    fn live_reader_preserves_absolute_position_across_compaction_and_seek() {
        let buffer_start = 41u64;
        let mut buffer = vec![0u8; LIVE_BUFFER_COMPACTION_THRESHOLD * 2];
        buffer[LIVE_BUFFER_COMPACTION_THRESHOLD] = 0xa5;
        let mut reader = HlsReader {
            mode: Mode::Live {
                playlist_url: url("data:text/plain,"),
                next_sequence: 0,
                next_refresh_at: Instant::now(),
                refresh_interval: Duration::from_secs(1),
            },
            cancellation: cancellation(),
            pending: VecDeque::new(),
            segment: None,
            buffer,
            buffer_start,
            pos: LIVE_BUFFER_COMPACTION_THRESHOLD,
            stall_timeout: Duration::from_millis(1),
        };

        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).expect("read after compaction");
        assert_eq!(byte, [0xa5]);
        assert_eq!(
            reader.buffer_start,
            buffer_start + LIVE_BUFFER_COMPACTION_THRESHOLD as u64
        );
        assert_eq!(
            reader.seek(SeekFrom::Current(0)).expect("current position"),
            buffer_start + LIVE_BUFFER_COMPACTION_THRESHOLD as u64 + 1
        );

        let target = buffer_start + LIVE_BUFFER_COMPACTION_THRESHOLD as u64;
        assert_eq!(
            reader.seek(SeekFrom::Start(target)).expect("absolute seek"),
            target
        );
        reader
            .read_exact(&mut byte)
            .expect("read after absolute seek");
        assert_eq!(byte, [0xa5]);
    }

    #[test]
    fn live_reader_compacts_consumed_bytes_before_fetching_future_segments() {
        let server = ScriptedHttpServer::new([ScriptedHttpResponse::fixed(200, b"future")]);
        let mut reader = HlsReader {
            mode: Mode::Live {
                playlist_url: url(&server.endpoint("playlist.m3u8")),
                next_sequence: 1,
                next_refresh_at: Instant::now() + Duration::from_secs(60),
                refresh_interval: Duration::from_secs(60),
            },
            cancellation: cancellation(),
            pending: VecDeque::from([url(&server.endpoint("future.ts"))]),
            segment: None,
            buffer: vec![b'x'; MAX_LIVE_BUFFER_BYTES],
            buffer_start: 0,
            pos: MAX_LIVE_BUFFER_BYTES,
            stall_timeout: Duration::from_millis(1),
        };

        assert!(reader.refill_live().expect("fetch future segment"));
        let mut bytes = [0u8; 6];
        reader.read_exact(&mut bytes).expect("read future segment");
        assert_eq!(&bytes, b"future");
        assert_eq!(reader.buffer_start, MAX_LIVE_BUFFER_BYTES as u64);
        assert_eq!(reader.buffer.len(), 6);
    }

    #[test]
    fn live_reader_repeats_refreshes_without_duplicates_and_preserves_order() {
        let initial = "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:1\n\
             #EXT-X-MEDIA-SEQUENCE:100\n\
             #EXTINF:1.0,\n\
             a100.ts\n\
             #EXTINF:1.0,\n\
             a101.ts\n";
        let refresh_one = "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:1\n\
             #EXT-X-MEDIA-SEQUENCE:102\n\
             #EXTINF:1.0,\n\
             a102.ts\n\
             #EXTINF:1.0,\n\
             a103.ts\n";
        let refresh_two = "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:1\n\
             #EXT-X-MEDIA-SEQUENCE:104\n\
             #EXTINF:1.0,\n\
             a104.ts\n\
             #EXTINF:1.0,\n\
             a105.ts\n";
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, initial),
            ScriptedHttpResponse::fixed(200, b"A"),
            ScriptedHttpResponse::fixed(200, b"B"),
            ScriptedHttpResponse::fixed(200, refresh_one),
            ScriptedHttpResponse::fixed(200, b"C"),
            ScriptedHttpResponse::fixed(200, b"D"),
            ScriptedHttpResponse::fixed(200, refresh_two),
            ScriptedHttpResponse::fixed(200, b"E"),
            ScriptedHttpResponse::fixed(200, b"F"),
        ]);
        let playlist_url = url(&server.endpoint("live.m3u8"));
        let mut reader = build_reader(&playlist_url, &cancellation()).expect("live HLS fixture");
        let mut output = Vec::new();
        let mut byte = [0u8; 1];

        for (index, expected) in [b'A', b'B', b'C', b'D', b'E', b'F'].into_iter().enumerate() {
            if matches!(index, 1 | 2 | 4) {
                if let Mode::Live {
                    next_refresh_at, ..
                } = &mut reader.mode
                {
                    *next_refresh_at = Instant::now() - Duration::from_secs(1);
                }
            }
            reader.read_exact(&mut byte).expect("read ordered segment");
            output.push(byte[0]);
            assert_eq!(byte[0], expected);
        }

        assert_eq!(output, b"ABCDEF");
        let requests = server.requests();
        let segment_paths = [
            "a100.ts", "a101.ts", "a102.ts", "a103.ts", "a104.ts", "a105.ts",
        ];
        let segment_requests: Vec<&String> = requests
            .iter()
            .filter(|request| request.contains(".ts"))
            .collect();
        assert_eq!(segment_requests.len(), segment_paths.len());
        for (request, path) in segment_requests.iter().zip(segment_paths) {
            assert!(
                request.contains(path),
                "segment request {request:?} was not {path}"
            );
        }
    }

    #[test]
    fn vod_reader_fetches_segments_incrementally() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, sample_media().as_bytes())
                .with_header("Content-Type", "application/vnd.apple.mpegurl"),
            ScriptedHttpResponse::fixed(200, b"segment bytes"),
            ScriptedHttpResponse::fixed(200, b"second segment"),
        ]);
        let playlist_url = url(&server.endpoint("vod.m3u8"));
        let token = cancellation();
        let reader = build_reader(&playlist_url, &token).expect("fetch VOD HLS fixture");
        let mut reader =
            stream_reader_from_hls(reader, &playlist_url, &token).expect("wrap VOD reader");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut bytes).expect("read VOD");

        assert_eq!(bytes, b"segment bytessecond segment");
        assert_eq!(reader.content_length(), None);
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn segment_reads_resume_losslessly_after_the_buffer_limit() {
        let server = ScriptedHttpServer::new([ScriptedHttpResponse::fixed(200, b"abcdef")]);
        let mut reader = HlsReader {
            mode: Mode::Vod,
            cancellation: cancellation(),
            pending: VecDeque::from([url(&server.endpoint("oversized.ts"))]),
            segment: None,
            buffer: Vec::new(),
            buffer_start: 0,
            pos: 0,
            stall_timeout: LIVE_STALL_TIMEOUT,
        };

        assert_eq!(
            reader.append_segment_chunk(4).expect("read segment"),
            Some(4)
        );
        assert_eq!(reader.buffer, b"abcd");
        assert!(reader.segment.is_some(), "the response must remain active");

        // A refill while the ring is still full must not consume the active
        // response. The consumer then frees only part of the ring, so the
        // unread bytes already buffered and the response remainder coexist.
        assert_eq!(
            reader.append_segment_chunk(4).expect("full buffer refill"),
            Some(0)
        );
        assert!(
            reader.segment.is_some(),
            "a full buffer must retain the active response"
        );
        let mut consumed = [0u8; 2];
        reader
            .read_exact(&mut consumed)
            .expect("drain part of buffer");
        assert_eq!(&consumed, b"ab");
        reader.compact_consumed(true);

        assert_eq!(
            reader.append_segment_chunk(4).expect("finish segment"),
            Some(2)
        );
        assert_eq!(reader.buffer, b"cdef");
        assert!(reader.segment.is_some(), "EOF is observed on the next read");
        let mut remainder = [0u8; 4];
        reader
            .read_exact(&mut remainder)
            .expect("drain buffered remainder");
        assert_eq!(&remainder, b"cdef");
        reader.compact_consumed(true);
        assert_eq!(
            reader.append_segment_chunk(4).expect("observe EOF"),
            Some(0)
        );
        assert!(reader.segment.is_none());
        assert_eq!(
            reader.append_segment_chunk(4).expect("no pending segment"),
            None
        );
    }

    #[test]
    fn live_end_seek_returns_a_typed_error() {
        let mut reader = HlsReader {
            mode: Mode::Live {
                playlist_url: url("data:text/plain,"),
                next_sequence: 0,
                next_refresh_at: Instant::now(),
                refresh_interval: Duration::from_secs(1),
            },
            cancellation: cancellation(),
            pending: VecDeque::new(),
            segment: None,
            buffer: Vec::new(),
            buffer_start: 0,
            pos: 0,
            stall_timeout: Duration::from_millis(1),
        };

        let error = reader
            .seek(SeekFrom::End(0))
            .expect_err("live end seek must fail");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<HlsSeekError>())
                .is_some_and(|error| *error == HlsSeekError::LiveEndPositionUnknown)
        );
    }

    /// `pick_audio_variant` must handle a master playlist whose CODECS
    /// list contains the canonical H.264 + AAC pairing by falling back
    /// to the highest-bandwidth variant. This is the YouTube Live
    /// shape.
    #[test]
    fn master_picker_handles_youtube_live_codec_shape() {
        let playlist = "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=192000,CODECS=\"mp4a.40.2,avc1.42c00d\"\n\
             v1.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=384000,CODECS=\"mp4a.40.2,avc1.42c00d\"\n\
             v2.m3u8\n";
        let master = MasterPlaylist::try_from(playlist).expect("parse");
        let base = url("https://manifest.googlevideo.com/");
        let picked = pick_audio_variant(&master, &base).expect("variant");
        assert_eq!(picked.as_str(), "https://manifest.googlevideo.com/v2.m3u8");
    }

    /// An empty master playlist (no `EXT-X-STREAM-INF`) must surface
    /// `StreamError::External` so the audio layer renders a clean
    /// notification rather than panicking on an empty vector.
    #[test]
    fn master_picker_rejects_empty_playlist() {
        let playlist = "#EXTM3U\n";
        let master = MasterPlaylist::try_from(playlist).expect("parse");
        let base = url("https://radio.example.com/");
        let error = pick_audio_variant(&master, &base).expect_err("empty");
        assert!(matches!(error, StreamError::External { .. }));
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 32,
            failure_persistence: None,
            max_shrink_iters: 128,
            rng_algorithm: proptest::test_runner::RngAlgorithm::ChaCha,
            rng_seed: proptest::test_runner::RngSeed::Fixed(0x4933_3006),
            .. ProptestConfig::default()
        })]

        #[test]
        fn arbitrary_hls_text_is_total(
            text in prop::collection::vec(any::<char>(), 0..=1024)
                .prop_map(|characters| characters.into_iter().collect::<String>()),
            _bytes in prop::collection::vec(any::<u8>(), 0..=2048),
        ) {
            // Parse both playlist forms directly so generated master playlists
            // cannot trigger a network fetch during this property test.
            let _ = MediaPlaylist::try_from(text.as_str());
            let _ = MasterPlaylist::try_from(text.as_str());
        }
    }
}
