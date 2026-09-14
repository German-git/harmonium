//! Orchestrates the registered providers and applies the spec's fallback chain.
//!
//! [`StreamResolver`] owns an ordered list of providers. When given a URL it
//! asks each provider whether it can handle it, runs the first one that
//! says yes, and layers progressively looser defaults on top of the result:
//!
//! 1. Provider metadata (Radio Browser directory, ICY headers from Icecast /
//!    SHOUTcast, …).
//! 2. URL-derived title (the original URL minus its scheme), only when the
//!    provider produced no title.
//! 3. Hostname / IP literal, used as the final fallback so the playlist
//!    entry never shows the full URL.
//!
//! The resolver is the only public surface a [`crate::stream::provider::StreamProvider`]
//! implementation needs to know about. The audio engine talks to the
//! resolver, the resolver talks to the providers.

use std::sync::Arc;

use url::Url;

use crate::stream::http::HttpProvider;
use crate::stream::provider::{
    PreparedStream, ResolvedStream, StreamCancellation, StreamError, StreamProvider, StreamReader,
};
use crate::stream::radio_browser::RadioBrowserProvider;
use crate::stream::source::StreamKind;
use crate::stream::url_detect::classify;
use crate::stream::youtube::YouTubeProvider;

/// Owns the providers and turns a URL into a [`ResolvedStream`] plus a
/// reader for playback.
#[derive(Clone)]
pub struct StreamResolver {
    providers: Vec<Arc<dyn StreamProvider>>,
}

impl std::fmt::Debug for StreamResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamResolver")
            .field("providers", &self.providers.len())
            .finish()
    }
}

impl StreamResolver {
    /// Build a resolver that holds exactly the providers in `providers`,
    /// tried in the given order. Useful for tests that want to swap a
    /// single backend for a fake.
    pub fn new(providers: Vec<Arc<dyn StreamProvider>>) -> Self {
        Self { providers }
    }

    /// Build the production resolver: YouTube, Radio Browser, then the
    /// generic HTTP provider as the catch-all. Order matters when more
    /// than one provider claims the same URL, such as YouTube hosts that
    /// also use HTTP transport.
    pub fn with_defaults() -> Self {
        resolver_with_defaults()
    }

    /// Detect the kind of `url` and pick the matching provider.
    ///
    /// Returns `None` when no provider claims the URL. The caller can
    /// surface a clear "unsupported URL" notification and skip the
    /// playlist write entirely.
    pub fn pick_provider(&self, url: &Url) -> Option<Arc<dyn StreamProvider>> {
        self.providers
            .iter()
            .find(|provider| provider.can_handle(url))
            .cloned()
    }

    /// Resolve metadata for `url` through the matching provider.
    ///
    /// This compatibility adapter preserves the original public call shape
    /// for callers that do not own a cancellation boundary.
    ///
    /// The returned [`ResolvedStream`] always carries the strongest title
    /// the pipeline can build; on a fully missing provider response the
    /// title is the URL hostname, exactly as the spec mandates.
    pub fn resolve(&self, url: &Url) -> Result<ResolvedStream, StreamError> {
        let cancellation = StreamCancellation::new();
        self.resolve_with_cancellation(url, &cancellation)
    }

    /// Resolve metadata while allowing the owning operation to stop provider
    /// network or subprocess work at its cancellation boundaries.
    pub fn resolve_with_cancellation(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<ResolvedStream, StreamError> {
        if cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }
        let provider = self
            .pick_provider(url)
            .ok_or_else(|| StreamError::Unsupported {
                url: url.clone(),
                message: "no provider claims this URL".into(),
            })?;

        let mut resolved = match provider.resolve(url, cancellation) {
            Ok(resolved) => resolved,
            Err(StreamError::Cancelled) => return Err(StreamError::Cancelled),
            Err(error) => {
                tracing::warn!(
                    "provider {:?} failed for {}: {error}; using fallback",
                    provider.kind(),
                    crate::net::safe_url(url)
                );
                ResolvedStream::default()
            }
        };

        if cancellation.is_cancelled() {
            return Err(StreamError::Cancelled);
        }

        apply_fallbacks(&mut resolved, url);
        Ok(resolved)
    }

    /// Open a reader for playback.
    pub fn open_reader(
        &self,
        url: &Url,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError> {
        let provider = self
            .pick_provider(url)
            .ok_or_else(|| StreamError::Unsupported {
                url: url.clone(),
                message: "no provider claims this URL".into(),
            })?;
        provider.open_reader(url, cancellation)
    }

    /// Open playback with the one-shot response prepared during resolution,
    /// when the selected provider has one.
    pub fn open_reader_with_prepared(
        &self,
        url: &Url,
        prepared: Option<&PreparedStream>,
        cancellation: &StreamCancellation,
    ) -> Result<StreamReader, StreamError> {
        let provider = self
            .pick_provider(url)
            .ok_or_else(|| StreamError::Unsupported {
                url: url.clone(),
                message: "no provider claims this URL".into(),
            })?;
        provider.open_reader_with_prepared(url, prepared, cancellation)
    }

    /// Classify a URL into a [`StreamKind`] using the same detector as the
    /// resolver. Centralised so the UI labels stay in sync with what the
    /// resolver actually picks.
    pub fn classify(&self, url: &Url) -> Option<StreamKind> {
        classify(url)
    }
}

/// Layer the spec's fallback chain on top of whatever the provider
/// returned.
///
/// The provider may fail entirely (the resolver already maps that to an
/// empty [`ResolvedStream`]) or return only a partial record. Either way
/// the playlist entry must end up with a sensible title, so we keep
/// walking the fallback chain until the title is non-empty.
fn apply_fallbacks(resolved: &mut ResolvedStream, url: &Url) {
    if resolved.title.as_ref().is_none_or(|t| t.trim().is_empty()) {
        resolved.title = Some(fallback_title(url));
    }

    // Duration is only meaningful for finite sources. A live radio has
    // `None` and the M3U8 serializer already encodes that as `-1`, so we
    // leave the field alone.
}

fn fallback_title(url: &Url) -> String {
    url.host_str()
        .map(|h| h.to_string())
        .unwrap_or_else(|| url.as_str().to_string())
}

/// Build the production resolver with the canonical provider order.
pub fn resolver_with_defaults() -> StreamResolver {
    StreamResolver::new(vec![
        Arc::new(YouTubeProvider::new()),
        Arc::new(RadioBrowserProvider::new()),
        Arc::new(HttpProvider::new()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::provider::StreamError;

    /// Minimal fake provider so we can assert fallback behaviour without
    /// touching the network or `yt-dlp`.
    #[derive(Debug)]
    struct FakeProvider {
        kind: StreamKind,
        host: &'static str,
        resolved: ResolvedStream,
        should_fail: bool,
    }

    impl StreamProvider for FakeProvider {
        fn can_handle(&self, url: &Url) -> bool {
            url.host_str()
                .map(|h| h.eq_ignore_ascii_case(self.host))
                .unwrap_or(false)
        }
        fn kind(&self) -> StreamKind {
            self.kind
        }
        fn resolve(
            &self,
            _url: &Url,
            _cancellation: &StreamCancellation,
        ) -> Result<ResolvedStream, StreamError> {
            if self.should_fail {
                Err(StreamError::Other("forced failure".into()))
            } else {
                Ok(self.resolved.clone())
            }
        }
        fn open_reader(
            &self,
            _url: &Url,
            _cancellation: &StreamCancellation,
        ) -> Result<StreamReader, StreamError> {
            Ok(StreamReader::buffered(std::io::Cursor::new(Vec::new())))
        }
    }

    fn empty_resolver() -> StreamResolver {
        StreamResolver::new(Vec::new())
    }

    #[test]
    fn resolver_rejects_unsupported_urls() {
        let resolver = empty_resolver();
        let url = Url::parse("ftp://example.com/file.mp3").unwrap();
        let error = resolver.resolve(&url).expect_err("unsupported");
        assert!(matches!(error, StreamError::Unsupported { .. }));
    }

    #[test]
    fn resolver_uses_provider_metadata_when_present() {
        let provider = FakeProvider {
            kind: StreamKind::Http,
            host: "radio.example",
            resolved: ResolvedStream {
                title: Some("Provider Title".into()),
                ..ResolvedStream::default()
            },
            should_fail: false,
        };
        let resolver = StreamResolver::new(vec![Arc::new(provider)]);
        let url = Url::parse("https://radio.example/live").unwrap();
        let resolved = resolver.resolve(&url).expect("ok");
        assert_eq!(resolved.title.as_deref(), Some("Provider Title"));
    }

    #[test]
    fn resolver_falls_back_to_hostname_when_provider_fails() {
        let provider = FakeProvider {
            kind: StreamKind::Http,
            host: "radio.example",
            resolved: ResolvedStream::default(),
            should_fail: true,
        };
        let resolver = StreamResolver::new(vec![Arc::new(provider)]);
        let url = Url::parse("https://radio.example/live").unwrap();
        let resolved = resolver.resolve(&url).expect("ok");
        assert_eq!(resolved.title.as_deref(), Some("radio.example"));
    }

    #[test]
    fn resolver_falls_back_to_url_when_host_is_missing() {
        let url = Url::parse("data:audio/mpeg;base64,").unwrap();
        // The empty resolver returns Unsupported; we want to assert the
        // pure fallback title helper directly to keep the test focused.
        assert_eq!(fallback_title(&url), url.as_str());
    }

    #[test]
    fn resolver_classify_uses_default_chain() {
        let resolver = resolver_with_defaults();
        assert_eq!(
            resolver.classify(&Url::parse("https://www.youtube.com/watch?v=abc").unwrap()),
            Some(StreamKind::YouTube)
        );
        assert_eq!(
            resolver.classify(&Url::parse("https://radio.example.com/live").unwrap()),
            Some(StreamKind::Http)
        );
    }

    #[test]
    fn defaults_pick_youtube_over_radio_browser_for_youtube_urls() {
        let resolver = resolver_with_defaults();
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        let provider = resolver.pick_provider(&url).expect("claimed");
        assert_eq!(provider.kind(), StreamKind::YouTube);
    }

    #[test]
    fn defaults_send_generic_http_urls_to_http_without_radio_browser_lookup() {
        let resolver = resolver_with_defaults();
        let url = Url::parse("https://radio.example.com/live").unwrap();
        let provider = resolver.pick_provider(&url).expect("claimed");

        assert_eq!(provider.kind(), StreamKind::Http);
    }
}
