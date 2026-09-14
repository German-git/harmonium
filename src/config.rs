//! Stable public facade for Harmonium's configuration domains.
//!
//! The implementation is split by responsibility, while these re-exports
//! keep the historical `crate::config::*` paths unchanged.

mod app;
mod columns;
mod keys;
mod paths;
mod state;

pub use app::{
    AlbumArtMode, AppConfig, ArtworkSource, BorderType, CROSSFADE_MAX_SECONDS,
    CROSSFADE_MIN_SECONDS, CROSSFADE_STEP_SECONDS, ConfigSaveError, GeneralConfig, LogConfig,
    PlaybackConfig, SoundConfig, UiConfig, ensure_themes_dir,
};
pub use columns::{PlaylistColumnsConfig, SortBy, SortMetadataField, SortTracksConfig};
pub use keys::{KeyBindingField, KeySettingsRow, KeysConfig};
pub use paths::{Paths, ensure_dir};
pub use state::{
    LegacyPreferences, PersistedState, StateSaveError, erase_migrated_preferences,
    load_legacy_preferences, migrate, pending_legacy_preferences,
};

pub use crate::audio::playback::{CrossfadeSeconds, GainDb, VolumePercent};
pub use crate::input::{KeyChord, KeyChordError};

#[cfg(test)]
pub(crate) use app::bundled_themes;
#[cfg(test)]
pub(crate) use state::{
    CONFIG_FILE_NAME, LEGACY_PREFERENCE_KEYS, STATE_FILE_NAME, erase_migrated_preferences_with,
};

#[cfg(test)]
#[path = "config/tests.rs"]
mod tests;
