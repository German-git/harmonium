//! Domain facing audio playback boundary.
//!
//! Everything outside this module speaks only the vocabulary defined here
//! or in `playback`: commands in, snapshots and failures out. Rodio, cpal
//! and decoder types never cross this line, which keeps the application
//! and UI layers testable without audio hardware.

// Keep backend and implementation modules crate-visible. External callers use
// the stable vocabulary re-exported below instead of reaching into internals.
pub(crate) mod engine;
pub(crate) mod error;
pub(crate) mod output;
pub(crate) mod pipewire_sink;
pub(crate) mod playback;
pub(crate) mod resample;

// AudioCommand and AudioEngineHandle are consumed by the application binary;
// output/playback values also cross the public AppEvent and state contracts.
// AudioError remains the typed failure vocabulary promised by this boundary.
// PipeWireSink and its result types are retained for the --pipewire-test
// binary path and direct sink usage. The worker constructor is internal
// runtime wiring and is intentionally not part of this public surface.
pub use engine::{AudioCommand, AudioEngineHandle};
pub use error::AudioError;
pub use output::{
    ArcOutputProvider, AudioOutput, NullOutputProvider, OutputProvider, OutputTarget,
    PipeWireOutputProvider, StartupOutputSelection, StaticOutputProvider, select_startup_output,
};
pub use pipewire_sink::{FlushGeneration, PipeWireSink, PipeWireStartupError, PushSamplesResult};
pub use playback::{
    CrossfadeSeconds, DEFAULT_VOLUME_PERCENT, GAIN_STEP_DB, GainDb, GainDbError, MAX_GAIN_DB,
    MIN_GAIN_DB, PlayStatus, PlaybackSnapshot, PlaybackState, SPEED_DEFAULT, SPEED_MAX, SPEED_MIN,
    SPEED_STEP, SinkHealth, SinkHealthSnapshot, SinkHealthTransition, Speed, SpeedError,
    VOLUME_STEP_PERCENT, VolumePercent, crossfade_gains, format_clock, gain_factor,
    previous_restarts_track, progress_ratio, seek_step, seek_target, speed_from_boundary,
    stepped_crossfade, stepped_speed, stepped_volume, volume_factor,
};
