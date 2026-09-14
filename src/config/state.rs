//! Runtime-state persistence and legacy preference migration.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml_edit::{DocumentMut, Item};

use crate::filesystem::persistence::atomic_replace;
use crate::playback_mode::RepeatMode;
use crate::track::TrackLocation;

pub(crate) const CONFIG_FILE_NAME: &str = "config.toml";
pub(crate) const STATE_FILE_NAME: &str = "state.toml";
pub(crate) const LEGACY_PREFERENCE_KEYS: [&str; 5] = [
    "volume_percent",
    "confirm_quit",
    "resume_previous_track",
    "browser_directory",
    "artwork_visible",
];

fn deserialize_repeat_mode<'de, D>(deserializer: D) -> Result<RepeatMode, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer).unwrap_or_default();
    Ok(RepeatMode::parse(&value))
}

/// A runtime-state persistence failure that retains its filesystem cause.
#[derive(Debug, Error)]
pub enum StateSaveError {
    #[error("cannot read state {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid state {path}; existing file was left unchanged")]
    Parse { path: PathBuf },
    #[error("cannot serialize state {path}")]
    Serialize { path: PathBuf },
    #[error("cannot create state directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write state {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Persisted runtime state that survives across sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    #[serde(deserialize_with = "deserialize_repeat_mode")]
    pub repeat_mode: RepeatMode,
    pub shuffle: bool,
    pub last_playlist: Option<String>,
    pub last_track_path: Option<String>,
    pub last_track_position_ms: u64,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            repeat_mode: RepeatMode::Off,
            shuffle: false,
            last_playlist: None,
            last_track_path: None,
            last_track_position_ms: 0,
        }
    }
}

/// User preferences from the pre-config/state split file format.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LegacyPreferences {
    pub volume_percent: Option<u16>,
    pub confirm_quit: Option<bool>,
    pub resume_previous_track: Option<bool>,
    pub browser_directory: Option<String>,
    pub artwork_visible: Option<bool>,
}

impl LegacyPreferences {
    pub fn is_empty(&self) -> bool {
        self.volume_percent.is_none()
            && self.confirm_quit.is_none()
            && self.resume_previous_track.is_none()
            && self.browser_directory.is_none()
            && self.artwork_visible.is_none()
    }
}

pub fn load_legacy_preferences(data_dir: &Path) -> Option<LegacyPreferences> {
    let path = data_dir.join(STATE_FILE_NAME);
    let contents = fs::read_to_string(path).ok()?;
    toml::from_str(&contents).ok()
}

pub fn pending_legacy_preferences(
    config_dir: &Path,
    legacy: &LegacyPreferences,
) -> LegacyPreferences {
    let path = config_dir.join(CONFIG_FILE_NAME);
    let document = fs::read_to_string(path)
        .ok()
        .and_then(|contents| toml::from_str::<toml::Value>(&contents).ok());
    let general = document
        .as_ref()
        .and_then(|document| document.get("general"))
        .and_then(toml::Value::as_table);
    let missing = |key: &str| !general.is_some_and(|general| general.contains_key(key));

    LegacyPreferences {
        volume_percent: missing("volume_percent")
            .then_some(legacy.volume_percent)
            .flatten(),
        confirm_quit: missing("confirm_quit")
            .then_some(legacy.confirm_quit)
            .flatten(),
        resume_previous_track: missing("resume_previous_track")
            .then_some(legacy.resume_previous_track)
            .flatten(),
        browser_directory: missing("browser_directory")
            .then_some(legacy.browser_directory.clone())
            .flatten(),
        artwork_visible: missing("artwork_visible")
            .then_some(legacy.artwork_visible)
            .flatten(),
    }
}

pub fn migrate(legacy: &LegacyPreferences, config: &mut crate::config::AppConfig) -> bool {
    let mut migrated = false;
    if let Some(volume) = legacy.volume_percent {
        config.general.volume_percent = crate::config::VolumePercent::from_boundary(volume);
        migrated = true;
    }
    if let Some(confirm) = legacy.confirm_quit {
        config.general.confirm_quit = confirm;
        migrated = true;
    }
    if let Some(resume) = legacy.resume_previous_track {
        config.general.resume_previous_track = resume;
        migrated = true;
    }
    if let Some(dir) = &legacy.browser_directory {
        config.general.browser_directory = dir.clone();
        migrated = true;
    }
    if let Some(visible) = legacy.artwork_visible {
        config.general.artwork_visible = visible;
        migrated = true;
    }
    migrated
}

pub fn erase_migrated_preferences(data_dir: &Path) -> Result<bool, String> {
    erase_migrated_preferences_with(data_dir, atomic_replace)
}

pub(crate) fn erase_migrated_preferences_with<F>(
    data_dir: &Path,
    replace: F,
) -> Result<bool, String>
where
    F: FnOnce(&Path, &[u8]) -> std::io::Result<()>,
{
    let path = data_dir.join(STATE_FILE_NAME);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot read state {}: {error}", path.display())),
    };
    let mut document = contents.parse::<DocumentMut>().map_err(|_error| {
        format!(
            "cannot parse state {} while removing migrated preferences",
            path.display()
        )
    })?;

    let mut removed = false;
    for key in LEGACY_PREFERENCE_KEYS {
        if document.as_table_mut().remove(key).is_some() {
            removed = true;
        }
    }
    if !removed {
        return Ok(false);
    }

    let replacement = document.to_string();
    match replace(&path, replacement.as_bytes()) {
        Ok(()) => Ok(true),
        Err(_error) if state_has_no_legacy_preferences(&path) => Ok(true),
        Err(_error) => Err(format!(
            "cannot verify migrated preference cleanup for state {}",
            path.display()
        )),
    }
}

fn state_has_no_legacy_preferences(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| contents.parse::<DocumentMut>().ok())
        .is_some_and(|document| {
            LEGACY_PREFERENCE_KEYS
                .iter()
                .all(|key| !document.as_table().contains_key(key))
        })
}

impl PersistedState {
    pub fn track_location(&self) -> Option<TrackLocation> {
        self.last_track_path
            .as_deref()
            .and_then(TrackLocation::from_persisted)
    }

    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(STATE_FILE_NAME);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                tracing::debug!("no state file at {}, using defaults", path.display());
                return Self::default();
            }
            Err(error) => {
                tracing::warn!(
                    "cannot read state {}: {error}, using defaults",
                    path.display()
                );
                return Self::default();
            }
        };
        match toml::from_str(&contents) {
            Ok(state) => state,
            Err(_error) => {
                tracing::warn!("invalid state {}, using defaults", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self, data_dir: &Path) {
        if let Err(error) = self.save_result(data_dir) {
            tracing::warn!("{error}");
        }
    }

    pub(crate) fn save_result(&self, data_dir: &Path) -> Result<(), StateSaveError> {
        let path = data_dir.join(STATE_FILE_NAME);
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            return Err(StateSaveError::CreateDirectory {
                path: parent.to_path_buf(),
                source: error,
            });
        }
        let mut document = match fs::read_to_string(&path) {
            Ok(contents) => contents
                .parse::<DocumentMut>()
                .map_err(|_error| StateSaveError::Parse { path: path.clone() })?,
            Err(error) if error.kind() == ErrorKind::NotFound => DocumentMut::new(),
            Err(error) => {
                return Err(StateSaveError::Read {
                    path: path.clone(),
                    source: error,
                });
            }
        };
        let contents = toml_edit::ser::to_string_pretty(self)
            .map_err(|_error| StateSaveError::Serialize { path: path.clone() })?;
        let replacement = contents
            .parse::<DocumentMut>()
            .map_err(|_error| StateSaveError::Serialize { path: path.clone() })?;
        merge_state_document(&mut document, &replacement);
        let rendered = document.to_string();
        atomic_replace(&path, rendered.as_bytes())
            .map_err(|source| StateSaveError::Write { path, source })
    }
}

fn merge_state_document(document: &mut DocumentMut, replacement: &DocumentMut) {
    for (key, replacement_item) in replacement.as_table().iter() {
        if let Some(existing_item) = document.as_table_mut().get_mut(key) {
            merge_item(existing_item, replacement_item);
        } else {
            document
                .as_table_mut()
                .insert(key, replacement_item.clone());
        }
    }
    for key in ["last_playlist", "last_track_path"] {
        if !replacement.as_table().contains_key(key) {
            document.as_table_mut().remove(key);
        }
    }
}

fn merge_item(existing: &mut Item, replacement: &Item) {
    if let (Some(existing_table), Some(replacement_table)) =
        (existing.as_table_mut(), replacement.as_table())
    {
        for (key, replacement_item) in replacement_table.iter() {
            if let Some(existing_item) = existing_table.get_mut(key) {
                merge_item(existing_item, replacement_item);
            } else {
                existing_table.insert(key, replacement_item.clone());
            }
        }
        return;
    }
    if let Item::Value(existing_value) = existing {
        if let Item::Value(replacement_value) = replacement {
            let decor = existing_value.decor().clone();
            *existing_value = replacement_value.clone();
            *existing_value.decor_mut() = decor;
            return;
        }
    }
    *existing = replacement.clone();
}
