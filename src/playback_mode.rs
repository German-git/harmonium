//! Owner playback order model: the repeat axis plus the shuffle switch.
//!
//! Harmonium models repeat and shuffle as two independent axes, giving six
//! combinable states. This deliberately supersedes the single LoopMode of
//! the audited reference player, whose semantics cannot express the full
//! matrix, so parity claims are avoided on purpose and the help surfaces
//! describe our own behavior instead.

use serde::{Deserialize, Serialize};

/// How playback behaves when a track finishes or the sequence runs out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    /// Finish the sequence and stop.
    #[default]
    Off,
    /// Replay the current track forever.
    Track,
    /// Wrap around to the start of the sequence.
    All,
}

impl RepeatMode {
    /// Parse the legacy state-file label, defaulting safely for unknown values.
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "track" => Self::Track,
            "all" => Self::All,
            _ => Self::Off,
        }
    }

    /// Next mode in the cycle off, track, all and back to off.
    pub fn cycle(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::Track,
            RepeatMode::Track => RepeatMode::All,
            RepeatMode::All => RepeatMode::Off,
        }
    }

    /// Short human readable label reused by the status line and notices.
    pub fn label(self) -> &'static str {
        match self {
            RepeatMode::Off => "Off",
            RepeatMode::Track => "Track",
            RepeatMode::All => "All",
        }
    }
}

/// Owner model of the two playback order axes.
///
/// The fields stay private so every mutation flows through the cycling and
/// toggling methods, which keeps the six state combinations reachable only
/// through the documented transitions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaybackMode {
    repeat: RepeatMode,
    shuffle: bool,
}

impl PlaybackMode {
    /// Build a mode from explicit axis values.
    pub fn new(repeat: RepeatMode, shuffle: bool) -> Self {
        Self { repeat, shuffle }
    }

    /// Current repeat axis value.
    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    /// Current shuffle axis value.
    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    /// Advance the repeat axis one step and return the new value.
    pub fn cycle_repeat(&mut self) -> RepeatMode {
        self.repeat = self.repeat.cycle();
        self.repeat
    }

    /// Flip the shuffle axis and return the new value.
    pub fn toggle_shuffle(&mut self) -> bool {
        self.shuffle = !self.shuffle;
        self.shuffle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_is_plain_sequential_playback() {
        let mode = PlaybackMode::default();

        assert_eq!(mode.repeat(), RepeatMode::Off);
        assert!(!mode.shuffle());
    }

    #[test]
    fn repeat_cycle_walks_every_step_and_closes_the_ring() {
        assert_eq!(RepeatMode::Off.cycle(), RepeatMode::Track);
        assert_eq!(RepeatMode::Track.cycle(), RepeatMode::All);
        assert_eq!(RepeatMode::All.cycle(), RepeatMode::Off);
    }

    #[test]
    fn cycle_repeat_reports_each_new_value() {
        let mut mode = PlaybackMode::default();

        assert_eq!(mode.cycle_repeat(), RepeatMode::Track);
        assert_eq!(mode.cycle_repeat(), RepeatMode::All);
        assert_eq!(mode.cycle_repeat(), RepeatMode::Off);
        assert_eq!(mode.repeat(), RepeatMode::Off);
    }

    #[test]
    fn toggle_shuffle_flips_and_reports_the_axis() {
        let mut mode = PlaybackMode::default();

        assert!(mode.toggle_shuffle());
        assert!(mode.shuffle());
        assert!(!mode.toggle_shuffle());
        assert!(!mode.shuffle());
    }

    #[test]
    fn labels_cover_every_repeat_mode() {
        assert_eq!(RepeatMode::Off.label(), "Off");
        assert_eq!(RepeatMode::Track.label(), "Track");
        assert_eq!(RepeatMode::All.label(), "All");
    }

    #[test]
    fn legacy_repeat_labels_parse_without_entering_runtime_as_strings() {
        assert_eq!(RepeatMode::parse("track"), RepeatMode::Track);
        assert_eq!(RepeatMode::parse("ALL"), RepeatMode::All);
        assert_eq!(RepeatMode::parse("off"), RepeatMode::Off);
        assert_eq!(RepeatMode::parse("unknown"), RepeatMode::Off);
    }
}
