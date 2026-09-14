//! Structured failures raised at the audio boundary.

use thiserror::Error;

use crate::stream::TrackSource;

/// Everything that can go wrong while driving playback.
///
/// Variants carry user actionable context so the worker can turn them into
/// notifications without stringly typed reconstruction at the call site.
#[derive(Debug, Error)]
pub enum AudioError {
    /// No usable output device or the stream could not be opened.
    ///
    /// This is an expected condition on machines without audio hardware,
    /// so callers must degrade to a warning instead of failing startup.
    #[error("audio output unavailable: {0}")]
    DeviceUnavailable(#[source] anyhow::Error),
    /// The file opened but its content is not decodable audio.
    #[error("cannot play {}: {message}", location_for(track_source))]
    Decode {
        /// Track source that failed to decode. Local files surface the
        /// path; streams surface the URL so the user can paste the right
        /// thing back into the search bar.
        track_source: TrackSource,
        /// Decoder message describing why the content was rejected.
        message: String,
    },
    /// The audio file could not be opened for reading.
    #[error("cannot read {location}: {io}", location = location_for(track_source))]
    Io {
        /// Track source that could not be opened.
        track_source: TrackSource,
        /// Underlying IO failure with its original kind preserved.
        #[source]
        io: std::io::Error,
    },
    /// The decoder rejected a seek request.
    #[error("seek failed: {0}")]
    Seek(#[source] rodio::source::SeekError),
    /// A stream URL failed to resolve into a playable reader.
    #[error("stream error: {0}")]
    Stream(#[source] anyhow::Error),
}

/// Render the most useful identifier for a track source in error messages.
///
/// Paths render as the filesystem path; streams render as the URL so the
/// user can tell which remote resource is misbehaving.
fn location_for(source: &TrackSource) -> String {
    match source {
        TrackSource::Local(path) => path.to_string_lossy().into_owned(),
        TrackSource::Stream { url, .. } => crate::net::safe_url(url),
    }
}

/// Convenience constructor for the IO variant when the caller already holds
/// a path and an io error.
impl AudioError {
    /// Convenience constructor for IO errors keyed on a [`TrackSource`].
    pub fn io(track_source: TrackSource, io: std::io::Error) -> Self {
        AudioError::Io { track_source, io }
    }

    /// Convenience constructor for decode errors keyed on a [`TrackSource`].
    pub fn decode(track_source: TrackSource, message: String) -> Self {
        AudioError::Decode {
            track_source,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_unavailable_mentions_the_underlying_detail() {
        let error = AudioError::DeviceUnavailable(anyhow::anyhow!("no device"));

        assert_eq!(error.to_string(), "audio output unavailable: no device");
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn device_unavailable_preserves_the_typed_source() {
        let cause = std::io::Error::new(std::io::ErrorKind::NotFound, "sink disappeared");
        let error = AudioError::DeviceUnavailable(anyhow::Error::new(cause));

        let AudioError::DeviceUnavailable(source) = &error else {
            unreachable!();
        };
        assert!(source.downcast_ref::<std::io::Error>().is_some());
    }

    #[test]
    fn decode_error_names_the_offending_file() {
        let error = AudioError::Decode {
            track_source: TrackSource::local("/music/broken.mp3"),
            message: "unrecognized format".to_string(),
        };

        assert!(error.to_string().contains("/music/broken.mp3"));
        assert!(error.to_string().contains("unrecognized format"));
    }

    #[test]
    fn io_error_preserves_the_source_path() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let error = AudioError::Io {
            track_source: TrackSource::local("/music/missing.flac"),
            io,
        };

        let msg = error.to_string();
        assert!(msg.contains("/music/missing.flac"), "must name the file");
        assert!(msg.contains("cannot read"), "must say cannot read");
    }

    #[test]
    fn seek_error_includes_detail() {
        let error = AudioError::Seek(rodio::source::SeekError::NotSupported {
            underlying_source: "test decoder",
        });

        assert_eq!(
            error.to_string(),
            "seek failed: Seeking is not supported by source: test decoder"
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn seek_error_preserves_the_typed_source() {
        let error = AudioError::Seek(rodio::source::SeekError::NotSupported {
            underlying_source: "test decoder",
        });

        assert!(
            std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<rodio::source::SeekError>())
                .is_some()
        );
    }

    #[test]
    fn stream_error_preserves_the_typed_source_and_safe_location() {
        let url = url::Url::parse("https://radio.example/live?token=secret").unwrap();
        let cause = crate::stream::StreamError::Network {
            url: url.clone(),
            message: "connection refused".to_string(),
        };
        let error = AudioError::Stream(anyhow::Error::new(cause).context(format!(
            "{}: network error for {}: connection refused",
            crate::net::safe_url(&url),
            crate::net::safe_url(&url)
        )));

        assert_eq!(
            error.to_string(),
            "stream error: https://radio.example/live: network error for https://radio.example/live: connection refused"
        );
        assert!(!error.to_string().contains("secret"));
        let AudioError::Stream(source) = &error else {
            unreachable!();
        };
        assert!(
            source
                .downcast_ref::<crate::stream::StreamError>()
                .is_some()
        );
    }
}
