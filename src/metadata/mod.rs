//! Audio tag extraction decoupled from lofty and the filesystem.
//!
//! [`TrackMetadata`] is a plain domain snapshot so neither the UI nor the
//! playlist ever depend on lofty types. Display fallbacks are resolved here,
//! at extraction time, keeping renderers free of fallback rules.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod reader;
pub mod writer;

/// Sentinel substituted for a missing artist tag.
///
/// The reader reports an absent artist as this literal so consumers can tell
/// "untagged" apart from a genuinely unknown value, and any code that must
/// avoid polluting labels or lookups with it refers to the same constant.
pub const UNKNOWN_ARTIST: &str = "Unknown Artist";

/// Sentinel substituted for a missing album tag (same rationale as artist).
pub const UNKNOWN_ALBUM: &str = "Unknown Album";

/// Sentinel substituted for a missing title tag (same rationale as artist).
pub const UNKNOWN_TITLE: &str = "Unknown Title";

impl Default for TrackMetadata {
    /// Tags and stream properties of one queued track, all fields empty.
    fn default() -> Self {
        Self {
            title: String::new(),
            title_tagged: false,
            artist: String::new(),
            album: String::new(),
            album_artist: None,
            track_number: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
            duration: Duration::ZERO,
            bitrate: None,
            sample_rate: None,
            codec: String::new(),
            format: String::new(),
        }
    }
}

/// Tags and stream properties of one queued track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackMetadata {
    /// Track title, falling back to the file stem when untagged.
    pub title: String,
    /// True when `title` was extracted from a real `Title` tag; false when the
    /// reader substituted the file stem fallback. The m3u8 EXTINF and the
    /// rename flow rely on this flag to decide between using `title` verbatim
    /// or substituting the full file name.
    pub title_tagged: bool,
    /// Artist name or `Unknown Artist`.
    pub artist: String,
    /// Album name or `Unknown Album`.
    pub album: String,
    /// Album artist when the tag carries one, raw without fallbacks.
    pub album_artist: Option<String>,
    /// Position inside the album, when the tag carries one.
    pub track_number: Option<u32>,
    /// Disc of a multi disc release, when the tag carries one.
    pub disc_number: Option<u32>,
    /// Genre label when the tag carries one, raw without fallbacks.
    pub genre: Option<String>,
    /// Release year as the tag spells it, raw without fallbacks.
    pub year: Option<String>,
    /// Composer when the tag carries one, raw without fallbacks.
    pub composer: Option<String>,
    /// Comment when the tag carries one, raw without fallbacks.
    pub comment: Option<String>,
    /// Decoded audio duration.
    pub duration: Duration,
    /// Overall bitrate in kbit/s, when the format reports one.
    pub bitrate: Option<u32>,
    /// Sample rate in hertz, when the format reports one.
    pub sample_rate: Option<u32>,
    /// Short codec label such as `MP3` or `FLAC`.
    pub codec: String,
    /// Human readable container or format name.
    pub format: String,
}

/// Fixed order of the ten editable metadata fields.
///
/// The metadata editor form, the prefill extraction and the tag writer all
/// index the same `[String; 10]` array by [`MetaField::index`], so the order
/// can never drift between what the form shows and what gets persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaField {
    /// Track title.
    Title,
    /// Performing artist.
    Artist,
    /// Album name.
    Album,
    /// Album artist.
    AlbumArtist,
    /// Position inside the album.
    TrackNumber,
    /// Disc of a multi disc release.
    DiscNumber,
    /// Genre label.
    Genre,
    /// Release year.
    Year,
    /// Composer.
    Composer,
    /// Free form comment.
    Comment,
}

impl MetaField {
    /// Every field in the strict form order: Title through Comment.
    pub const ALL: [MetaField; 10] = [
        MetaField::Title,
        MetaField::Artist,
        MetaField::Album,
        MetaField::AlbumArtist,
        MetaField::TrackNumber,
        MetaField::DiscNumber,
        MetaField::Genre,
        MetaField::Year,
        MetaField::Composer,
        MetaField::Comment,
    ];

    /// Display label shown by the metadata editor form.
    pub fn label(self) -> &'static str {
        match self {
            MetaField::Title => "Title",
            MetaField::Artist => "Artist",
            MetaField::Album => "Album",
            MetaField::AlbumArtist => "Album Artist",
            MetaField::TrackNumber => "Track Number",
            MetaField::DiscNumber => "Disc Number",
            MetaField::Genre => "Genre",
            MetaField::Year => "Year",
            MetaField::Composer => "Composer",
            MetaField::Comment => "Comment",
        }
    }

    /// Position inside the shared ten field array.
    pub fn index(self) -> usize {
        match self {
            MetaField::Title => 0,
            MetaField::Artist => 1,
            MetaField::Album => 2,
            MetaField::AlbumArtist => 3,
            MetaField::TrackNumber => 4,
            MetaField::DiscNumber => 5,
            MetaField::Genre => 6,
            MetaField::Year => 7,
            MetaField::Composer => 8,
            MetaField::Comment => 9,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the strict ten field order the spec mandates for the editor form.
    #[test]
    fn meta_field_order_is_strict_and_contiguous() {
        let labels: Vec<&str> = MetaField::ALL.iter().map(|field| field.label()).collect();
        assert_eq!(
            labels,
            [
                "Title",
                "Artist",
                "Album",
                "Album Artist",
                "Track Number",
                "Disc Number",
                "Genre",
                "Year",
                "Composer",
                "Comment"
            ]
        );
        for (index, field) in MetaField::ALL.iter().enumerate() {
            assert_eq!(field.index(), index, "index must match array position");
        }
    }

    #[test]
    fn decode_error_display_includes_path_and_decoder_message() {
        let error = MetadataError::Decode {
            path: PathBuf::from("/music/broken.mp3"),
            message: "unsupported codec".to_string(),
        };

        let rendered = error.to_string();
        assert!(rendered.contains("/music/broken.mp3"));
        assert!(rendered.contains("unsupported codec"));
    }

    #[test]
    fn io_error_display_and_source_preserve_the_metadata_path() {
        let error = MetadataError::Io {
            path: PathBuf::from("/music/missing.mp3"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "file is absent"),
        };

        assert_eq!(
            error.to_string(),
            "could not read /music/missing.mp3: file is absent"
        );
        let source = std::error::Error::source(&error)
            .expect("metadata IO source is retained")
            .downcast_ref::<std::io::Error>()
            .expect("source remains an io::Error");
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
    }
}

/// Structured failure of a metadata extraction attempt.
///
/// Every variant carries the offending path so background workers can log
/// precisely without reconstructing context. Not `Clone` on purpose: the
/// bridge bus only transports extracted snapshots and failure counts.
#[derive(Debug, Error)]
pub enum MetadataError {
    /// The file could not be opened for reading.
    #[error("could not read {}: {source}", path.display())]
    Io {
        /// File that could not be opened.
        path: PathBuf,
        /// Underlying IO failure with its original kind preserved.
        #[source]
        source: std::io::Error,
    },
    /// The file exists but is not decodable audio.
    #[error("could not decode {}: {message}", path.display())]
    Decode {
        /// File that failed to parse.
        path: PathBuf,
        /// Decoder message describing why the content was rejected.
        message: String,
    },
    /// The file's tags could not be written back to disk.
    #[error("could not write tags to {}: {message}", path.display())]
    Write {
        /// File whose tags failed to persist.
        path: PathBuf,
        /// Encoder message describing why the write was rejected.
        message: String,
    },
    /// A numeric metadata field does not hold a valid number.
    #[error("invalid {field} on {}: {value}", path.display())]
    InvalidNumber {
        /// File whose numeric field was rejected.
        path: PathBuf,
        /// Human readable field name such as `Track Number`.
        field: &'static str,
        /// The unparsable value the user submitted.
        value: String,
    },
}
