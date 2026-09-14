//! Radio Browser provider.
//!
//! Radio Browser is a community-maintained directory of radio stations with
//! a public, no-auth JSON API. The provider has two responsibilities:
//!
//! 1. **Resolve a station URL.** When the user pastes a `radio-browser.info`
//!    page (the human-facing site) the provider queries the API with the
//!    station UUID embedded in the path and pulls the direct stream URL
//!    from the JSON record.
//! 2. **Enrich with directory metadata.** Radio Browser hosts can be resolved
//!    through the directory API and return station metadata such as name,
//!    country, language, codec, bitrate and logo. Generic stream URLs stay on
//!    the generic HTTP provider and are never sent to the public directory.
//!
//! The directory base URL is configurable in one place so tests can swap it
//! out for a fixture server. The public deployment at `de1.api.radio-browser.info`
//! is the default; mirror deployments work without code changes.

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::blocking::Client;
use url::Url;

use crate::stream::provider::{
    ResolvedStream, StreamCancellation, StreamError, StreamProvider, StreamReader,
};
use crate::stream::source::StreamKind;
use crate::stream::url_detect::is_radio_browser_host;

/// Public Radio Browser directory base URL.
///
/// The API exposes the same shape on every mirror; pinning the default here
/// keeps the network code honest about which deployment it talks to.
const DEFAULT_API_BASE: &str = "https://de1.api.radio-browser.info";
/// Radio Browser station records are small JSON documents.
const MAX_RADIO_BROWSER_BODY_BYTES: usize = 1024 * 1024;
const RADIO_BROWSER_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const RADIO_BROWSER_USER_AGENT: &str = "harmonium/0.1";

/// Resolves Radio Browser station URLs and stream URLs through the public
/// directory API.
#[derive(Debug, Clone)]
pub struct RadioBrowserProvider {
    /// Base URL of the directory API; injectable for tests.
    api_base: Url,
}

impl Default for RadioBrowserProvider {
    fn default() -> Self {
        Self {
            api_base: Url::parse(DEFAULT_API_BASE).expect("valid default base URL"),
        }
    }
}

impl RadioBrowserProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the API base URL. Used by tests and by users who want to
    /// pin a specific mirror.
    pub fn with_api_base(base: Url) -> Self {
        Self { api_base: base }
    }
}

impl StreamProvider for RadioBrowserProvider {
    fn can_handle(&self, url: &Url) -> bool {
        url.host_str()
            .map(|host| is_radio_browser_host(&host.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    fn kind(&self) -> StreamKind {
        StreamKind::RadioBrowser
    }

    fn resolve(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<ResolvedStream, StreamError> {
        if cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        if !self.can_handle(url) {
            return Err(StreamError::Unsupported {
                url: url.clone(),
                message: "Radio Browser provider only handles Radio Browser hosts".into(),
            });
        }
        // Two entry points:
        // - A radio-browser.info page: parse the station UUID and query the
        //   API for the canonical record.
        let record = lookup_station_by_url(url, &self.api_base, cancellation)?;
        if cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        record.ok_or_else(|| StreamError::Unsupported {
            url: url.clone(),
            message: "Radio Browser returned no matching station".into(),
        })
    }

    fn open_reader(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError> {
        // For a radio-browser.info page the actual stream URL lives inside
        // the JSON record; resolve the page into the direct stream URL and
        // hand it to the generic HTTP reader.
        if url
            .host_str()
            .map(|h| is_radio_browser_host(&h.to_ascii_lowercase()))
            .unwrap_or(false)
        {
            let record =
                lookup_station_record(url, &self.api_base, cancellation)?.ok_or_else(|| {
                    StreamError::Unsupported {
                        url: url.clone(),
                        message: "Radio Browser returned no matching station".into(),
                    }
                })?;
            let stream_url = parse_resolved_stream_url(&record.url_resolved, url)?;
            return crate::stream::http::HttpProvider::new().open_reader(&stream_url, cancellation);
        }
        Err(StreamError::Unsupported {
            url: url.clone(),
            message: "Radio Browser provider only handles Radio Browser hosts".into(),
        })
    }
}

fn parse_resolved_stream_url(value: &str, original: &Url) -> Result<Url, StreamError> {
    let stream_url = Url::parse(value).map_err(|error| StreamError::Unsupported {
        url: original.clone(),
        message: format!("station record carried an invalid stream URL: {error}"),
    })?;
    if !crate::stream::http::is_valid_http_url(&stream_url) {
        return Err(StreamError::Unsupported {
            url: original.clone(),
            message: "station record stream URL must use http/https and include a host".into(),
        });
    }
    Ok(stream_url)
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
struct StationRecord {
    #[serde(default)]
    name: Option<String>,
    /// Resolved stream URL after Radio Browser's own redirect resolution.
    /// This is the direct playback URL returned by the station API.
    #[serde(default)]
    url_resolved: String,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    favicon: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    tags: Option<String>,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    bitrate: Option<u32>,
}

impl StationRecord {
    /// Translate the raw record into a [`ResolvedStream`] the rest of the
    /// resolver pipeline can merge with fallback values.
    fn into_resolved(self) -> ResolvedStream {
        ResolvedStream {
            title: self.name.clone().filter(|s| !s.is_empty()),
            station: self.name.filter(|s| !s.is_empty()),
            homepage: self.homepage.as_deref().and_then(|s| Url::parse(s).ok()),
            logo_url: self.favicon.as_deref().and_then(|s| Url::parse(s).ok()),
            country: self.country,
            language: self.language,
            genre: self.tags,
            codec: self.codec,
            bitrate: self.bitrate,
            ..ResolvedStream::default()
        }
    }
}

/// Fetch a single station record by its directory URL, returning the raw
/// record so the playback path can pull the direct stream URL out of it.
fn lookup_station_record(
    url: &Url,
    api_base: &Url,
    cancellation: &StreamCancellation,
) -> Result<Option<StationRecord>, StreamError> {
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let station_id = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
        .map(|s| s.to_string());
    let Some(station_id) = station_id else {
        return Ok(None);
    };
    let endpoint = api_base
        .join(&format!("json/stations/byuuid/{station_id}"))
        .map_err(|error| StreamError::Network {
            url: url.clone(),
            message: format!("could not build station lookup URL: {error}"),
        })?;
    fetch_one_record(&endpoint, url, cancellation)
}

/// Fetch a single station record by its directory URL.
fn lookup_station_by_url(
    url: &Url,
    api_base: &Url,
    cancellation: &StreamCancellation,
) -> Result<Option<ResolvedStream>, StreamError> {
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let station_id = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
        .map(|s| s.to_string());
    let Some(station_id) = station_id else {
        return Ok(None);
    };
    let endpoint = api_base
        .join(&format!("json/stations/byuuid/{station_id}"))
        .map_err(|error| StreamError::Network {
            url: url.clone(),
            message: format!("could not build station lookup URL: {error}"),
        })?;
    fetch_one(&endpoint, url, cancellation)
}

fn fetch_one(
    endpoint: &Url,
    original: &Url,
    cancellation: &StreamCancellation,
) -> Result<Option<ResolvedStream>, StreamError> {
    Ok(fetch_station_list(endpoint, original, cancellation)?
        .into_iter()
        .next()
        .map(StationRecord::into_resolved))
}

fn fetch_one_record(
    endpoint: &Url,
    original: &Url,
    cancellation: &StreamCancellation,
) -> Result<Option<StationRecord>, StreamError> {
    Ok(fetch_station_list(endpoint, original, cancellation)?
        .into_iter()
        .next())
}

/// Decode the official station-list wire shape and select the first record
/// returned by the exact `byuuid` endpoint. The endpoint always returns an
/// array, including when the UUID identifies one station.
fn fetch_station_list(
    endpoint: &Url,
    original: &Url,
    cancellation: &StreamCancellation,
) -> Result<Vec<StationRecord>, StreamError> {
    let body = http_get(endpoint, original, cancellation)?;
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let records = serde_json::from_str(&body).map_err(|error| StreamError::Network {
        url: original.clone(),
        message: format!("Radio Browser reply was not a station list: {error}"),
    })?;
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    Ok(records)
}

fn http_get(
    endpoint: &Url,
    original: &Url,
    cancellation: &StreamCancellation,
) -> Result<String, StreamError> {
    let client = shared_api_client().ok_or_else(|| {
        StreamError::Other("could not initialize the shared Radio Browser HTTP client".into())
    })?;
    let cancellable_client = shared_cancellable_api_client().ok_or_else(|| {
        StreamError::Other("could not initialize the cancellable Radio Browser HTTP client".into())
    })?;
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let endpoint = endpoint.clone();
    let original = original.clone();
    let response = crate::stream::http::send_cancellable_request(
        &original,
        cancellation,
        cancellable_client,
        client,
        move |client| client.get(endpoint.as_str()),
    )?;
    let body = crate::stream::http::read_bounded_response_cancellable(
        response,
        &original,
        cancellation,
        MAX_RADIO_BROWSER_BODY_BYTES,
    )?;
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    let body = String::from_utf8(body).map_err(|error| StreamError::Network {
        url: original.clone(),
        message: format!("Radio Browser response was not valid UTF-8: {error}"),
    })?;
    if cancellation.is_cancelled() {
        return Err(StreamError::Cancelled);
    }
    Ok(body)
}

fn shared_api_client() -> Option<&'static Client> {
    static CLIENT: OnceLock<Option<Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            crate::net::build_finite_client(
                RADIO_BROWSER_REQUEST_TIMEOUT,
                Some(RADIO_BROWSER_USER_AGENT),
            )
            .map_err(|error| tracing::warn!("Radio Browser HTTP client build failed: {error}"))
            .ok()
        })
        .as_ref()
}

fn shared_cancellable_api_client() -> Option<&'static Client> {
    static CLIENT: OnceLock<Option<Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .connect_timeout(crate::net::STREAM_CONNECT_TIMEOUT)
                .timeout(crate::net::STREAM_CANCELLATION_POLL)
                .redirect(crate::net::secure_redirect_policy())
                .user_agent(RADIO_BROWSER_USER_AGENT)
                .build()
                .map_err(|error| {
                    tracing::warn!("cancellable Radio Browser client build failed: {error}")
                })
                .ok()
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radio_browser_provider_accepts_directory_pages() {
        let provider = RadioBrowserProvider::new();
        assert!(provider.can_handle(&Url::parse("https://www.radio-browser.info/abc").unwrap()));
        assert!(provider.can_handle(
            &Url::parse("https://de1.api.radio-browser.info/json/stations/search").unwrap()
        ));
        assert!(!provider.can_handle(&Url::parse("https://radio.example.com/live").unwrap()));
        assert_eq!(provider.kind(), StreamKind::RadioBrowser);
    }

    #[test]
    fn radio_browser_api_client_reuses_its_finite_profile() {
        let first = shared_api_client().expect("Radio Browser client");
        let second = shared_api_client().expect("Radio Browser client");
        assert!(std::ptr::eq(first, second));
    }

    #[test]
    fn generic_http_resolution_is_rejected_without_directory_access() {
        let provider = RadioBrowserProvider::new();
        let url = Url::parse("https://radio.example.com/live").unwrap();

        let error = provider
            .resolve(&url, &StreamCancellation::new())
            .expect_err("generic HTTP is not Radio Browser");

        assert!(matches!(error, StreamError::Unsupported { .. }));
    }

    #[test]
    fn resolve_cancellation_interrupts_a_delayed_directory_lookup() {
        let server = crate::test_support::ScriptedHttpServer::new([
            crate::test_support::ScriptedHttpResponse::fixed(200, b"[]")
                .delay_headers(Duration::from_secs(1)),
        ]);
        let provider =
            RadioBrowserProvider::with_api_base(Url::parse(&server.url()).expect("test API URL"));
        let page =
            Url::parse("https://www.radio-browser.info/station/test-id").expect("station page URL");
        let cancellation = StreamCancellation::new();
        let cancellation_for_worker = cancellation.clone();
        let worker = std::thread::spawn(move || provider.resolve(&page, &cancellation_for_worker));

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while server.requests().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(server.requests().len(), 1, "directory request must start");
        cancellation.cancel();

        let error = worker
            .join()
            .expect("directory worker must join")
            .expect_err("cancelled");
        assert!(matches!(error, StreamError::Cancelled));
        assert_eq!(cancellation.active_http_requests(), 0);
    }

    #[test]
    fn station_record_into_resolved_keeps_all_metadata() {
        let record = StationRecord {
            name: Some("Radio Paradise".into()),
            url_resolved: "https://stream.example.com/live".into(),
            homepage: Some("https://radioparadise.example/".into()),
            favicon: Some("https://radioparadise.example/logo.png".into()),
            country: Some("US".into()),
            language: Some("english".into()),
            tags: Some("eclectic rock".into()),
            codec: Some("MP3".into()),
            bitrate: Some(192),
        };
        let resolved = record.into_resolved();
        assert_eq!(resolved.title.as_deref(), Some("Radio Paradise"));
        assert_eq!(resolved.station.as_deref(), Some("Radio Paradise"));
        assert_eq!(resolved.country.as_deref(), Some("US"));
        assert_eq!(resolved.language.as_deref(), Some("english"));
        assert_eq!(resolved.genre.as_deref(), Some("eclectic rock"));
        assert_eq!(resolved.codec.as_deref(), Some("MP3"));
        assert_eq!(resolved.bitrate, Some(192));
        assert!(resolved.logo_url.is_some());
        assert!(resolved.homepage.is_some());
    }

    #[test]
    fn station_record_with_missing_fields_resolves_to_empty() {
        let record = StationRecord {
            url_resolved: String::new(),
            ..StationRecord::default()
        };
        let resolved = record.into_resolved();
        assert_eq!(resolved.title, None);
        assert_eq!(resolved.bitrate, None);
    }

    #[test]
    fn url_resolved_requires_http_or_https_with_a_host() {
        let original = Url::parse("https://www.radio-browser.info/station/test-id").unwrap();
        for value in ["", "file:///tmp/radio", "https://", "data:audio/mpeg"] {
            assert!(
                parse_resolved_stream_url(value, &original).is_err(),
                "invalid url_resolved must be rejected: {value:?}"
            );
        }
        assert_eq!(
            parse_resolved_stream_url("http://cdn.example/live", &original)
                .expect("valid stream URL")
                .as_str(),
            "http://cdn.example/live"
        );
    }

    #[test]
    fn station_page_playback_uses_url_resolved_from_the_directory_record() {
        use std::io::Read;

        use crate::test_support::{ScriptedHttpResponse, ScriptedHttpServer};

        let server = ScriptedHttpServer::new(std::iter::empty());
        let direct_url = server.endpoint("stream");
        server.push_response(
            ScriptedHttpResponse::fixed(200, format!(r#"[{{"url_resolved":"{direct_url}"}}]"#))
                .with_header("Content-Type", "application/json"),
        );
        server.push_response(
            ScriptedHttpResponse::fixed(200, b"stream bytes")
                .with_header("Content-Type", "audio/mpeg"),
        );
        server.push_response(
            ScriptedHttpResponse::fixed(200, b"stream bytes")
                .with_header("Content-Type", "audio/mpeg"),
        );

        let provider =
            RadioBrowserProvider::with_api_base(Url::parse(&server.url()).expect("test API URL"));
        let page =
            Url::parse("https://www.radio-browser.info/station/test-id").expect("station page URL");
        let cancellation = StreamCancellation::new();
        let mut reader = provider
            .open_reader(&page, &cancellation)
            .expect("direct stream reader");
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("read direct stream");
        assert_eq!(bytes, b"stream bytes");
    }
}
