//! URL classification shared by the resolver, the popup and the UI labels.
//!
//! The single source of truth for "which provider owns this URL?" lives here
//! so the rest of the codebase does not end up repeating the same
//! `host == "youtube.com"` ladder. New providers append a clause to
//! [`classify`] and the rest of the system picks them up automatically.
//!
//! Detection is purely lexical: it inspects scheme, host and path, never
//! touches the network. Heavier work (querying Radio Browser, resolving a
//! YouTube redirect) belongs in the [`crate::stream::provider::StreamProvider`]
//! implementations themselves.

use url::Url;

use crate::stream::source::StreamKind;

/// Recognise the streaming platform a URL points at.
///
/// Returns `Some(kind)` when we know how to handle the URL, `None` for
/// anything we cannot classify. A `None` answer is **not** an error: the
/// resolver falls back to the generic HTTP provider when the URL is a plain
/// HTTP/HTTPS audio stream, and reports an unsupported URL when it is not.
pub fn classify(url: &Url) -> Option<StreamKind> {
    let host = url.host_str()?.to_ascii_lowercase();

    if is_youtube_host(&host) {
        return Some(StreamKind::YouTube);
    }

    if is_radio_browser_host(&host) {
        return Some(StreamKind::RadioBrowser);
    }

    // Plain HTTP/HTTPS audio streams fall through to the generic provider;
    // every other scheme (file://, ftp://, …) is unsupported.
    if matches!(url.scheme(), "http" | "https") {
        return Some(StreamKind::Http);
    }

    None
}

/// Recognise the YouTube hosts the public site actually serves.
///
/// Covers the canonical `youtube.com`, the international `youtu.be` short
/// link, and the embedded player host. Music.youtube.com is intentionally
/// classified the same way: the spec asks for "public YouTube" support and
/// the resolver uses the same `yt-dlp` invocation for both.
pub fn is_youtube_host(host: &str) -> bool {
    matches!(
        host,
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be"
    )
}

/// Recognise the public Radio Browser API and web hosts.
///
/// The directory at `www.radio-browser.info` exposes both the human-facing
/// site and the JSON API; the spec calls for station metadata, so we accept
/// both shapes and let the provider decide which endpoint to query.
pub fn is_radio_browser_host(host: &str) -> bool {
    matches!(
        host,
        "www.radio-browser.info" | "radio-browser.info" | "de1.api.radio-browser.info"
    )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("valid fixture")
    }

    #[test]
    fn classify_recognises_youtube_variants() {
        for raw in [
            "https://www.youtube.com/watch?v=abc",
            "https://youtube.com/watch?v=abc",
            "https://m.youtube.com/watch?v=abc",
            "https://music.youtube.com/watch?v=abc",
            "https://youtu.be/abc",
        ] {
            assert_eq!(
                classify(&url(raw)),
                Some(StreamKind::YouTube),
                "fixture {raw}"
            );
        }
    }

    #[test]
    fn classify_recognises_radio_browser() {
        for raw in [
            "https://www.radio-browser.info/webservice/something",
            "https://de1.api.radio-browser.info/json/stations/search?limit=1",
        ] {
            assert_eq!(
                classify(&url(raw)),
                Some(StreamKind::RadioBrowser),
                "fixture {raw}"
            );
        }
    }

    #[test]
    fn classify_falls_through_to_generic_http() {
        for raw in [
            "http://stream.example.com/live.mp3",
            "https://radio.example.com:8000/live",
            "http://192.168.1.10:8000/radio",
        ] {
            assert_eq!(classify(&url(raw)), Some(StreamKind::Http), "fixture {raw}");
        }
    }

    #[test]
    fn classify_rejects_unsupported_schemes() {
        let ftp = Url::parse("ftp://example.com/file.mp3").expect("parse");
        let file = Url::parse("file:///music/song.mp3").expect("parse");
        assert_eq!(classify(&ftp), None);
        assert_eq!(classify(&file), None);
    }

    #[test]
    fn classify_is_case_insensitive_on_the_host() {
        let upper = url("HTTPS://WWW.YOUTUBE.COM/watch?v=abc");
        assert_eq!(classify(&upper), Some(StreamKind::YouTube));
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 48,
            failure_persistence: None,
            max_shrink_iters: 128,
            rng_algorithm: proptest::test_runner::RngAlgorithm::ChaCha,
            rng_seed: proptest::test_runner::RngSeed::Fixed(0x4933_3003),
            .. ProptestConfig::default()
        })]

        #[test]
        fn classification_is_stable_for_arbitrary_valid_urls(
            scheme in prop::sample::select(vec!["http", "https", "ftp", "file"]),
            host in "[a-z]{1,12}\\.example\\.com",
            path in prop::collection::vec("[a-z0-9]{1,8}", 0..=4),
            query in prop::option::of("[a-z0-9]{0,12}"),
        ) {
            let mut raw = format!("{scheme}://{host}/{}", path.join("/"));
            if let Some(query) = query {
                raw.push('?');
                raw.push_str(&query);
            }
            let parsed = Url::parse(&raw).expect("strategy creates valid URLs");
            let reparsed = Url::parse(parsed.as_str()).expect("serialized URL stays valid");

            prop_assert_eq!(classify(&parsed), classify(&reparsed));
            prop_assert_eq!(classify(&parsed), classify(&parsed.clone()));
        }
    }
}
