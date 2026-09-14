//! Playlist sorting and column presentation configuration.

use serde::{Deserialize, Serialize};

use super::app::deserialize_or_default;

/// How a playlist is presented or reordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortBy {
    /// Alphabetical by file name (default).
    #[default]
    Filename,
    /// By selected metadata fields.
    Metadata,
}

/// Metadata fields in the canonical order used by playlist columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMetadataField {
    Artist,
    Album,
    TrackNumber,
    Title,
}

impl SortMetadataField {
    pub const ORDER: [Self; 4] = [Self::Artist, Self::Album, Self::TrackNumber, Self::Title];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Artist => "Artist",
            Self::Album => "Album",
            Self::TrackNumber => "Track Number",
            Self::Title => "Title",
        }
    }

    pub const fn key(self) -> &'static str {
        match self {
            Self::Artist => "artist",
            Self::Album => "album",
            Self::TrackNumber => "track_number",
            Self::Title => "title",
        }
    }

    pub const fn is_enabled(self, config: &SortTracksConfig) -> bool {
        match self {
            Self::Artist => config.metadata_artist,
            Self::Album => config.metadata_album,
            Self::TrackNumber => config.metadata_track_number,
            Self::Title => config.metadata_title,
        }
    }

    pub const fn is_enabled_playlist(self, config: &PlaylistColumnsConfig) -> bool {
        match self {
            Self::Artist => config.metadata_artist,
            Self::Album => config.metadata_album,
            Self::TrackNumber => config.metadata_track_number,
            Self::Title => true,
        }
    }
}

/// Legacy Now Playing ordering preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SortTracksConfig {
    #[serde(deserialize_with = "deserialize_or_default")]
    pub sort_by: SortBy,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub metadata_track_number: bool,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub metadata_artist: bool,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub metadata_album: bool,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub metadata_title: bool,
}

impl Default for SortTracksConfig {
    fn default() -> Self {
        Self {
            sort_by: SortBy::Filename,
            metadata_track_number: false,
            metadata_artist: false,
            metadata_album: false,
            metadata_title: false,
        }
    }
}

/// Playlist-column presentation and explicit reorder criteria.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistColumnsConfig {
    #[serde(alias = "sort_by", deserialize_with = "deserialize_or_default")]
    pub display_by: SortBy,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub metadata_artist: bool,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub metadata_album: bool,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub metadata_track_number: bool,
}

impl Default for PlaylistColumnsConfig {
    fn default() -> Self {
        Self {
            display_by: SortBy::Filename,
            metadata_artist: false,
            metadata_album: false,
            metadata_track_number: false,
        }
    }
}

impl From<SortTracksConfig> for PlaylistColumnsConfig {
    fn from(legacy: SortTracksConfig) -> Self {
        Self {
            display_by: legacy.sort_by,
            metadata_artist: legacy.metadata_artist,
            metadata_album: legacy.metadata_album,
            metadata_track_number: legacy.metadata_track_number,
        }
    }
}
