//! Generic HTTP/HTTPS stream provider.
//!
//! This is the workhorse that powers Icecast, SHOUTcast and any plain
//! HTTP/HTTPS radio that does not need platform-specific resolution. The
//! detection layer already labelled the URL as [`StreamKind::Http`], so
//! every such stream lands here.
//!
//! Metadata strategy (priority order, matching the spec):
//!
//! 1. The provider probes the URL with a HEAD request and pulls ICY headers
//!    (`icy-name`, `icy-genre`, `icy-br`) that Icecast/SHOUTcast servers
//!    attach to the response. The `StreamTitle` value that lives inside the
//!    audio payload itself is intentionally **not** read here: parsing the
//!    in-band metadata while the audio is playing requires sharing the
//!    transport with rodio, and the spec asks for the metadata to be
//!    available *when the stream is added*. The fallback chain downstream
//!    accepts the partial result.
//! 2. If HEAD is rejected (some servers return 405 for stream endpoints), the
//!    provider falls back to a body-bearing GET. Its untouched response is
//!    handed to playback, so this path does not issue a second GET.
//! 3. If neither yields a useful title, the resolver layer falls back to
//!    the URL hostname (or IP), as the spec mandates.
//!
//! The reader returned for playback is a [`crate::stream::seekable::SeekableHttpReader`]
//! wrapping the HTTP body. Forward seeks within the buffered region are
//! cheap; backward seeks reopen the connection with a `Range` request, so
//! symphonia can probe the format header (which requires `Seek`) without
//! forcing the streaming layer to buffer the whole body upfront.

use std::io::{self, Read};
use std::time::Instant;

use reqwest::blocking::{Client, RequestBuilder, Response};
use url::Url;

use crate::stream::provider::{
    PreparedStream, ResolvedStream, StreamCancellation, StreamError, StreamProvider, StreamReader,
};
use crate::stream::seekable::SeekableHttpReader;
use crate::stream::source::StreamKind;

/// Generic HTTP/HTTPS provider. It carries no mutable state; cloning is cheap.
#[derive(Clone)]
pub struct HttpProvider {
    _private: (),
}

impl std::fmt::Debug for HttpProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpProvider").finish_non_exhaustive()
    }
}

impl HttpProvider {
    /// Construct a provider ready to handle `http://` and `https://` URLs.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

struct PreparedHttpResponse {
    response: Response,
    final_url: Url,
}

impl crate::stream::provider::PreparedStreamHandle for PreparedHttpResponse {
    fn open(
        self: Box<Self>,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError> {
        open_response(self.response, self.final_url, cancellation)
    }
}

/// Header names treated as `icy-*` metadata. Kept lowercase because HTTP
/// headers are case-insensitive and the comparisons below rely on that.
const ICY_HEADERS: &[&str] = &["icy-name", "icy-genre", "icy-br", "icy-url"];

impl StreamProvider for HttpProvider {
    fn can_handle(&self, url: &Url) -> bool {
        is_valid_http_url(url)
    }

    fn kind(&self) -> StreamKind {
        StreamKind::Http
    }

    fn resolve(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<ResolvedStream, StreamError> {
        match probe_headers(url, cancellation) {
            Ok(probe) => {
                let HeaderProbe {
                    headers,
                    final_url,
                    reusable,
                } = probe;
                let mut resolved = resolved_from_headers(&headers);
                resolved.prepared = reusable.map(|response| {
                    PreparedStream::new(Box::new(PreparedHttpResponse {
                        response,
                        final_url,
                    }))
                });
                Ok(resolved)
            }
            Err(StreamError::Cancelled) => Err(StreamError::Cancelled),
            Err(error) => {
                // A failed probe is not a hard failure: the stream may still
                // play fine, we just lost the chance to enrich the title.
                // Surface a partial result so the resolver can fall back to
                // the hostname without aborting the whole flow.
                tracing::warn!(
                    "metadata probe failed for {}: {error}",
                    crate::net::safe_url(url)
                );
                Ok(ResolvedStream::default())
            }
        }
    }

    fn open_reader(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError> {
        validate_http_url(url)?;
        let response = get_response(url, cancellation)?;
        let final_url = response.url().clone();
        open_response(response, final_url, cancellation)
    }

    fn open_reader_with_prepared(
        &self,
        url: &Url,
        prepared: Option<&PreparedStream>,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError> {
        validate_http_url(url)?;
        if let Some(prepared) = prepared
            && let Some(result) = prepared.open(cancellation)
        {
            return result;
        }

        self.open_reader(url, cancellation)
    }
}

fn open_response(
    response: Response,
    final_url: Url,
    cancellation: &StreamCancellation,
) -> Result<StreamReader, StreamError> {
    // YouTube Live and most radio CDNs respond with an
    // `application/vnd.apple.mpegurl` (or `.m3u8` URL) manifest, not
    // raw audio. Hand it to the HLS demuxer instead of streaming the
    // playlist text straight into the decoder.
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let path_is_m3u8 = final_url
        .path()
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("m3u8"));
    if content_type.starts_with("application/vnd.apple.mpegurl") || path_is_m3u8 {
        let body = read_bounded_response(
            response,
            &final_url,
            cancellation,
            crate::stream::hls::MAX_HLS_MANIFEST_BYTES,
        )?;
        let hls_reader =
            crate::stream::hls::build_reader_from_body(&body, &final_url, cancellation)?;
        return crate::stream::hls::stream_reader_from_hls(hls_reader, &final_url, cancellation);
    }

    let seekable = SeekableHttpReader::from_response(response, final_url, cancellation)?;
    let content_length = seekable.content_length();
    StreamReader::seekable_with_content_length(Box::new(seekable), content_length)
        .progressive(cancellation)
}

pub(crate) fn is_valid_http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some_and(|host| !host.is_empty())
}

fn validate_http_url(url: &Url) -> Result<(), StreamError> {
    if is_valid_http_url(url) {
        Ok(())
    } else {
        Err(StreamError::Unsupported {
            url: url.clone(),
            message: "stream URL must use http/https and include a host".into(),
        })
    }
}

/// Execute one blocking request on the caller's thread with a short timeout
/// first, falling back to the normal streaming profile only when the short
/// header exchange timed out before cancellation was requested.
pub(crate) fn send_cancellable_request<F>(
    diagnostic_url: &Url,
    cancellation: &StreamCancellation,
    short_client: &Client,
    fallback_client: &Client,
    build_request: F,
) -> Result<Response, StreamError>
where
    F: Fn(&Client) -> RequestBuilder,
{
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    #[cfg(test)]
    let _active_request = cancellation.begin_http_request();

    let response = match build_request(short_client).send() {
        Err(error) if error.is_timeout() => {
            if cancellation.is_cancelled() {
                return Err(StreamError::Cancelled);
            }
            build_request(fallback_client).send()
        }
        result => result,
    };
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let response = response
        .and_then(|response| response.error_for_status())
        .map_err(|error| StreamError::Network {
            url: diagnostic_url.clone(),
            message: crate::net::normalize_diagnostic(&error.to_string(), 256),
        })?;
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    Ok(response)
}

/// Read a bounded finite response without handing the blocking body read to a
/// detached worker. The short client normally makes each read timeout a
/// cancellation polling point; a slower fallback response remains bounded by
/// the normal streaming read timeout.
pub(crate) fn read_bounded_response_cancellable(
    mut response: Response,
    diagnostic_url: &Url,
    cancellation: &StreamCancellation,
    limit: usize,
) -> Result<Vec<u8>, StreamError> {
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    #[cfg(test)]
    let _active_request = cancellation.begin_http_request();

    let declared_length = response.content_length();
    if let Some(actual) = declared_length
        && actual > limit as u64
    {
        return Err(StreamError::Network {
            url: diagnostic_url.clone(),
            message: format!(
                "response body declares {actual} bytes, exceeding the {limit} byte limit"
            ),
        });
    }

    let capacity = declared_length
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(limit)
        .min(limit);
    let mut body = Vec::with_capacity(capacity);
    let mut chunk = [0u8; 16 * 1024];
    let deadline = Instant::now() + crate::net::STREAM_READ_TIMEOUT;
    loop {
        if cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        let read = match response.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if is_timeout_error(&error) && Instant::now() < deadline => continue,
            Err(error) => {
                return Err(StreamError::Network {
                    url: diagnostic_url.clone(),
                    message: format!("response body read failed: {error}"),
                });
            }
        };
        if cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        if read == 0 {
            return Ok(body);
        }
        if body.len().saturating_add(read) > limit {
            return Err(StreamError::Network {
                url: diagnostic_url.clone(),
                message: format!("response body exceeds the {limit} byte limit"),
            });
        }
        body.extend_from_slice(&chunk[..read]);
    }
}

fn is_timeout_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|source| source.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout)
}

pub(crate) fn get_response(
    url: &Url,
    cancellation: &StreamCancellation,
) -> Result<reqwest::blocking::Response, StreamError> {
    let client = crate::net::shared_streaming_client().ok_or_else(|| {
        StreamError::Other("could not initialize the shared stream HTTP client".into())
    })?;
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let response = client
        .get(url.as_str())
        .header("Icy-MetaData", "0")
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|error| StreamError::Network {
            url: url.clone(),
            message: crate::net::normalize_diagnostic(&error.to_string(), 256),
        })?;
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    Ok(response)
}

pub(crate) fn read_bounded_response(
    response: reqwest::blocking::Response,
    url: &Url,
    cancellation: &StreamCancellation,
    limit: usize,
) -> Result<Vec<u8>, StreamError> {
    read_bounded_response_cancellable(response, url, cancellation, limit)
}

/// Run the lightweight header probe used to surface ICY metadata.
///
struct HeaderProbe {
    headers: std::collections::HashMap<String, String>,
    final_url: Url,
    reusable: Option<Response>,
}

/// Tries HEAD first because it does not transfer the body, falling back to a
/// body-bearing GET when the server rejects HEAD. The fallback response is
/// deliberately left unread so playback can consume it from byte zero.
fn probe_headers(url: &Url, cancellation: &StreamCancellation) -> Result<HeaderProbe, StreamError> {
    let short_client = crate::net::cancellable_streaming_client().ok_or_else(|| {
        StreamError::Other("could not initialize the cancellable stream HTTP client".to_string())
    })?;
    let fallback_client = crate::net::shared_streaming_client().ok_or_else(|| {
        StreamError::Other("could not initialize the shared stream HTTP client".to_string())
    })?;

    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let head_url = url.clone();
    let head = send_cancellable_request(
        url,
        cancellation,
        short_client,
        fallback_client,
        move |client| client.head(head_url.as_str()),
    );
    match head {
        Ok(response) => {
            if cancellation.is_cancelled() {
                return Err(StreamError::Cancelled);
            }
            let headers = headers_to_lower(response.headers());
            if cancellation.is_cancelled() {
                return Err(StreamError::Cancelled);
            }
            return Ok(HeaderProbe {
                final_url: response.url().clone(),
                headers,
                reusable: None,
            });
        }
        Err(StreamError::Cancelled) => return Err(StreamError::Cancelled),
        Err(_) => {}
    }

    // Some Icecast servers answer HEAD with 405; fall back to a full GET. Do
    // not add Range and do not consume the body: it becomes the playback
    // reader's initial response.
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let ranged_url = url.clone();
    let response = send_cancellable_request(
        url,
        cancellation,
        short_client,
        fallback_client,
        move |client| client.get(ranged_url.as_str()).header("Icy-MetaData", "0"),
    )?;
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let final_url = response.url().clone();
    let headers = headers_to_lower(response.headers());
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    Ok(HeaderProbe {
        headers,
        final_url,
        reusable: Some(response),
    })
}

/// Lower-case the header names so the rest of the probe can compare without
/// thinking about case. Values stay as the server wrote them.
fn headers_to_lower(
    headers: &reqwest::header::HeaderMap,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for (name, value) in headers.iter() {
        if let Ok(value) = value.to_str() {
            map.insert(name.as_str().to_ascii_lowercase(), value.to_string());
        }
    }
    map
}

/// Translate the probed headers into a [`ResolvedStream`].
///
/// Only the ICY fields the spec asks for are surfaced: name (station), genre
/// and bitrate. Other headers are ignored on purpose; the audio engine has
/// no use for them and the playlist row only displays a small subset.
fn resolved_from_headers(headers: &std::collections::HashMap<String, String>) -> ResolvedStream {
    let mut resolved = ResolvedStream::default();
    for name in ICY_HEADERS {
        if let Some(value) = headers.get(*name) {
            match *name {
                "icy-name" => resolved.station = Some(value.clone()),
                "icy-genre" => resolved.genre = Some(value.clone()),
                "icy-br" => resolved.bitrate = value.parse::<u32>().ok(),
                "icy-url" => {
                    if let Ok(url) = Url::parse(value) {
                        resolved.homepage = Some(url);
                    }
                }
                _ => {}
            }
        }
    }
    // ICY servers carry the station name; the title of a live radio is
    // whatever the broadcaster puts in the icy-name header. We mirror the
    // station into `title` so the playlist row already shows something
    // useful when no song-level metadata is available.
    if resolved.title.is_none() {
        resolved.title = resolved.station.clone();
    }
    resolved
}

/// Prepend the stream-kind label to a fallback title.
///
/// The fallback chain produces bare hostnames; prefixing the kind here keeps
/// the playlist row honest about why the title is just a host. Used by the
/// resolver, not by this provider directly.
pub fn fallback_title(kind: StreamKind, host: &str) -> String {
    format!("{kind} • {host}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ScriptedHttpResponse, ScriptedHttpServer};
    use std::io::Seek;
    use std::time::Duration;

    #[test]
    fn http_provider_accepts_http_and_https() {
        let provider = HttpProvider::new();
        assert!(provider.can_handle(&Url::parse("http://x/y").unwrap()));
        assert!(provider.can_handle(&Url::parse("https://x/y").unwrap()));
        assert!(!provider.can_handle(&Url::parse("ftp://x/y").unwrap()));
        assert!(!provider.can_handle(&Url::parse("data:audio/mpeg").unwrap()));
        assert_eq!(provider.kind(), StreamKind::Http);
    }

    #[test]
    fn open_reader_rejects_unsupported_or_hostless_urls_before_network_io() {
        let provider = HttpProvider::new();
        let cancellation = StreamCancellation::new();

        for url in [
            Url::parse("ftp://example.test/audio.mp3").unwrap(),
            Url::parse("data:audio/mpeg").unwrap(),
        ] {
            let error = provider
                .open_reader(&url, &cancellation)
                .expect_err("invalid URL");
            assert!(matches!(error, StreamError::Unsupported { .. }));
        }
    }

    #[test]
    fn resolved_from_headers_picks_icy_fields() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("icy-name".into(), "Radio Paradise".into());
        headers.insert("icy-genre".into(), "Eclectic".into());
        headers.insert("icy-br".into(), "192".into());
        headers.insert("icy-url".into(), "https://radio.example.com/".into());

        let resolved = resolved_from_headers(&headers);
        assert_eq!(resolved.station.as_deref(), Some("Radio Paradise"));
        assert_eq!(resolved.genre.as_deref(), Some("Eclectic"));
        assert_eq!(resolved.bitrate, Some(192));
        assert_eq!(
            resolved.homepage.as_ref().map(Url::as_str),
            Some("https://radio.example.com/")
        );
        // The title mirrors the station so the playlist row has something
        // meaningful without a song-level ICY update.
        assert_eq!(resolved.title.as_deref(), Some("Radio Paradise"));
    }

    #[test]
    fn resolved_from_headers_handles_missing_icy() {
        let headers = std::collections::HashMap::new();
        let resolved = resolved_from_headers(&headers);
        assert_eq!(resolved.title, None);
        assert_eq!(resolved.station, None);
        assert_eq!(resolved.bitrate, None);
    }

    #[test]
    fn probe_headers_reads_icy_metadata_from_a_loopback_server() {
        let server = ScriptedHttpServer::new([ScriptedHttpResponse::fixed(200, b"audio")
            .with_header("icy-name", "Fixture Radio")
            .with_header("icy-genre", "Test")
            .with_header("icy-br", "128")]);
        let url = Url::parse(&server.url()).expect("fixture URL");
        let cancellation = StreamCancellation::new();
        let probe = probe_headers(&url, &cancellation).expect("HEAD response");
        let resolved = resolved_from_headers(&probe.headers);

        assert_eq!(resolved.station.as_deref(), Some("Fixture Radio"));
        assert_eq!(resolved.genre.as_deref(), Some("Test"));
        assert_eq!(resolved.bitrate, Some(128));
    }

    #[test]
    fn probe_headers_retries_slow_headers_without_cancellation() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, b"audio")
                .with_header("icy-name", "Slow Fixture")
                .delay_headers(Duration::from_millis(100)),
            ScriptedHttpResponse::fixed(200, b"audio").with_header("icy-name", "Retry Fixture"),
        ]);
        let url = Url::parse(&server.url()).expect("fixture URL");
        let cancellation = StreamCancellation::new();
        let probe = probe_headers(&url, &cancellation).expect("retry response");
        assert_eq!(
            probe.headers.get("icy-name").map(String::as_str),
            Some("Retry Fixture")
        );
        assert_eq!(server.requests().len(), 2, "slow headers must be retried");
    }

    #[test]
    fn rejected_head_reuses_one_body_bearing_get_for_playback() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(405, b"head rejected"),
            ScriptedHttpResponse::fixed(200, b"prepared audio")
                .with_header("icy-name", "Prepared Radio"),
        ]);
        let url = Url::parse(&server.url()).expect("fixture URL");
        let cancellation = StreamCancellation::new();
        let provider = HttpProvider::new();

        let resolved = provider.resolve(&url, &cancellation).expect("resolve");
        let mut reader = provider
            .open_reader_with_prepared(&url, resolved.prepared.as_ref(), &cancellation)
            .expect("open prepared response");
        let mut body = Vec::new();
        reader.read_to_end(&mut body).expect("read prepared body");

        assert_eq!(body, b"prepared audio");
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "HEAD plus one reusable GET");
        assert!(requests[0].starts_with("HEAD "));
        assert!(requests[1].starts_with("GET "));
        assert!(!requests[1].to_ascii_lowercase().contains("range:"));
    }

    #[test]
    fn successful_head_keeps_the_existing_single_playback_open() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, b"head body").with_header("icy-name", "Head Radio"),
            ScriptedHttpResponse::fixed(200, b"playback audio"),
        ]);
        let url = Url::parse(&server.url()).expect("fixture URL");
        let cancellation = StreamCancellation::new();
        let provider = HttpProvider::new();

        let resolved = provider.resolve(&url, &cancellation).expect("resolve");
        assert!(resolved.prepared.is_none(), "HEAD has no reusable body");
        let mut reader = provider
            .open_reader_with_prepared(&url, None, &cancellation)
            .expect("open playback response");
        let mut body = Vec::new();
        reader.read_to_end(&mut body).expect("read playback body");

        assert_eq!(body, b"playback audio");
        assert_eq!(server.requests().len(), 2, "HEAD plus one playback GET");
    }

    #[test]
    fn prepared_response_uses_final_redirect_url_for_backward_ranges() {
        let server = ScriptedHttpServer::new([]);
        let final_url = server.endpoint("media/live");
        server.push_response(ScriptedHttpResponse::fixed(405, b"head rejected"));
        server.push_response(
            ScriptedHttpResponse::fixed(302, b"").with_header("Location", final_url.clone()),
        );
        server.push_response(ScriptedHttpResponse::fixed(200, b"abcdefghij"));
        server.push_response(
            ScriptedHttpResponse::fixed(206, b"defghij")
                .with_header("Content-Range", "bytes 3-9/10"),
        );

        let url = Url::parse(&server.url()).expect("fixture URL");
        let cancellation = StreamCancellation::new();
        let provider = HttpProvider::new();
        let resolved = provider.resolve(&url, &cancellation).expect("resolve");
        let mut reader = provider
            .open_reader_with_prepared(&url, resolved.prepared.as_ref(), &cancellation)
            .expect("open prepared response");
        assert_eq!(reader.content_length(), Some(10));

        let mut initial = [0u8; 8];
        reader.read_exact(&mut initial).expect("read initial body");
        assert_eq!(&initial, b"abcdefgh");
        reader
            .seek(std::io::SeekFrom::Start(3))
            .expect("backward seek");
        let mut suffix = [0u8; 4];
        reader.read_exact(&mut suffix).expect("read ranged body");
        assert_eq!(&suffix, b"defg");

        let requests = server.requests();
        assert_eq!(requests.len(), 4, "HEAD, redirected GET, and one Range GET");
        assert!(requests[2].contains("GET /media/live "));
        assert!(requests[3].contains("GET /media/live "));
        assert!(requests[3].to_ascii_lowercase().contains("range: bytes=3-"));
    }

    #[test]
    fn prepared_response_preserves_cancellable_seekable_reads() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(405, b"head rejected"),
            ScriptedHttpResponse::chunked(
                200,
                [b"first".as_slice(), b"second".as_slice()],
                Duration::from_millis(250),
            ),
        ]);
        let url = Url::parse(&server.url()).expect("fixture URL");
        let cancellation = StreamCancellation::new();
        let provider = HttpProvider::new();
        let resolved = provider.resolve(&url, &cancellation).expect("resolve");
        let mut reader = provider
            .open_reader_with_prepared(&url, resolved.prepared.as_ref(), &cancellation)
            .expect("open prepared response");

        let mut first = [0u8; 5];
        reader.read_exact(&mut first).expect("read first chunk");
        assert_eq!(&first, b"first");
        let handle = std::thread::spawn(move || {
            let mut second = [0u8; 6];
            reader
                .read(&mut second)
                .expect_err("read must be cancelled")
        });
        std::thread::sleep(Duration::from_millis(25));
        cancellation.cancel();

        let error = handle.join().expect("reader thread must join");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            error.get_ref().and_then(
                |source| source.downcast_ref::<crate::stream::seekable::SeekableReadError>()
            ),
            Some(&crate::stream::seekable::SeekableReadError::Cancelled)
        );
    }

    #[test]
    fn resolve_cancellation_interrupts_a_delayed_header_probe() {
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, b"audio").delay_headers(Duration::from_secs(1))
        ]);
        let url = Url::parse(&server.url()).expect("fixture URL");
        let cancellation = StreamCancellation::new();
        let cancellation_for_worker = cancellation.clone();
        let worker =
            std::thread::spawn(move || HttpProvider::new().resolve(&url, &cancellation_for_worker));

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while server.requests().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(server.requests().len(), 1, "probe request must start");
        cancellation.cancel();

        let error = worker
            .join()
            .expect("probe worker must join")
            .expect_err("cancelled");
        assert!(matches!(error, StreamError::Cancelled));
        assert_eq!(cancellation.active_http_requests(), 0);
    }

    #[test]
    fn fallback_title_includes_the_kind() {
        assert_eq!(
            fallback_title(StreamKind::Http, "stream.example.com"),
            "Stream • stream.example.com"
        );
    }

    /// `BufRead::fill_buf` smoke test for the wrapped reader; ensures the
    /// reader is not dropped between construction and use.
    #[test]
    fn stream_reader_wrapper_returns_bytes() {
        let mut reader = StreamReader::buffered(std::io::Cursor::new(b"abc".to_vec()));
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut reader, &mut buf).unwrap();
        assert_eq!(buf, "abc");
    }

    #[test]
    fn live_hls_open_reader_returns_incremental_seekable_stream() {
        let manifest = b"#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:2\n\
             #EXT-X-MEDIA-SEQUENCE:1000\n\
             #EXTINF:2.0,\n\
             youtube-segment-1000.ts\n";
        let server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, manifest)
                .with_header("Content-Type", "application/vnd.apple.mpegurl"),
            ScriptedHttpResponse::fixed(200, b"initial live audio"),
        ]);
        let url = Url::parse(&server.endpoint("youtube-live.m3u8")).expect("fixture URL");
        let started = std::time::Instant::now();
        let cancellation = StreamCancellation::new();
        let reader = HttpProvider::new()
            .open_reader(&url, &cancellation)
            .expect("open live HLS reader");
        let mut reader = reader;
        assert_eq!(reader.content_length(), None);
        let mut bytes = [0u8; 18];
        std::io::Read::read_exact(&mut reader, &mut bytes).expect("read initial live audio");

        assert_eq!(&bytes, b"initial live audio");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "open_reader must not drain a live playlist to EOF"
        );
        assert_eq!(server.requests().len(), 2);
    }
}
