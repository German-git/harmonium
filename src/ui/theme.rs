//! Central color palette consumed by every rendering routine.
//!
//! Panels must never reference raw colors, they read exclusively from this
//! struct so a future TOML theme loader can swap the palette in one place.
//! The defaults are temporary until theme files land in phase 7.

use std::fs;
use std::io;
use std::path::Path;

use ratatui::style::Color;
use ratatui::widgets::BorderType as RatatuiBorderType;
use serde::{Deserialize, Serialize};

use crate::filesystem::persistence::atomic_replace;
use crate::filesystem::safety::is_valid_path_component;

/// File extension of a theme document.
pub(crate) const THEME_FILE_EXTENSION: &str = "toml";

/// Whether `name` is a safe theme file name.
///
/// The name becomes part of a path (`themes/<name>.toml`), so it must be a
/// single path component: no separators, no dot segments, no control
/// characters. This keeps a hand-edited `config.toml` (or a hostile theme
/// name) from escaping the themes directory via path traversal, and keeps
/// terminal control bytes out of the file name (they would otherwise land in
/// the `themes/` listing and the settings UI).
pub fn valid_theme_name(name: &str) -> bool {
    is_valid_path_component(name)
}

/// TOML representation of the `[colors]` section in a theme file.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ThemeFile {
    colors: ThemeColors,
}

impl ThemeFile {
    /// Apply valid, non-empty values from `overlay` without losing whether a
    /// field was explicitly configured.
    fn merge_from(&mut self, overlay: &Self) {
        self.colors.merge_from(&overlay.colors);
    }

    fn validate(&self) -> Result<(), ThemeColorParseError> {
        self.colors.validate()
    }
}

/// One editable theme color field in the canonical Appearance order.
///
/// The five lyrics fields intentionally remain part of this list even when
/// their values are empty: an empty value means that the renderer inherits its
/// effective role at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeColorField {
    Background,
    Foreground,
    TextMuted,
    Border,
    BorderFocused,
    Selection,
    Highlight,
    Error,
    Warning,
    Success,
    StatusLine,
    Playing,
    Paused,
    Stopped,
    Progress,
    ArtworkArea,
    Separator,
    PopupBorder,
    TimeText,
    LyricsText,
    LyricsHighlight,
    LyricsBackground,
    LyricsBorder,
    LyricsBorderFocused,
}

impl ThemeColorField {
    /// All editable color fields in the order shown by the settings UI.
    pub const ALL: [Self; 24] = [
        Self::Background,
        Self::Foreground,
        Self::TextMuted,
        Self::Border,
        Self::BorderFocused,
        Self::Selection,
        Self::Highlight,
        Self::Error,
        Self::Warning,
        Self::Success,
        Self::StatusLine,
        Self::Playing,
        Self::Paused,
        Self::Stopped,
        Self::Progress,
        Self::ArtworkArea,
        Self::Separator,
        Self::PopupBorder,
        Self::TimeText,
        Self::LyricsText,
        Self::LyricsHighlight,
        Self::LyricsBackground,
        Self::LyricsBorder,
        Self::LyricsBorderFocused,
    ];

    /// Convert a UI cursor into a field without selecting a different field.
    pub fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    /// Zero-based position in [`Self::ALL`].
    pub const fn index(self) -> usize {
        match self {
            Self::Background => 0,
            Self::Foreground => 1,
            Self::TextMuted => 2,
            Self::Border => 3,
            Self::BorderFocused => 4,
            Self::Selection => 5,
            Self::Highlight => 6,
            Self::Error => 7,
            Self::Warning => 8,
            Self::Success => 9,
            Self::StatusLine => 10,
            Self::Playing => 11,
            Self::Paused => 12,
            Self::Stopped => 13,
            Self::Progress => 14,
            Self::ArtworkArea => 15,
            Self::Separator => 16,
            Self::PopupBorder => 17,
            Self::TimeText => 18,
            Self::LyricsText => 19,
            Self::LyricsHighlight => 20,
            Self::LyricsBackground => 21,
            Self::LyricsBorder => 22,
            Self::LyricsBorderFocused => 23,
        }
    }

    /// TOML/UI label for this field.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::Foreground => "foreground",
            Self::TextMuted => "text_muted",
            Self::Border => "border",
            Self::BorderFocused => "border_focused",
            Self::Selection => "selection",
            Self::Highlight => "highlight",
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Success => "success",
            Self::StatusLine => "status_line",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Progress => "progress",
            Self::ArtworkArea => "artwork_area",
            Self::Separator => "separator",
            Self::PopupBorder => "popup_border",
            Self::TimeText => "time_text",
            Self::LyricsText => "lyrics_text",
            Self::LyricsHighlight => "lyrics_highlight",
            Self::LyricsBackground => "lyrics_background",
            Self::LyricsBorder => "lyrics_border",
            Self::LyricsBorderFocused => "lyrics_border_focused",
        }
    }

    /// Read the value owned by this field.
    pub fn get(self, colors: &ThemeColors) -> &str {
        match self {
            Self::Background => &colors.background,
            Self::Foreground => &colors.foreground,
            Self::TextMuted => &colors.text_muted,
            Self::Border => &colors.border,
            Self::BorderFocused => &colors.border_focused,
            Self::Selection => &colors.selection,
            Self::Highlight => &colors.highlight,
            Self::Error => &colors.error,
            Self::Warning => &colors.warning,
            Self::Success => &colors.success,
            Self::StatusLine => &colors.status_line,
            Self::Playing => &colors.playing,
            Self::Paused => &colors.paused,
            Self::Stopped => &colors.stopped,
            Self::Progress => &colors.progress,
            Self::ArtworkArea => &colors.artwork_area,
            Self::Separator => &colors.separator,
            Self::PopupBorder => &colors.popup_border,
            Self::TimeText => &colors.time_text,
            Self::LyricsText => &colors.lyrics_text,
            Self::LyricsHighlight => &colors.lyrics_highlight,
            Self::LyricsBackground => &colors.lyrics_background,
            Self::LyricsBorder => &colors.lyrics_border,
            Self::LyricsBorderFocused => &colors.lyrics_border_focused,
        }
    }

    /// Replace the value owned by this field.
    pub fn set(self, colors: &mut ThemeColors, value: String) {
        *match self {
            Self::Background => &mut colors.background,
            Self::Foreground => &mut colors.foreground,
            Self::TextMuted => &mut colors.text_muted,
            Self::Border => &mut colors.border,
            Self::BorderFocused => &mut colors.border_focused,
            Self::Selection => &mut colors.selection,
            Self::Highlight => &mut colors.highlight,
            Self::Error => &mut colors.error,
            Self::Warning => &mut colors.warning,
            Self::Success => &mut colors.success,
            Self::StatusLine => &mut colors.status_line,
            Self::Playing => &mut colors.playing,
            Self::Paused => &mut colors.paused,
            Self::Stopped => &mut colors.stopped,
            Self::Progress => &mut colors.progress,
            Self::ArtworkArea => &mut colors.artwork_area,
            Self::Separator => &mut colors.separator,
            Self::PopupBorder => &mut colors.popup_border,
            Self::TimeText => &mut colors.time_text,
            Self::LyricsText => &mut colors.lyrics_text,
            Self::LyricsHighlight => &mut colors.lyrics_highlight,
            Self::LyricsBackground => &mut colors.lyrics_background,
            Self::LyricsBorder => &mut colors.lyrics_border,
            Self::LyricsBorderFocused => &mut colors.lyrics_border_focused,
        } = value;
    }
}

/// All 24 theme color fields in editable TOML format.
///
/// Every field is a plain color string (named, indexed or `#rrggbb`) so the
/// settings UI can edit them directly and serialize them back verbatim. An
/// empty string is treated as "unset" and falls back to the built-in default
/// when parsed, which keeps a partially edited theme valid. The five
/// `lyrics_*` keys are the exception: when unset they keep inheriting their
/// role from the effective theme at render time instead of a fixed default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ThemeColors {
    /// Terminal or fill color behind all content.
    pub background: String,
    /// Primary foreground color for regular text.
    pub foreground: String,
    /// Dimmed foreground for hints and secondary labels.
    pub text_muted: String,
    /// Border color of unfocused blocks.
    pub border: String,
    /// Border color of the focused block.
    pub border_focused: String,
    /// Background used to mark selected rows.
    pub selection: String,
    /// Accent used to highlight the playing entry.
    pub highlight: String,
    /// Error messages and destructive actions.
    pub error: String,
    /// Warnings and cautionary prompts.
    pub warning: String,
    /// Success confirmations.
    pub success: String,
    /// Base color of the status line values.
    pub status_line: String,
    /// State color while a track plays.
    pub playing: String,
    /// State color while playback is paused.
    pub paused: String,
    /// State color while playback is stopped.
    pub stopped: String,
    /// Fill color of the playback progress gauge.
    pub progress: String,
    /// Background of the now playing band and its artwork cell.
    pub artwork_area: String,
    /// Thin separators inside composite widgets.
    pub separator: String,
    /// Border color of modal popups.
    pub popup_border: String,
    /// Color of the time text overlaid on the progress bar.
    pub time_text: String,
    /// Regular lyric text; unset keeps the plain and muted text roles.
    pub lyrics_text: String,
    /// Foreground of the reached karaoke line; unset keeps the highlight role.
    pub lyrics_highlight: String,
    /// Lyrics panel background; unset keeps the global background.
    pub lyrics_background: String,
    /// Lyrics panel border in both focus states; unset keeps the border roles.
    pub lyrics_border: String,
    /// Lyrics panel border while focused; unset follows the lyrics_border
    /// chain (see [`Theme::lyrics_border_focused`]).
    pub lyrics_border_focused: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid theme color `{field}`: {value}")]
pub struct ThemeColorParseError {
    pub field: &'static str,
    pub value: String,
}

/// A theme file failed at the filesystem or strict color-parsing boundary.
#[derive(Debug, thiserror::Error)]
pub enum ThemeLoadError {
    #[error("cannot read theme {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid theme {path}: {source}")]
    Parse {
        path: std::path::PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid color in theme {path}: {source}")]
    Color {
        path: std::path::PathBuf,
        #[source]
        source: ThemeColorParseError,
    },
    #[error("invalid theme name: {0}")]
    InvalidName(String),
}

/// Replaceable boundary for theme persistence and loading.
pub trait ThemeRepository: Send + Sync {
    fn load(&self, themes_dir: &Path, name: &str) -> Result<ThemeColors, ThemeLoadError>;
    fn save(&self, themes_dir: &Path, name: &str, colors: &ThemeColors) -> io::Result<()>;
}

/// Production filesystem implementation used by [`crate::runtime::AppServices`].
#[derive(Debug, Default)]
pub struct FileThemeRepository;

impl ThemeRepository for FileThemeRepository {
    fn load(&self, themes_dir: &Path, name: &str) -> Result<ThemeColors, ThemeLoadError> {
        Theme::load(themes_dir, name).map(|theme| theme.to_colors())
    }

    fn save(&self, themes_dir: &Path, name: &str, colors: &ThemeColors) -> io::Result<()> {
        write_theme_file(themes_dir, name, colors)
    }
}

/// Built in palette with ANSI friendly defaults.
///
/// The five `lyrics_*` slots are `Option` because they override several
/// roles at once when set (`lyrics_text` covers the plain and the muted
/// lyric colors, `lyrics_border` both focus states unless
/// `lyrics_border_focused` narrows the focused one) and must keep rendering
/// the effective theme's roles untouched when unset. `None` defers that
/// choice to render time, so a gruvbox theme without lyrics keys still shows
/// gruvbox's own palette, never the built-in defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Border glyph style shared by every bordered widget.
    pub border_type: RatatuiBorderType,
    /// Terminal or fill color behind all content.
    pub background: Color,
    /// Primary foreground color for regular text.
    pub text: Color,
    /// Dimmed foreground for hints and secondary labels.
    pub text_muted: Color,
    /// Border color of unfocused blocks.
    pub border: Color,
    /// Border color of the focused block.
    pub border_focused: Color,
    /// Background used to mark selected rows.
    pub selection: Color,
    /// Accent used to highlight the playing entry.
    pub highlight: Color,
    /// Error messages and destructive actions.
    pub error: Color,
    /// Warnings and cautionary prompts.
    pub warning: Color,
    /// Success confirmations.
    pub success: Color,
    /// Base color of the status line values.
    pub status_line: Color,
    /// State color while a track plays.
    pub playing: Color,
    /// State color while playback is paused.
    pub paused: Color,
    /// State color while playback is stopped.
    pub stopped: Color,
    /// Fill color of the playback progress gauge.
    pub progress: Color,
    /// Background of the now playing band and its artwork cell.
    pub artwork_area: Color,
    /// Thin separators inside composite widgets.
    pub separator: Color,
    /// Border color of modal popups.
    pub popup_border: Color,
    /// Color of the time text overlaid on the progress bar.
    pub time_text: Color,
    /// Override for regular lyric text; `None` keeps the per-context roles.
    pub lyrics_text: Option<Color>,
    /// Override for the reached karaoke line; `None` keeps `highlight`.
    pub lyrics_highlight: Option<Color>,
    /// Override for the lyrics panel background; `None` keeps `background`.
    pub lyrics_background: Option<Color>,
    /// Override for the lyrics panel border; `None` keeps the border roles.
    pub lyrics_border: Option<Color>,
    /// Focused lyrics border; precedence when focused is
    /// `lyrics_border_focused` → `lyrics_border` → `border_focused`.
    pub lyrics_border_focused: Option<Color>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            border_type: RatatuiBorderType::Plain,
            background: Color::Reset,
            text: Color::White,
            text_muted: Color::DarkGray,
            border: Color::DarkGray,
            border_focused: Color::Cyan,
            selection: Color::Blue,
            highlight: Color::Yellow,
            error: Color::Red,
            warning: Color::Yellow,
            success: Color::Green,
            status_line: Color::White,
            playing: Color::Green,
            paused: Color::Yellow,
            stopped: Color::DarkGray,
            progress: Color::Cyan,
            artwork_area: Color::Black,
            separator: Color::DarkGray,
            popup_border: Color::Cyan,
            time_text: Color::White,
            // Unset: the lyrics panel keeps inheriting the effective theme's
            // text/highlight/background/border roles, byte-identical to the
            // rendering before these keys existed.
            lyrics_text: None,
            lyrics_highlight: None,
            lyrics_background: None,
            lyrics_border: None,
            lyrics_border_focused: None,
        }
    }
}

/// Parse a color string into a ratatui Color.
///
/// Accepts named colors, indexed colors (0-255), and hex strings (#rrggbb).
/// Returns None on invalid input so callers can fall back gracefully.
fn color_from_str(s: &str) -> Option<Color> {
    // ratatui's Color::FromStr handles named, indexed, and hex colors
    s.parse().ok()
}

/// Merge an optional TOML color value over a base color.
fn merge_color(base: Color, override_color: Option<&str>) -> Color {
    override_color.and_then(color_from_str).unwrap_or(base)
}

/// Treat an empty color string as unset so partial themes keep defaults.
fn color_opt(s: &str) -> Option<&str> {
    if s.is_empty() { None } else { Some(s) }
}

/// Apply valid, non-empty color strings from `overlay` over this file layer.
fn merge_theme_color(base: &mut String, overlay: &str) {
    if color_opt(overlay).and_then(color_from_str).is_some() {
        *base = overlay.to_string();
    }
}

impl ThemeColors {
    fn validate(&self) -> Result<(), ThemeColorParseError> {
        for field in ThemeColorField::ALL {
            let value = field.get(self);
            if !value.trim().is_empty() && color_from_str(value).is_none() {
                return Err(ThemeColorParseError {
                    field: field.label(),
                    value: value.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Merge a file layer while retaining explicit values equal to defaults.
    fn merge_from(&mut self, overlay: &Self) {
        macro_rules! merge_field {
            ($field:ident) => {
                merge_theme_color(&mut self.$field, &overlay.$field);
            };
        }

        merge_field!(background);
        merge_field!(foreground);
        merge_field!(text_muted);
        merge_field!(border);
        merge_field!(border_focused);
        merge_field!(selection);
        merge_field!(highlight);
        merge_field!(error);
        merge_field!(warning);
        merge_field!(success);
        merge_field!(status_line);
        merge_field!(playing);
        merge_field!(paused);
        merge_field!(stopped);
        merge_field!(progress);
        merge_field!(artwork_area);
        merge_field!(separator);
        merge_field!(popup_border);
        merge_field!(time_text);
        merge_field!(lyrics_text);
        merge_field!(lyrics_highlight);
        merge_field!(lyrics_background);
        merge_field!(lyrics_border);
        merge_field!(lyrics_border_focused);
    }
}

/// List every theme name available in `themes_dir`, sorted and deduplicated.
///
/// Reads the file stems of `*.toml` entries when the directory exists and
/// always includes the bundled `default` and `gruvbox` names so the settings
/// UI never shows an empty list, even before the first run writes them.
pub fn list_theme_names(themes_dir: &Path) -> Vec<String> {
    let mut names = vec!["default".to_string(), "gruvbox".to_string()];

    if let Ok(entries) = fs::read_dir(themes_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && !stem.is_empty()
            {
                names.push(stem.to_string());
            }
        }
    }

    names.sort();
    names.dedup();
    names
}

/// Re-insert every table in a TOML value with its keys sorted.
///
/// The `toml` crate serializes structs in field declaration order, so the
/// document is rebuilt from a `toml::Value` and the entries are re-inserted
/// in sorted order. This is map-backend agnostic (indexmap keeps insertion
/// order, a BTreeMap would already be sorted), and it recurses so any future
/// nested sections are sorted too.
fn sort_toml_tables(value: &mut toml::Value) {
    if let toml::Value::Table(table) = value {
        let mut entries: Vec<(String, toml::Value)> = std::mem::take(table).into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, mut child) in entries {
            sort_toml_tables(&mut child);
            table.insert(key, child);
        }
    }
}

/// Serialize a TOML value before any destination file is touched.
fn serialize_sorted_toml<T: Serialize>(value: &T) -> std::io::Result<String> {
    let mut value = toml::Value::try_from(value).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("could not serialize theme: {error}"),
        )
    })?;
    sort_toml_tables(&mut value);
    toml::to_string(&value).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("could not serialize theme: {error}"),
        )
    })
}

/// Write a theme's `[colors]` section to `themes/<name>.toml`.
///
/// The themes directory is created on demand. Failures are reported through
/// the `io::Result` so the caller can notify the user. The name is validated
/// first so a hostile value cannot escape the themes directory; the caller is
/// responsible for avoiding bundled file names (see the settings apply path).
///
/// Theme files are written with alphabetically sorted color keys, so saves
/// are deterministic and diffs stay stable. This file order is deliberately
/// independent of the settings editor, which lists the fields grouped by role
/// (see [`ThemeColorField::ALL`]). Every key is always emitted; unset colors
/// serialize as the empty string so inheritance round-trips.
pub fn write_theme_file(
    themes_dir: &Path,
    name: &str,
    colors: &ThemeColors,
) -> std::io::Result<()> {
    if !valid_theme_name(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid theme name: {name}"),
        ));
    }
    let file = ThemeFile {
        colors: colors.clone(),
    };
    // Serialization through a Value plus the sorted rebuild is what keeps the
    // `[colors]` section alphabetical on disk; to_string already terminates
    // the document with a newline.
    let contents = serialize_sorted_toml(&file)?;
    fs::create_dir_all(themes_dir)?;
    let path = themes_dir.join(format!("{name}.{THEME_FILE_EXTENSION}"));
    atomic_replace(&path, contents.as_bytes())
}

impl Theme {
    /// Parse every configured color before constructing a runtime theme.
    /// Unlike the compatibility `from_colors` helper, this boundary never
    /// silently replaces a user's invalid value with a default.
    pub fn try_from_colors(colors: ThemeColors) -> Result<Self, ThemeColorParseError> {
        colors.validate()?;
        Ok(Self::from_colors(colors))
    }

    /// Strictly load a theme by name from the themes directory, with fallback chain:
    /// 1. Built-in defaults
    /// 2. themes/default.toml (if exists, merged over built-in)
    /// 3. themes/<name>.toml (merged over step 2)
    ///
    /// Missing files retain the built-in/default layers, while malformed TOML,
    /// invalid colors, and unsafe names are returned to the caller.
    pub fn load(themes_dir: &Path, theme_name: &str) -> Result<Self, ThemeLoadError> {
        let mut layers = ThemeFile::default();

        // Layer 2: merge default.toml if it exists
        let default_path = themes_dir.join("default.toml");
        if default_path.exists() {
            let overlay = Self::load_from_file(&default_path)?;
            layers.merge_from(&overlay);
        }

        // Layer 3: merge the user-selected theme. An invalid name (from a
        // hand-edited config.toml) must not build a traversal path; degrade to
        // the built-in defaults we already loaded.
        if theme_name != "default" && valid_theme_name(theme_name) {
            let theme_path = themes_dir.join(format!("{theme_name}.{THEME_FILE_EXTENSION}"));
            if theme_path.exists() {
                let overlay = Self::load_from_file(&theme_path)?;
                layers.merge_from(&overlay);
            } else {
                tracing::warn!(
                    "theme file {} not found, using previous defaults",
                    theme_path.display()
                );
            }
        } else if theme_name != "default" {
            return Err(ThemeLoadError::InvalidName(theme_name.to_string()));
        }

        Ok(Self::default().merged_with_file(&layers))
    }

    /// Load and strictly validate one theme file.
    fn load_from_file(path: &Path) -> Result<ThemeFile, ThemeLoadError> {
        let contents = fs::read_to_string(path).map_err(|source| ThemeLoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let file: ThemeFile =
            toml::from_str(&contents).map_err(|source| ThemeLoadError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        file.validate().map_err(|source| ThemeLoadError::Color {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(file)
    }

    /// Parse a TOML string into a Theme, overlaying on defaults.
    ///
    /// Only fields present in the `[colors]` section are overridden,
    /// missing fields keep their built-in default values.
    pub fn load_from_toml(toml_str: &str) -> Result<Self, toml::de::Error> {
        let file: ThemeFile = toml::from_str(toml_str)?;
        let base = Self::default();
        Ok(base.merged_with_file(&file))
    }

    /// Build a preview [`Theme`] from the editable color strings.
    ///
    /// Invalid color strings fall back to the built-in default for that slot,
    /// so a half-edited theme still renders instead of erroring out. The
    /// `lyrics_*` slots have no fixed default: an empty or invalid string
    /// leaves them `None`, which makes the renderer inherit the effective
    /// theme's roles.
    pub fn from_colors(colors: ThemeColors) -> Self {
        let base = Self::default();
        let c = &colors;
        Self {
            border_type: base.border_type,
            background: color_from_str(&c.background).unwrap_or(base.background),
            text: color_from_str(&c.foreground).unwrap_or(base.text),
            text_muted: color_from_str(&c.text_muted).unwrap_or(base.text_muted),
            border: color_from_str(&c.border).unwrap_or(base.border),
            border_focused: color_from_str(&c.border_focused).unwrap_or(base.border_focused),
            selection: color_from_str(&c.selection).unwrap_or(base.selection),
            highlight: color_from_str(&c.highlight).unwrap_or(base.highlight),
            error: color_from_str(&c.error).unwrap_or(base.error),
            warning: color_from_str(&c.warning).unwrap_or(base.warning),
            success: color_from_str(&c.success).unwrap_or(base.success),
            status_line: color_from_str(&c.status_line).unwrap_or(base.status_line),
            playing: color_from_str(&c.playing).unwrap_or(base.playing),
            paused: color_from_str(&c.paused).unwrap_or(base.paused),
            stopped: color_from_str(&c.stopped).unwrap_or(base.stopped),
            progress: color_from_str(&c.progress).unwrap_or(base.progress),
            artwork_area: color_from_str(&c.artwork_area).unwrap_or(base.artwork_area),
            separator: color_from_str(&c.separator).unwrap_or(base.separator),
            popup_border: color_from_str(&c.popup_border).unwrap_or(base.popup_border),
            time_text: color_from_str(&c.time_text).unwrap_or(base.time_text),
            lyrics_text: color_opt(&c.lyrics_text).and_then(color_from_str),
            lyrics_highlight: color_opt(&c.lyrics_highlight).and_then(color_from_str),
            lyrics_background: color_opt(&c.lyrics_background).and_then(color_from_str),
            lyrics_border: color_opt(&c.lyrics_border).and_then(color_from_str),
            lyrics_border_focused: color_opt(&c.lyrics_border_focused).and_then(color_from_str),
        }
    }

    /// Serialize the current palette back into editable color strings.
    ///
    /// Used by the Appearance tab to populate the draft and to write the
    /// `[colors]` section of a theme file. `Color` renders as a round-trippable
    /// string (named or `#rrggbb`), so re-parsing yields an identical theme.
    /// Saving a loaded theme flattens the global slots to their resolved
    /// values; the `lyrics_*` slots are the exception because the parsed
    /// palette stores them as `Option`: unset stays the empty string and only
    /// an explicit override is written.
    pub fn to_colors(&self) -> ThemeColors {
        let lyric_color = |color: Option<Color>| color.map(|c| c.to_string()).unwrap_or_default();
        ThemeColors {
            background: self.background.to_string(),
            foreground: self.text.to_string(),
            text_muted: self.text_muted.to_string(),
            border: self.border.to_string(),
            border_focused: self.border_focused.to_string(),
            selection: self.selection.to_string(),
            highlight: self.highlight.to_string(),
            error: self.error.to_string(),
            warning: self.warning.to_string(),
            success: self.success.to_string(),
            status_line: self.status_line.to_string(),
            playing: self.playing.to_string(),
            paused: self.paused.to_string(),
            stopped: self.stopped.to_string(),
            progress: self.progress.to_string(),
            artwork_area: self.artwork_area.to_string(),
            separator: self.separator.to_string(),
            popup_border: self.popup_border.to_string(),
            time_text: self.time_text.to_string(),
            lyrics_text: lyric_color(self.lyrics_text),
            lyrics_highlight: lyric_color(self.lyrics_highlight),
            lyrics_background: lyric_color(self.lyrics_background),
            lyrics_border: lyric_color(self.lyrics_border),
            lyrics_border_focused: lyric_color(self.lyrics_border_focused),
        }
    }

    /// Apply the color overrides from a parsed theme file over self.
    fn merged_with_file(self, file: &ThemeFile) -> Self {
        let c = &file.colors;
        Self {
            border_type: self.border_type,
            background: merge_color(self.background, color_opt(&c.background)),
            text: merge_color(self.text, color_opt(&c.foreground)),
            text_muted: merge_color(self.text_muted, color_opt(&c.text_muted)),
            border: merge_color(self.border, color_opt(&c.border)),
            border_focused: merge_color(self.border_focused, color_opt(&c.border_focused)),
            selection: merge_color(self.selection, color_opt(&c.selection)),
            highlight: merge_color(self.highlight, color_opt(&c.highlight)),
            error: merge_color(self.error, color_opt(&c.error)),
            warning: merge_color(self.warning, color_opt(&c.warning)),
            success: merge_color(self.success, color_opt(&c.success)),
            status_line: merge_color(self.status_line, color_opt(&c.status_line)),
            playing: merge_color(self.playing, color_opt(&c.playing)),
            paused: merge_color(self.paused, color_opt(&c.paused)),
            stopped: merge_color(self.stopped, color_opt(&c.stopped)),
            progress: merge_color(self.progress, color_opt(&c.progress)),
            artwork_area: merge_color(self.artwork_area, color_opt(&c.artwork_area)),
            separator: merge_color(self.separator, color_opt(&c.separator)),
            popup_border: merge_color(self.popup_border, color_opt(&c.popup_border)),
            time_text: merge_color(self.time_text, color_opt(&c.time_text)),
            // An unset or invalid lyrics key keeps whatever a lower layer set;
            // when no layer sets it the slot stays None and the renderer
            // inherits this theme's own resolved roles.
            lyrics_text: color_opt(&c.lyrics_text)
                .and_then(color_from_str)
                .or(self.lyrics_text),
            lyrics_highlight: color_opt(&c.lyrics_highlight)
                .and_then(color_from_str)
                .or(self.lyrics_highlight),
            lyrics_background: color_opt(&c.lyrics_background)
                .and_then(color_from_str)
                .or(self.lyrics_background),
            lyrics_border: color_opt(&c.lyrics_border)
                .and_then(color_from_str)
                .or(self.lyrics_border),
            lyrics_border_focused: color_opt(&c.lyrics_border_focused)
                .and_then(color_from_str)
                .or(self.lyrics_border_focused),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_dir;

    #[test]
    fn default_theme_uses_expected_base_colors() {
        let theme = Theme::default();

        assert_eq!(theme.background, Color::Reset);
        assert_eq!(theme.error, Color::Red);
        assert_eq!(theme.success, Color::Green);
        assert_eq!(theme.stopped, Color::DarkGray);
        assert_eq!(theme.progress, Color::Cyan);
        // Focused elements must be distinguishable from unfocused ones
        assert_ne!(theme.border, theme.border_focused);
        assert_ne!(theme.playing, theme.stopped);
    }

    #[test]
    fn color_from_str_parses_named_colors() {
        assert_eq!(color_from_str("red"), Some(Color::Red));
        assert_eq!(color_from_str("cyan"), Some(Color::Cyan));
        assert_eq!(color_from_str("dark_gray"), Some(Color::DarkGray));
        assert_eq!(color_from_str("black"), Some(Color::Black));
        assert_eq!(color_from_str("white"), Some(Color::White));
    }

    #[test]
    fn color_from_str_parses_hex_colors() {
        assert_eq!(color_from_str("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(color_from_str("#282828"), Some(Color::Rgb(40, 40, 40)));
        assert_eq!(color_from_str("#ebdbb2"), Some(Color::Rgb(235, 219, 178)));
    }

    #[test]
    fn color_from_str_parses_indexed_colors() {
        assert_eq!(color_from_str("0"), Some(Color::Indexed(0)));
        assert_eq!(color_from_str("255"), Some(Color::Indexed(255)));
    }

    #[test]
    fn color_from_str_rejects_invalid_input() {
        assert_eq!(color_from_str("not_a_color"), None);
        assert_eq!(color_from_str(""), None);
    }

    #[test]
    fn load_from_toml_applies_all_provided_colors() {
        let toml = "\
[colors]
background = \"#282828\"
foreground = \"#ebdbb2\"
border = \"#928374\"
border_focused = \"#d65d0e\"
selection = \"#504945\"
highlight = \"#d79921\"
error = \"#cc241d\"
warning = \"#d79921\"
success = \"#98971a\"
status_line = \"#ebdbb2\"
playing = \"#98971a\"
paused = \"#d79921\"
stopped = \"#928374\"
progress = \"#458588\"
artwork_area = \"#282828\"
separator = \"#928374\"
popup_border = \"#458588\"
text_muted = \"#a89984\"
time_text = \"#ebdbb2\"
lyrics_text = \"#b8bb26\"
lyrics_highlight = \"#fabd2f\"
lyrics_background = \"#1d2021\"
lyrics_border = \"#fe8019\"
lyrics_border_focused = \"#fabd2f\"
";

        let theme = Theme::load_from_toml(toml).expect("valid toml");

        assert_eq!(theme.background, Color::Rgb(40, 40, 40));
        assert_eq!(theme.text, Color::Rgb(235, 219, 178));
        assert_eq!(theme.border, Color::Rgb(146, 131, 116));
        assert_eq!(theme.border_focused, Color::Rgb(214, 93, 14));
        assert_eq!(theme.error, Color::Rgb(204, 36, 29));
        assert_eq!(theme.popup_border, Color::Rgb(69, 133, 136));
        assert_eq!(theme.lyrics_text, Some(Color::Rgb(184, 187, 38)));
        assert_eq!(theme.lyrics_highlight, Some(Color::Rgb(250, 189, 47)));
        assert_eq!(theme.lyrics_background, Some(Color::Rgb(29, 32, 33)));
        assert_eq!(theme.lyrics_border, Some(Color::Rgb(254, 128, 25)));
        assert_eq!(theme.lyrics_border_focused, Some(Color::Rgb(250, 189, 47)));
    }

    #[test]
    fn load_from_toml_partial_fields_keep_defaults() {
        let toml = "\
[colors]
background = \"#000000\"
error = \"#ff0000\"
";

        let theme = Theme::load_from_toml(toml).expect("valid toml");

        assert_eq!(theme.background, Color::Rgb(0, 0, 0));
        assert_eq!(theme.error, Color::Rgb(255, 0, 0));
        // Unset fields keep defaults
        assert_eq!(theme.text, Color::White);
        assert_eq!(theme.success, Color::Green);
    }

    #[test]
    fn load_from_toml_invalid_toml_returns_error() {
        let result = Theme::load_from_toml("not valid toml {{{");
        assert!(result.is_err());
    }

    #[test]
    fn load_theme_from_directory_with_user_theme() {
        let root = unique_temp_dir("theme-load");
        let themes_dir = root.join("themes");
        fs::create_dir_all(&themes_dir).expect("dir");

        // Write a custom theme
        fs::write(
            themes_dir.join("custom.toml"),
            "[colors]\nbackground = \"#111111\"\nforeground = \"#eeeeee\"\n",
        )
        .expect("theme fixture");

        let theme = Theme::load(&themes_dir, "custom").expect("valid theme");

        assert_eq!(theme.background, Color::Rgb(17, 17, 17));
        assert_eq!(theme.text, Color::Rgb(238, 238, 238));
    }

    #[test]
    fn load_theme_missing_name_warns_and_uses_defaults() {
        let root = unique_temp_dir("theme-missing");
        let themes_dir = root.join("themes");
        fs::create_dir_all(&themes_dir).expect("dir");

        let theme = Theme::load(&themes_dir, "nonexistent").expect("missing theme uses defaults");

        assert_eq!(theme, Theme::default());
    }

    #[test]
    fn load_theme_default_name_only_loads_default_toml() {
        let root = unique_temp_dir("theme-default-name");
        let themes_dir = root.join("themes");
        fs::create_dir_all(&themes_dir).expect("dir");

        fs::write(
            themes_dir.join("default.toml"),
            "[colors]\nbackground = \"#000001\"\n",
        )
        .expect("theme fixture");

        let theme = Theme::load(&themes_dir, "default").expect("valid default theme");

        assert_eq!(theme.background, Color::Rgb(0, 0, 1));
    }

    #[test]
    fn theme_file_layers_apply_explicit_values() {
        let root = unique_temp_dir("theme-layering");
        let themes_dir = root.join("themes");
        fs::create_dir_all(&themes_dir).expect("dir");
        fs::write(
            themes_dir.join("default.toml"),
            "[colors]\nbackground = \"black\"\nerror = \"magenta\"\n",
        )
        .expect("default fixture");
        fs::write(
            themes_dir.join("custom.toml"),
            "[colors]\nforeground = \"#ebdbb2\"\n",
        )
        .expect("custom fixture");

        let merged = Theme::load(&themes_dir, "custom").expect("valid layered theme");

        assert_eq!(merged.background, Color::Black);
        assert_eq!(merged.text, Color::Rgb(235, 219, 178));
        assert_eq!(merged.error, Color::Magenta);
    }

    #[test]
    fn explicitly_configured_default_value_overrides_a_lower_layer() {
        let root = unique_temp_dir("theme-explicit-default");
        let themes_dir = root.join("themes");
        fs::create_dir_all(&themes_dir).expect("dir");
        fs::write(
            themes_dir.join("default.toml"),
            "[colors]\nhighlight = \"red\"\n",
        )
        .expect("default fixture");
        fs::write(
            themes_dir.join("custom.toml"),
            "[colors]\nhighlight = \"yellow\"\n",
        )
        .expect("custom fixture");

        let theme = Theme::load(&themes_dir, "custom").expect("valid layered theme");

        assert_eq!(theme.highlight, Color::Yellow);
    }

    #[test]
    fn theme_color_labels_cover_every_editable_field() {
        assert_eq!(ThemeColorField::ALL.len(), 24);
        assert_eq!(ThemeColorField::ALL[19].label(), "lyrics_text");
        assert_eq!(ThemeColorField::ALL[20].label(), "lyrics_highlight");
        assert_eq!(ThemeColorField::ALL[21].label(), "lyrics_background");
        assert_eq!(ThemeColorField::ALL[22].label(), "lyrics_border");
        assert_eq!(ThemeColorField::ALL[23].label(), "lyrics_border_focused");
    }

    #[test]
    fn theme_color_fields_round_trip_without_numeric_fallbacks() {
        let mut colors = ThemeColors::default();
        for (index, field) in ThemeColorField::ALL.into_iter().enumerate() {
            field.set(&mut colors, format!("color-{index}"));
            assert_eq!(field.get(&colors), format!("color-{index}"));
            assert_eq!(ThemeColorField::from_index(field.index()), Some(field));
        }

        assert_eq!(
            ThemeColorField::from_index(ThemeColorField::ALL.len()),
            None
        );
        assert_eq!(ThemeColorField::from_index(usize::MAX), None);
    }

    #[test]
    fn default_theme_leaves_the_lyrics_slots_unset() {
        let theme = Theme::default();

        // Unset means "inherit the effective roles at render time", never a
        // frozen copy of the built-in palette.
        assert_eq!(theme.lyrics_text, None);
        assert_eq!(theme.lyrics_highlight, None);
        assert_eq!(theme.lyrics_background, None);
        assert_eq!(theme.lyrics_border, None);
        assert_eq!(theme.lyrics_border_focused, None);
    }

    #[test]
    fn load_from_toml_partial_fields_keep_lyrics_slots_unset() {
        // The gruvbox palette without any lyrics keys: the slots stay unset so
        // the renderer shows gruvbox's own text_muted/highlight, not the
        // built-in defaults.
        let toml = "\
[colors]
background = \"#282828\"
foreground = \"#ebdbb2\"
text_muted = \"#a89984\"
highlight = \"#d79921\"
";

        let theme = Theme::load_from_toml(toml).expect("valid toml");

        assert_eq!(theme.text_muted, Color::Rgb(168, 153, 132));
        assert_eq!(theme.lyrics_text, None);
        assert_eq!(theme.lyrics_highlight, None);
        assert_eq!(theme.lyrics_background, None);
        assert_eq!(theme.lyrics_border, None);
        assert_eq!(theme.lyrics_border_focused, None);
    }

    #[test]
    fn from_colors_maps_empty_or_invalid_lyrics_strings_to_unset() {
        let colors = ThemeColors {
            lyrics_text: "".to_string(),
            lyrics_highlight: "not_a_color".to_string(),
            lyrics_background: "#1d2021".to_string(),
            lyrics_border: "200".to_string(),
            ..ThemeColors::default()
        };

        let theme = Theme::from_colors(colors);

        assert_eq!(theme.lyrics_text, None, "empty string must stay unset");
        assert_eq!(
            theme.lyrics_highlight, None,
            "an invalid string must stay unset"
        );
        assert_eq!(theme.lyrics_background, Some(Color::Rgb(29, 32, 33)));
        assert_eq!(theme.lyrics_border, Some(Color::Indexed(200)));
    }

    #[test]
    fn try_from_colors_surfaces_invalid_user_colors() {
        let colors = ThemeColors {
            progress: "definitely-not-a-color".to_string(),
            ..ThemeColors::default()
        };
        let error = Theme::try_from_colors(colors).expect_err("invalid color must be surfaced");
        assert_eq!(error.field, "progress");
    }

    #[test]
    fn progress_and_border_focused_remain_distinct_theme_roles() {
        let colors = ThemeColors {
            progress: "red".to_string(),
            border_focused: "blue".to_string(),
            ..ThemeColors::default()
        };
        let theme = Theme::try_from_colors(colors).expect("valid colors");
        assert_eq!(theme.progress, Color::Red);
        assert_eq!(theme.border_focused, Color::Blue);
    }

    #[test]
    fn lyrics_colors_round_trip_through_write_and_load() {
        let root = unique_temp_dir("theme-lyrics-round-trip");
        let themes_dir = root.join("themes");

        let mut colors = Theme::default().to_colors();
        colors.lyrics_text = "#b8bb26".to_string();
        colors.lyrics_highlight = "magenta".to_string();
        colors.lyrics_border_focused = "200".to_string();
        write_theme_file(&themes_dir, "mine", &colors).expect("write");

        let loaded = Theme::load(&themes_dir, "mine").expect("valid saved theme");
        assert_eq!(loaded.lyrics_text, Some(Color::Rgb(184, 187, 38)));
        assert_eq!(loaded.lyrics_highlight, Some(Color::Magenta));
        assert_eq!(loaded.lyrics_border_focused, Some(Color::Indexed(200)));
        // Unset lyrics slots serialize as the empty string and reload unset,
        // so saving never freezes an inherited value into the file.
        assert_eq!(loaded.lyrics_background, None);
        assert_eq!(loaded.lyrics_border, None);

        let written = std::fs::read_to_string(themes_dir.join("mine.toml")).expect("read");
        assert!(
            written.contains("lyrics_background = \"\""),
            "unset must serialize as the empty string: {written}"
        );
    }

    #[test]
    fn default_toml_lyrics_layer_is_inherited_by_themes_without_the_key() {
        let root = unique_temp_dir("theme-lyrics-layering");
        let themes_dir = root.join("themes");
        fs::create_dir_all(&themes_dir).expect("dir");

        fs::write(
            themes_dir.join("default.toml"),
            "[colors]\ntext_muted = \"#a89984\"\nlyrics_highlight = \"green\"\n",
        )
        .expect("fixture");
        fs::write(
            themes_dir.join("quiet.toml"),
            "[colors]\nbackground = \"#282828\"\n",
        )
        .expect("fixture");
        fs::write(
            themes_dir.join("loud.toml"),
            "[colors]\nlyrics_highlight = \"red\"\n",
        )
        .expect("fixture");

        // A theme without the key keeps the default.toml override.
        let quiet = Theme::load(&themes_dir, "quiet").expect("valid quiet theme");
        assert_eq!(quiet.lyrics_highlight, Some(Color::Green));

        // A theme that sets the key overrides the default.toml layer.
        let loud = Theme::load(&themes_dir, "loud").expect("valid loud theme");
        assert_eq!(loud.lyrics_highlight, Some(Color::Red));
    }

    #[test]
    fn theme_file_layers_override_and_inherit_lyrics_slots() {
        let mut layers = ThemeFile {
            colors: ThemeColors {
                lyrics_text: "green".to_string(),
                lyrics_border: "blue".to_string(),
                ..ThemeColors::default()
            },
        };
        let overlay = ThemeFile {
            colors: ThemeColors {
                lyrics_text: "red".to_string(),
                ..ThemeColors::default()
            },
        };
        layers.merge_from(&overlay);

        let merged = Theme::default().merged_with_file(&layers);

        assert_eq!(merged.lyrics_text, Some(Color::Red));
        assert_eq!(
            merged.lyrics_border,
            Some(Color::Blue),
            "an unset overlay slot must keep the base override"
        );
    }

    #[test]
    fn serialize_sorted_toml_propagates_conversion_failures() {
        struct FailingSerialize;

        impl Serialize for FailingSerialize {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("injected serialization failure"))
            }
        }

        let error = serialize_sorted_toml(&FailingSerialize).expect_err("serialization must fail");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("injected serialization failure"));
    }

    #[test]
    fn valid_theme_name_accepts_plain_single_components() {
        assert!(valid_theme_name("default"));
        assert!(valid_theme_name("gruvbox"));
        assert!(valid_theme_name("my-theme"));
        assert!(valid_theme_name("日本語-🎵"));
    }

    #[test]
    fn valid_theme_name_rejects_traversal_and_control() {
        assert!(!valid_theme_name(""));
        assert!(!valid_theme_name("   "));
        assert!(!valid_theme_name(" leading"));
        assert!(!valid_theme_name("trailing "));
        assert!(!valid_theme_name("."));
        assert!(!valid_theme_name(".."));
        assert!(!valid_theme_name(".hidden"));
        assert!(!valid_theme_name("../etc"));
        assert!(!valid_theme_name("a/b"));
        assert!(!valid_theme_name("a\\b"));
        assert!(!valid_theme_name("bad\tname"));
        assert!(!valid_theme_name("bad\nname"));
        assert!(!valid_theme_name("bad\0name"));
    }

    #[test]
    fn theme_storage_uses_the_shared_name_policy() {
        let root = crate::test_support::unique_temp_dir("theme-invalid");
        for bad in [
            "",
            "   ",
            ".",
            "..",
            ".hidden",
            "../escape",
            "a/b",
            "a\\b",
            "bad\tname",
            "bad\nname",
            "bad\0name",
        ] {
            let error = write_theme_file(&root, bad, &ThemeColors::default())
                .expect_err("invalid name must fail");
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::InvalidInput,
                "name {bad:?}"
            );
        }

        write_theme_file(&root, "日本語-🎵", &ThemeColors::default()).expect("unicode name");
        assert!(root.join("日本語-🎵.toml").is_file());
    }

    #[test]
    fn write_theme_file_sorts_the_color_keys_alphabetically() {
        let root = unique_temp_dir("theme-sorted");
        // A half-filled draft whose declaration order is deliberately not
        // alphabetical: the file on disk must still come out sorted.
        let colors = ThemeColors {
            background: "black".to_string(),
            time_text: "white".to_string(),
            lyrics_highlight: "cyan".to_string(),
            artwork_area: "black".to_string(),
            ..ThemeColors::default()
        };
        write_theme_file(&root, "sorted", &colors).expect("write");

        let contents = fs::read_to_string(root.join("sorted.toml")).expect("read");
        let keys: Vec<&str> = contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('['))
            .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim()))
            .collect();
        let mut expected: Vec<&str> = ThemeColorField::ALL
            .into_iter()
            .map(ThemeColorField::label)
            .collect();
        expected.sort();
        assert_eq!(
            keys, expected,
            "every key must be emitted once, in alphabetical order, in the raw bytes: {contents}"
        );
    }
}
