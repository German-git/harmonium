//! Central translation from raw key events to application commands.
//!
//! This module owns normal (non-modal) key bindings. Modal precedence and
//! modal-owned keys live in [`crate::app::modal_router::ModalRouter`].

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::command::Command;
use crate::config::KeysConfig;

/// A key binding accepted by the input grammar.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyChord(String);

impl KeyChord {
    /// Parse and retain a key binding in its canonical trimmed form.
    pub fn new(value: impl Into<String>) -> Result<Self, KeyChordError> {
        let value = value.into();
        let trimmed = value.trim();
        if parse_key_string(trimmed).is_none() {
            return Err(KeyChordError);
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A raw key binding could not be parsed by the input grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChordError;

impl std::fmt::Display for KeyChordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid key binding")
    }
}

impl TryFrom<&str> for KeyChord {
    type Error = KeyChordError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for KeyChord {
    type Error = KeyChordError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for KeyChord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for KeyChord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Snapshot of UI conditions that influence key routing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputContext {
    /// True while the file browser panel holds keyboard focus.
    pub browser_focused: bool,
    /// True while the playlist panel holds keyboard focus.
    pub playlist_focused: bool,
    /// True while the lyrics panel holds keyboard focus.
    pub lyrics_focused: bool,
}

/// A parsed key binding: keycode, modifiers, and the command it triggers.
#[derive(Debug, Clone)]
struct KeyBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
    command: Command,
}

/// Translates key events into commands based on the current context.
///
/// Holds the user-configurable global key bindings built from `KeysConfig`.
/// Panel specific bindings (browser, playlist, dialog) stay hardcoded.
#[derive(Debug)]
pub struct InputMapper {
    /// Configurable global bindings parsed from `KeysConfig`.
    global_bindings: Vec<KeyBinding>,
}

impl Default for InputMapper {
    fn default() -> Self {
        Self::from_config(&KeysConfig::default())
    }
}

impl InputMapper {
    /// Build the mapper from user-configurable key bindings.
    ///
    /// Unknown key strings fall back to the default binding for that action
    /// and emit a warning. Invalid key strings use the default and warn.
    pub fn from_config(keys: &KeysConfig) -> Self {
        let mut global_bindings = Vec::new();

        // Default bindings used as fallback when the user key string is invalid
        let defaults: Vec<(KeyCode, KeyModifiers, Command)> = vec![
            (KeyCode::Char('q'), KeyModifiers::NONE, Command::Quit),
            (KeyCode::Char(' '), KeyModifiers::NONE, Command::TogglePause),
            (KeyCode::Char('n'), KeyModifiers::NONE, Command::NextTrack),
            (
                KeyCode::Char('N'),
                KeyModifiers::SHIFT,
                Command::PreviousTrack,
            ),
            (KeyCode::Char('+'), KeyModifiers::NONE, Command::VolumeUp),
            (KeyCode::Char('-'), KeyModifiers::NONE, Command::VolumeDown),
            (KeyCode::Char('m'), KeyModifiers::NONE, Command::CycleRepeat),
            (
                KeyCode::Char('s'),
                KeyModifiers::NONE,
                Command::ToggleShuffle,
            ),
            (
                KeyCode::Char('L'),
                KeyModifiers::SHIFT,
                Command::ToggleLyrics,
            ),
        ];

        let bindings: Vec<(&str, Command, &str)> = vec![
            (keys.quit.as_str(), Command::Quit, "q"),
            (keys.help.as_str(), Command::ToggleHelp, "ctrl+h"),
            (keys.play_pause.as_str(), Command::TogglePause, "space"),
            (keys.next.as_str(), Command::NextTrack, "n"),
            (keys.previous.as_str(), Command::PreviousTrack, "N"),
            (keys.volume_up.as_str(), Command::VolumeUp, "+"),
            (keys.volume_down.as_str(), Command::VolumeDown, "-"),
            (keys.repeat.as_str(), Command::CycleRepeat, "m"),
            (keys.shuffle.as_str(), Command::ToggleShuffle, "s"),
            (keys.lyrics.as_str(), Command::ToggleLyrics, "shift+L"),
        ];

        for (key_str, command, default_key) in bindings {
            match parse_key_string(key_str) {
                Some((code, modifiers)) => {
                    global_bindings.push(KeyBinding {
                        code,
                        modifiers,
                        command,
                    });
                }
                None => {
                    tracing::warn!(
                        "invalid key string \"{key_str}\" for {command:?}, falling back to \"{default_key}\""
                    );
                    // Use the hardcoded default for this action
                    if let Some(default) = defaults.iter().find(|d| d.2 == command) {
                        global_bindings.push(KeyBinding {
                            code: default.0,
                            modifiers: default.1,
                            command,
                        });
                    }
                }
            }
        }

        Self { global_bindings }
    }

    /// Resolve the command triggered by a non-modal key event, if any.
    ///
    /// Non press events are ignored so terminals that report release or
    /// repeat transitions never trigger a command twice. Modal input is
    /// classified before this mapper is called.
    pub fn map_key(&self, key: KeyEvent, context: InputContext) -> Option<Command> {
        if key.kind != KeyEventKind::Press {
            return None;
        }

        if key.code == KeyCode::Enter {
            if context.playlist_focused {
                return Some(Command::PlaySelected);
            }
            if context.browser_focused {
                return Some(Command::EnterSelected);
            }
        }

        // Check configurable global bindings first, then hardcoded fallbacks
        self.lookup_global(key.code, key.modifiers)
            .or_else(|| hardcoded_global_command(key.code, key.modifiers))
            .or_else(|| {
                if context.browser_focused {
                    browser_command(key.code, key.modifiers)
                } else if context.playlist_focused {
                    playlist_command(key.code, key.modifiers)
                } else if context.lyrics_focused {
                    lyrics_command(key.code, key.modifiers)
                } else {
                    None
                }
            })
            .or_else(|| playlist_management_command(key.code, context))
    }

    /// Linear scan of the configurable binding table.
    ///
    /// A linear scan is fine for ~11 entries and avoids building a HashMap
    /// that would dominate startup for negligible lookup benefit.
    ///
    /// Uppercase letter bindings ignore only SHIFT because terminals disagree
    /// on whether uppercase letters carry that modifier. Other modifiers must
    /// still match the configured binding.
    fn lookup_global(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<Command> {
        self.global_bindings
            .iter()
            .find(|b| {
                if b.code != code {
                    return false;
                }

                if matches!(b.code, KeyCode::Char(c) if c.is_uppercase()) {
                    b.modifiers & !KeyModifiers::SHIFT == modifiers & !KeyModifiers::SHIFT
                } else {
                    b.modifiers == modifiers
                }
            })
            .map(|b| b.command.clone())
    }
}

/// Parse a human-readable key string into a `(KeyCode, KeyModifiers)` pair.
///
/// Accepts:
/// - Single characters: `"q"`, `"+"`, `"["`
/// - Named keys: `"space"`, `"esc"`, `"enter"`, `"tab"`, `"backtab"`,
///   `"pageup"`, `"pagedown"`, `"home"`, `"end"`
/// - Modifier chords: `"ctrl+h"`, `"shift+tab"`, `"alt+x"`
/// - Uppercase letters as shift: `"N"` maps to `(Char('N'), SHIFT)`
///
/// Returns `None` for empty strings or completely invalid input.
fn parse_key_string(s: &str) -> Option<(KeyCode, KeyModifiers)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let mut modifiers = KeyModifiers::NONE;
    let mut key_part = s;

    // Parse modifier prefixes separated by '+', but only when the key part
    // (rightmost segment) is non-empty and there's at least one modifier-like
    // segment to the left. This prevents standalone '+' from being split.
    let parts: Vec<&str> = s.split('+').collect();
    if parts.len() > 1 {
        let potential_key = *parts.last()?;
        let modifier_parts = &parts[..parts.len() - 1];
        // Only treat as modifier chord if the key part is a valid single key
        // and there are modifier-like names to parse
        if !potential_key.is_empty() && !modifier_parts.is_empty() {
            key_part = potential_key;
            for modifier_str in modifier_parts {
                match modifier_str.to_lowercase().as_str() {
                    "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                    "shift" => modifiers |= KeyModifiers::SHIFT,
                    "alt" | "meta" => modifiers |= KeyModifiers::ALT,
                    _ => {
                        tracing::warn!("unknown modifier \"{}\"", modifier_str);
                        return None;
                    }
                }
            }
        }
    }

    // Parse the key part (rightmost segment after modifiers)
    let (code, extra_shift) = match key_part {
        "" => return None,
        "space" => (KeyCode::Char(' '), false),
        "esc" | "escape" => (KeyCode::Esc, false),
        "enter" | "return" => (KeyCode::Enter, false),
        "tab" => (KeyCode::Tab, false),
        "backtab" => (KeyCode::BackTab, false),
        "pageup" | "page_up" => (KeyCode::PageUp, false),
        "pagedown" | "page_down" => (KeyCode::PageDown, false),
        "home" => (KeyCode::Home, false),
        "end" => (KeyCode::End, false),
        "insert" | "ins" => (KeyCode::Insert, false),
        "delete" | "del" => (KeyCode::Delete, false),
        "up" => (KeyCode::Up, false),
        "down" => (KeyCode::Down, false),
        "left" => (KeyCode::Left, false),
        "right" => (KeyCode::Right, false),
        "f1" => (KeyCode::F(1), false),
        "f2" => (KeyCode::F(2), false),
        "f3" => (KeyCode::F(3), false),
        "f4" => (KeyCode::F(4), false),
        "f5" => (KeyCode::F(5), false),
        "f6" => (KeyCode::F(6), false),
        "f7" => (KeyCode::F(7), false),
        "f8" => (KeyCode::F(8), false),
        "f9" => (KeyCode::F(9), false),
        "f10" => (KeyCode::F(10), false),
        "f11" => (KeyCode::F(11), false),
        "f12" => (KeyCode::F(12), false),
        s if s.len() == 1 => {
            let ch = s.chars().next()?;
            // Uppercase letters implicitly carry the shift modifier
            let shift = ch.is_uppercase();
            (KeyCode::Char(ch), shift)
        }
        _ => {
            tracing::warn!("unknown key name \"{key_part}\"");
            return None;
        }
    };

    if extra_shift {
        modifiers |= KeyModifiers::SHIFT;
    }

    // When only shift is held with a non-letter key (like Shift+Tab),
    // crossterm uses BackTab rather than Tab+SHIFT
    if code == KeyCode::Tab && modifiers.contains(KeyModifiers::SHIFT) {
        return Some((KeyCode::BackTab, KeyModifiers::NONE));
    }

    Some((code, modifiers))
}

/// How a reserved key compares terminal-reported modifiers.
///
/// Reserved bindings are not all exact chords: Esc, the printable help
/// fallback and lyrics intentionally accept any modifiers, while Ctrl+H
/// intentionally checks that CONTROL is present. Keeping that policy in the
/// table prevents consumers from silently inventing a different contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReservedMatchPolicy {
    Exact,
    Any,
    Contains,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelpSection {
    Global,
    Navigation,
    Streaming,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpKey {
    Literal(&'static str),
    ConfiguredHelp,
    ConfiguredLyrics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservedHelpId {
    Esc,
    FocusNext,
    FocusPrevious,
    Help,
    Artwork,
    Lyrics,
    Speed,
    SpeedReset,
    AddStream,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReservedHelpRow {
    id: ReservedHelpId,
    section: HelpSection,
    key: HelpKey,
    description: &'static str,
    order: u8,
}

/// One non-configurable key mapping and its cross-surface metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReservedBinding {
    pub(crate) code: KeyCode,
    pub(crate) modifiers: KeyModifiers,
    pub(crate) policy: ReservedMatchPolicy,
    pub(crate) command: Command,
    pub(crate) action: &'static str,
    pub(crate) settings_key: &'static str,
    help: Option<ReservedHelpRow>,
}

impl ReservedBinding {
    fn matches(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        self.code == code
            && match self.policy {
                ReservedMatchPolicy::Exact => self.modifiers == modifiers,
                ReservedMatchPolicy::Any => true,
                ReservedMatchPolicy::Contains => modifiers.contains(self.modifiers),
            }
    }
}

/// Authoritative policy for hardcoded global bindings.
///
/// The order is significant where terminal encodings overlap: Shift+Tab must
/// be considered before the wildcard Tab binding. The settings samples are
/// representative spellings used by validation tests and diagnostics.
pub(crate) const RESERVED: &[ReservedBinding] = &[
    ReservedBinding {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::NONE,
        policy: ReservedMatchPolicy::Any,
        command: Command::CancelPopup,
        action: "cancel",
        settings_key: "esc",
        help: Some(ReservedHelpRow {
            id: ReservedHelpId::Esc,
            section: HelpSection::Global,
            key: HelpKey::Literal("esc"),
            description: "cancel the active popup",
            order: 0,
        }),
    },
    ReservedBinding {
        code: KeyCode::Tab,
        modifiers: KeyModifiers::SHIFT,
        policy: ReservedMatchPolicy::Exact,
        command: Command::FocusPreviousPanel,
        action: "focus previous panel",
        settings_key: "shift+tab",
        help: Some(ReservedHelpRow {
            id: ReservedHelpId::FocusPrevious,
            section: HelpSection::Navigation,
            key: HelpKey::Literal("shift+tab"),
            description: "focus the previous panel",
            order: 1,
        }),
    },
    ReservedBinding {
        code: KeyCode::Tab,
        modifiers: KeyModifiers::NONE,
        policy: ReservedMatchPolicy::Any,
        command: Command::FocusNextPanel,
        action: "focus next panel",
        settings_key: "tab",
        help: Some(ReservedHelpRow {
            id: ReservedHelpId::FocusNext,
            section: HelpSection::Navigation,
            key: HelpKey::Literal("tab"),
            description: "focus the next panel",
            order: 0,
        }),
    },
    ReservedBinding {
        code: KeyCode::BackTab,
        modifiers: KeyModifiers::NONE,
        policy: ReservedMatchPolicy::Any,
        command: Command::FocusPreviousPanel,
        action: "focus previous panel",
        settings_key: "backtab",
        help: None,
    },
    ReservedBinding {
        code: KeyCode::Char('h'),
        modifiers: KeyModifiers::CONTROL,
        policy: ReservedMatchPolicy::Contains,
        command: Command::ToggleHelp,
        action: "toggle help",
        settings_key: "ctrl+h",
        help: Some(ReservedHelpRow {
            id: ReservedHelpId::Help,
            section: HelpSection::Global,
            key: HelpKey::ConfiguredHelp,
            description: "open or close this help",
            order: 4,
        }),
    },
    ReservedBinding {
        code: KeyCode::Char('?'),
        modifiers: KeyModifiers::NONE,
        policy: ReservedMatchPolicy::Any,
        command: Command::ToggleHelp,
        action: "toggle help",
        settings_key: "?",
        help: None,
    },
    ReservedBinding {
        code: KeyCode::Char('z'),
        modifiers: KeyModifiers::NONE,
        policy: ReservedMatchPolicy::Exact,
        command: Command::ToggleArtwork,
        action: "toggle artwork",
        settings_key: "z",
        help: Some(ReservedHelpRow {
            id: ReservedHelpId::Artwork,
            section: HelpSection::Global,
            key: HelpKey::Literal("z"),
            description: "toggle artwork visibility",
            order: 5,
        }),
    },
    ReservedBinding {
        code: KeyCode::Char('L'),
        modifiers: KeyModifiers::NONE,
        policy: ReservedMatchPolicy::Any,
        command: Command::ToggleLyrics,
        action: "toggle lyrics",
        settings_key: "L",
        help: Some(ReservedHelpRow {
            id: ReservedHelpId::Lyrics,
            section: HelpSection::Global,
            key: HelpKey::ConfiguredLyrics,
            description: "toggle the lyrics panel",
            order: 6,
        }),
    },
    ReservedBinding {
        code: KeyCode::Char('['),
        modifiers: KeyModifiers::NONE,
        policy: ReservedMatchPolicy::Exact,
        command: Command::SpeedDown,
        action: "speed down",
        settings_key: "[",
        help: Some(ReservedHelpRow {
            id: ReservedHelpId::Speed,
            section: HelpSection::Global,
            key: HelpKey::Literal("[ / ]"),
            description: "decrease or increase playback speed",
            order: 2,
        }),
    },
    ReservedBinding {
        code: KeyCode::Char(']'),
        modifiers: KeyModifiers::NONE,
        policy: ReservedMatchPolicy::Exact,
        command: Command::SpeedUp,
        action: "speed up",
        settings_key: "]",
        help: None,
    },
    ReservedBinding {
        code: KeyCode::Char('\\'),
        modifiers: KeyModifiers::NONE,
        policy: ReservedMatchPolicy::Exact,
        command: Command::SpeedReset,
        action: "speed reset",
        settings_key: "\\",
        help: Some(ReservedHelpRow {
            id: ReservedHelpId::SpeedReset,
            section: HelpSection::Global,
            key: HelpKey::Literal("\\"),
            description: "reset playback speed to 1.0x",
            order: 3,
        }),
    },
    ReservedBinding {
        code: KeyCode::Char('S'),
        modifiers: KeyModifiers::SHIFT,
        policy: ReservedMatchPolicy::Exact,
        command: Command::AddStream,
        action: "open Add Stream",
        settings_key: "shift+S",
        help: Some(ReservedHelpRow {
            id: ReservedHelpId::AddStream,
            section: HelpSection::Streaming,
            key: HelpKey::Literal("shift+S"),
            description: "open the Add Stream popup for a YouTube, Radio Browser or HTTP URL",
            order: 0,
        }),
    },
    ReservedBinding {
        code: KeyCode::Char('C'),
        modifiers: KeyModifiers::SHIFT,
        policy: ReservedMatchPolicy::Exact,
        command: Command::OpenSettings,
        action: "open settings",
        settings_key: "shift+C",
        help: Some(ReservedHelpRow {
            id: ReservedHelpId::Settings,
            section: HelpSection::Settings,
            key: HelpKey::Literal("shift+C"),
            description: "open the settings screen (esc discards and closes)",
            order: 0,
        }),
    },
];

/// Bindings valid while a modal popup is on screen.
fn hardcoded_global_command(code: KeyCode, modifiers: KeyModifiers) -> Option<Command> {
    RESERVED
        .iter()
        .find(|binding| binding.matches(code, modifiers))
        .map(|binding| binding.command.clone())
}

/// Return the reserved action for a user-entered key spelling, if it matches
/// the same policy used by runtime dispatch.
pub(crate) fn reserved_global_action_for(input: &str) -> Option<&'static str> {
    let (code, modifiers) = parse_key_string(input)?;
    RESERVED
        .iter()
        .find(|binding| binding.matches(code, modifiers))
        .map(|binding| binding.action)
}

/// Resolve a help row from the authoritative reserved-key metadata.
fn reserved_help_row(keys: &KeysConfig, id: ReservedHelpId) -> (String, &'static str) {
    RESERVED
        .iter()
        .filter_map(|binding| binding.help)
        .find(|row| row.id == id)
        .map(|row| {
            let key = match row.key {
                HelpKey::Literal(key) => key.to_string(),
                HelpKey::ConfiguredHelp => format!("{} or ?", display_key(&keys.help)),
                HelpKey::ConfiguredLyrics => display_key(&keys.lyrics),
            };
            (key, row.description)
        })
        .expect("reserved help row must have authoritative metadata")
}

fn reserved_help_rows(keys: &KeysConfig, section: HelpSection) -> Vec<(String, &'static str)> {
    let mut rows: Vec<_> = RESERVED
        .iter()
        .filter_map(|binding| binding.help)
        .filter(|row| row.section == section)
        .map(|row| {
            let key = match row.key {
                HelpKey::Literal(key) => key.to_string(),
                HelpKey::ConfiguredHelp => format!("{} or ?", display_key(&keys.help)),
                HelpKey::ConfiguredLyrics => display_key(&keys.lyrics),
            };
            (row.order, key, row.description)
        })
        .collect();
    rows.sort_by_key(|(order, _, _)| *order);
    rows.into_iter()
        .map(|(_, key, description)| (key, description))
        .collect()
}

/// Named-playlist bindings that work everywhere the queue is the focus.
///
/// `a` is deliberately suppressed while the browser panel is focused so it
/// keeps its long-standing "add selection" meaning; every other playlist
/// management key is global. The modal router runs before this mapper, so this
/// branch is reached only when no popup owns the key.
fn playlist_management_command(code: KeyCode, context: InputContext) -> Option<Command> {
    if context.browser_focused && (code == KeyCode::Char('a') || code == KeyCode::Char('A')) {
        return None;
    }
    match code {
        KeyCode::Char('p') => Some(Command::OpenPlaylistManager),
        KeyCode::Char('r') => Some(Command::RenamePlaylist),
        KeyCode::Char('a') => Some(Command::SaveAsPlaylist),
        KeyCode::Char('A') => Some(Command::NewPlaylist),
        KeyCode::Char('d') => Some(Command::DeletePlaylist),
        KeyCode::Char('x') => Some(Command::DeleteQueueEntry),
        _ => None,
    }
}

/// Browser bindings active only while the file panel holds focus.
///
/// Audited against Termusic v0.13.2 defaults. `v` mark and `a` add are
/// Harmonium extensions documented on the matching commands.
fn browser_command(code: KeyCode, modifiers: KeyModifiers) -> Option<Command> {
    match (code, modifiers) {
        (KeyCode::Char('k') | KeyCode::Up, _) => Some(Command::CursorUp),
        (KeyCode::Char('j') | KeyCode::Down, _) => Some(Command::CursorDown),
        (KeyCode::Char('g') | KeyCode::Home, _) => Some(Command::CursorTop),
        // Capital G arrives with the shift flag on most terminals
        (KeyCode::Char('G'), _) | (KeyCode::End, _) => Some(Command::CursorBottom),
        (KeyCode::PageUp, _) => Some(Command::PageUp),
        (KeyCode::PageDown, _) => Some(Command::PageDown),
        (KeyCode::Char('h') | KeyCode::Left, _) => Some(Command::ParentDir),
        (KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter, _) => Some(Command::EnterSelected),
        (KeyCode::Char('v'), _) => Some(Command::ToggleMark),
        (KeyCode::Char('a'), _) => Some(Command::AddSelected),
        // Uppercase letters keep the lowercase homonyms (r playlist rename,
        // e unused) available, so both file actions stay reachable here
        (KeyCode::Char('R'), _) => Some(Command::RenameFile),
        (KeyCode::Char('E'), _) => Some(Command::EditMetadata),
        (KeyCode::Char('/'), _) => Some(Command::OpenSearch),
        _ => None,
    }
}

/// Playlist bindings active only while the queue panel holds focus.
///
/// Left and right seek the playing track with an adaptive step, while
/// enter or `l` start the selected entry, matching the audited Termusic
/// behavior where these keys never collide with browser navigation
/// because each panel routes its own scope. Cursor moves reuse the shared
/// cursor commands and route by focused panel, capital `J` and `K` swap
/// entries, `d` deletes one entry and `D` clears the queue.
fn playlist_command(code: KeyCode, modifiers: KeyModifiers) -> Option<Command> {
    match (code, modifiers) {
        (KeyCode::Left, _) => Some(Command::SeekBackward),
        (KeyCode::Right, _) => Some(Command::SeekForward),
        (KeyCode::Char('l') | KeyCode::Enter, _) => Some(Command::PlaySelected),
        (KeyCode::Char('k') | KeyCode::Up, _) => Some(Command::CursorUp),
        (KeyCode::Char('j') | KeyCode::Down, _) => Some(Command::CursorDown),
        // Jump and page navigation mirror the browser so both list panels
        // share the same home/end/page vocabulary.
        (KeyCode::Char('g') | KeyCode::Home, _) => Some(Command::CursorTop),
        // Capital G arrives with the shift flag on most terminals
        (KeyCode::Char('G'), _) | (KeyCode::End, _) => Some(Command::CursorBottom),
        (KeyCode::PageUp, _) => Some(Command::PageUp),
        (KeyCode::PageDown, _) => Some(Command::PageDown),
        // Case separates swapping from plain cursor moves, mirroring n/N
        (KeyCode::Char('K'), _) => Some(Command::SwapSelectedUp),
        (KeyCode::Char('J'), _) => Some(Command::SwapSelectedDown),
        // `d` deletes the playing playlist in this context; removing a single
        // queue entry moves to `x` to keep the two deletes distinct
        (KeyCode::Char('d'), _) => Some(Command::DeletePlaylist),
        (KeyCode::Char('x'), _) => Some(Command::DeleteQueueEntry),
        (KeyCode::Char('D'), _) => Some(Command::ClearQueue),
        // File actions on the selected queue entry: uppercase keeps them
        // distinct from the lowercase playlist-management homonyms
        (KeyCode::Char('R'), _) => Some(Command::RenameFile),
        (KeyCode::Char('E'), _) => Some(Command::EditMetadata),
        (KeyCode::Char('/'), _) => Some(Command::OpenSearch),
        _ => None,
    }
}

/// Lyrics bindings active only while the lyrics panel holds focus.
///
/// The vocabulary is the shared scroll set (cursor, pages, jumps, closes via
/// global Esc); nothing here mutates the panels behind it.
fn lyrics_command(code: KeyCode, modifiers: KeyModifiers) -> Option<Command> {
    match (code, modifiers) {
        (KeyCode::Char('k') | KeyCode::Up, _) => Some(Command::CursorUp),
        (KeyCode::Char('j') | KeyCode::Down, _) => Some(Command::CursorDown),
        (KeyCode::Char('g') | KeyCode::Home, _) => Some(Command::CursorTop),
        // Capital G arrives with the shift flag on most terminals
        (KeyCode::Char('G'), _) | (KeyCode::End, _) => Some(Command::CursorBottom),
        (KeyCode::PageUp, _) => Some(Command::PageUp),
        (KeyCode::PageDown, _) => Some(Command::PageDown),
        _ => None,
    }
}

/// One titled group of the keybinding reference.
///
/// Grouping exists because the help popup renders a section per context,
/// while the footer hints and any future consumer share the same rows, so
/// wording can never drift away from the real bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapSection {
    /// Section heading shown in the help popup.
    pub title: &'static str,
    /// `(keys, description)` rows in display order.
    pub rows: Vec<(String, &'static str)>,
}

/// Unstyled help content owned by the application and refreshed with key edits.
///
/// Keeping the display data separate from ratatui styles lets rendering borrow
/// the cached strings while applying the current theme on every frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpContentCache {
    keys: KeysConfig,
    lines: Vec<HelpContentLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpContentLine {
    Header(&'static str),
    Blank,
    Row {
        key: String,
        description: &'static str,
    },
}

impl HelpContentCache {
    pub fn new(keys: &KeysConfig) -> Self {
        Self {
            keys: keys.clone(),
            lines: build_help_content_lines(keys),
        }
    }

    /// Rebuild the content only when the authoritative key configuration changed.
    pub fn refresh(&mut self, keys: &KeysConfig) {
        if self.keys == *keys {
            return;
        }

        self.keys = keys.clone();
        self.lines = build_help_content_lines(keys);
    }

    pub fn lines(&self) -> &[HelpContentLine] {
        &self.lines
    }
}

/// Human readable keybinding reference grouped by context.
///
/// This is the single source of truth for what keys do. The help popup
/// renders these sections verbatim and tests pin both the structure and
/// the row count, so a new binding is only complete once it appears here.
///
/// When a `KeysConfig` is provided, the Global section reflects actual
/// user bindings instead of hardcoded defaults.
pub fn default_keymap_summary(keys: &KeysConfig) -> Vec<KeymapSection> {
    vec![
        KeymapSection {
            title: "Global",
            rows: vec![
                (display_key(&keys.quit), "quit, asks for confirmation"),
                reserved_help_row(keys, ReservedHelpId::Esc),
                (
                    display_key(&keys.play_pause),
                    "toggle pause of the playing track",
                ),
                (display_key(&keys.next), "play the next track"),
                (display_key(&keys.previous), "previous track or restart"),
                (
                    format!(
                        "{} / {}",
                        display_key(&keys.volume_up),
                        display_key(&keys.volume_down)
                    ),
                    "raise or lower the volume",
                ),
                reserved_help_row(keys, ReservedHelpId::Speed),
                reserved_help_row(keys, ReservedHelpId::SpeedReset),
                (display_key(&keys.repeat), "cycle repeat mode off track all"),
                (display_key(&keys.shuffle), "toggle shuffle"),
                reserved_help_row(keys, ReservedHelpId::Help),
                reserved_help_row(keys, ReservedHelpId::Artwork),
                reserved_help_row(keys, ReservedHelpId::Lyrics),
            ],
        },
        KeymapSection {
            title: "Navigation",
            rows: vec![
                reserved_help_row(keys, ReservedHelpId::FocusNext),
                reserved_help_row(keys, ReservedHelpId::FocusPrevious),
                (
                    "j / k or down / up".to_string(),
                    "move the focused list cursor",
                ),
                ("g or home".to_string(), "jump to the first entry"),
                ("G or end".to_string(), "jump to the last entry"),
                ("page up / page down".to_string(), "page the focused list"),
                ("/".to_string(), "search the focused panel"),
            ],
        },
        KeymapSection {
            title: "File Browser",
            rows: vec![
                ("h or left".to_string(), "open the parent directory"),
                (
                    "l, right or enter".to_string(),
                    "open directory or add file",
                ),
                ("v".to_string(), "toggle a mark on the cursor entry"),
                ("a".to_string(), "add marked entries or the cursor entry"),
                ("R".to_string(), "rename the file under the cursor"),
                (
                    "E".to_string(),
                    "edit the metadata tags of the file under the cursor",
                ),
            ],
        },
        KeymapSection {
            title: "Playlist",
            rows: vec![
                ("l or enter".to_string(), "play the selected entry"),
                ("left / right".to_string(), "seek the playing track"),
                ("j / k or up / down".to_string(), "move the selection"),
                ("home / end".to_string(), "jump to the first or last entry"),
                ("page up / page down".to_string(), "page the queue"),
                ("J / K".to_string(), "swap the selected entry down or up"),
                ("x".to_string(), "delete the selected queue entry"),
                ("D".to_string(), "clear the queue"),
                ("R".to_string(), "rename the selected entry's file"),
                ("E".to_string(), "edit the selected entry's metadata tags"),
            ],
        },
        KeymapSection {
            title: "Streaming",
            rows: {
                let mut rows = reserved_help_rows(keys, HelpSection::Streaming);
                rows.extend([
                    (
                        "R".to_string(),
                        "rename a stream entry (edits its display title)",
                    ),
                    ("E".to_string(), "edit the display title of a stream entry"),
                ]);
                rows
            },
        },
        KeymapSection {
            title: "Playlist manager",
            rows: vec![
                ("p".to_string(), "open or close the manager"),
                ("r".to_string(), "rename the active or selected playlist"),
                ("a".to_string(), "save the current playlist with a new name"),
                ("A".to_string(), "create a new empty playlist"),
                ("d".to_string(), "delete the playing or selected playlist"),
                ("x".to_string(), "delete the selected queue entry"),
                ("up / down".to_string(), "move the manager cursor"),
                ("enter".to_string(), "load the selected playlist"),
                ("esc".to_string(), "close the manager"),
            ],
        },
        KeymapSection {
            title: "Popups",
            rows: vec![
                ("y / n".to_string(), "answer the quit confirmation"),
                ("esc, enter or q".to_string(), "close the popup"),
                ("j / k or down / up".to_string(), "scroll the help text"),
                ("page up / page down".to_string(), "page the help text"),
                (
                    "g / G or home / end".to_string(),
                    "jump to the top or bottom",
                ),
            ],
        },
        KeymapSection {
            title: "Settings",
            rows: reserved_help_rows(keys, HelpSection::Settings),
        },
    ]
}

fn build_help_content_lines(keys: &KeysConfig) -> Vec<HelpContentLine> {
    let sections = default_keymap_summary(keys);
    let key_width = sections
        .iter()
        .flat_map(|section| section.rows.iter())
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);

    let mut lines = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        if index > 0 {
            lines.push(HelpContentLine::Blank);
        }
        lines.push(HelpContentLine::Header(section.title));
        for (key, description) in &section.rows {
            lines.push(HelpContentLine::Row {
                key: format!(" {key:<key_width$}"),
                description,
            });
        }
    }
    lines
}

/// Format a key string for display in the help popup.
///
/// The input is the validated key chord from `KeysConfig`.
fn display_key(key: &KeyChord) -> String {
    key.as_str().to_string()
}

/// Total line count of the rendered help content.
///
/// The renderer builds one line per binding row, one header per section
/// and one blank separator between sections. This count is the scroll
/// ceiling used by the popup commands, and a panels test pins that the
/// built lines match it exactly.
pub fn help_line_count(keys: &KeysConfig) -> usize {
    let sections = default_keymap_summary(keys);
    let content: usize = sections.iter().map(|section| section.rows.len() + 1).sum();
    content + sections.len().saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_keys() -> KeysConfig {
        KeysConfig::default()
    }

    fn key(value: &str) -> KeyChord {
        KeyChord::try_from(value).expect("test key must be valid")
    }

    fn mapped(code: KeyCode) -> Option<Command> {
        let mapper = InputMapper::from_config(&default_keys());
        mapper.map_key(
            KeyEvent::new(code, KeyModifiers::NONE),
            InputContext::default(),
        )
    }

    fn mapped_in_browser(code: KeyCode) -> Option<Command> {
        let mapper = InputMapper::from_config(&default_keys());
        mapper.map_key(
            KeyEvent::new(code, KeyModifiers::NONE),
            InputContext {
                browser_focused: true,
                ..InputContext::default()
            },
        )
    }

    fn mapped_in_playlist(code: KeyCode) -> Option<Command> {
        let mapper = InputMapper::from_config(&default_keys());
        mapper.map_key(
            KeyEvent::new(code, KeyModifiers::NONE),
            InputContext {
                playlist_focused: true,
                ..InputContext::default()
            },
        )
    }

    #[test]
    fn q_requests_quit() {
        assert_eq!(mapped(KeyCode::Char('q')), Some(Command::Quit));
    }

    #[test]
    fn escape_cancels_even_without_dialog() {
        assert_eq!(mapped(KeyCode::Esc), Some(Command::CancelPopup));
    }

    #[test]
    fn every_reserved_binding_dispatches_from_the_authoritative_table() {
        let mapper = InputMapper::from_config(&default_keys());

        for binding in RESERVED {
            assert_eq!(
                mapper.map_key(
                    KeyEvent::new(binding.code, binding.modifiers),
                    InputContext::default()
                ),
                Some(binding.command.clone()),
                "{} must dispatch through the reserved table",
                binding.settings_key
            );
        }
    }

    #[test]
    fn tab_moves_focus_forward() {
        assert_eq!(mapped(KeyCode::Tab), Some(Command::FocusNextPanel));
    }

    #[test]
    fn backtab_moves_focus_backward_in_plain_encoding() {
        assert_eq!(mapped(KeyCode::BackTab), Some(Command::FocusPreviousPanel));
    }

    #[test]
    fn shift_tab_moves_focus_backward_in_alternate_encoding() {
        let mapper = InputMapper::from_config(&default_keys());
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);

        assert_eq!(
            mapper.map_key(key, InputContext::default()),
            Some(Command::FocusPreviousPanel)
        );
    }

    #[test]
    fn every_browser_binding_maps_exactly_once() {
        let cases: [(KeyCode, KeyModifiers, Command); 18] = [
            (KeyCode::Char('k'), KeyModifiers::NONE, Command::CursorUp),
            (KeyCode::Up, KeyModifiers::NONE, Command::CursorUp),
            (KeyCode::Char('j'), KeyModifiers::NONE, Command::CursorDown),
            (KeyCode::Down, KeyModifiers::NONE, Command::CursorDown),
            (KeyCode::Char('g'), KeyModifiers::NONE, Command::CursorTop),
            (KeyCode::Home, KeyModifiers::NONE, Command::CursorTop),
            (
                KeyCode::Char('G'),
                KeyModifiers::SHIFT,
                Command::CursorBottom,
            ),
            (KeyCode::End, KeyModifiers::NONE, Command::CursorBottom),
            (KeyCode::PageUp, KeyModifiers::NONE, Command::PageUp),
            (KeyCode::PageDown, KeyModifiers::NONE, Command::PageDown),
            (KeyCode::Char('h'), KeyModifiers::NONE, Command::ParentDir),
            (KeyCode::Left, KeyModifiers::NONE, Command::ParentDir),
            (
                KeyCode::Char('l'),
                KeyModifiers::NONE,
                Command::EnterSelected,
            ),
            (KeyCode::Right, KeyModifiers::NONE, Command::EnterSelected),
            (KeyCode::Enter, KeyModifiers::NONE, Command::EnterSelected),
            (KeyCode::Char('v'), KeyModifiers::NONE, Command::ToggleMark),
            (KeyCode::Char('R'), KeyModifiers::SHIFT, Command::RenameFile),
            (
                KeyCode::Char('E'),
                KeyModifiers::SHIFT,
                Command::EditMetadata,
            ),
        ];

        for (code, modifiers, expected) in cases {
            let mapper = InputMapper::from_config(&default_keys());
            let key = KeyEvent::new(code, modifiers);
            let context = InputContext {
                browser_focused: true,
                ..InputContext::default()
            };

            assert_eq!(
                mapper.map_key(key, context),
                Some(expected.clone()),
                "{code:?} must map to {expected:?} exactly once"
            );
        }
    }

    #[test]
    fn a_adds_selection_in_the_browser_but_saves_as_elsewhere() {
        assert_eq!(
            mapped_in_browser(KeyCode::Char('a')),
            Some(Command::AddSelected)
        );
        assert_eq!(mapped(KeyCode::Char('a')), Some(Command::SaveAsPlaylist));
    }

    #[test]
    fn slash_opens_contextual_search_in_both_list_panels() {
        assert_eq!(
            mapped_in_browser(KeyCode::Char('/')),
            Some(Command::OpenSearch)
        );
        assert_eq!(
            mapped_in_playlist(KeyCode::Char('/')),
            Some(Command::OpenSearch)
        );
        assert_eq!(mapped(KeyCode::Char('/')), None);
    }

    #[test]
    fn browser_bindings_stay_dead_while_the_playlist_is_focused() {
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Char('v'),
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Enter,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ] {
            assert_eq!(
                mapped(code),
                None,
                "{code:?} must stay unbound while the playlist holds focus"
            );
        }
    }

    #[test]
    fn playlist_manager_keys_are_global_outside_the_browser() {
        assert_eq!(
            mapped(KeyCode::Char('p')),
            Some(Command::OpenPlaylistManager)
        );
        assert_eq!(mapped(KeyCode::Char('r')), Some(Command::RenamePlaylist));
        assert_eq!(mapped(KeyCode::Char('a')), Some(Command::SaveAsPlaylist));
        assert_eq!(mapped(KeyCode::Char('d')), Some(Command::DeletePlaylist));
        assert_eq!(mapped(KeyCode::Char('x')), Some(Command::DeleteQueueEntry));
    }

    #[test]
    fn playlist_manager_keys_stay_active_inside_the_browser() {
        // `a` keeps its browser meaning; the rest are global regardless of panel
        assert_eq!(
            mapped_in_browser(KeyCode::Char('a')),
            Some(Command::AddSelected)
        );
        assert_eq!(
            mapped_in_browser(KeyCode::Char('p')),
            Some(Command::OpenPlaylistManager)
        );
        assert_eq!(
            mapped_in_browser(KeyCode::Char('r')),
            Some(Command::RenamePlaylist)
        );
        assert_eq!(
            mapped_in_browser(KeyCode::Char('d')),
            Some(Command::DeletePlaylist)
        );
        assert_eq!(
            mapped_in_browser(KeyCode::Char('x')),
            Some(Command::DeleteQueueEntry)
        );
        assert_eq!(mapped(KeyCode::Char('?')), Some(Command::ToggleHelp));
    }

    #[test]
    fn release_events_are_ignored() {
        let mapper = InputMapper::from_config(&default_keys());
        let released = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );

        assert_eq!(mapper.map_key(released, InputContext::default()), None);
    }

    #[test]
    fn release_events_never_trigger_browser_bindings_either() {
        let mapper = InputMapper::from_config(&default_keys());
        let released = KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        let context = InputContext {
            browser_focused: true,
            ..InputContext::default()
        };

        assert_eq!(mapper.map_key(released, context), None);
    }

    #[test]
    fn keymap_summary_groups_every_default_binding() {
        let keys = default_keys();
        let summary = default_keymap_summary(&keys);

        assert_eq!(
            summary
                .iter()
                .map(|section| section.title)
                .collect::<Vec<_>>(),
            [
                "Global",
                "Navigation",
                "File Browser",
                "Playlist",
                "Streaming",
                "Playlist manager",
                "Popups",
                "Settings"
            ]
        );
        let rows: usize = summary.iter().map(|section| section.rows.len()).sum();
        assert_eq!(rows, 54);
        assert_eq!(summary[0].rows[0].0, "q");
        assert_eq!(summary[0].rows[0].1, "quit, asks for confirmation");
        assert_eq!(summary[0].rows[6].0, "[ / ]");
        assert_eq!(summary[0].rows[6].1, "decrease or increase playback speed");
        assert_eq!(summary[0].rows[7].0, "\\");
        assert_eq!(summary[0].rows[7].1, "reset playback speed to 1.0x");
        assert_eq!(summary[0].rows[10].0, "ctrl+h or ?");
        assert_eq!(summary[0].rows[10].1, "open or close this help");
        assert_eq!(summary[0].rows[11].0, "z");
        assert_eq!(summary[0].rows[11].1, "toggle artwork visibility");
        assert_eq!(summary[0].rows[12].0, "shift+L");
        assert_eq!(summary[0].rows[12].1, "toggle the lyrics panel");
    }

    #[test]
    fn help_line_count_matches_the_rendered_structure() {
        let keys = default_keys();
        assert_eq!(help_line_count(&keys), 69);
    }

    #[test]
    fn help_cache_refreshes_after_a_key_edit_without_changing_row_order() {
        let mut keys = default_keys();
        let mut cache = HelpContentCache::new(&keys);
        let before = cache.lines().to_vec();

        keys.quit = key("Q");
        cache.refresh(&keys);

        assert_ne!(cache.lines(), before.as_slice());
        assert!(matches!(
            cache.lines().get(1),
            Some(HelpContentLine::Row { key, description })
                if key.starts_with(" Q") && *description == "quit, asks for confirmation"
        ));
        assert!(matches!(
            cache.lines().get(15),
            Some(HelpContentLine::Header("Navigation"))
        ));
        let refreshed = cache.lines().to_vec();
        cache.refresh(&keys);
        assert_eq!(cache.lines(), refreshed.as_slice());
    }

    #[test]
    fn space_toggles_pause_in_normal_input_routing() {
        assert_eq!(mapped(KeyCode::Char(' ')), Some(Command::TogglePause));
    }

    #[test]
    fn n_walks_forward_and_capital_n_backward() {
        assert_eq!(mapped(KeyCode::Char('n')), Some(Command::NextTrack));
        assert_eq!(
            mapped_with_modifiers(KeyCode::Char('N'), KeyModifiers::SHIFT),
            Some(Command::PreviousTrack)
        );
        assert_eq!(mapped(KeyCode::Char('N')), Some(Command::PreviousTrack));
    }

    #[test]
    fn plus_and_minus_step_volume_while_equals_stays_unbound() {
        assert_eq!(mapped(KeyCode::Char('+')), Some(Command::VolumeUp));
        assert_eq!(mapped(KeyCode::Char('-')), Some(Command::VolumeDown));
        assert_eq!(mapped(KeyCode::Char('=')), None);
    }

    #[test]
    fn brackets_step_speed_and_backslash_resets_from_every_panel() {
        for map in [mapped, mapped_in_browser, mapped_in_playlist] {
            assert_eq!(map(KeyCode::Char('[')), Some(Command::SpeedDown));
            assert_eq!(map(KeyCode::Char(']')), Some(Command::SpeedUp));
            assert_eq!(map(KeyCode::Char('\\')), Some(Command::SpeedReset));
        }
    }

    #[test]
    fn m_cycles_repeat_and_s_toggles_shuffle_from_every_panel() {
        for map in [mapped, mapped_in_browser, mapped_in_playlist] {
            assert_eq!(map(KeyCode::Char('m')), Some(Command::CycleRepeat));
            assert_eq!(map(KeyCode::Char('s')), Some(Command::ToggleShuffle));
        }
        assert_eq!(
            mapped_in_playlist(KeyCode::Char('p')),
            Some(Command::OpenPlaylistManager)
        );
    }

    #[test]
    fn help_and_artwork_keys_are_global() {
        for map in [mapped, mapped_in_browser, mapped_in_playlist] {
            assert_eq!(map(KeyCode::Char('?')), Some(Command::ToggleHelp));
            assert_eq!(map(KeyCode::Char('z')), Some(Command::ToggleArtwork));
            assert_eq!(map(KeyCode::Char('L')), Some(Command::ToggleLyrics));
        }

        let mapper = InputMapper::from_config(&default_keys());
        let ctrl_h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(
            mapper.map_key(ctrl_h, InputContext::default()),
            Some(Command::ToggleHelp)
        );
        assert_eq!(mapped(KeyCode::Char('h')), None);
        assert_eq!(
            mapped_in_browser(KeyCode::Char('h')),
            Some(Command::ParentDir)
        );
        // Shift+L (or the plain uppercase encoding terminals send) toggles lyrics
        let shift_l = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(
            mapper.map_key(shift_l, InputContext::default()),
            Some(Command::ToggleLyrics)
        );
    }

    #[test]
    fn lyrics_focus_routes_the_scroll_vocabulary() {
        let mapper = InputMapper::from_config(&default_keys());
        let context = InputContext {
            lyrics_focused: true,
            ..InputContext::default()
        };
        let map = |code| mapper.map_key(KeyEvent::new(code, KeyModifiers::NONE), context.clone());

        assert_eq!(map(KeyCode::Char('j')), Some(Command::CursorDown));
        assert_eq!(map(KeyCode::Down), Some(Command::CursorDown));
        assert_eq!(map(KeyCode::Char('k')), Some(Command::CursorUp));
        assert_eq!(map(KeyCode::Up), Some(Command::CursorUp));
        assert_eq!(map(KeyCode::PageDown), Some(Command::PageDown));
        assert_eq!(map(KeyCode::PageUp), Some(Command::PageUp));
        assert_eq!(map(KeyCode::Char('g')), Some(Command::CursorTop));
        assert_eq!(map(KeyCode::Home), Some(Command::CursorTop));
        assert_eq!(map(KeyCode::Char('G')), Some(Command::CursorBottom));
        assert_eq!(map(KeyCode::End), Some(Command::CursorBottom));
        // Playlist-only words must not leak into the lyrics panel; the
        // global playlist-management keys keep their usual meaning.
        assert_eq!(map(KeyCode::Char('D')), None);
        assert_eq!(map(KeyCode::Char('d')), Some(Command::DeletePlaylist));
    }

    #[test]
    fn playlist_focus_routes_cursor_moves_and_queue_management() {
        let cases = [
            (KeyCode::Char('k'), KeyModifiers::NONE, Command::CursorUp),
            (KeyCode::Up, KeyModifiers::NONE, Command::CursorUp),
            (KeyCode::Char('j'), KeyModifiers::NONE, Command::CursorDown),
            (KeyCode::Down, KeyModifiers::NONE, Command::CursorDown),
            (
                KeyCode::Char('K'),
                KeyModifiers::NONE,
                Command::SwapSelectedUp,
            ),
            (
                KeyCode::Char('J'),
                KeyModifiers::SHIFT,
                Command::SwapSelectedDown,
            ),
            (
                KeyCode::Char('x'),
                KeyModifiers::NONE,
                Command::DeleteQueueEntry,
            ),
            (KeyCode::Char('D'), KeyModifiers::SHIFT, Command::ClearQueue),
            (KeyCode::Char('R'), KeyModifiers::SHIFT, Command::RenameFile),
            (
                KeyCode::Char('E'),
                KeyModifiers::SHIFT,
                Command::EditMetadata,
            ),
        ];

        let mapper = InputMapper::from_config(&default_keys());
        let context = InputContext {
            playlist_focused: true,
            ..InputContext::default()
        };

        for (code, modifiers, expected) in cases {
            let key = KeyEvent::new(code, modifiers);
            assert_eq!(
                mapper.map_key(key, context.clone()),
                Some(expected),
                "{code:?} must drive the queue exactly once"
            );
        }
    }

    #[test]
    fn playlist_focus_routes_d_to_delete_the_playing_playlist() {
        let mapper = InputMapper::from_config(&default_keys());
        let context = InputContext {
            playlist_focused: true,
            ..InputContext::default()
        };

        assert_eq!(
            mapper.map_key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
                context
            ),
            Some(Command::DeletePlaylist)
        );
    }

    #[test]
    fn playlist_focus_routes_seek_and_play_selection() {
        assert_eq!(
            mapped_in_playlist(KeyCode::Left),
            Some(Command::SeekBackward)
        );
        assert_eq!(
            mapped_in_playlist(KeyCode::Right),
            Some(Command::SeekForward)
        );
        assert_eq!(
            mapped_in_playlist(KeyCode::Enter),
            Some(Command::PlaySelected)
        );
        assert_eq!(
            mapped_in_playlist(KeyCode::Char('l')),
            Some(Command::PlaySelected)
        );
    }

    #[test]
    fn browser_navigation_keys_stay_dead_on_the_playlist_panel() {
        for code in [KeyCode::Char('h'), KeyCode::Char('v')] {
            assert_eq!(
                mapped_in_playlist(code),
                None,
                "{code:?} must not leak browser behavior into the playlist"
            );
        }
    }

    #[test]
    fn playlist_focus_routes_jump_and_page_navigation() {
        assert_eq!(
            mapped_in_playlist(KeyCode::Char('g')),
            Some(Command::CursorTop)
        );
        assert_eq!(mapped_in_playlist(KeyCode::Home), Some(Command::CursorTop));
        assert_eq!(
            mapped_in_playlist(KeyCode::Char('G')),
            Some(Command::CursorBottom)
        );
        assert_eq!(
            mapped_in_playlist(KeyCode::End),
            Some(Command::CursorBottom)
        );
        assert_eq!(mapped_in_playlist(KeyCode::PageUp), Some(Command::PageUp));
        assert_eq!(
            mapped_in_playlist(KeyCode::PageDown),
            Some(Command::PageDown)
        );
    }

    #[test]
    fn release_events_are_ignored_for_playback_keys_too() {
        let mapper = InputMapper::from_config(&default_keys());
        let released = KeyEvent::new_with_kind(
            KeyCode::Char('n'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );

        assert_eq!(mapper.map_key(released, InputContext::default()), None);
    }

    fn mapped_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> Option<Command> {
        let mapper = InputMapper::from_config(&default_keys());
        mapper.map_key(KeyEvent::new(code, modifiers), InputContext::default())
    }

    // --- Configurable keybinding tests ---

    #[test]
    fn custom_quit_key_replaces_default() {
        let keys = KeysConfig {
            quit: key("Q"),
            ..KeysConfig::default()
        };
        let mapper = InputMapper::from_config(&keys);

        // Custom binding works
        let key = KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT);
        assert_eq!(
            mapper.map_key(key, InputContext::default()),
            Some(Command::Quit)
        );

        // Default 'q' no longer triggers quit
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(mapper.map_key(key, InputContext::default()), None);
    }

    #[test]
    fn custom_play_pause_key() {
        let keys = KeysConfig {
            play_pause: key("enter"),
            ..KeysConfig::default()
        };
        let mapper = InputMapper::from_config(&keys);

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            mapper.map_key(key, InputContext::default()),
            Some(Command::TogglePause)
        );
    }

    #[test]
    fn playlist_enter_precedes_custom_play_pause_binding() {
        let keys = KeysConfig {
            play_pause: key("enter"),
            ..KeysConfig::default()
        };
        let mapper = InputMapper::from_config(&keys);
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            mapper.map_key(
                key,
                InputContext {
                    playlist_focused: true,
                    ..InputContext::default()
                }
            ),
            Some(Command::PlaySelected)
        );
        assert_eq!(
            mapper.map_key(
                key,
                InputContext {
                    browser_focused: true,
                    ..InputContext::default()
                }
            ),
            Some(Command::EnterSelected)
        );
    }

    #[test]
    fn custom_ctrl_modifier_key() {
        let keys = KeysConfig {
            quit: key("ctrl+x"),
            ..KeysConfig::default()
        };
        let mapper = InputMapper::from_config(&keys);

        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(
            mapper.map_key(key, InputContext::default()),
            Some(Command::Quit)
        );
    }

    #[test]
    fn uppercase_custom_binding_accepts_missing_or_reported_shift() {
        let keys = KeysConfig {
            quit: key("Q"),
            ..KeysConfig::default()
        };
        let mapper = InputMapper::from_config(&keys);

        for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            assert_eq!(
                mapper.map_key(
                    KeyEvent::new(KeyCode::Char('Q'), modifiers),
                    InputContext::default()
                ),
                Some(Command::Quit)
            );
        }
    }

    #[test]
    fn uppercase_custom_binding_rejects_unconfigured_control_and_alt() {
        let keys = KeysConfig {
            quit: key("Q"),
            ..KeysConfig::default()
        };
        let mapper = InputMapper::from_config(&keys);

        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            assert_eq!(
                mapper.map_key(
                    KeyEvent::new(KeyCode::Char('Q'), modifiers),
                    InputContext::default()
                ),
                None
            );
        }
    }

    #[test]
    fn uppercase_custom_binding_preserves_configured_modifiers() {
        let keys = KeysConfig {
            quit: key("ctrl+alt+Q"),
            ..KeysConfig::default()
        };
        let mapper = InputMapper::from_config(&keys);
        let configured = KeyModifiers::CONTROL | KeyModifiers::ALT;

        for modifiers in [configured, configured | KeyModifiers::SHIFT] {
            assert_eq!(
                mapper.map_key(
                    KeyEvent::new(KeyCode::Char('Q'), modifiers),
                    InputContext::default()
                ),
                Some(Command::Quit)
            );
        }
    }

    #[test]
    fn invalid_key_chords_are_rejected_before_input_mapping() {
        assert!(KeyChord::try_from("not_a_real_key").is_err());
        assert!(KeyChord::try_from("").is_err());
    }

    #[test]
    fn parse_key_string_handles_all_named_keys() {
        assert_eq!(
            parse_key_string("space"),
            Some((KeyCode::Char(' '), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("esc"),
            Some((KeyCode::Esc, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("enter"),
            Some((KeyCode::Enter, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("tab"),
            Some((KeyCode::Tab, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("backtab"),
            Some((KeyCode::BackTab, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("pageup"),
            Some((KeyCode::PageUp, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("pagedown"),
            Some((KeyCode::PageDown, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("home"),
            Some((KeyCode::Home, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("end"),
            Some((KeyCode::End, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("up"),
            Some((KeyCode::Up, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("down"),
            Some((KeyCode::Down, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("left"),
            Some((KeyCode::Left, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("right"),
            Some((KeyCode::Right, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("f1"),
            Some((KeyCode::F(1), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("f12"),
            Some((KeyCode::F(12), KeyModifiers::NONE))
        );
    }

    #[test]
    fn parse_key_string_handles_modifiers() {
        assert_eq!(
            parse_key_string("ctrl+h"),
            Some((KeyCode::Char('h'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key_string("shift+tab"),
            Some((KeyCode::BackTab, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("alt+x"),
            Some((KeyCode::Char('x'), KeyModifiers::ALT))
        );
        assert_eq!(
            parse_key_string("ctrl+shift+a"),
            Some((
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ))
        );
    }

    #[test]
    fn parse_key_string_handles_single_chars() {
        assert_eq!(
            parse_key_string("q"),
            Some((KeyCode::Char('q'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("+"),
            Some((KeyCode::Char('+'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key_string("["),
            Some((KeyCode::Char('['), KeyModifiers::NONE))
        );
    }

    #[test]
    fn parse_key_string_uppercase_implies_shift() {
        assert_eq!(
            parse_key_string("N"),
            Some((KeyCode::Char('N'), KeyModifiers::SHIFT))
        );
    }

    #[test]
    fn parse_key_string_empty_and_invalid() {
        assert_eq!(parse_key_string(""), None);
        assert_eq!(parse_key_string("   "), None);
        assert_eq!(parse_key_string("unknownkey"), None);
        assert_eq!(parse_key_string("ctrl+"), None);
    }

    #[test]
    fn keymap_summary_reflects_custom_bindings() {
        let keys = KeysConfig {
            quit: key("Q"),
            play_pause: key("enter"),
            ..KeysConfig::default()
        };
        let summary = default_keymap_summary(&keys);

        assert_eq!(summary[0].rows[0].0, "Q");
        assert_eq!(summary[0].rows[2].0, "enter");
    }

    #[test]
    fn reserved_help_metadata_is_rendered_verbatim() {
        let keys = default_keys();
        let summary = default_keymap_summary(&keys);

        for binding in RESERVED {
            let Some(help) = binding.help else {
                continue;
            };
            let title = match help.section {
                HelpSection::Global => "Global",
                HelpSection::Navigation => "Navigation",
                HelpSection::Streaming => "Streaming",
                HelpSection::Settings => "Settings",
            };
            let expected = reserved_help_row(&keys, help.id);
            assert!(
                summary
                    .iter()
                    .find(|section| section.title == title)
                    .is_some_and(|section| section.rows.contains(&expected)),
                "reserved help row for {} must be derived into {title}",
                binding.settings_key
            );
        }

        assert!(summary[4].rows.contains(&(
            "shift+S".to_string(),
            "open the Add Stream popup for a YouTube, Radio Browser or HTTP URL"
        )));
        assert_eq!(summary[7].rows[0].0, "shift+C");
    }
}
