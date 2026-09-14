//! HTTP response wrapper that exposes a seekable byte stream.
//!
//! Symphonia (the decoder used through rodio) requires a `Read + Seek` source
//! to probe the format header before it can begin decoding. A plain HTTP body
//! cannot satisfy `Seek` because the underlying socket has no random access.
//! The previous workaround buffered the body into an in-memory `Cursor` capped
//! at 32 MiB; long progressive audio files (a multi-hour YouTube stream,
//! for example) hit the cap and stopped mid-track.
//!
//! [`SeekableHttpReader`] solves the truncation by making the body seekable
//! at the HTTP layer. Forward seeks within the bytes already pulled from the
//! socket drain the buffered reader cheaply; backward seeks reopen the
//! connection with a `Range` request so the body really does start at the
//! requested offset. The audio engine consumes bytes lazily, so a track
//! longer than any in-memory cap plays through to its real end.
//!
//! Trade-offs:
//!
//! - Forward seek beyond the buffered bytes still pulls bytes from the
//!   network; that is the only way to honour the requested offset, and
//!   symphonia only does it once per track (header probe + initial scan).
//! - A backward seek issues a new GET, which costs one extra round-trip
//!   the first time. For streams the decoder typically seeks to 0 only,
//!   so the common path is the forward-drain one.

use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::time::Instant;

use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use url::Url;

use crate::stream::provider::{StreamCancellation, StreamError};

/// Typed interruption returned when a seekable stream is superseded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekableReadError {
    /// The owning playback request was replaced or shut down.
    Cancelled,
}

impl std::fmt::Display for SeekableReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("seekable HTTP read was cancelled"),
        }
    }
}

impl std::error::Error for SeekableReadError {}

/// HTTP body reader that supports `Seek` without buffering the whole payload.
///
/// Forward seeks drain bytes from the live socket; backward seeks reopen the
/// connection with a `Range` request so the body starts at the new offset.
/// See the module-level documentation for the rationale.
pub struct SeekableHttpReader {
    url: Url,
    /// Logical position of the next byte that will be returned by `read`.
    position: u64,
    /// Length reported by the first successful response (before any Range
    /// request resets the per-response Content-Length).
    content_length: Option<u64>,
    /// Active HTTP body, wrapped in a `BufReader` so repeated small reads do
    /// not pay one syscall each. Always populated while the reader is alive.
    response: BufReader<reqwest::blocking::Response>,
    cancellation: StreamCancellation,
    poll_read_timeouts: bool,
}

impl SeekableHttpReader {
    /// Open `url` and prepare a seekable reader for its body.
    ///
    /// The first GET issues no `Range` header so the server returns the full
    /// body from offset 0; the reader records `Content-Length` (when
    /// present) so `SeekFrom::End` resolves without a second round-trip.
    pub fn new(url: &Url) -> Result<Self, StreamError> {
        let cancellation = StreamCancellation::new();
        let (response, content_length) = open_range(url, None, &cancellation, false)?;
        Ok(Self {
            url: url.clone(),
            position: 0,
            content_length,
            response: BufReader::new(response),
            cancellation,
            poll_read_timeouts: false,
        })
    }

    /// Open `url` with cooperative cancellation owned by the caller.
    #[cfg(test)]
    pub(crate) fn new_with_cancellation(
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<Self, StreamError> {
        let (response, content_length) = open_range(url, None, cancellation, true)?;
        Ok(Self {
            url: url.clone(),
            position: 0,
            content_length,
            response: BufReader::new(response),
            cancellation: cancellation.clone(),
            poll_read_timeouts: true,
        })
    }

    /// Wrap an already-open response without issuing another request.
    ///
    /// `final_url` must be the response's post-redirect URL so subsequent
    /// backward seeks send Range requests to the same resource that supplied
    /// the initial body. The response body is untouched, therefore the first
    /// read starts at byte zero.
    pub(crate) fn from_response(
        response: reqwest::blocking::Response,
        final_url: Url,
        cancellation: &StreamCancellation,
    ) -> Result<Self, StreamError> {
        if cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        let content_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        Ok(Self {
            url: final_url,
            position: 0,
            content_length,
            response: BufReader::new(response),
            cancellation: cancellation.clone(),
            poll_read_timeouts: true,
        })
    }

    /// Total body length as reported by the initial response, when known.
    /// `None` for chunked or ICY responses that do not advertise a length.
    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// Issue a fresh GET for the body starting at `from`.
    ///
    /// The new `Content-Length` is intentionally discarded: the original
    /// response already gave us the authoritative body size, and a 206
    /// partial response would advertise the size of the remaining suffix
    /// (which depends on the offset). Keeping the first value stable lets
    /// `SeekFrom::End` always agree with the body that was first opened.
    fn reopen_at(&mut self, from: u64) -> io::Result<()> {
        self.check_cancelled()?;
        let (response, _new_length) = open_range(&self.url, Some(from), &self.cancellation, true)
            .map_err(|error| match error {
            StreamError::Cancelled => cancelled_io_error(),
            StreamError::Io(error) => error,
            error => io::Error::other(error),
        })?;
        self.check_cancelled()?;
        self.response = BufReader::new(response);
        self.position = from;
        Ok(())
    }

    /// Drain `bytes` from the current response, discarding the contents.
    ///
    /// Returns `Ok(())` when the requested amount was drained; returns early
    /// on EOF (allowed) or on an IO error. Reaching EOF mid-drain is allowed
    /// and just leaves `position` short of the requested offset; the next
    /// read surfaces EOF.
    fn drain(&mut self, bytes: u64) -> io::Result<()> {
        let mut remaining = bytes;
        let mut chunk = [0u8; 16 * 1024];
        while remaining > 0 {
            self.check_cancelled()?;
            let take = (remaining as usize).min(chunk.len());
            let result = self.read_response(&mut chunk[..take]);
            match result {
                Ok(0) => break,
                Ok(n) => {
                    self.position += n as u64;
                    remaining -= n as u64;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self.cancellation.is_cancelled() {
            Err(cancelled_io_error())
        } else {
            Ok(())
        }
    }

    fn read_response(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let deadline = Instant::now() + crate::net::STREAM_READ_TIMEOUT;
        loop {
            self.check_cancelled()?;
            let result = self.response.read(buf);
            if self.cancellation.is_cancelled() {
                return Err(cancelled_io_error());
            }
            match result {
                Err(error) if self.poll_read_timeouts && is_timeout_error(&error) => {
                    if Instant::now() >= deadline {
                        return Err(error);
                    }
                    continue;
                }
                result => return result,
            }
        }
    }
}

impl Read for SeekableHttpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.read_response(buf)?;
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for SeekableHttpReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.check_cancelled()?;
        match pos {
            SeekFrom::Start(target) => {
                if target == self.position {
                    return Ok(self.position);
                }
                if target > self.position {
                    self.drain(target - self.position)?;
                } else {
                    self.reopen_at(target)?;
                }
                Ok(self.position)
            }
            SeekFrom::Current(delta) => {
                let target = match (self.position as i128).checked_add(delta as i128) {
                    Some(value) if value >= 0 => value as u64,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "seek before start of stream",
                        ));
                    }
                };
                self.seek(SeekFrom::Start(target))
            }
            SeekFrom::End(delta) => {
                let content_length = self.content_length.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "seek from end requires a Content-Length",
                    )
                })?;
                let target = match (content_length as i128).checked_add(delta as i128) {
                    Some(value) if value >= 0 => value as u64,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "seek before start of stream",
                        ));
                    }
                };
                self.seek(SeekFrom::Start(target))
            }
        }
    }
}

/// Issue the underlying HTTP request and return the live response plus the
/// `Content-Length` reported by the server (when present).
///
/// `range_from = Some(p)` adds `Range: bytes=p-` so the body starts at byte
/// `p`; the resulting `Content-Length` is discarded by the caller because
/// the offset-adjusted length depends on the requested position.
fn open_range(
    url: &Url,
    range_from: Option<u64>,
    cancellation: &StreamCancellation,
    cancellation_aware: bool,
) -> Result<(reqwest::blocking::Response, Option<u64>), StreamError> {
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let client = if cancellation_aware {
        crate::net::cancellable_streaming_client()
    } else {
        crate::net::shared_streaming_client()
    }
    .ok_or_else(|| {
        StreamError::Other("could not initialize the shared stream HTTP client".into())
    })?;

    let build_request = |client: &reqwest::blocking::Client| {
        let mut request = client
            .get(url.as_str())
            .header("Icy-MetaData", "0")
            .header(reqwest::header::ACCEPT, "*/*");
        if let Some(from) = range_from {
            request = request.header(RANGE, format!("bytes={from}-"));
        }
        request
    };
    let response = match build_request(client).send() {
        Err(error) if cancellation_aware && error.is_timeout() && !cancellation.is_cancelled() => {
            // The short polling client also bounds response-header acquisition.
            // Retry a slow header exchange with the normal streaming profile so
            // cancellation polling cannot regress slow but valid connections.
            let fallback = crate::net::shared_streaming_client().ok_or_else(|| {
                StreamError::Other("could not initialize the shared stream HTTP client".into())
            })?;
            build_request(fallback).send()
        }
        result => result,
    }
    .and_then(|response| response.error_for_status())
    .map_err(|error| StreamError::Network {
        url: url.clone(),
        message: crate::net::normalize_diagnostic(&error.to_string(), 256),
    })?;
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }

    if let Some(from) = range_from {
        validate_range_response(&response, from)?;
    }

    let content_length = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    Ok((response, content_length))
}

fn cancelled_io_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, SeekableReadError::Cancelled)
}

fn is_timeout_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|source| source.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout)
}

fn validate_range_response(
    response: &reqwest::blocking::Response,
    requested_offset: u64,
) -> Result<(), StreamError> {
    if response.status().as_u16() != 206 {
        return Err(StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "ranged HTTP response did not return 206 Partial Content",
        )));
    }

    let content_range = response.headers().get(CONTENT_RANGE).ok_or_else(|| {
        StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "ranged HTTP response is missing Content-Range",
        ))
    })?;
    let content_range = content_range.to_str().map_err(|_| {
        StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "ranged HTTP response has an invalid Content-Range",
        ))
    })?;

    let mut parts = content_range.split_whitespace();
    let unit = parts.next();
    let range_and_total = parts.next();
    if parts.next().is_some() || !unit.is_some_and(|unit| unit.eq_ignore_ascii_case("bytes")) {
        return Err(StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "ranged HTTP response has an invalid Content-Range",
        )));
    }

    let Some(range_and_total) = range_and_total else {
        return Err(StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "ranged HTTP response has an invalid Content-Range",
        )));
    };
    let Some((range, total)) = range_and_total.split_once('/') else {
        return Err(StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "ranged HTTP response has an invalid Content-Range",
        )));
    };
    let Some((start, end)) = range.split_once('-') else {
        return Err(StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "ranged HTTP response has an invalid Content-Range",
        )));
    };
    let Ok(start) = start.parse::<u64>() else {
        return Err(StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "ranged HTTP response has an invalid Content-Range",
        )));
    };
    let Ok(end) = end.parse::<u64>() else {
        return Err(StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "ranged HTTP response has an invalid Content-Range",
        )));
    };
    if start != requested_offset || end < start {
        return Err(StreamError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "ranged HTTP response starts at the wrong offset",
        )));
    }
    if total != "*" {
        let Ok(total) = total.parse::<u64>() else {
            return Err(StreamError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "ranged HTTP response has an invalid Content-Range",
            )));
        };
        if total <= end {
            return Err(StreamError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "ranged HTTP response has an invalid Content-Range",
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ScriptedHttpResponse, ScriptedHttpServer};
    use std::io::{Cursor, Read};
    use std::thread;
    use std::time::Duration;

    /// Spawn a one-shot HTTP server on localhost that answers every request
    /// with `body` and tracks how many connections it accepted.
    fn one_shot_server(body: &'static [u8]) -> ScriptedHttpServer {
        reusable_server(vec![body.to_vec()])
    }

    /// Spawn a server that serves the given bodies in order. Each request
    /// consumes the next entry; once exhausted the server replies with 500.
    /// The returned `ServerHandle` lets tests drop the listener cleanly so
    /// the accept loop exits before the next test starts; without this,
    /// parallel test runs accumulate open sockets and the OS runs out of
    /// ephemeral ports within a few hundred tests.
    fn reusable_server(bodies: Vec<Vec<u8>>) -> ScriptedHttpServer {
        ScriptedHttpServer::new(bodies.into_iter().map(|body| {
            ScriptedHttpResponse::fixed(200, body)
                .with_header("Content-Type", "application/octet-stream")
        }))
    }

    #[test]
    fn reads_bytes_from_the_start() {
        let server = one_shot_server(b"hello world");
        let url = Url::parse(&server.url()).expect("valid url");
        let mut reader = SeekableHttpReader::new(&url).expect("open");
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"hello");
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).expect("read_to_end");
        assert_eq!(rest, b" world");
    }

    #[test]
    fn forward_seek_advances_position_without_reopening_the_connection() {
        let server = one_shot_server(b"abcdefghij");
        let url = Url::parse(&server.url()).expect("valid url");
        let mut reader = SeekableHttpReader::new(&url).expect("open");
        // Drain the open count to ignore the constructor's GET.
        let initial = server.requests().len();

        // Seek forward inside the buffered region.
        reader.seek(SeekFrom::Start(3)).expect("seek");
        assert_eq!(reader.position, 3);
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"defg");

        // The seek must not have triggered a second connection.
        let after = server.requests().len();
        assert_eq!(after, initial, "forward seek must not reopen the socket");
    }

    #[test]
    fn backward_seek_reopens_the_connection_with_a_range_request() {
        // The first connection gets the full body; the second connection
        // must arrive with a Range header and is served the matching
        // suffix. Capturing the wire request lets the test assert the
        // exact header value the reader sends.
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, b"abcdefghij")
                .with_header("Content-Type", "application/octet-stream"),
            ScriptedHttpResponse::fixed(206, b"defghij")
                .with_header("Content-Type", "application/octet-stream")
                .with_header("Content-Range", "bytes 3-9/10"),
        ]);
        let url = Url::parse(&server.url()).expect("url");
        let mut reader = SeekableHttpReader::new(&url).expect("open");
        // Read past byte 7 so a backward seek is required to land on 3.
        let mut scratch = [0u8; 8];
        reader.read_exact(&mut scratch).expect("drain");
        let opens_before = server.requests().len();

        reader.seek(SeekFrom::Start(3)).expect("backward seek");
        assert_eq!(reader.position, 3);
        let mut after = [0u8; 4];
        reader.read_exact(&mut after).expect("read");
        assert_eq!(&after, b"defg");

        let opens_after = server.requests().len();
        assert!(
            opens_after > opens_before,
            "backward seek must open a new connection (before={opens_before}, after={opens_after})"
        );

        let captured = server.requests();
        assert_eq!(captured.len(), 2, "two requests must have been issued");
        assert!(
            !captured[0].to_ascii_lowercase().contains("range:"),
            "the initial GET must not carry a Range header: {:?}",
            captured[0]
        );
        assert!(
            captured[1].to_ascii_lowercase().contains("range: bytes=3-"),
            "the backward-seek GET must carry Range: bytes=3-, got: {:?}",
            captured[1]
        );
    }

    #[test]
    fn backward_seek_rejects_a_server_that_ignores_range() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, b"abcdefghij"),
            ScriptedHttpResponse::fixed(200, b"abcdefghij"),
        ]);
        let url = Url::parse(&server.url()).expect("url");
        let mut reader = SeekableHttpReader::new(&url).expect("open");
        let mut scratch = [0u8; 8];
        reader.read_exact(&mut scratch).expect("drain");

        let error = reader
            .seek(SeekFrom::Start(3))
            .expect_err("a 200 response must not be treated as ranged data");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(reader.position, 8, "failed reopen must preserve position");
    }

    #[test]
    fn backward_seek_rejects_malformed_content_range() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, b"abcdefghij"),
            ScriptedHttpResponse::fixed(206, b"defghij").with_header("Content-Range", "bytes nope"),
        ]);
        let url = Url::parse(&server.url()).expect("url");
        let mut reader = SeekableHttpReader::new(&url).expect("open");
        let mut scratch = [0u8; 8];
        reader.read_exact(&mut scratch).expect("drain");

        let error = reader
            .seek(SeekFrom::Start(3))
            .expect_err("malformed Content-Range must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn backward_seek_rejects_mismatched_content_range() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, b"abcdefghij"),
            ScriptedHttpResponse::fixed(206, b"efghij")
                .with_header("Content-Range", "bytes 4-9/10"),
        ]);
        let url = Url::parse(&server.url()).expect("url");
        let mut reader = SeekableHttpReader::new(&url).expect("open");
        let mut scratch = [0u8; 8];
        reader.read_exact(&mut scratch).expect("drain");

        let error = reader
            .seek(SeekFrom::Start(3))
            .expect_err("mismatched Content-Range must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn eof_returns_zero_after_the_full_body_is_consumed() {
        let server = one_shot_server(b"xyz");
        let url = Url::parse(&server.url()).expect("valid url");
        let mut reader = SeekableHttpReader::new(&url).expect("open");
        let mut buf = [0u8; 16];
        let n = reader.read(&mut buf).expect("read");
        assert_eq!(n, 3);
        assert_eq!(&buf[..n], b"xyz");
        let n = reader.read(&mut buf).expect("read eof");
        assert_eq!(n, 0, "EOF must surface as Ok(0)");
    }

    #[test]
    fn seek_from_end_lands_at_content_length_minus_offset() {
        let server = one_shot_server(b"abcdef");
        let url = Url::parse(&server.url()).expect("valid url");
        let mut reader = SeekableHttpReader::new(&url).expect("open");
        let pos = reader.seek(SeekFrom::End(-2)).expect("seek end");
        assert_eq!(pos, 4);
        assert_eq!(reader.position, 4);
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"ef");
    }

    #[test]
    fn seek_before_start_returns_an_io_error() {
        let server = one_shot_server(b"abcdef");
        let url = Url::parse(&server.url()).expect("valid url");
        let mut reader = SeekableHttpReader::new(&url).expect("open");
        let error = reader
            .seek(SeekFrom::Current(-1))
            .expect_err("must reject negative");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn content_length_is_exposed_to_callers() {
        let server = one_shot_server(b"12345");
        let url = Url::parse(&server.url()).expect("valid url");
        let reader = SeekableHttpReader::new(&url).expect("open");
        assert_eq!(reader.content_length(), Some(5));
    }

    #[test]
    fn cancelled_open_returns_the_typed_stream_error_without_network_io() {
        let server = one_shot_server(b"unused");
        let url = Url::parse(&server.url()).expect("valid url");
        let cancellation = StreamCancellation::new();
        cancellation.cancel();

        let error = match SeekableHttpReader::new_with_cancellation(&url, &cancellation) {
            Ok(_) => panic!("cancelled open must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, StreamError::Cancelled));
        assert!(server.requests().is_empty());
    }

    #[test]
    fn cancellation_interrupts_a_delayed_read_with_a_typed_error() {
        let server = ScriptedHttpServer::new([ScriptedHttpResponse::chunked(
            200,
            [b"first".as_slice(), b"second".as_slice()],
            Duration::from_millis(250),
        )]);
        let url = Url::parse(&server.url()).expect("valid url");
        let cancellation = StreamCancellation::new();
        let mut reader = SeekableHttpReader::new_with_cancellation(&url, &cancellation)
            .expect("open delayed response");
        let mut first = [0u8; 5];
        reader.read_exact(&mut first).expect("read first chunk");
        assert_eq!(&first, b"first");
        let handle = thread::spawn(move || {
            let mut reader = reader;
            let mut second = [0u8; 6];
            reader
                .read(&mut second)
                .expect_err("cancelled read must fail")
        });

        thread::sleep(Duration::from_millis(25));
        cancellation.cancel();
        let error = handle.join().expect("reader thread must join");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<SeekableReadError>()),
            Some(&SeekableReadError::Cancelled)
        );
    }

    #[test]
    fn cancellation_interrupts_a_delayed_forward_seek_drain() {
        let server = ScriptedHttpServer::new([ScriptedHttpResponse::chunked(
            200,
            [b"first".as_slice(), b"second".as_slice()],
            Duration::from_millis(250),
        )]);
        let url = Url::parse(&server.url()).expect("valid url");
        let cancellation = StreamCancellation::new();
        let reader = SeekableHttpReader::new_with_cancellation(&url, &cancellation)
            .expect("open delayed response");
        let handle = thread::spawn(move || {
            let mut reader = reader;
            reader
                .seek(SeekFrom::Start(10))
                .expect_err("cancelled drain must fail")
        });

        thread::sleep(Duration::from_millis(25));
        cancellation.cancel();
        let error = handle.join().expect("seek thread must join");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<SeekableReadError>()),
            Some(&SeekableReadError::Cancelled)
        );
    }

    #[test]
    fn cancellation_prevents_a_backward_reopen_request() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, b"abcdefghij"),
            ScriptedHttpResponse::fixed(206, b"defghij")
                .with_header("Content-Range", "bytes 3-9/10"),
        ]);
        let url = Url::parse(&server.url()).expect("valid url");
        let cancellation = StreamCancellation::new();
        let mut reader =
            SeekableHttpReader::new_with_cancellation(&url, &cancellation).expect("open response");
        let request_count = server.requests().len();

        cancellation.cancel();
        let error = reader
            .seek(SeekFrom::Start(3))
            .expect_err("cancelled seek must fail");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<SeekableReadError>()),
            Some(&SeekableReadError::Cancelled)
        );
        assert_eq!(server.requests().len(), request_count);
    }

    /// `Cursor<Vec<u8>>` is the canonical `Read + Seek` implementation in
    /// the standard library; this assertion documents that the
    /// `ReadSeek` blanket impl really does cover the in-memory type used by
    /// the `StreamReader` adapter.
    #[test]
    fn read_seek_blanket_impl_covers_cursor() {
        fn assert_read_sek<T: Read + Seek + ?Sized>() {}
        assert_read_sek::<Cursor<Vec<u8>>>();
        assert_read_sek::<Cursor<&[u8]>>();
    }
}
