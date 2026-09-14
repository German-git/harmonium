//! Pure playback domain logic decoupled from any audio backend.
//!
//! Every rule the application needs for progress display, seek targets and
//! volume stepping lives here as a total function over plain values, so all
//! of it is unit testable without an output device. The engine feeds real
//! observations into [`PlaybackState`] while the UI reads it back.

use std::fmt;
use std::time::Duration;

use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A validated master volume percentage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VolumePercent(u8);

impl VolumePercent {
    pub const MIN: Self = Self(0);
    pub const MAX: Self = Self(100);
    pub const STEP: u8 = 5;

    /// Construct a volume from a value crossing a raw-value boundary.
    pub const fn from_boundary(value: u16) -> Self {
        Self(if value > Self::MAX.0 as u16 {
            Self::MAX.0
        } else {
            value as u8
        })
    }

    /// Construct a volume only when the value is in its domain.
    pub const fn new(value: u16) -> Option<Self> {
        if value <= Self::MAX.0 as u16 {
            Some(Self(value as u8))
        } else {
            None
        }
    }

    pub const fn as_u8(self) -> u8 {
        self.0
    }

    pub const fn as_u16(self) -> u16 {
        self.0 as u16
    }
}

impl Default for VolumePercent {
    fn default() -> Self {
        Self(DEFAULT_VOLUME_PERCENT_VALUE)
    }
}

impl fmt::Display for VolumePercent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl TryFrom<u16> for VolumePercent {
    type Error = ();

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(())
    }
}

impl Serialize for VolumePercent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(self.0)
    }
}

impl<'de> Deserialize<'de> for VolumePercent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u16::deserialize(deserializer)?;
        Ok(Self::from_boundary(value))
    }
}

/// A validated preamp gain represented in half-decibel units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GainDb(i8);

impl GainDb {
    pub const MIN: Self = Self(-30);
    pub const MAX: Self = Self(30);
    pub const DEFAULT: Self = Self(0);
    pub const STEP: i8 = 1;

    pub const fn from_half_db(value: i8) -> Option<Self> {
        if value >= Self::MIN.0 && value <= Self::MAX.0 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Convert a raw command or migration value into the nearest legal step.
    pub fn from_boundary(value: f32) -> Result<Self, GainDbError> {
        if !value.is_finite() {
            return Err(GainDbError::NonFinite);
        }
        let clamped = value.clamp(MIN_GAIN_DB_VALUE, MAX_GAIN_DB_VALUE);
        let half_db = (clamped * 2.0).round() as i8;
        Self::from_half_db(half_db).ok_or(GainDbError::OutOfRange)
    }

    pub const fn half_db(self) -> i8 {
        self.0
    }

    /// Step the gain while keeping it inside the representable range.
    pub const fn stepped(self, forward: bool) -> Self {
        let value = if forward {
            self.0.saturating_add(Self::STEP)
        } else {
            self.0.saturating_sub(Self::STEP)
        };
        if value < Self::MIN.0 {
            Self::MIN
        } else if value > Self::MAX.0 {
            Self::MAX
        } else {
            Self(value)
        }
    }

    pub const fn as_f32(self) -> f32 {
        self.0 as f32 / 2.0
    }
}

impl Default for GainDb {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for GainDb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:+.1}", self.as_f32())
    }
}

impl TryFrom<f32> for GainDb {
    type Error = GainDbError;

    fn try_from(value: f32) -> Result<Self, Self::Error> {
        if !value.is_finite() {
            return Err(GainDbError::NonFinite);
        }
        if !(MIN_GAIN_DB_VALUE..=MAX_GAIN_DB_VALUE).contains(&value) {
            return Err(GainDbError::OutOfRange);
        }
        let half_db = (value * 2.0).round();
        if (value * 2.0 - half_db).abs() > f32::EPSILON {
            return Err(GainDbError::NotAStep);
        }
        Self::from_half_db(half_db as i8).ok_or(GainDbError::OutOfRange)
    }
}

impl Serialize for GainDb {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f32(self.as_f32())
    }
}

impl<'de> Deserialize<'de> for GainDb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_boundary(f32::deserialize(deserializer)?)
            .map_err(|error| de::Error::custom(error.to_string()))
    }
}

/// Why a raw gain cannot enter the typed playback domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GainDbError {
    NonFinite,
    OutOfRange,
    NotAStep,
}

impl fmt::Display for GainDbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NonFinite => "gain must be finite",
            Self::OutOfRange => "gain is outside -15..=15 dB",
            Self::NotAStep => "gain must use 0.5 dB steps",
        })
    }
}

/// A validated crossfade duration. Zero means disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CrossfadeSeconds(u8);

impl CrossfadeSeconds {
    pub const DISABLED: Self = Self(0);
    pub const MIN: Self = Self(5);
    pub const MAX: Self = Self(30);
    pub const STEP: u8 = 5;

    pub const fn new(value: u16) -> Option<Self> {
        if value == 0
            || (value >= Self::MIN.0 as u16
                && value <= Self::MAX.0 as u16
                && value % Self::STEP as u16 == 0)
        {
            Some(Self(value as u8))
        } else {
            None
        }
    }

    /// Normalize a raw command or migration value to the documented domain.
    pub const fn from_boundary(value: u16) -> Self {
        let value = if value > Self::MAX.0 as u16 {
            Self::MAX.0 as u16
        } else {
            value
        };
        if value < Self::MIN.0 as u16 {
            Self::DISABLED
        } else {
            Self((value / Self::STEP as u16 * Self::STEP as u16) as u8)
        }
    }

    pub const fn as_u8(self) -> u8 {
        self.0
    }

    pub const fn as_u16(self) -> u16 {
        self.0 as u16
    }

    pub const fn is_enabled(self) -> bool {
        self.0 != 0
    }
}

impl Default for CrossfadeSeconds {
    fn default() -> Self {
        Self::DISABLED
    }
}

impl TryFrom<u16> for CrossfadeSeconds {
    type Error = ();

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(())
    }
}

impl fmt::Display for CrossfadeSeconds {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for CrossfadeSeconds {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(self.0)
    }
}

impl<'de> Deserialize<'de> for CrossfadeSeconds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_boundary(u16::deserialize(deserializer)?))
    }
}

/// Apply one crossfade slider step with saturation at both ends.
pub const fn stepped_crossfade(current: CrossfadeSeconds, forward: bool) -> CrossfadeSeconds {
    let value = if forward {
        current
            .as_u16()
            .saturating_add(CrossfadeSeconds::STEP as u16)
    } else {
        current
            .as_u16()
            .saturating_sub(CrossfadeSeconds::STEP as u16)
    };
    CrossfadeSeconds::from_boundary(value)
}

const DEFAULT_VOLUME_PERCENT_VALUE: u8 = 70;
const MIN_GAIN_DB_VALUE: f32 = -15.0;
const MAX_GAIN_DB_VALUE: f32 = 15.0;

/// Initial output volume in percent.
pub const DEFAULT_VOLUME_PERCENT: VolumePercent = VolumePercent::from_boundary(70);
/// Percent added or removed by one volume step.
pub const VOLUME_STEP_PERCENT: u8 = VolumePercent::STEP;
/// Seek step used for tracks shorter than the long track threshold.
const SHORT_SEEK_STEP: Duration = Duration::from_secs(5);
/// Seek step used once a track is long enough to need bigger jumps.
const LONG_SEEK_STEP: Duration = Duration::from_secs(30);
/// Tracks at or above this duration use the long seek step.
const LONG_TRACK_THRESHOLD: Duration = Duration::from_secs(600);
/// Restarting the current track wins over going back only past this point.
const RESTART_THRESHOLD: Duration = Duration::from_secs(3);

/// Playback status surfaced by the audio worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayStatus {
    /// Nothing is loaded or the queue finished.
    Stopped,
    /// A track is actively rendering audio.
    Playing,
    /// A loaded track is holding its position.
    Paused,
}

/// Health of the output sink used by the audio worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SinkHealth {
    /// Samples are being accepted by a live output sink.
    Healthy,
    /// The previously active sink disappeared and playback is frozen until a
    /// user-initiated resume or play operation rebuilds it.
    Lost,
    /// No usable sink is currently available, including a failed recovery.
    #[default]
    Unavailable,
}

/// Typed health observation suitable for UI state or structured logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkHealthSnapshot {
    /// Health at the observation boundary.
    pub health: SinkHealth,
}

impl SinkHealth {
    /// Capture the current health without creating an event.
    pub const fn snapshot(self) -> SinkHealthSnapshot {
        SinkHealthSnapshot { health: self }
    }
}

/// One health transition, emitted only when the state actually changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkHealthTransition {
    /// State before the transition.
    pub from: SinkHealth,
    /// State after the transition.
    pub to: SinkHealth,
}

impl SinkHealthTransition {
    /// Return a diagnostic transition only when the values differ.
    pub fn new(from: SinkHealth, to: SinkHealth) -> Option<Self> {
        if from == to {
            None
        } else {
            Some(Self { from, to })
        }
    }
}

/// Point in time observation of the playback engine.
///
/// The worker publishes these periodically so the UI thread never has to
/// reach into audio internals to know where a track stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackSnapshot {
    /// Engine status at observation time.
    pub status: PlayStatus,
    /// Queue index of the observed track.
    pub track_index: Option<usize>,
    /// Elapsed position inside the current track.
    pub elapsed: Duration,
    /// Total track length when the decoder could report one.
    pub duration: Option<Duration>,
    /// Output sink health observed with this snapshot.
    pub sink_health: SinkHealth,
}

impl PlaybackSnapshot {
    /// Typed sink observation for consumers that do not need the full snapshot.
    pub const fn sink_health_snapshot(self) -> SinkHealthSnapshot {
        self.sink_health.snapshot()
    }
}

/// Application side view of playback merged from snapshots and commands.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackState {
    /// Current playback status.
    pub status: PlayStatus,
    /// Queue index of the track the state describes.
    pub track_index: Option<usize>,
    /// Last known position inside that track.
    pub elapsed: Duration,
    /// Track length when known, driving seek bounds and the gauge.
    pub duration: Option<Duration>,
    /// Output volume in percent, owned by the UI side so it survives
    /// devices being unavailable.
    pub volume_percent: VolumePercent,
    /// Playback speed multiplier in [SPEED_MIN, SPEED_MAX], owned by the UI
    /// side and never persisted: it resets to SPEED_DEFAULT on each launch and
    /// is pushed to the audio worker through `AudioCommand::SetSpeed`.
    pub speed: Speed,
    /// Last output sink health reported by the audio worker.
    pub sink_health: SinkHealth,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            status: PlayStatus::Stopped,
            track_index: None,
            elapsed: Duration::ZERO,
            duration: None,
            volume_percent: DEFAULT_VOLUME_PERCENT,
            speed: SPEED_DEFAULT,
            sink_health: SinkHealth::Unavailable,
        }
    }
}

impl PlaybackState {
    /// Merge one worker snapshot, leaving volume untouched on purpose
    /// because the application owns that value optimistically.
    ///
    /// Duration follows a different rule: the metadata layer is the
    /// authority for total track length. The decoder may report None
    /// before it parses the container, so we only upgrade — never
    /// downgrade — an existing value. Once metadata fills the field,
    /// the decoder's late Some overwrites it with the authoritative
    /// number, but a transient None never erases it.
    pub fn apply_snapshot(&mut self, snapshot: PlaybackSnapshot) {
        // A different track starts from a clean slate: never inherit the
        // previous track's length, which would make the progress ratio and the
        // remaining-time readout meaningless. Within the same track we keep the
        // upgrade-only rule so a transient `None` from the decoder (before it
        // parses the container) never erases a known duration.
        let track_changed = snapshot.track_index != self.track_index;
        self.status = snapshot.status;
        self.track_index = snapshot.track_index;
        self.elapsed = snapshot.elapsed;
        self.sink_health = snapshot.sink_health;
        if track_changed || snapshot.duration.is_some() {
            self.duration = snapshot.duration;
        }
    }
}

/// Whether the previous-track command must restart the current track.
///
/// Deep into a track, stepping back loses the listener's place, so the
/// track starts over instead of moving to a neighbour. The comparison uses
/// strictly greater than the threshold, so exactly three seconds counts as
/// near the start and still switches. Which neighbour to switch to is a
/// queue ordering question owned by the playlist selection engine.
pub fn previous_restarts_track(elapsed: Duration) -> bool {
    elapsed > RESTART_THRESHOLD
}

/// Apply one volume step with saturation at both ends of the scale.
pub fn stepped_volume(current: VolumePercent, delta: i32) -> VolumePercent {
    let stepped = i32::from(current.as_u16()) + delta;
    VolumePercent::from_boundary(stepped.clamp(0, 100) as u16)
}

/// Convert a percent volume into the linear gain factor backends expect.
pub fn volume_factor(percent: VolumePercent) -> f32 {
    f32::from(percent.as_u16()) / 100.0
}

/// Preamp gain range (dB) exposed in the Playback settings tab.
pub const MIN_GAIN_DB: f32 = MIN_GAIN_DB_VALUE;
/// Upper bound of the preamp gain range (dB).
pub const MAX_GAIN_DB: f32 = MAX_GAIN_DB_VALUE;
/// Size of one gain step in the settings slider (0.5 dB).
pub const GAIN_STEP_DB: f32 = 0.5;

/// Whether two gain values differ by more than the settings-domain tolerance.
///
/// Finite values use half a slider step so representational noise does not
/// trigger effects or persistence decisions. Non-finite values stay under the
/// existing config sanitization and worker validation rules.
pub(crate) fn gain_changed(previous: GainDb, current: GainDb) -> bool {
    previous != current
}

/// Convert a decibel gain into the linear factor backends expect.
///
/// `factor = 10^(dB / 20)`: 0 dB -> 1.0 (no change), +6 dB -> ~2.0, -6 dB -> 0.5.
pub fn gain_factor(db: GainDb) -> f32 {
    10.0_f32.powf(db.as_f32() / 20.0)
}

/// Valid playback speed represented as tenths of the normal rate.
///
/// Keeping the slider value as an integer makes every step, including a round
/// trip back to unity, exact. Floating-point conversion belongs at the
/// SoundTouch boundary only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Speed(u8);

impl Speed {
    /// Lowest supported playback speed: 0.5x.
    pub const MIN: Self = Self(5);
    /// Highest supported playback speed: 2.0x.
    pub const MAX: Self = Self(20);
    /// One slider step, expressed in tenths.
    pub const STEP: u8 = 1;
    /// Normal playback speed: 1.0x.
    pub const UNITY: Self = Self(10);

    /// Construct a speed from its validated tenths representation.
    pub const fn from_tenths(tenths: u8) -> Option<Self> {
        if tenths >= Self::MIN.0 && tenths <= Self::MAX.0 {
            Some(Self(tenths))
        } else {
            None
        }
    }

    /// Return the exact tenths representation.
    pub const fn tenths(self) -> u8 {
        self.0
    }

    /// Convert a validated speed for the SoundTouch API.
    pub const fn as_f32(self) -> f32 {
        self.0 as f32 / 10.0
    }
}

/// Why a raw speed multiplier cannot enter the typed playback domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedError {
    /// The input was NaN or an infinity.
    NonFinite,
    /// The input was outside the supported range.
    OutOfRange,
    /// The input was not an exact slider step.
    NotAStep,
}

impl TryFrom<f32> for Speed {
    type Error = SpeedError;

    fn try_from(value: f32) -> Result<Self, Self::Error> {
        if !value.is_finite() {
            return Err(SpeedError::NonFinite);
        }
        let tenths = (value * 10.0).round();
        if !(f32::from(Self::MIN.0)..=f32::from(Self::MAX.0)).contains(&tenths) {
            return Err(SpeedError::OutOfRange);
        }
        if (value - tenths / 10.0).abs() > 1e-5 {
            return Err(SpeedError::NotAStep);
        }
        Self::from_tenths(tenths as u8).ok_or(SpeedError::OutOfRange)
    }
}

impl fmt::Display for Speed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.0 / 10, self.0 % 10)
    }
}

/// Lower bound of the playback speed scale.
pub const SPEED_MIN: Speed = Speed::MIN;
/// Upper bound of the playback speed scale.
pub const SPEED_MAX: Speed = Speed::MAX;
/// Size of one playback speed step in tenths.
pub const SPEED_STEP: i8 = Speed::STEP as i8;
/// Default (and reset) playback speed.
pub const SPEED_DEFAULT: Speed = Speed::UNITY;

/// Convert a raw command/config value at the boundary, preserving finite
/// out-of-range saturation without allowing invalid values into `Speed`.
pub fn speed_from_boundary(value: f32) -> Result<Speed, SpeedError> {
    if !value.is_finite() {
        return Err(SpeedError::NonFinite);
    }
    let min = f32::from(Speed::MIN.tenths()) / 10.0;
    let max = f32::from(Speed::MAX.tenths()) / 10.0;
    Speed::try_from(value.clamp(min, max))
}

/// Apply one playback speed step with saturation at both ends of the scale.
pub fn stepped_speed(current: Speed, delta: i8) -> Speed {
    let tenths = i16::from(current.tenths()) + i16::from(delta);
    let tenths = tenths.clamp(
        i16::from(Speed::MIN.tenths()),
        i16::from(Speed::MAX.tenths()),
    );
    Speed::from_tenths(tenths as u8).expect("clamped speed must be valid")
}

/// Equal-power crossfade gains for normalized progress `t` in `[0, 1]`.
///
/// Returns `(outgoing, incoming)`: `out = cos(t·π/2)` and `in = sin(t·π/2)`.
/// The endpoints are exact (`t = 0` → 1.0/0.0, `t = 1` → 0.0/1.0) and
/// `out² + in² = 1`, so the summed loudness stays even instead of dipping at
/// the midpoint of a linear fade.
pub fn crossfade_gains(t: f64) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let phase = t * std::f64::consts::FRAC_PI_2;
    (phase.cos() as f32, phase.sin() as f32)
}

/// Adaptive seek step chosen from the track length.
///
/// Short tracks get small jumps because thirty seconds can exceed their
/// whole length, long tracks get large jumps because five seconds barely
/// moves inside an audiobook chapter. Unknown lengths choose the short
/// step so blind seeking stays fine grained.
pub fn seek_step(duration: Option<Duration>) -> Duration {
    match duration {
        Some(duration) if duration >= LONG_TRACK_THRESHOLD => LONG_SEEK_STEP,
        _ => SHORT_SEEK_STEP,
    }
}

/// Target position after one adaptive seek, clamped into the track.
///
/// Without a known duration the backward direction clamps to zero while
/// forward simply adds the step, mirroring how players behave when the
/// total length is still unknown.
pub fn seek_target(elapsed: Duration, backward: bool, duration: Option<Duration>) -> Duration {
    seek_by_target(elapsed, !backward, seek_step(duration), duration)
}

/// Target position after a relative seek, clamped into the track when its
/// duration is known.
pub fn seek_by_target(
    position: Duration,
    forward: bool,
    amount: Duration,
    duration: Option<Duration>,
) -> Duration {
    let target = if forward {
        position.saturating_add(amount)
    } else {
        position.saturating_sub(amount)
    };

    match duration {
        Some(duration) => target.min(duration),
        None => target,
    }
}

/// Fill ratio of the progress gauge, always within zero and one.
///
/// Unknown or zero durations yield zero fill instead of dividing by zero,
/// which renders an empty bar rather than a broken frame.
pub fn progress_ratio(elapsed: Duration, duration: Option<Duration>) -> f64 {
    let Some(duration) = duration.filter(|value| !value.is_zero()) else {
        return 0.0;
    };

    let ratio = elapsed.as_secs_f64() / duration.as_secs_f64();
    ratio.clamp(0.0, 1.0)
}

/// Render a duration as `m:ss` or `h:mm:ss` past one hour.
pub fn format_clock(value: Duration) -> String {
    let total_seconds = value.as_secs();
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3600;

    match hours {
        0 => format!("{minutes}:{seconds:02}"),
        _ => format!("{hours}:{minutes:02}:{seconds:02}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_steps_saturate_at_both_ends() {
        let volume = |value| VolumePercent::new(value).unwrap();
        assert_eq!(stepped_volume(volume(50), 5), volume(55));
        assert_eq!(stepped_volume(volume(50), -5), volume(45));
        assert_eq!(stepped_volume(volume(98), 5), volume(100));
        assert_eq!(stepped_volume(volume(100), 5), volume(100));
        assert_eq!(stepped_volume(volume(3), -5), volume(0));
        assert_eq!(stepped_volume(volume(0), -5), volume(0));
        assert_eq!(stepped_volume(volume(0), 5), volume(5));
    }

    #[test]
    fn gain_factor_maps_db_to_the_expected_linear_factor() {
        let gain = |value| GainDb::try_from(value).unwrap();
        assert_eq!(gain_factor(gain(0.0)), 1.0);
        // 6 dB ≈ 2.0x amplitude (10^0.3 = 1.9953), -6 dB ≈ 0.5x (0.5012).
        assert!(
            (gain_factor(gain(6.0)) - 2.0).abs() < 1e-2,
            "6 dB should double"
        );
        assert!(
            (gain_factor(gain(-6.0)) - 0.5).abs() < 1e-2,
            "-6 dB should halve"
        );
        assert!((gain_factor(gain(3.0)) - 10.0_f32.powf(3.0 / 20.0)).abs() < 1e-4);
    }

    #[test]
    fn gain_bounds_are_symmetric_and_stepped() {
        assert!((MIN_GAIN_DB + MAX_GAIN_DB).abs() < f32::EPSILON);
        assert_eq!(GAIN_STEP_DB, 0.5);
    }

    #[test]
    fn gain_steps_saturate_at_both_endpoints() {
        assert_eq!(GainDb::MAX.stepped(true), GainDb::MAX);
        assert_eq!(GainDb::MIN.stepped(false), GainDb::MIN);
        assert_eq!(
            GainDb::MIN.stepped(true),
            GainDb::from_half_db(-29).unwrap()
        );
        assert_eq!(
            GainDb::MAX.stepped(false),
            GainDb::from_half_db(29).unwrap()
        );
    }

    #[test]
    fn typed_values_accept_valid_toml_and_preserve_the_legacy_shape() {
        #[derive(Debug, Deserialize, Serialize, PartialEq)]
        struct Values {
            volume_percent: VolumePercent,
            gain_db: GainDb,
            crossfade_seconds: CrossfadeSeconds,
        }

        let values: Values =
            toml::from_str("volume_percent = 73\ngain_db = 4.5\ncrossfade_seconds = 15\n").unwrap();
        assert_eq!(values.volume_percent.as_u16(), 73);
        assert_eq!(values.gain_db, GainDb::try_from(4.5).unwrap());
        assert_eq!(values.crossfade_seconds, CrossfadeSeconds::new(15).unwrap());

        let rendered = toml::to_string(&values).unwrap();
        assert!(rendered.contains("volume_percent = 73"));
        assert!(rendered.contains("gain_db = 4.5"));
        assert!(rendered.contains("crossfade_seconds = 15"));
        assert_eq!(toml::from_str::<Values>(&rendered).unwrap(), values);
    }

    #[test]
    fn constructors_and_deserialization_reject_invalid_domains() {
        #[allow(dead_code)]
        #[derive(Debug, Deserialize)]
        struct VolumeValue {
            #[allow(dead_code)]
            volume_percent: VolumePercent,
        }
        #[allow(dead_code)]
        #[derive(Debug, Deserialize)]
        struct GainValue {
            #[allow(dead_code)]
            gain_db: GainDb,
        }

        assert_eq!(VolumePercent::new(101), None);
        assert_eq!(CrossfadeSeconds::new(4), None);
        assert_eq!(CrossfadeSeconds::new(12), None);
        assert_eq!(GainDb::try_from(1.25), Err(GainDbError::NotAStep));
        assert_eq!(GainDb::try_from(16.0), Err(GainDbError::OutOfRange));
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(GainDb::try_from(value), Err(GainDbError::NonFinite));
        }

        assert!(toml::from_str::<VolumeValue>("volume_percent = \"101\"").is_err());
        assert!(toml::from_str::<GainValue>("gain_db = \"NaN\"").is_err());
        assert!(toml::from_str::<GainValue>("gain_db = \"inf\"").is_err());
    }

    #[test]
    fn boundary_normalization_is_idempotent_and_preserves_documented_steps() {
        for value in [0, 1, 4, 5, 7, 12, 30, 47, u16::MAX] {
            let normalized = CrossfadeSeconds::from_boundary(value);
            assert_eq!(
                CrossfadeSeconds::from_boundary(normalized.as_u16()),
                normalized
            );
            assert!(
                normalized == CrossfadeSeconds::DISABLED
                    || normalized.as_u8() >= CrossfadeSeconds::MIN.as_u8()
                        && normalized.as_u8() <= CrossfadeSeconds::MAX.as_u8()
                        && normalized.as_u8() % CrossfadeSeconds::STEP == 0
            );
        }
        assert_eq!(VolumePercent::from_boundary(101), VolumePercent::MAX);
        assert_eq!(GainDb::from_boundary(99.0).unwrap(), GainDb::MAX);
    }

    #[test]
    fn sink_health_snapshot_and_transition_are_typed_and_non_flooding() {
        assert_eq!(
            SinkHealth::Lost.snapshot(),
            SinkHealthSnapshot {
                health: SinkHealth::Lost
            }
        );
        assert_eq!(
            SinkHealthTransition::new(SinkHealth::Healthy, SinkHealth::Lost),
            Some(SinkHealthTransition {
                from: SinkHealth::Healthy,
                to: SinkHealth::Lost,
            })
        );
        assert_eq!(
            SinkHealthTransition::new(SinkHealth::Lost, SinkHealth::Lost),
            None
        );
    }

    #[test]
    fn stepped_speed_clamps_within_bounds_and_steps_by_tenths() {
        assert!(SPEED_MIN.tenths() < SPEED_DEFAULT.tenths());
        assert!(SPEED_DEFAULT.tenths() < SPEED_MAX.tenths());
        assert_eq!(SPEED_STEP, 1);
        // Step up and down from the default.
        assert_eq!(
            stepped_speed(SPEED_DEFAULT, SPEED_STEP),
            Speed::from_tenths(11).unwrap()
        );
        assert_eq!(
            stepped_speed(SPEED_DEFAULT, -SPEED_STEP),
            Speed::from_tenths(9).unwrap()
        );
        // Saturation at both ends, no overflow via accumulated presses.
        for _ in 0..30 {
            assert_eq!(stepped_speed(SPEED_MAX, SPEED_STEP), SPEED_MAX);
            assert_eq!(stepped_speed(SPEED_MIN, -SPEED_STEP), SPEED_MIN);
        }
    }

    #[test]
    fn every_speed_slider_step_round_trips_exactly() {
        for tenths in Speed::MIN.tenths()..=Speed::MAX.tenths() {
            let speed = Speed::from_tenths(tenths).unwrap();
            assert_eq!(Speed::try_from(speed.as_f32()), Ok(speed));
        }

        let mut speed = Speed::MIN;
        while speed != Speed::MAX {
            speed = stepped_speed(speed, SPEED_STEP);
        }
        assert_eq!(speed, Speed::MAX);
    }

    #[test]
    fn speed_display_preserves_the_existing_one_decimal_ui_value() {
        assert_eq!(format!("{:.1}x", Speed::UNITY), "1.0x");
        assert_eq!(format!("{:.1}x", Speed::MIN), "0.5x");
    }

    #[test]
    fn speed_boundary_rejects_non_finite_and_off_step_values() {
        assert_eq!(Speed::from_tenths(4), None);
        assert_eq!(Speed::from_tenths(21), None);
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(Speed::try_from(value), Err(SpeedError::NonFinite));
            assert_eq!(speed_from_boundary(value), Err(SpeedError::NonFinite));
        }
        assert_eq!(Speed::try_from(0.4), Err(SpeedError::OutOfRange));
        assert_eq!(Speed::try_from(1.25), Err(SpeedError::NotAStep));
        assert_eq!(
            speed_from_boundary(-1.0),
            Ok(SPEED_MIN),
            "finite command values preserve boundary saturation"
        );
        assert_eq!(speed_from_boundary(3.0), Ok(SPEED_MAX));
    }

    #[test]
    fn crossfade_gains_are_equal_power_with_exact_endpoints() {
        let (out0, in0) = crossfade_gains(0.0);
        let (out_mid, in_mid) = crossfade_gains(0.5);
        let (out1, in1) = crossfade_gains(1.0);
        // Endpoints are exact.
        assert!((out0 - 1.0).abs() < f32::EPSILON && (in0 - 0.0).abs() < f32::EPSILON);
        assert!((out1 - 0.0).abs() < f32::EPSILON && (in1 - 1.0).abs() < f32::EPSILON);
        // Midpoint is ~0.707 for both, and out² + in² = 1 (equal power).
        assert!((out_mid - std::f64::consts::FRAC_1_SQRT_2 as f32).abs() < 1e-3);
        assert!((in_mid - std::f64::consts::FRAC_1_SQRT_2 as f32).abs() < 1e-3);
        let power = out_mid * out_mid + in_mid * in_mid;
        assert!(
            (power - 1.0).abs() < 1e-3,
            "equal power must sum to 1, got {power}"
        );
        // Monotonic: outgoing decreases, incoming increases across the ramp.
        let (out_a, in_a) = crossfade_gains(0.25);
        let (out_b, in_b) = crossfade_gains(0.75);
        assert!(out_a > out_b && in_a < in_b);
    }

    #[test]
    fn volume_factor_maps_percent_linearly() {
        assert_eq!(volume_factor(VolumePercent::MIN), 0.0);
        assert_eq!(volume_factor(VolumePercent::MAX), 1.0);
        assert!((volume_factor(DEFAULT_VOLUME_PERCENT) - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn volume_factor_clamps_oversized_percent_values() {
        assert_eq!(volume_factor(VolumePercent::from_boundary(140)), 1.0);
    }

    #[test]
    fn seek_step_is_adaptive_to_track_length() {
        // Boundary contract below is inclusive: exactly ten minutes
        // already belongs to the long step window
        assert_eq!(seek_step(None), SHORT_SEEK_STEP);
        assert_eq!(seek_step(Some(Duration::ZERO)), SHORT_SEEK_STEP);
        assert_eq!(
            seek_step(Some(LONG_TRACK_THRESHOLD - Duration::from_secs(1))),
            SHORT_SEEK_STEP
        );
        assert_eq!(seek_step(Some(LONG_TRACK_THRESHOLD)), LONG_SEEK_STEP);
        assert_eq!(
            seek_step(Some(Duration::from_secs(5 * 3600))),
            LONG_SEEK_STEP
        );
    }

    #[test]
    fn seek_target_moves_by_the_adaptive_step() {
        let short = Some(Duration::from_secs(120));
        assert_eq!(
            seek_target(Duration::from_secs(30), false, short),
            Duration::from_secs(35)
        );
        assert_eq!(
            seek_target(Duration::from_secs(30), true, short),
            Duration::from_secs(25)
        );

        let long = Some(Duration::from_secs(3600));
        assert_eq!(
            seek_target(Duration::from_secs(60), false, long),
            Duration::from_secs(90)
        );
        assert_eq!(
            seek_target(Duration::from_secs(60), true, long),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn seek_target_clamps_into_track_bounds() {
        let duration = Some(Duration::from_secs(100));
        assert_eq!(
            seek_target(Duration::from_secs(98), false, duration),
            Duration::from_secs(100)
        );
        assert_eq!(
            seek_target(Duration::from_secs(2), true, duration),
            Duration::ZERO
        );
    }

    #[test]
    fn seek_without_duration_stays_finite_backwards_and_steps_forward() {
        assert_eq!(seek_target(Duration::ZERO, true, None), Duration::ZERO);
        assert_eq!(
            seek_target(Duration::from_secs(10), false, None),
            SHORT_SEEK_STEP + Duration::from_secs(10)
        );
    }

    #[test]
    fn relative_seek_target_accumulates_and_clamps() {
        let duration = Some(Duration::from_secs(40));
        let mut position = Duration::ZERO;
        for _ in 0..10 {
            position = seek_by_target(position, true, Duration::from_secs(5), duration);
        }
        assert_eq!(position, Duration::from_secs(40));
        assert_eq!(
            seek_by_target(position, false, Duration::from_secs(5), duration),
            Duration::from_secs(35)
        );
    }

    #[test]
    fn progress_ratio_handles_degenerate_inputs() {
        assert_eq!(progress_ratio(Duration::from_secs(5), None), 0.0);
        assert_eq!(
            progress_ratio(Duration::from_secs(5), Some(Duration::ZERO)),
            0.0
        );
        assert_eq!(
            progress_ratio(Duration::ZERO, Some(Duration::from_secs(10))),
            0.0
        );
        assert_eq!(
            progress_ratio(Duration::from_secs(5), Some(Duration::from_secs(10))),
            0.5
        );
        assert_eq!(
            progress_ratio(Duration::from_secs(99), Some(Duration::from_secs(10))),
            1.0
        );
    }

    #[test]
    fn clock_format_covers_minutes_and_hours() {
        assert_eq!(format_clock(Duration::ZERO), "0:00");
        assert_eq!(format_clock(Duration::from_secs(65)), "1:05");
        assert_eq!(format_clock(Duration::from_secs(599)), "9:59");
        assert_eq!(format_clock(Duration::from_secs(3600)), "1:00:00");
        assert_eq!(format_clock(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn previous_restarts_deep_into_a_track() {
        assert!(previous_restarts_track(Duration::from_secs(4)));
        assert!(previous_restarts_track(Duration::from_secs(30)));
    }

    #[test]
    fn previous_switches_near_the_start_with_the_boundary_inclusive() {
        // Exactly at the threshold counts as near the start on purpose
        assert!(!previous_restarts_track(RESTART_THRESHOLD));
        assert!(!previous_restarts_track(Duration::from_secs(1)));
        assert!(!previous_restarts_track(Duration::ZERO));
    }

    #[test]
    fn snapshots_merge_into_state_while_preserving_volume() {
        let mut state = PlaybackState {
            volume_percent: VolumePercent::new(40).unwrap(),
            ..PlaybackState::default()
        };

        state.apply_snapshot(PlaybackSnapshot {
            status: PlayStatus::Playing,
            track_index: Some(3),
            elapsed: Duration::from_secs(7),
            duration: Some(Duration::from_secs(70)),
            sink_health: SinkHealth::Healthy,
        });

        assert_eq!(state.status, PlayStatus::Playing);
        assert_eq!(state.track_index, Some(3));
        assert_eq!(state.elapsed, Duration::from_secs(7));
        assert_eq!(state.duration, Some(Duration::from_secs(70)));
        assert_eq!(
            state.volume_percent,
            VolumePercent::new(40).unwrap(),
            "the UI owns volume exclusively"
        );
    }

    #[test]
    fn snapshot_none_duration_does_not_erase_existing_value() {
        // Same track still playing: the decoder publishes before it knows the
        // total length, so a transient None must not erase the known duration.
        let mut state = PlaybackState {
            track_index: Some(0),
            duration: Some(Duration::from_secs(210)),
            ..PlaybackState::default()
        };

        state.apply_snapshot(PlaybackSnapshot {
            status: PlayStatus::Playing,
            track_index: Some(0),
            elapsed: Duration::from_secs(5),
            duration: None,
            sink_health: SinkHealth::Healthy,
        });

        assert_eq!(
            state.duration,
            Some(Duration::from_secs(210)),
            "a transient None from the decoder must not overwrite metadata duration"
        );
    }

    #[test]
    fn snapshot_from_a_new_track_resets_duration() {
        let mut state = PlaybackState {
            track_index: Some(0),
            duration: Some(Duration::from_secs(210)),
            ..PlaybackState::default()
        };

        // A different track starts; even an unknown duration must replace the
        // previous value so the UI never shows the old track's length.
        state.apply_snapshot(PlaybackSnapshot {
            status: PlayStatus::Playing,
            track_index: Some(1),
            elapsed: Duration::ZERO,
            duration: None,
            sink_health: SinkHealth::Healthy,
        });

        assert_eq!(state.track_index, Some(1));
        assert_eq!(state.duration, None);
    }

    #[test]
    fn default_playback_state_starts_stopped_at_the_default_volume() {
        let state = PlaybackState::default();

        assert_eq!(state.status, PlayStatus::Stopped);
        assert_eq!(state.track_index, None);
        assert_eq!(state.elapsed, Duration::ZERO);
        assert_eq!(state.duration, None);
        assert_eq!(state.volume_percent, DEFAULT_VOLUME_PERCENT);
    }
}
