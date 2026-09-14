//! Application configuration values, persistence, and bundled themes.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml_edit::{DocumentMut, Item};

use crate::audio::playback::{CrossfadeSeconds, GainDb, VolumePercent};
use crate::filesystem::persistence::atomic_replace;
use crate::ui::theme::THEME_FILE_EXTENSION;

pub(crate) const DEFAULT_THEME_NAME: &str = "default";

pub(crate) fn deserialize_or_value<'de, D, T>(deserializer: D, fallback: T) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(T::deserialize(deserializer).unwrap_or(fallback))
}

pub(crate) fn deserialize_or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    deserialize_or_value(deserializer, T::default())
}

fn deserialize_or_true<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_or_value(deserializer, true)
}

fn deserialize_or_volume_default<'de, D>(deserializer: D) -> Result<VolumePercent, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_or_value(deserializer, VolumePercent::default())
}

fn deserialize_or_theme_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_or_value(deserializer, DEFAULT_THEME_NAME.to_string())
}

fn deserialize_or_log_level_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_or_value(deserializer, "info".to_string())
}

/// Artwork rendering mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlbumArtMode {
    #[default]
    Auto,
    Image,
    Unicode,
    Off,
}

/// User configuration root.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(deserialize_with = "deserialize_or_default")]
    pub ui: UiConfig,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub general: GeneralConfig,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub keys: crate::config::KeysConfig,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub playback: PlaybackConfig,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub sound: SoundConfig,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub log: LogConfig,
}

/// A configuration persistence failure safe to surface to the UI.
#[derive(Debug, Error)]
pub enum ConfigSaveError {
    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config {path}; existing file was left unchanged")]
    Parse { path: PathBuf },
    #[error("cannot serialize [{section}] for config {path}")]
    Serialize {
        path: PathBuf,
        section: &'static str,
    },
    #[error("cannot create config directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write config {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Playback behavior preferences under `[playback]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackConfig {
    #[serde(deserialize_with = "deserialize_or_default")]
    pub remote_lyrics: bool,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub gain_db: GainDb,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub crossfade_seconds: CrossfadeSeconds,
}

pub const CROSSFADE_MIN_SECONDS: u16 = 5;
pub const CROSSFADE_MAX_SECONDS: u16 = 30;
pub const CROSSFADE_STEP_SECONDS: u16 = 5;

/// How a playlist is presented or reordered.
pub use super::columns::{PlaylistColumnsConfig, SortTracksConfig};

/// General application preferences under `[general]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GeneralConfig {
    pub confirm_quit: bool,
    pub volume_percent: VolumePercent,
    pub resume_previous_track: bool,
    #[serde(default)]
    pub browser_directory: String,
    pub artwork_visible: bool,
    pub show_hidden: bool,
    pub playlist_columns: PlaylistColumnsConfig,
    pub now_playing_display: SortTracksConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct GeneralConfigFile {
    #[serde(deserialize_with = "deserialize_or_true")]
    confirm_quit: bool,
    #[serde(deserialize_with = "deserialize_or_volume_default")]
    volume_percent: VolumePercent,
    #[serde(deserialize_with = "deserialize_or_default")]
    resume_previous_track: bool,
    #[serde(deserialize_with = "deserialize_or_default")]
    browser_directory: String,
    #[serde(deserialize_with = "deserialize_or_true")]
    artwork_visible: bool,
    #[serde(deserialize_with = "deserialize_or_default")]
    show_hidden: bool,
    #[serde(deserialize_with = "deserialize_or_default")]
    playlist_columns: Option<PlaylistColumnsConfig>,
    #[serde(deserialize_with = "deserialize_or_default")]
    now_playing_display: SortTracksConfig,
    #[serde(deserialize_with = "deserialize_or_default")]
    sort_tracks: Option<SortTracksConfig>,
}

impl Default for GeneralConfigFile {
    fn default() -> Self {
        let defaults = GeneralConfig::default();
        Self {
            confirm_quit: defaults.confirm_quit,
            volume_percent: defaults.volume_percent,
            resume_previous_track: defaults.resume_previous_track,
            browser_directory: defaults.browser_directory,
            artwork_visible: defaults.artwork_visible,
            show_hidden: defaults.show_hidden,
            playlist_columns: None,
            now_playing_display: defaults.now_playing_display,
            sort_tracks: None,
        }
    }
}

impl<'de> Deserialize<'de> for GeneralConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let file = GeneralConfigFile::deserialize(deserializer)?;
        let playlist_columns = file
            .playlist_columns
            .or_else(|| file.sort_tracks.map(PlaylistColumnsConfig::from))
            .unwrap_or_default();
        Ok(Self {
            confirm_quit: file.confirm_quit,
            volume_percent: file.volume_percent,
            resume_previous_track: file.resume_previous_track,
            browser_directory: file.browser_directory,
            artwork_visible: file.artwork_visible,
            show_hidden: file.show_hidden,
            playlist_columns,
            now_playing_display: file.now_playing_display,
        })
    }
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            confirm_quit: true,
            volume_percent: VolumePercent::default(),
            resume_previous_track: false,
            browser_directory: String::new(),
            artwork_visible: true,
            show_hidden: false,
            playlist_columns: PlaylistColumnsConfig::default(),
            now_playing_display: SortTracksConfig::default(),
        }
    }
}

/// Output device selection under `[sound]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SoundConfig {
    #[serde(deserialize_with = "deserialize_or_default")]
    pub output_sink_id: String,
}

/// Logging configuration under `[log]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    #[serde(deserialize_with = "deserialize_or_log_level_default")]
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

/// Where the artwork pipeline looks for cover art.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtworkSource {
    #[default]
    All,
    Metadata,
    Local,
    Remote,
}

/// Border glyph style used by every bordered UI element.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BorderType {
    #[default]
    Plain,
    Rounded,
    Double,
    Thick,
}

impl BorderType {
    pub const ALL: [Self; 4] = [Self::Plain, Self::Rounded, Self::Double, Self::Thick];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Plain => "Plain",
            Self::Rounded => "Rounded",
            Self::Double => "Double",
            Self::Thick => "Thick",
        }
    }

    pub const fn to_ratatui(self) -> ratatui::widgets::BorderType {
        match self {
            Self::Plain => ratatui::widgets::BorderType::Plain,
            Self::Rounded => ratatui::widgets::BorderType::Rounded,
            Self::Double => ratatui::widgets::BorderType::Double,
            Self::Thick => ratatui::widgets::BorderType::Thick,
        }
    }

    pub const fn from_index(index: usize) -> Option<Self> {
        match index {
            1 => Some(Self::Plain),
            2 => Some(Self::Rounded),
            3 => Some(Self::Double),
            4 => Some(Self::Thick),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for BorderType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BorderTypeVisitor;
        impl<'de> Visitor<'de> for BorderTypeVisitor {
            type Value = BorderType;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a border type name")
            }
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(match value.to_ascii_lowercase().as_str() {
                    "rounded" => BorderType::Rounded,
                    "double" => BorderType::Double,
                    "thick" => BorderType::Thick,
                    _ => BorderType::Plain,
                })
            }
            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&value)
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(BorderType::Plain)
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(BorderType::Plain)
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(BorderType::Plain)
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(BorderType::Plain)
            }
            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(BorderType::Plain)
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(BorderType::Plain)
            }
            fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                while access.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BorderType::Plain)
            }
            fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                while access.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(BorderType::Plain)
            }
        }
        deserializer.deserialize_any(BorderTypeVisitor)
    }
}

/// Interface preferences under `[ui]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    #[serde(deserialize_with = "deserialize_or_true")]
    pub show_album_art: bool,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub album_art_mode: AlbumArtMode,
    #[serde(deserialize_with = "deserialize_or_theme_default")]
    pub theme: String,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub artwork_source: ArtworkSource,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub border_type: BorderType,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_album_art: true,
            album_art_mode: AlbumArtMode::Auto,
            theme: DEFAULT_THEME_NAME.to_string(),
            artwork_source: ArtworkSource::default(),
            border_type: BorderType::default(),
        }
    }
}

impl AppConfig {
    /// Load `config.toml`, degrading to defaults on any read or parse failure.
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join("config.toml");
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                tracing::debug!("no config file at {}, using defaults", path.display());
                return Self::default();
            }
            Err(error) => {
                tracing::warn!(
                    "cannot read config {}: {error}, using defaults",
                    path.display()
                );
                return Self::default();
            }
        };
        let document = match toml::from_str::<toml::Value>(&contents) {
            Ok(document) => document,
            Err(error) => {
                tracing::warn!("invalid config {}: {error}, using defaults", path.display());
                return Self::default();
            }
        };
        match document.try_into::<Self>() {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!("invalid config {}: {error}, using defaults", path.display());
                Self::default()
            }
        }
    }

    /// Persist application-owned sections while preserving user content.
    pub fn save(&self, config_dir: &Path) -> Result<(), ConfigSaveError> {
        let path = config_dir.join("config.toml");
        let mut document = match fs::read_to_string(&path) {
            Ok(contents) => contents
                .parse::<DocumentMut>()
                .map_err(|_error| ConfigSaveError::Parse { path: path.clone() })?,
            Err(error) if error.kind() == ErrorKind::NotFound => DocumentMut::new(),
            Err(error) => {
                return Err(ConfigSaveError::Read {
                    path,
                    source: error,
                });
            }
        };
        let owned = [
            ("ui", serialize_config_section(&self.ui)),
            ("general", serialize_config_section(&self.general)),
            ("keys", serialize_config_section(&self.keys)),
            ("playback", serialize_config_section(&self.playback)),
            ("sound", serialize_config_section(&self.sound)),
            ("log", serialize_config_section(&self.log)),
        ];
        let mut serialized = Vec::with_capacity(owned.len());
        for (name, item) in owned {
            match item {
                Ok(item) => serialized.push((name, item)),
                Err(_) => {
                    return Err(ConfigSaveError::Serialize {
                        path: path.clone(),
                        section: name,
                    });
                }
            }
        }
        for (name, item) in serialized {
            merge_owned_section(&mut document, name, item);
        }
        remove_legacy_sort_tracks(&mut document);
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            return Err(ConfigSaveError::CreateDirectory {
                path: parent.to_path_buf(),
                source: error,
            });
        }
        let rendered = document.to_string();
        atomic_replace(&path, rendered.as_bytes())
            .map_err(|source| ConfigSaveError::Write { path, source })
    }
}

fn serialize_config_section<T: Serialize>(value: &T) -> Result<Item, String> {
    let rendered = toml_edit::ser::to_string_pretty(value).map_err(|error| error.to_string())?;
    rendered
        .parse::<DocumentMut>()
        .map(DocumentMut::into_item)
        .map_err(|error| error.to_string())
}

fn merge_owned_section(document: &mut DocumentMut, name: &str, replacement: Item) {
    if let Some(existing) = document.as_table_mut().get_mut(name) {
        merge_item(existing, &replacement);
    } else {
        document.as_table_mut().insert(name, replacement);
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

fn remove_legacy_sort_tracks(document: &mut DocumentMut) {
    if let Some(general) = document
        .as_table_mut()
        .get_mut("general")
        .and_then(Item::as_table_mut)
    {
        general.remove("sort_tracks");
    }
}

#[derive(Clone, Copy)]
pub(crate) struct BundledTheme {
    pub(crate) name: &'static str,
    pub(crate) content: &'static str,
}

const BUNDLED_THEMES: [BundledTheme; 24] = [
    BundledTheme {
        name: "default",
        content: include_str!("../../assets/themes/default.toml"),
    },
    BundledTheme {
        name: "gruvbox",
        content: include_str!("../../assets/themes/gruvbox.toml"),
    },
    BundledTheme {
        name: "catppuccin-mocha",
        content: include_str!("../../assets/themes/catppuccin-mocha.toml"),
    },
    BundledTheme {
        name: "dracula",
        content: include_str!("../../assets/themes/dracula.toml"),
    },
    BundledTheme {
        name: "nord",
        content: include_str!("../../assets/themes/nord.toml"),
    },
    BundledTheme {
        name: "tokyo-night",
        content: include_str!("../../assets/themes/tokyo-night.toml"),
    },
    BundledTheme {
        name: "gruvbox-light",
        content: include_str!("../../assets/themes/gruvbox-light.toml"),
    },
    BundledTheme {
        name: "catppuccin-latte",
        content: include_str!("../../assets/themes/catppuccin-latte.toml"),
    },
    BundledTheme {
        name: "catppuccin-macchiato",
        content: include_str!("../../assets/themes/catppuccin-macchiato.toml"),
    },
    BundledTheme {
        name: "solarized-dark",
        content: include_str!("../../assets/themes/solarized-dark.toml"),
    },
    BundledTheme {
        name: "solarized-light",
        content: include_str!("../../assets/themes/solarized-light.toml"),
    },
    BundledTheme {
        name: "monokai",
        content: include_str!("../../assets/themes/monokai.toml"),
    },
    BundledTheme {
        name: "one-dark",
        content: include_str!("../../assets/themes/one-dark.toml"),
    },
    BundledTheme {
        name: "github-dark",
        content: include_str!("../../assets/themes/github-dark.toml"),
    },
    BundledTheme {
        name: "rose-pine",
        content: include_str!("../../assets/themes/rose-pine.toml"),
    },
    BundledTheme {
        name: "rose-pine-dawn",
        content: include_str!("../../assets/themes/rose-pine-dawn.toml"),
    },
    BundledTheme {
        name: "synthwave-84",
        content: include_str!("../../assets/themes/synthwave-84.toml"),
    },
    BundledTheme {
        name: "kanagawa",
        content: include_str!("../../assets/themes/kanagawa.toml"),
    },
    BundledTheme {
        name: "everforest",
        content: include_str!("../../assets/themes/everforest.toml"),
    },
    BundledTheme {
        name: "palenight",
        content: include_str!("../../assets/themes/palenight.toml"),
    },
    BundledTheme {
        name: "horizon",
        content: include_str!("../../assets/themes/horizon.toml"),
    },
    BundledTheme {
        name: "iceberg",
        content: include_str!("../../assets/themes/iceberg.toml"),
    },
    BundledTheme {
        name: "matrix",
        content: include_str!("../../assets/themes/matrix.toml"),
    },
    BundledTheme {
        name: "moonfly",
        content: include_str!("../../assets/themes/moonfly.toml"),
    },
];

/// List the bundled theme starter files.
pub(crate) fn bundled_themes() -> Vec<BundledTheme> {
    BUNDLED_THEMES.to_vec()
}

fn write_bundled_themes_if_needed(themes_dir: &Path) {
    for theme in bundled_themes() {
        let path = themes_dir.join(format!("{}.{}", theme.name, THEME_FILE_EXTENSION));
        if path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(error) = atomic_replace(&path, theme.content.as_bytes()) {
            tracing::warn!("cannot write bundled theme {}: {error}", path.display());
        }
    }
}

/// Ensure the themes directory exists and ships bundled defaults on first run.
pub fn ensure_themes_dir(config_dir: &Path) -> PathBuf {
    let themes_dir = config_dir.join("themes");
    if let Err(error) = fs::create_dir_all(&themes_dir) {
        tracing::warn!(
            "cannot create themes directory {}: {error}",
            themes_dir.display()
        );
    }
    write_bundled_themes_if_needed(&themes_dir);
    themes_dir
}
