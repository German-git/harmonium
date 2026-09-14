//! Configurable key bindings and their settings rows.

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

use crate::input::{KeyChord, KeyChordError};

fn deserialize_key_or_default<'de, D>(
    deserializer: D,
    field: &'static str,
    default: &'static str,
) -> Result<KeyChord, D::Error>
where
    D: Deserializer<'de>,
{
    match KeyChord::deserialize(deserializer) {
        Ok(key) => Ok(key),
        Err(_) => {
            tracing::warn!(field, "invalid key binding; using the default");
            Ok(KeyChord::try_from(default).expect("default key is valid"))
        }
    }
}

macro_rules! key_deserializer {
    ($name:ident, $field:literal, $default:literal) => {
        fn $name<'de, D>(deserializer: D) -> Result<KeyChord, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserialize_key_or_default(deserializer, $field, $default)
        }
    };
}

key_deserializer!(deserialize_or_quit_default, "quit", "q");
key_deserializer!(deserialize_or_help_default, "help", "ctrl+h");
key_deserializer!(deserialize_or_play_pause_default, "play_pause", "space");
key_deserializer!(deserialize_or_next_default, "next", "n");
key_deserializer!(deserialize_or_previous_default, "previous", "N");
key_deserializer!(deserialize_or_volume_up_default, "volume_up", "+");
key_deserializer!(deserialize_or_volume_down_default, "volume_down", "-");
key_deserializer!(deserialize_or_repeat_default, "repeat", "m");
key_deserializer!(deserialize_or_shuffle_default, "shuffle", "s");
key_deserializer!(deserialize_or_cancel_default, "cancel", "esc");
key_deserializer!(deserialize_or_toggle_artwork_default, "toggle_artwork", "z");
key_deserializer!(deserialize_or_lyrics_default, "lyrics", "shift+L");

/// A live configurable global key binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyBindingField {
    Quit,
    Help,
    PlayPause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    Repeat,
    Shuffle,
    Lyrics,
}

impl KeyBindingField {
    pub const ALL: [Self; 10] = [
        Self::Quit,
        Self::Help,
        Self::PlayPause,
        Self::Next,
        Self::Previous,
        Self::VolumeUp,
        Self::VolumeDown,
        Self::Repeat,
        Self::Shuffle,
        Self::Lyrics,
    ];

    pub fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    pub const fn index(self) -> usize {
        match self {
            Self::Quit => 0,
            Self::Help => 1,
            Self::PlayPause => 2,
            Self::Next => 3,
            Self::Previous => 4,
            Self::VolumeUp => 5,
            Self::VolumeDown => 6,
            Self::Repeat => 7,
            Self::Shuffle => 8,
            Self::Lyrics => 9,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Help => "help",
            Self::PlayPause => "play_pause",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::VolumeUp => "volume_up",
            Self::VolumeDown => "volume_down",
            Self::Repeat => "repeat",
            Self::Shuffle => "shuffle",
            Self::Lyrics => "lyrics",
        }
    }

    pub fn get(self, keys: &KeysConfig) -> &str {
        match self {
            Self::Quit => keys.quit.as_str(),
            Self::Help => keys.help.as_str(),
            Self::PlayPause => keys.play_pause.as_str(),
            Self::Next => keys.next.as_str(),
            Self::Previous => keys.previous.as_str(),
            Self::VolumeUp => keys.volume_up.as_str(),
            Self::VolumeDown => keys.volume_down.as_str(),
            Self::Repeat => keys.repeat.as_str(),
            Self::Shuffle => keys.shuffle.as_str(),
            Self::Lyrics => keys.lyrics.as_str(),
        }
    }

    pub fn set(self, keys: &mut KeysConfig, value: String) -> Result<(), KeyChordError> {
        let value = KeyChord::try_from(value)?;
        *match self {
            Self::Quit => &mut keys.quit,
            Self::Help => &mut keys.help,
            Self::PlayPause => &mut keys.play_pause,
            Self::Next => &mut keys.next,
            Self::Previous => &mut keys.previous,
            Self::VolumeUp => &mut keys.volume_up,
            Self::VolumeDown => &mut keys.volume_down,
            Self::Repeat => &mut keys.repeat,
            Self::Shuffle => &mut keys.shuffle,
            Self::Lyrics => &mut keys.lyrics,
        } = value;
        Ok(())
    }
}

/// Rows shown by the Keys settings tab, including legacy `cancel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySettingsRow {
    Quit,
    Cancel,
    PlayPause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    Repeat,
    Shuffle,
    Lyrics,
}

impl KeySettingsRow {
    pub const ALL: [Self; 10] = [
        Self::Quit,
        Self::Cancel,
        Self::PlayPause,
        Self::Next,
        Self::Previous,
        Self::VolumeUp,
        Self::VolumeDown,
        Self::Repeat,
        Self::Shuffle,
        Self::Lyrics,
    ];

    pub fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    pub const fn index(self) -> usize {
        match self {
            Self::Quit => 0,
            Self::Cancel => 1,
            Self::PlayPause => 2,
            Self::Next => 3,
            Self::Previous => 4,
            Self::VolumeUp => 5,
            Self::VolumeDown => 6,
            Self::Repeat => 7,
            Self::Shuffle => 8,
            Self::Lyrics => 9,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Cancel => "cancel",
            Self::PlayPause => "play_pause",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::VolumeUp => "volume_up",
            Self::VolumeDown => "volume_down",
            Self::Repeat => "repeat",
            Self::Shuffle => "shuffle",
            Self::Lyrics => "lyrics",
        }
    }

    pub fn get(self, keys: &KeysConfig) -> &str {
        match self {
            Self::Cancel => keys.cancel.as_str(),
            Self::Quit => KeyBindingField::Quit.get(keys),
            Self::PlayPause => KeyBindingField::PlayPause.get(keys),
            Self::Next => KeyBindingField::Next.get(keys),
            Self::Previous => KeyBindingField::Previous.get(keys),
            Self::VolumeUp => KeyBindingField::VolumeUp.get(keys),
            Self::VolumeDown => KeyBindingField::VolumeDown.get(keys),
            Self::Repeat => KeyBindingField::Repeat.get(keys),
            Self::Shuffle => KeyBindingField::Shuffle.get(keys),
            Self::Lyrics => KeyBindingField::Lyrics.get(keys),
        }
    }

    pub fn set(self, keys: &mut KeysConfig, value: String) -> Result<(), KeyChordError> {
        match self {
            Self::Cancel => keys.cancel = KeyChord::try_from(value)?,
            Self::Quit => KeyBindingField::Quit.set(keys, value)?,
            Self::PlayPause => KeyBindingField::PlayPause.set(keys, value)?,
            Self::Next => KeyBindingField::Next.set(keys, value)?,
            Self::Previous => KeyBindingField::Previous.set(keys, value)?,
            Self::VolumeUp => KeyBindingField::VolumeUp.set(keys, value)?,
            Self::VolumeDown => KeyBindingField::VolumeDown.set(keys, value)?,
            Self::Repeat => KeyBindingField::Repeat.set(keys, value)?,
            Self::Shuffle => KeyBindingField::Shuffle.set(keys, value)?,
            Self::Lyrics => KeyBindingField::Lyrics.set(keys, value)?,
        }
        Ok(())
    }

    pub const fn live_field(self) -> Option<KeyBindingField> {
        match self {
            Self::Cancel => None,
            Self::Quit => Some(KeyBindingField::Quit),
            Self::PlayPause => Some(KeyBindingField::PlayPause),
            Self::Next => Some(KeyBindingField::Next),
            Self::Previous => Some(KeyBindingField::Previous),
            Self::VolumeUp => Some(KeyBindingField::VolumeUp),
            Self::VolumeDown => Some(KeyBindingField::VolumeDown),
            Self::Repeat => Some(KeyBindingField::Repeat),
            Self::Shuffle => Some(KeyBindingField::Shuffle),
            Self::Lyrics => Some(KeyBindingField::Lyrics),
        }
    }
}

/// Configurable global key bindings under `[keys]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeysConfig {
    #[serde(deserialize_with = "deserialize_or_quit_default")]
    pub quit: KeyChord,
    #[serde(deserialize_with = "deserialize_or_help_default")]
    pub help: KeyChord,
    #[serde(deserialize_with = "deserialize_or_play_pause_default")]
    pub play_pause: KeyChord,
    #[serde(deserialize_with = "deserialize_or_next_default")]
    pub next: KeyChord,
    #[serde(deserialize_with = "deserialize_or_previous_default")]
    pub previous: KeyChord,
    #[serde(deserialize_with = "deserialize_or_volume_up_default")]
    pub volume_up: KeyChord,
    #[serde(deserialize_with = "deserialize_or_volume_down_default")]
    pub volume_down: KeyChord,
    #[serde(deserialize_with = "deserialize_or_repeat_default")]
    pub repeat: KeyChord,
    #[serde(deserialize_with = "deserialize_or_shuffle_default")]
    pub shuffle: KeyChord,
    #[serde(deserialize_with = "deserialize_or_cancel_default")]
    pub cancel: KeyChord,
    #[serde(deserialize_with = "deserialize_or_toggle_artwork_default")]
    pub toggle_artwork: KeyChord,
    #[serde(deserialize_with = "deserialize_or_lyrics_default")]
    pub lyrics: KeyChord,
}

impl Default for KeysConfig {
    fn default() -> Self {
        Self {
            quit: KeyChord::try_from("q").expect("default key is valid"),
            help: KeyChord::try_from("ctrl+h").expect("default key is valid"),
            play_pause: KeyChord::try_from("space").expect("default key is valid"),
            next: KeyChord::try_from("n").expect("default key is valid"),
            previous: KeyChord::try_from("N").expect("default key is valid"),
            volume_up: KeyChord::try_from("+").expect("default key is valid"),
            volume_down: KeyChord::try_from("-").expect("default key is valid"),
            repeat: KeyChord::try_from("m").expect("default key is valid"),
            shuffle: KeyChord::try_from("s").expect("default key is valid"),
            cancel: KeyChord::try_from("esc").expect("default key is valid"),
            toggle_artwork: KeyChord::try_from("z").expect("default key is valid"),
            lyrics: KeyChord::try_from("shift+L").expect("default key is valid"),
        }
    }
}
