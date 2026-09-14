//! Small blocking-network helpers shared by remote providers.

use std::io::{self, Read};
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::blocking::Client;
use thiserror::Error;
use url::Url;

/// Maximum number of redirects followed by an HTTP boundary.
pub(crate) const MAX_HTTP_REDIRECT_HOPS: usize = 5;
/// Connection deadline for streaming HTTP clients.
pub(crate) const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// Inactivity deadline for one streaming read operation.
pub(crate) const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(8);
/// Poll interval used by seekable readers while waiting for the next body
/// chunk. A short reqwest operation timeout lets them observe cancellation
/// without introducing a worker thread around every blocking read.
pub(crate) const STREAM_CANCELLATION_POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy)]
enum RedirectRejection {
    InvalidTarget,
    HttpsDowngrade,
    TooManyHops,
}

impl std::fmt::Display for RedirectRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTarget => f.write_str("redirect target is not an HTTP(S) URL"),
            Self::HttpsDowngrade => f.write_str("HTTPS to HTTP redirects are not allowed"),
            Self::TooManyHops => f.write_str("redirect hop limit exceeded"),
        }
    }
}

impl std::error::Error for RedirectRejection {}

/// Check the safety rules shared by every remote HTTP profile.
///
/// Host changes are intentionally allowed: legitimate radio streams commonly
/// redirect to a CDN or a different station host. HTTPS downgrade is checked
/// against the complete chain so `https -> http -> https -> http` is rejected.
pub(crate) fn redirect_is_allowed(previous: &[Url], next: &Url) -> bool {
    if previous.len() > MAX_HTTP_REDIRECT_HOPS {
        return false;
    }
    if !matches!(next.scheme(), "http" | "https")
        || next.host_str().is_none_or(|host| host.is_empty())
    {
        return false;
    }
    !(next.scheme() == "http" && previous.iter().any(|url| url.scheme() == "https"))
}

/// Build the explicit redirect policy used by all production HTTP profiles.
pub(crate) fn secure_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if redirect_is_allowed(attempt.previous(), attempt.url()) {
            attempt.follow()
        } else if attempt.previous().len() > MAX_HTTP_REDIRECT_HOPS {
            attempt.error(RedirectRejection::TooManyHops)
        } else if attempt.url().scheme() == "http"
            && attempt.previous().iter().any(|url| url.scheme() == "https")
        {
            attempt.error(RedirectRejection::HttpsDowngrade)
        } else {
            attempt.error(RedirectRejection::InvalidTarget)
        }
    })
}

/// Build the blocking client profile used by long-lived stream readers.
///
/// Reqwest 0.12's blocking builder calls this timeout the operation timeout:
/// it bounds each connect/read/write operation and resets after progress,
/// rather than imposing a total body deadline. That is the blocking equivalent
/// of an inactivity timeout for a live stream; finite-body providers use the
/// separate finite-body profile below and enforce their byte bounds separately.
pub(crate) fn build_streaming_client(
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<Client, reqwest::Error> {
    Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(read_timeout)
        .redirect(secure_redirect_policy())
        .build()
}

/// Build a finite-body client profile with an operation timeout and optional
/// provider-specific user agent.
///
/// Blocking reqwest applies `.timeout()` to each connect/read/write operation;
/// it does not impose a total elapsed-time deadline while a caller consumes a
/// body through `Read`. The bounded-body helpers enforce the finite byte limit;
/// this client profile does not enforce a total body-read deadline.
pub(crate) fn build_finite_client(
    timeout: Duration,
    user_agent: Option<&str>,
) -> Result<Client, reqwest::Error> {
    let builder = Client::builder()
        .timeout(timeout)
        .redirect(secure_redirect_policy());
    let builder = match user_agent {
        Some(user_agent) => builder.user_agent(user_agent),
        None => builder,
    };
    builder.build()
}

/// Lazily construct and reuse the streaming profile for every stream/HLS
/// request. `Client` is cloneable and internally pools connections; keeping a
/// single instance avoids discarding that pool for every seek or segment.
pub(crate) fn shared_streaming_client() -> Option<&'static Client> {
    static CLIENT: OnceLock<Option<Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            build_streaming_client(STREAM_CONNECT_TIMEOUT, STREAM_READ_TIMEOUT)
                .map_err(|error| tracing::warn!("stream HTTP client build failed: {error}"))
                .ok()
        })
        .as_ref()
}

/// Lazily construct the short-operation-timeout profile used by cancellable
/// seekable readers. The response body remains owned by the reader, so no
/// detached operation is needed to make a blocked read interruptible. A slow
/// response-header exchange is retried through the normal streaming profile by
/// the seekable reader before the response is handed to the short-timeout body
/// path.
pub(crate) fn cancellable_streaming_client() -> Option<&'static Client> {
    static CLIENT: OnceLock<Option<Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .connect_timeout(STREAM_CONNECT_TIMEOUT)
                .timeout(STREAM_CANCELLATION_POLL)
                .redirect(secure_redirect_policy())
                .build()
                .map_err(|error| tracing::warn!("cancellable stream client build failed: {error}"))
                .ok()
        })
        .as_ref()
}

/// Failure raised when a response cannot be safely materialized.
#[derive(Debug, Error)]
pub(crate) enum BoundedBodyError {
    /// The response advertised more bytes than the caller allows.
    #[error("response body declares {actual} bytes, exceeding the {limit} byte limit")]
    DeclaredTooLarge { actual: u64, limit: usize },
    /// The body exceeded the limit while it was being read.
    #[error("response body exceeds the {limit} byte limit")]
    TooLarge { limit: usize },
    /// The underlying blocking response could not be read.
    #[error("response body read failed: {0}")]
    Read(#[from] io::Error),
}

/// Read a blocking reqwest response without materializing more than `limit`
/// bytes. A declared oversized body is rejected before the first body read.
pub(crate) fn read_bounded_response(
    response: &mut reqwest::blocking::Response,
    limit: usize,
) -> Result<Vec<u8>, BoundedBodyError> {
    let declared_length = response.content_length();
    read_bounded_body(response, declared_length, limit)
}

/// Read any blocking byte source with the same deterministic bound used for
/// HTTP responses. The optional declared length is checked before reading.
pub(crate) fn read_bounded_body(
    reader: &mut impl Read,
    declared_length: Option<u64>,
    limit: usize,
) -> Result<Vec<u8>, BoundedBodyError> {
    if let Some(actual) = declared_length
        && actual > limit as u64
    {
        return Err(BoundedBodyError::DeclaredTooLarge { actual, limit });
    }

    let capacity = declared_length
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(limit)
        .min(limit);
    let mut body = Vec::with_capacity(capacity);
    let read_limit = (limit as u64).saturating_add(1);
    reader.take(read_limit).read_to_end(&mut body)?;
    if body.len() > limit {
        return Err(BoundedBodyError::TooLarge { limit });
    }
    Ok(body)
}

/// Return a URL suitable for logs, errors, notifications, and other
/// diagnostics. Query parameters and fragments are deliberately removed so
/// signed URLs and tokens cannot escape through a diagnostic path.
pub(crate) fn safe_url(url: &Url) -> String {
    if url.host_str().is_none() {
        return format!("{}:", url.scheme());
    }
    let mut safe = url.clone();
    let _ = safe.set_username("");
    let _ = safe.set_password(None);
    safe.set_query(None);
    safe.set_fragment(None);
    safe.to_string()
}

/// Sanitize a string that may be either a local path or a stream URL.
pub(crate) fn safe_location(location: &str) -> String {
    Url::parse(location)
        .map(|url| safe_url(&url))
        .unwrap_or_else(|_| location.to_string())
}

/// Collapse external diagnostic text and cap it before it reaches a user or
/// log sink. Absolute HTTP(S) URLs in the text are sanitized as well.
pub(crate) fn normalize_diagnostic(text: &str, max_chars: usize) -> String {
    text.split_whitespace()
        .map(redact_sensitive_token)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn redact_sensitive_token(token: &str) -> String {
    let sanitized_url = redact_url_token(token);
    if sanitized_url != token {
        return sanitized_url;
    }

    let lower = token.to_ascii_lowercase();
    for marker in ["access_token=", "api_key=", "apikey=", "token="] {
        if let Some(start) = lower.find(marker) {
            let value_start = start + marker.len();
            let value_end = token[value_start..]
                .find([',', ';', ')', ']', '}'])
                .map_or(token.len(), |offset| value_start + offset);
            return format!(
                "{}{}<redacted>{}",
                &token[..start],
                &token[start..value_start],
                &token[value_end..]
            );
        }
    }
    token.to_string()
}

fn redact_url_token(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    let Some(start) = ["http://", "https://"]
        .into_iter()
        .filter_map(|scheme| lower.find(scheme))
        .min()
    else {
        return token.to_string();
    };
    let (prefix, candidate) = token.split_at(start);
    let trimmed = candidate.trim_end_matches(|character: char| {
        matches!(
            character,
            ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
        )
    });
    let suffix = &candidate[trimmed.len()..];
    match Url::parse(trimmed) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => {
            format!("{prefix}{}{suffix}", safe_url(&url))
        }
        _ => token.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::test_support::{ScriptedHttpResponse, ScriptedHttpServer};

    fn local_response(
        body: &[u8],
        content_length: Option<usize>,
    ) -> (ScriptedHttpServer, reqwest::blocking::Response) {
        let response = match content_length {
            Some(length) => ScriptedHttpResponse::fixed(200, body)
                .with_header("Content-Length", length.to_string()),
            None => ScriptedHttpResponse::chunked(200, [body.to_vec()], std::time::Duration::ZERO),
        };
        let server = ScriptedHttpServer::new([response]);
        let http_response = reqwest::blocking::Client::new()
            .get(server.endpoint("payload"))
            .send()
            .expect("request local response");
        (server, http_response)
    }

    #[test]
    fn bounded_response_reads_a_normal_local_body() {
        let (_server, mut response) = local_response(b"hello", Some(5));
        assert_eq!(
            read_bounded_response(&mut response, 5).expect("body fits"),
            b"hello"
        );
    }

    #[test]
    fn bounded_response_rejects_a_declared_oversized_local_body() {
        let (_server, mut response) = local_response(b"hello", Some(5));
        let error = read_bounded_response(&mut response, 4).expect_err("body is oversized");
        assert!(matches!(
            error,
            BoundedBodyError::DeclaredTooLarge {
                actual: 5,
                limit: 4
            }
        ));
    }

    #[test]
    fn bounded_response_rejects_a_chunked_oversized_local_body() {
        let (_server, mut response) = local_response(b"hello", None);
        let error = read_bounded_response(&mut response, 4).expect_err("body is oversized");
        assert!(matches!(error, BoundedBodyError::TooLarge { limit: 4 }));
    }

    #[test]
    fn safe_url_removes_credentials_query_and_fragment() {
        let url = Url::parse("https://user:password@example.com/live?token=secret#part")
            .expect("valid URL");
        assert_eq!(safe_url(&url), "https://example.com/live");
    }

    #[test]
    fn diagnostic_text_redacts_urls_and_truncates() {
        let text = "failed https://example.com/live?token=secret#fragment with details";
        let normalized = normalize_diagnostic(text, 36);
        assert!(!normalized.contains("secret"));
        assert!(normalized.starts_with("failed https://example.com/live"));
        assert!(normalized.chars().count() <= 36);
    }

    #[test]
    fn diagnostic_text_redacts_bare_token_assignments() {
        let normalized = normalize_diagnostic("resolver token=secret123 api_key=another", 256);

        assert_eq!(normalized, "resolver token=<redacted> api_key=<redacted>");
        assert!(!normalized.contains("secret123"));
        assert!(!normalized.contains("another"));
    }

    #[test]
    fn shared_streaming_client_reuses_one_profile_instance() {
        let first = shared_streaming_client().expect("streaming client");
        let second = shared_streaming_client().expect("streaming client");
        assert!(std::ptr::eq(first, second));
    }

    #[test]
    fn streaming_profile_allows_a_body_longer_than_its_operation_timeout() {
        let timeout = Duration::from_millis(40);
        let inter_read_gap = Duration::from_millis(15);
        let server = ScriptedHttpServer::new([ScriptedHttpResponse::chunked(
            200,
            [
                b"a".as_slice(),
                b"b".as_slice(),
                b"c".as_slice(),
                b"d".as_slice(),
            ],
            inter_read_gap,
        )]);
        let client =
            build_streaming_client(Duration::from_secs(1), timeout).expect("streaming test client");
        let mut response = client
            .get(server.endpoint("slow"))
            .send()
            .expect("response headers");
        let started = std::time::Instant::now();
        let mut body = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let read = response
                .read(&mut byte)
                .expect("each read stays within timeout");
            if read == 0 {
                break;
            }
            body.extend_from_slice(&byte[..read]);
        }

        assert_eq!(body, b"abcd");
        assert!(
            started.elapsed() > timeout,
            "the body must take longer than one configured operation timeout"
        );
    }

    #[test]
    fn streaming_profile_rejects_a_gap_longer_than_its_operation_timeout() {
        let timeout = Duration::from_millis(25);
        let server = ScriptedHttpServer::new([ScriptedHttpResponse::chunked(
            200,
            [b"a".as_slice(), b"b".as_slice()],
            Duration::from_millis(60),
        )]);
        let client =
            build_streaming_client(Duration::from_secs(1), timeout).expect("streaming test client");
        let mut response = client
            .get(server.endpoint("slow"))
            .send()
            .expect("response headers");
        let mut byte = [0u8; 1];

        assert_eq!(response.read(&mut byte).expect("first chunk"), 1);
        let error = response
            .read(&mut byte)
            .expect_err("a gap longer than the operation timeout must fail");
        let timeout_error = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout);
        assert!(timeout_error, "expected a timeout error, got {error:?}");
    }

    #[test]
    fn redirect_policy_allows_cross_host_http_redirects() {
        let previous = [Url::parse("http://radio.example/live").unwrap()];
        let next = Url::parse("http://cdn.example/live").unwrap();
        assert!(redirect_is_allowed(&previous, &next));
    }

    #[test]
    fn redirect_policy_rejects_https_downgrades_and_invalid_targets() {
        let previous = [Url::parse("https://radio.example/live").unwrap()];
        assert!(!redirect_is_allowed(
            &previous,
            &Url::parse("http://cdn.example/live").unwrap()
        ));
        assert!(!redirect_is_allowed(
            &previous,
            &Url::parse("file:///tmp/live").unwrap()
        ));
    }

    #[test]
    fn redirect_policy_rejects_a_chain_after_the_hop_limit() {
        let mut previous = Vec::new();
        for index in 0..=MAX_HTTP_REDIRECT_HOPS {
            previous.push(Url::parse(&format!("http://radio.example/{index}")).unwrap());
        }
        assert!(!redirect_is_allowed(
            &previous,
            &Url::parse("http://cdn.example/live").unwrap()
        ));
    }

    #[test]
    fn finite_profile_enforces_the_redirect_hop_limit() {
        let server = ScriptedHttpServer::new(std::iter::empty());
        let location = server.endpoint("next");
        for _ in 0..=MAX_HTTP_REDIRECT_HOPS {
            server.push_response(
                ScriptedHttpResponse::fixed(302, Vec::new())
                    .with_header("Location", location.clone()),
            );
        }
        let client = build_finite_client(Duration::from_secs(1), None).expect("finite client");
        let error = client
            .get(server.endpoint("start"))
            .send()
            .expect_err("redirect chain must be rejected");
        assert!(error.to_string().contains("redirect"));
        assert_eq!(server.requests().len(), MAX_HTTP_REDIRECT_HOPS + 1);
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 48,
            failure_persistence: None,
            max_shrink_iters: 128,
            rng_algorithm: proptest::test_runner::RngAlgorithm::ChaCha,
            rng_seed: proptest::test_runner::RngSeed::Fixed(0x4933_3005),
            .. ProptestConfig::default()
        })]

        #[test]
        fn bounded_reader_never_returns_more_than_its_limit(
            bytes in prop::collection::vec(any::<u8>(), 0..=2048),
            limit in 0usize..=1024,
            declared_length in prop::option::of(0u64..=2048),
        ) {
            let mut reader = io::Cursor::new(bytes.clone());
            let result = read_bounded_body(&mut reader, declared_length, limit);

            if declared_length.is_some_and(|declared| declared > limit as u64) {
                let rejected_declared_size =
                    matches!(result, Err(BoundedBodyError::DeclaredTooLarge { .. }));
                prop_assert!(rejected_declared_size);
            } else if bytes.len() > limit {
                let rejected_body_size = matches!(result, Err(BoundedBodyError::TooLarge { .. }));
                prop_assert!(rejected_body_size);
            } else {
                prop_assert_eq!(result.expect("body fits"), bytes);
            }
        }

        #[test]
        fn diagnostic_normalization_is_bounded_for_arbitrary_text(
            text in prop::collection::vec(any::<char>(), 0..=512)
                .prop_map(|characters| characters.into_iter().collect::<String>()),
            limit in 0usize..=256,
        ) {
            let normalized = normalize_diagnostic(&text, limit);
            prop_assert!(normalized.chars().count() <= limit);
        }
    }
}
