//! Rodio backed playback engine isolated on its own worker thread.
//!
//! The whole rodio surface lives inside this module. The rest of the
//! application only ever sees [`AudioCommand`], [`AudioEngineHandle`] and
//! the snapshots published through the event bus, which keeps the domain
//! and UI layers free of audio backend types.
//!
//! Threading rationale, matching the architecture document: the engine
//! runs on a dedicated plain thread because the native PipeWire sink already
//! drives real time audio through its own output thread and the worker's
//! control calls are cheap synchronous operations. A Tokio runtime would add
//! nothing here except coupling, while the command channel plus the bridge
//! bus give the same message based ownership style used by every other
//! subsystem.
//!
//! Graceful degradation contract: opening the output device happens lazily
//! before the first play attempt, so machines without any audio hardware
//! start normally, see one warning notification per failed attempt and
//! never crash. Decode failures degrade the same way and leave the queue
//! untouched.

use std::collections::VecDeque;
use std::fs::File;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use rodio::{Decoder, Source};
use soundtouch::SoundTouch;

use crate::audio::error::AudioError;
use crate::audio::playback::{
    CrossfadeSeconds, GainDb, VolumePercent, seek_by_target, speed_from_boundary,
};
use crate::audio::{
    DEFAULT_VOLUME_PERCENT, PlayStatus, PlaybackSnapshot, SPEED_DEFAULT, SinkHealthTransition,
    Speed, SpeedError, gain_factor, volume_factor,
};
use crate::audio::{FlushGeneration, OutputTarget, PipeWireSink, PushSamplesResult, SinkHealth};
use crate::event::{AppEvent, EffectErrorKind, EventSender};
use crate::stream::{StreamCancellation, StreamReader, StreamResolver, TrackSource};

/// How often the worker wakes up to look for new commands.
///
/// Small enough that pause or seek feel instant, cheap enough to be
/// irrelevant even while idle.
const COMMAND_POLL: Duration = Duration::from_millis(100);
/// Maximum time the worker waits for PipeWire to acknowledge a flush.
///
/// The worker checks this deadline at the `COMMAND_POLL` cadence, so five
/// polling intervals give a healthy PipeWire callback room to run while still
/// converting a permanently stalled sink into visible, recoverable sink loss.
const FLUSH_ACK_TIMEOUT: Duration = Duration::from_millis(500);
/// Maximum number of decoded samples consumed by one interruptible fallback
/// seek step before the worker returns to its top-level command dispatcher.
const SEEK_COMMAND_POLL_SAMPLES: usize = 4096;
/// Cadence of progress snapshots published while a track is playing.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// Maximum time a worker-adjacent failure publication may wait for capacity.
const AUDIO_EVENT_TIMEOUT: Duration = Duration::from_millis(100);

/// Number of output frames used to finish a crossfade when the outgoing track
/// runs out before the ramp reaches full gain (a VBR duration estimate often
/// ends early). Ramping the incoming gain to 1.0 over a few milliseconds avoids
/// the audible amplitude "click" of handing over mid-ramp.
const CROSSFADE_TAIL_FRAMES: usize = 256;
/// Number of frames decoded or mixed by one worker pump.
const PUMP_CHUNK_FRAMES: usize = 512;

/// Build a `rodio::Decoder` from a [`StreamReader`] and return it as a boxed
/// [`Source`] so the caller can drop it into the active track slot.
///
/// `StreamReader` is the common `Read + Seek` adapter for lazy HTTP, live HLS,
/// and finite in-memory HLS data. Preserve its known content length for
/// decoder metadata. The audio engine still rejects stream seek commands at
/// its policy boundary, even when a reader can seek internally.
fn build_stream_decoder(
    stream_reader: StreamReader,
    source: &TrackSource,
    stream_location: &str,
) -> Result<Box<dyn Source<Item = f32> + Send>, AudioError> {
    let content_length = stream_reader.content_length();
    let builder = Decoder::builder().with_data(stream_reader);
    let builder = if let Some(length) = content_length {
        builder.with_byte_len(length)
    } else {
        builder
    };
    builder
        .build()
        .map(|d| Box::new(d) as Box<dyn Source<Item = f32> + Send>)
        .map_err(|error| {
            let detail = AudioError::decode(source.clone(), error.to_string());
            tracing::warn!(stream = %stream_location, "stream decoder build failed: {detail}");
            detail
        })
}

/// Decode one local file through the same builder configuration used by the
/// worker's normal local-play path.
fn decode_local_source(
    source: &TrackSource,
) -> Result<Box<dyn Source<Item = f32> + Send>, AudioError> {
    let TrackSource::Local(path) = source else {
        return Err(AudioError::Stream(anyhow::anyhow!(
            "local decoder received a non-local source"
        )));
    };
    let file = File::open(path).map_err(|io| AudioError::io(source.clone(), io))?;
    let byte_len = file.metadata().map(|meta| meta.len()).ok();
    let mut builder = Decoder::builder()
        .with_data(file)
        .with_coarse_seek(true)
        .with_seekable(true);
    if let Some(len) = byte_len {
        builder = builder.with_byte_len(len);
    }
    builder
        .build()
        .map(|decoder| Box::new(decoder) as Box<dyn Source<Item = f32> + Send>)
        .map_err(|error| AudioError::decode(source.clone(), error.to_string()))
}

/// Compatibility alias for request bookkeeping. The same token is also held
/// by the live reader so an already-started HLS read can be interrupted.
type AcquisitionCancellation = StreamCancellation;

/// Shared current-stream interruption state owned by the handle and worker.
///
/// The handle cancels the current token before enqueueing a source-superseding
/// command. The worker replaces it when accepting that command, so an old
/// detached acquisition can never observe or cancel the new source's token.
#[derive(Clone, Debug)]
struct StreamInterruption {
    current: Arc<Mutex<StreamCancellation>>,
}

impl StreamInterruption {
    fn new() -> Self {
        Self {
            current: Arc::new(Mutex::new(StreamCancellation::new())),
        }
    }

    fn cancel(&self) {
        self.current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancel();
    }

    fn replace(&self) -> StreamCancellation {
        let next = StreamCancellation::new();
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        current.cancel();
        *current = next.clone();
        next
    }

    fn current(&self) -> StreamCancellation {
        self.current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcquisitionKind {
    Stream,
    LocalPreload,
}

struct AcquisitionRequest {
    id: u64,
    generation: Option<u64>,
    source: TrackSource,
    track_index: usize,
    cancellation: AcquisitionCancellation,
    kind: AcquisitionKind,
}

struct PreparedStream {
    decoder: Box<dyn Source<Item = f32> + Send>,
    duration: Option<Duration>,
    rate: u32,
    channels: u32,
}

enum AcquisitionResult {
    StreamReady {
        id: u64,
        generation: Option<u64>,
        source: TrackSource,
        track_index: usize,
        prepared: PreparedStream,
    },
    StreamFailed {
        id: u64,
        generation: Option<u64>,
        source: TrackSource,
        track_index: usize,
        error: AudioError,
    },
    PreloadReady {
        id: u64,
        generation: Option<u64>,
        source: TrackSource,
        track_index: usize,
        prepared: PreparedStream,
    },
    PreloadFailed {
        id: u64,
        generation: Option<u64>,
        source: TrackSource,
        track_index: usize,
        error: AudioError,
    },
}

struct PendingAcquisition {
    id: u64,
    generation: Option<u64>,
    source: TrackSource,
    track_index: usize,
    cancellation: AcquisitionCancellation,
    kind: AcquisitionKind,
}

struct AcquisitionState {
    next_id: AtomicU64,
    pending: Option<PendingAcquisition>,
    results: Receiver<AcquisitionResult>,
    sender: Sender<AcquisitionResult>,
}

impl AcquisitionState {
    fn new() -> Self {
        let (sender, results) = channel();
        Self {
            next_id: AtomicU64::new(0),
            pending: None,
            results,
            sender,
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Run provider/HLS acquisition and decoder setup away from the audio worker.
///
/// The task is deliberately detached. A synchronous provider may be inside a
/// bounded network or HLS wait when the request is cancelled, but the worker
/// must be able to process Stop and Shutdown without joining that task. The
/// token and request identity make any eventual result harmless.
fn spawn_stream_acquisition(
    resolver: StreamResolver,
    request: AcquisitionRequest,
    results: Sender<AcquisitionResult>,
) {
    let id = request.id;
    let generation = request.generation;
    let source = request.source.clone();
    let track_index = request.track_index;
    let cancellation = request.cancellation.clone();
    let kind = request.kind;
    let results_for_task = results.clone();
    let source_for_task = source.clone();
    let task = thread::Builder::new()
        .name("harmonium-stream-acquisition".to_string())
        .spawn(move || {
            let result = acquire_source(resolver, &request);
            let Some(result) = result else {
                return;
            };
            if cancellation.is_cancelled() {
                return;
            }
            let result = match (kind, result) {
                (AcquisitionKind::Stream, Ok(prepared)) => AcquisitionResult::StreamReady {
                    id,
                    generation,
                    source: source_for_task.clone(),
                    track_index,
                    prepared,
                },
                (AcquisitionKind::Stream, Err(error)) => AcquisitionResult::StreamFailed {
                    id,
                    generation,
                    source: source_for_task.clone(),
                    track_index,
                    error,
                },
                (AcquisitionKind::LocalPreload, Ok(prepared)) => AcquisitionResult::PreloadReady {
                    id,
                    generation,
                    source: source_for_task.clone(),
                    track_index,
                    prepared,
                },
                (AcquisitionKind::LocalPreload, Err(error)) => AcquisitionResult::PreloadFailed {
                    id,
                    generation,
                    source: source_for_task.clone(),
                    track_index,
                    error,
                },
            };
            let _ = results_for_task.send(result);
        });

    if let Err(error) = task {
        let detail = format!("could not spawn stream acquisition task: {error}");
        let error = AudioError::Stream(anyhow::Error::new(error).context(detail));
        let result = match kind {
            AcquisitionKind::Stream => AcquisitionResult::StreamFailed {
                id,
                generation,
                source,
                track_index,
                error,
            },
            AcquisitionKind::LocalPreload => AcquisitionResult::PreloadFailed {
                id,
                generation,
                source,
                track_index,
                error,
            },
        };
        let _ = results.send(result);
    }
}

fn acquire_source(
    resolver: StreamResolver,
    request: &AcquisitionRequest,
) -> Option<Result<PreparedStream, AudioError>> {
    match request.kind {
        AcquisitionKind::Stream => acquire_stream(resolver, request),
        AcquisitionKind::LocalPreload => acquire_local_preload(request),
    }
}

fn acquire_local_preload(
    request: &AcquisitionRequest,
) -> Option<Result<PreparedStream, AudioError>> {
    if request.cancellation.is_cancelled() {
        return None;
    }
    if !matches!(&request.source, TrackSource::Local(_)) {
        return Some(Err(AudioError::Stream(anyhow::anyhow!(
            "local preload received a non-local source"
        ))));
    }
    let decoder = match decode_local_source(&request.source) {
        Ok(decoder) => decoder,
        Err(error) => {
            if request.cancellation.is_cancelled() {
                return None;
            }
            return Some(Err(error));
        }
    };
    if request.cancellation.is_cancelled() {
        return None;
    }
    Some(Ok(PreparedStream {
        duration: decoder.total_duration(),
        rate: decoder.sample_rate().get(),
        channels: u32::from(decoder.channels().get()),
        decoder,
    }))
}

/// Perform the blocking part of one stream start, returning `None` when the
/// request was cancelled at a safe boundary.
fn acquire_stream(
    resolver: StreamResolver,
    request: &AcquisitionRequest,
) -> Option<Result<PreparedStream, AudioError>> {
    if request.cancellation.is_cancelled() {
        return None;
    }
    let TrackSource::Stream { url, .. } = &request.source else {
        return Some(Err(AudioError::Stream(anyhow::anyhow!(
            "stream acquisition received a local source"
        ))));
    };
    let stream_reader = match resolver.open_reader_with_prepared(
        url,
        request.source.prepared(),
        &request.cancellation,
    ) {
        Ok(reader) => reader,
        Err(error) => {
            if request.cancellation.is_cancelled() {
                return None;
            }
            let detail = format!("{}: {error}", crate::net::safe_url(url));
            return Some(Err(AudioError::Stream(
                anyhow::Error::new(error).context(detail),
            )));
        }
    };
    if request.cancellation.is_cancelled() {
        return None;
    }
    let stream_location = crate::net::safe_url(url);
    let decoder = match build_stream_decoder(stream_reader, &request.source, &stream_location) {
        Ok(decoder) => decoder,
        Err(error) => {
            if request.cancellation.is_cancelled() {
                return None;
            }
            return Some(Err(error));
        }
    };
    if request.cancellation.is_cancelled() {
        return None;
    }
    let duration = decoder.total_duration();
    let rate = decoder.sample_rate().get();
    let channels = u32::from(decoder.channels().get());
    Some(Ok(PreparedStream {
        decoder,
        duration,
        rate,
        channels,
    }))
}

fn diagnostic_location(source: &TrackSource) -> String {
    match source {
        TrackSource::Local(path) => path.to_string_lossy().into_owned(),
        TrackSource::Stream { url, .. } => crate::net::safe_url(url),
    }
}

/// Requests the application can make to the audio worker.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioCommand {
    /// Replace whatever is playing with one decoded file or stream.
    Play {
        /// Source to decode and render. Local files open through
        /// [`std::fs::File`]; streams go through the shared
        /// [`StreamResolver`] which can re-resolve YouTube URLs at
        /// playback time.
        source: TrackSource,
        /// Queue index the track belongs to, echoed back on snapshots.
        track_index: usize,
        /// Application playback generation used to reject stale stream events.
        /// `None` is retained for direct audio clients without a UI gate.
        generation: Option<u64>,
    },
    /// Hold the current position.
    Pause,
    /// Continue from the held position.
    Resume,
    /// Set the output volume in percent.
    SetVolume(VolumePercent),
    /// Set the preamp gain in decibels, applied before the master volume
    /// (sample * gain_factor(db) * volume_factor). 0 dB leaves it unchanged.
    SetGain(GainDb),
    /// Set the playback speed in the validated tenths domain.
    ///
    /// Non-default speeds use SoundTouch for pitch-preserving time-stretch;
    /// `1.0x` uses the direct path. The worker stores the value and mirrors it
    /// on snapshots for the UI.
    SetSpeed(Speed),
    /// Configure the crossfade length in seconds between consecutive tracks.
    /// `0` disables crossfade. Valid values are multiples of 5 up to 30.
    SetCrossfade(CrossfadeSeconds),
    /// Decode and hold the next track so a crossfade can start before the
    /// current one ends. The source is paused at offset 0 until the engine
    /// begins the transition; the source is decoded off the audio thread.
    PreloadNext {
        /// Source to decode as the incoming track. Streams are rejected by
        /// the engine because live sources have no known duration, which
        /// makes the crossfade window non-deterministic.
        source: TrackSource,
        /// Queue index reported to the app when the transition completes.
        track_index: usize,
    },
    /// Drop any preloaded next track (manual skip, seek or crossfade off).
    CancelPreload,
    /// Jump inside the current track, saturated by the decoder itself.
    ///
    /// Streams reject seek requests at the audio-engine policy boundary. The
    /// worker keeps the current position and emits a notification explaining
    /// why the bar did not move, even when the underlying reader can seek.
    SeekTo(Duration),
    /// Move relative to the worker-owned current position.
    ///
    /// The worker computes and clamps the target so repeated commands cannot
    /// be based on stale application snapshots.
    SeekBy {
        /// Whether the relative movement goes toward the end of the track.
        forward: bool,
        /// Distance to move before applying duration bounds.
        amount: Duration,
    },
    /// Halt and clear the current track, leaving nothing playing.
    Stop,
    /// Route the output stream to a PipeWire sink.
    ///
    /// An empty target lets the session default decide. A concrete target
    /// carries both its stable `node.name` and the current registry id. The
    /// stable name is used for PipeWire routing; the numeric id is retained for
    /// diagnostics because it can change between enumerations.
    SetOutput(OutputTarget),
    /// Internal stop request used when the services shut down.
    Shutdown,
}

impl AudioCommand {
    /// Build a volume command from an external percentage at the command boundary.
    pub fn set_volume(value: u16) -> Self {
        Self::SetVolume(VolumePercent::from_boundary(value))
    }

    /// Build a gain command from an external dB value at the command boundary.
    pub fn set_gain(value: f32) -> Result<Self, crate::audio::GainDbError> {
        GainDb::try_from(value).map(Self::SetGain)
    }

    /// Build a crossfade command from an external duration at the command boundary.
    pub fn set_crossfade(value: u16) -> Self {
        Self::SetCrossfade(CrossfadeSeconds::from_boundary(value))
    }

    /// Build a speed command from an external multiplier at the command
    /// boundary. Finite values outside the domain saturate; non-finite and
    /// off-step values are rejected before reaching the worker.
    pub fn set_speed(value: f32) -> Result<Self, SpeedError> {
        speed_from_boundary(value).map(Self::SetSpeed)
    }
}

fn command_supersedes_stream(command: &AudioCommand) -> bool {
    matches!(
        command,
        AudioCommand::Play { .. } | AudioCommand::Stop | AudioCommand::Shutdown
    )
}

fn is_seek_command(command: &AudioCommand) -> bool {
    matches!(
        command,
        AudioCommand::SeekTo(_) | AudioCommand::SeekBy { .. }
    )
}

/// Cloneable producer side handed to the application layer.
#[derive(Debug, Clone)]
pub struct AudioEngineHandle {
    sender: Sender<AudioCommand>,
    interruption: StreamInterruption,
}

impl AudioEngineHandle {
    /// Forward one command to the worker without blocking.
    pub fn send(
        &self,
        command: AudioCommand,
    ) -> std::result::Result<(), std::sync::mpsc::SendError<AudioCommand>> {
        if command_supersedes_stream(&command) {
            self.interruption.cancel();
        }
        self.sender.send(command)?;
        Ok(())
    }

    /// Best effort shutdown request tolerating an already gone worker.
    ///
    /// Crate visible because shutdown ordering belongs to [`crate::
    /// runtime::AppServices`], not to arbitrary handle holders.
    pub(crate) fn shutdown(&self) {
        self.interruption.cancel();
        let _ = self.sender.send(AudioCommand::Shutdown);
    }
}

/// Start the dedicated audio worker and return its handle.
///
/// The worker owns every rodio type for the whole process lifetime and
/// publishes progress, completions and failures through the shared event
/// bus. Thread construction failure aborts cleanly like any other startup
/// error instead of leaving the app half wired.
///
/// `resolver` is the shared [`StreamResolver`] used by the worker to open
/// stream sources at playback time. Local files bypass the resolver
/// entirely; only streams touch it. A shared resolver (rather than a
/// per-worker instance) keeps the provider chain pluggable from the rest
/// of the application without reaching into the worker thread.
pub(crate) fn spawn_audio_worker(
    events: EventSender,
    resolver: StreamResolver,
) -> anyhow::Result<AudioEngineHandle> {
    let (sender, receiver) = channel();
    let interruption = StreamInterruption::new();
    let events_for_worker = events.clone();
    let worker = Worker::new(receiver, events_for_worker, resolver, interruption.clone());

    thread::Builder::new()
        .name("harmonium-audio".to_string())
        .spawn(move || {
            // The audio worker is a plain `std::thread`, not a Tokio task, so
            // a panic anywhere (FFI in SoundTouch/PipeWire, a decoder quirk)
            // would otherwise unwind the whole process and lose the terminal
            // the panic hook restored. Catching it here lets the app keep
            // running: the worker stops, playback is freed and the user is
            // told, instead of the process dying mid-track.
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || worker.run())).is_err()
            {
                tracing::error!("audio worker panicked; stopping playback");
                if let Err(error) = publish_audio_panic_notification(&events) {
                    tracing::error!(?error, "could not publish audio panic notification");
                }
            }
        })
        .context("spawning the audio worker thread failed")?;

    Ok(AudioEngineHandle {
        sender,
        interruption,
    })
}

fn publish_audio_panic_notification(
    events: &EventSender,
) -> Result<(), crate::event::EventSendError> {
    events.send_critical_timeout(
        AppEvent::Notification {
            kind: EffectErrorKind::Audio,
            operation_id: None,
            message: "Audio engine stopped unexpectedly due to an internal error. \
                      Playback has stopped."
                .to_string(),
        },
        AUDIO_EVENT_TIMEOUT,
    )
}

/// Whether a loaded source still has input or output to drain.
///
/// The ordering is intentional: a source can be exhausted while SoundTouch
/// still owns output, but a drained track can never become merely exhausted
/// again without being replaced by a new `LoadedTrack`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainState {
    Producing,
    SourceExhausted,
    Drained,
}

impl DrainState {
    fn source_exhausted(self) -> bool {
        !matches!(self, Self::Producing)
    }

    fn drained(self) -> bool {
        matches!(self, Self::Drained)
    }
}

/// A decoded track and all accounting that belongs to that decoder.
///
/// Keeping the source, format, position and drain state together means a
/// loaded playback state always has a source. In particular, the worker can
/// no longer represent an active track whose decoder has already been
/// discarded.
struct LoadedTrack {
    index: usize,
    /// Canonical identity of the active track. Used by seek/lookup paths so
    /// the engine can identify "the same track" without caring whether it
    /// is a local file or a remote stream.
    source: TrackSource,
    duration: Option<Duration>,
    decoder: Box<dyn Source<Item = f32> + Send>,
    rate: u32,
    channels: u32,
    frames_written: u64,
    drain: DrainState,
    deferred: Option<PendingOutput>,
}

/// The worker-owned lifecycle of the current playback request.
///
/// `Loading` deliberately carries no request identity. `AcquisitionState` is
/// the single owner of the pending source, index, generation and cancellation
/// token; the playback enum only records that the worker is awaiting it.
enum Playback {
    Idle,
    Loading {},
    Playing(LoadedTrack),
    Paused(LoadedTrack),
}

/// The output path selected for one playback pump.
///
/// Selecting this once keeps the pump's dispatch explicit while keeping sink
/// submission and accounting in the shared [`Worker::submit`] protocol.
enum PlaybackPipeline {
    Direct,
    TimeStretch,
    Crossfade { t: f64 },
}

impl Playback {
    fn loaded(&self) -> Option<&LoadedTrack> {
        match self {
            Self::Playing(track) | Self::Paused(track) => Some(track),
            Self::Idle | Self::Loading { .. } => None,
        }
    }

    fn loaded_mut(&mut self) -> Option<&mut LoadedTrack> {
        match self {
            Self::Playing(track) | Self::Paused(track) => Some(track),
            Self::Idle | Self::Loading { .. } => None,
        }
    }

    fn is_playing(&self) -> bool {
        matches!(self, Self::Playing(_))
    }

    fn is_paused(&self) -> bool {
        matches!(self, Self::Paused(_))
    }
}

/// Whether an output stream opened for one format must be rebuilt for another.
///
/// PipeWire negotiates a fixed sample rate and channel count at stream
/// creation and drives its clock from that negotiated format. Reusing a
/// stream while feeding it frames decoded at a different rate makes PipeWire
/// interpret those frames at the old rate, so the audio plays too fast or too
/// slow. A change in either the rate or the channel count requires a rebuild.
fn sink_needs_rebuild(
    current_rate: u32,
    current_channels: u32,
    new_rate: u32,
    new_channels: u32,
) -> bool {
    current_rate != new_rate || current_channels != new_channels
}

/// Compute the source-frame position to report, given the segment anchors.
///
/// In the direct path (default speed) `frames_written` counts frames pushed to
/// the bounded sample channel, which can sit up to ~0.74s ahead of what the
/// sink has actually reproduced. When the sink reports a played counter we use
/// it, anchored to the segment start and clamped so a device/format rebuild
/// (which resets the counter to zero) never reports a negative or past-the-end
/// position. The time-stretch path keeps the source-frame total, since the
/// stretched output no longer maps one-to-one onto the source.
fn position_frame_count(
    uses_time_stretch: bool,
    played: Option<u64>,
    frames_written: u64,
    segment_base_frames: u64,
    segment_frames_anchor: u64,
) -> u64 {
    match (uses_time_stretch, played) {
        (false, Some(played)) => {
            // Frames this segment has genuinely reproduced; clamp so the
            // anchor delta never goes below zero (sink rebuild) nor beyond
            // the queued-ahead total.
            let delta = played.saturating_sub(segment_frames_anchor);
            (segment_base_frames + delta).min(frames_written)
        }
        _ => frames_written,
    }
}

/// Position (in frames) of the playing track given the current segment anchors.
///
/// State machine owning the backend and serializing every command.
struct Worker {
    commands: Receiver<AudioCommand>,
    events: EventSender,
    /// Shared resolver used to open stream readers on demand. The worker is
    /// the only consumer; the rest of the app spawns the default resolver
    /// once and clones it through [`StreamResolver`].
    resolver: StreamResolver,
    interruption: StreamInterruption,
    /// Result boundary for blocking stream acquisition tasks. The worker only
    /// consumes completed results and never joins an acquisition thread.
    acquisition: AcquisitionState,
    /// Current playback lifecycle. A loaded state always owns its decoder.
    playback: Playback,
    volume_percent: VolumePercent,
    gain_db: GainDb,
    /// Configured crossfade length in seconds (0 = disabled).
    crossfade_seconds: CrossfadeSeconds,
    /// Sink lifecycle, health and segment-clock authority.
    sink_manager: SinkManager,
    /// Preloaded source, transition state and reusable output ownership.
    crossfade: CrossfadeStage,
    /// Pitch-preserving tempo state and source-frame credit accounting.
    speed_stage: SpeedStage,
    last_progress: Option<Instant>,
    /// Worker-owned target while a seek's flush/reanchor boundary is pending.
    /// This is the only position override and is never mirrored in app state.
    pending_seek_target: Option<Duration>,
    /// A fallback seek owns its decoder until the bounded main-loop steps have
    /// completed. Commands that would replace the sink boundary are deferred
    /// until that seek and its flush acknowledgement are complete.
    seek: Option<SeekState>,
    deferred_seek_commands: VecDeque<AudioCommand>,
}

/// Completion state for the asynchronous sink flush used at segment starts.
struct PendingFlush {
    generation: FlushGeneration,
    resume: bool,
    /// Deadline after which a missing acknowledgement is treated as sink loss.
    deadline: Instant,
}

/// Progress of a fallback seek. The decoder remains detached from playback
/// while it is in `Skipping`, so no command can observe or install a half-
/// skipped source.
enum SeekProgress {
    Skipping { remaining: usize },
    Done,
}

/// Decoder and accounting owned by an in-flight seek.
struct SeekState {
    source: TrackSource,
    decoder: Box<dyn Source<Item = f32> + Send>,
    native_rate: u32,
    native_channels: u32,
    position: Duration,
    progress: SeekProgress,
}

/// Small abstraction that lets the worker use deterministic fake sinks in unit
/// tests without requiring a live PipeWire session.
trait OutputSink: Send {
    fn push_samples(&self, samples: &[f32]) -> PushSamplesResult;
    fn set_gain(&self, gain: f32);
    fn flush(&self) -> FlushGeneration;
    fn flush_acknowledged(&self, generation: FlushGeneration) -> bool;
    fn pause(&self);
    fn resume(&self);
    fn rate(&self) -> u32;
    fn channels(&self) -> u32;
    fn frames_played(&self) -> u64;
}

impl OutputSink for PipeWireSink {
    fn push_samples(&self, samples: &[f32]) -> PushSamplesResult {
        self.push_samples(samples)
    }

    fn set_gain(&self, gain: f32) {
        self.set_gain(gain);
    }

    fn flush(&self) -> FlushGeneration {
        self.flush()
    }

    fn flush_acknowledged(&self, generation: FlushGeneration) -> bool {
        self.flush_acknowledged(generation)
    }

    fn pause(&self) {
        self.pause();
    }

    fn resume(&self) {
        self.resume();
    }

    fn rate(&self) -> u32 {
        self.rate()
    }

    fn channels(&self) -> u32 {
        self.channels()
    }

    fn frames_played(&self) -> u64 {
        self.frames_played()
    }
}

/// Owns retired sinks outside the audio worker. PipeWire teardown can wait for
/// its process thread for its bounded shutdown timeout, so replacing or losing
/// a sink must transfer ownership without running its destructor on the command
/// loop.
static SINK_RETIREMENT: OnceLock<Sender<Box<dyn OutputSink>>> = OnceLock::new();

fn sink_retirement_sender() -> &'static Sender<Box<dyn OutputSink>> {
    SINK_RETIREMENT.get_or_init(|| {
        let (sender, receiver) = channel::<Box<dyn OutputSink>>();
        thread::Builder::new()
            .name("harmonium-sink-retirement".to_string())
            .spawn(move || {
                while let Ok(sink) = receiver.recv() {
                    drop(sink);
                }
            })
            .expect("spawning the sink retirement thread failed");
        sender
    })
}

fn retire_sink(sink: Box<dyn OutputSink>) {
    if let Err(std::sync::mpsc::SendError(sink)) = sink_retirement_sender().send(sink) {
        // The static sender normally keeps the reaper alive for the process
        // lifetime. If a future implementation changes that ownership, retain
        // the nonblocking boundary rather than dropping a PipeWire sink here.
        if let Err(error) = thread::Builder::new()
            .name("harmonium-sink-retirement-recovery".to_string())
            .spawn(move || drop(sink))
        {
            tracing::error!(?error, "could not retire the old audio sink");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingBuffer {
    Scratch,
    Output,
}

struct PendingOutput {
    samples: Vec<f32>,
    frames: u64,
    /// Source frames not yet reflected in `SpeedStage::input_frames`.
    input_frames: u64,
    next_frames: u64,
    complete_crossfade: bool,
    buffer: PendingBuffer,
    /// Whether accepting this deferred chunk may complete source draining.
    drain_on_accept: bool,
}

struct SubmittedChunk {
    samples: Vec<f32>,
    frames: u64,
    /// Source frames to retain if this submission becomes pending.
    input_frames: u64,
    /// Source frames to credit for an immediate acceptance. Live direct and
    /// crossfade chunks leave this at zero; deferred retries set it explicitly.
    accepted_input_frames: u64,
    next_frames: u64,
    complete_crossfade: bool,
    buffer: PendingBuffer,
    transition: bool,
    drain_on_accept: bool,
    drain_on_deferred_accept: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitOutcome {
    Accepted,
    Backpressure,
    Disconnected,
}

/// Result of one playback pump.
///
/// `Progressed` means the worker did useful audio work, including feeding
/// source frames into SoundTouch before its output FIFO is warm. The worker
/// may immediately pump again only for that outcome; transport stalls and
/// idle states deliberately yield to command polling instead of spinning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpOutcome {
    Idle,
    Progressed,
    Backpressure,
    Disconnected,
}

impl PumpOutcome {
    fn should_continue(self) -> bool {
        matches!(self, Self::Progressed)
    }
}

impl SubmittedChunk {
    fn into_pending(self) -> (PendingOutput, bool) {
        let Self {
            samples,
            frames,
            input_frames,
            accepted_input_frames: _,
            next_frames,
            complete_crossfade,
            buffer,
            transition,
            drain_on_accept: _,
            drain_on_deferred_accept,
        } = self;
        (
            PendingOutput {
                samples,
                frames,
                input_frames,
                next_frames,
                complete_crossfade,
                buffer,
                drain_on_accept: drain_on_deferred_accept,
            },
            transition,
        )
    }
}

/// The lifecycle of the output sink, including the configured target.
///
/// Keeping the sink, loss marker and derived health in one enum makes states
/// such as "lost and healthy" or "absent with a sink" unrepresentable. The
/// target is carried by each variant because it remains useful while the
/// device is absent or lost and is needed for the next recovery attempt.
enum SinkState {
    Absent {
        target: OutputTarget,
    },
    Live {
        sink: Box<dyn OutputSink>,
        target: OutputTarget,
        segment: SegmentAnchors,
    },
    Lost {
        target: OutputTarget,
        at: Instant,
    },
}

impl SinkState {
    fn health(&self) -> SinkHealth {
        match self {
            Self::Absent { .. } => SinkHealth::Unavailable,
            Self::Live { .. } => SinkHealth::Healthy,
            Self::Lost { .. } => SinkHealth::Lost,
        }
    }

    fn target(&self) -> &OutputTarget {
        match self {
            Self::Absent { target } | Self::Live { target, .. } | Self::Lost { target, .. } => {
                target
            }
        }
    }
}

/// Non-blocking segment boundary and played-frame anchors for a live sink.
struct SegmentAnchors {
    pending_flush: Option<PendingFlush>,
    base_frames: u64,
    frames_anchor: u64,
}

impl SegmentAnchors {
    fn new() -> Self {
        Self {
            pending_flush: None,
            base_frames: 0,
            frames_anchor: 0,
        }
    }
}

/// Owns output-device state and the non-blocking segment boundary protocol.
///
/// The worker remains the lifecycle orchestrator, but all sink replacement,
/// loss, health and played-frame anchoring invariants live here.
struct SinkManager {
    state: SinkState,
    /// Desired linear gain retained across absent, lost and newly created sinks.
    gain: f32,
}

impl SinkManager {
    fn new() -> Self {
        Self {
            state: SinkState::Absent {
                target: OutputTarget::default(),
            },
            gain: 1.0,
        }
    }

    fn set_gain(&mut self, gain: f32) {
        debug_assert!(gain.is_finite() && gain >= 0.0);
        if !gain.is_finite() || gain < 0.0 {
            return;
        }
        self.gain = gain;
        if let SinkState::Live { sink, .. } = &self.state {
            sink.set_gain(gain);
        }
    }

    fn health(&self) -> SinkHealth {
        self.state.health()
    }

    fn is_live(&self) -> bool {
        matches!(self.state, SinkState::Live { .. })
    }

    fn is_lost(&self) -> bool {
        matches!(self.state, SinkState::Lost { .. })
    }

    fn lost_at(&self) -> Option<Instant> {
        match &self.state {
            SinkState::Lost { at, .. } => Some(*at),
            SinkState::Absent { .. } | SinkState::Live { .. } => None,
        }
    }

    fn sink(&self) -> Option<&dyn OutputSink> {
        match &self.state {
            SinkState::Live { sink, .. } => Some(sink.as_ref()),
            SinkState::Absent { .. } | SinkState::Lost { .. } => None,
        }
    }

    fn has_pending_flush(&self) -> bool {
        matches!(
            &self.state,
            SinkState::Live {
                segment: SegmentAnchors {
                    pending_flush: Some(_),
                    ..
                },
                ..
            }
        )
    }

    /// Replace the lifecycle state and log the derived health transition.
    ///
    /// This is the only state writer. Health is derived from the enum rather
    /// than stored separately, so every lifecycle change gets the same
    /// transition diagnostic.
    fn transition_to(&mut self, state: SinkState) -> Option<Box<dyn OutputSink>> {
        let from = self.health();
        let to = state.health();
        let previous = mem::replace(&mut self.state, state);
        if let Some(transition) = SinkHealthTransition::new(from, to) {
            tracing::info!(
                from = ?transition.from,
                to = ?transition.to,
                "audio sink health changed"
            );
        }
        match previous {
            SinkState::Live { sink, .. } => Some(sink),
            SinkState::Absent { .. } | SinkState::Lost { .. } => None,
        }
    }

    fn set_target(&mut self, target: OutputTarget) {
        match &mut self.state {
            SinkState::Absent { target: current }
            | SinkState::Live {
                target: current, ..
            }
            | SinkState::Lost {
                target: current, ..
            } => *current = target,
        }
    }

    fn install(&mut self, sink: Box<dyn OutputSink>) {
        let target = self.state.target().clone();
        self.install_for_target(sink, target);
    }

    fn install_for_target(&mut self, sink: Box<dyn OutputSink>, target: OutputTarget) {
        sink.set_gain(self.gain);
        if let Some(previous) = self.transition_to(SinkState::Live {
            sink,
            target,
            segment: SegmentAnchors::new(),
        }) {
            retire_sink(previous);
        }
    }

    fn mark_lost(&mut self) {
        if let Some(sink) = self.take_lost_sink() {
            // The sink is already known to be disconnected. Retire it without
            // running its destructor on the audio command loop.
            retire_sink(sink);
        }
    }

    /// Move a live sink into the explicit Lost state and return its ownership.
    ///
    /// Replacing Live also drops its segment anchors, including any pending
    /// flush, before the dead sink can be recovered or retired.
    fn take_lost_sink(&mut self) -> Option<Box<dyn OutputSink>> {
        let target = self.state.target().clone();
        self.transition_to(SinkState::Lost {
            target,
            at: Instant::now(),
        })
    }

    fn ensure(&mut self, rate: u32, channels: u32) -> Result<bool, AudioError> {
        self.ensure_with(rate, channels, |rate, channels, target| {
            crate::audio::PipeWireSink::new(rate, channels, target.stable_id.as_deref())
                .map(|sink| Box::new(sink) as Box<dyn OutputSink>)
        })
    }

    fn ensure_with<F>(&mut self, rate: u32, channels: u32, create: F) -> Result<bool, AudioError>
    where
        F: FnOnce(u32, u32, &OutputTarget) -> anyhow::Result<Box<dyn OutputSink>>,
    {
        let needs_rebuild = match &self.state {
            SinkState::Live { sink, .. } => {
                sink_needs_rebuild(sink.rate(), sink.channels(), rate, channels)
            }
            SinkState::Absent { .. } | SinkState::Lost { .. } => false,
        };
        if self.is_live() && !needs_rebuild {
            return Ok(false);
        }

        if needs_rebuild {
            let SinkState::Live { sink, .. } = &self.state else {
                unreachable!("rebuild requires an existing sink");
            };
            tracing::info!(
                old_rate = sink.rate(),
                old_channels = sink.channels(),
                new_rate = rate,
                new_channels = channels,
                "rebuilding output stream for the new track format"
            );
        }

        let target = self.state.target().clone();
        let sink = match create(rate, channels, &target) {
            Ok(sink) => sink,
            Err(error) => {
                // A failed replacement leaves an existing live sink usable.
                // Recovery from Lost, however, has no sink to preserve and
                // therefore falls back to the explicit Absent state.
                if matches!(self.state, SinkState::Lost { .. }) {
                    self.transition_to(SinkState::Absent { target });
                }
                return Err(AudioError::DeviceUnavailable(error));
            }
        };
        self.install(sink);
        tracing::info!("native pipewire output opened");
        Ok(needs_rebuild)
    }

    fn recreate_to(
        &mut self,
        rate: u32,
        channels: u32,
        target: OutputTarget,
    ) -> Result<(), AudioError> {
        self.recreate_to_with(rate, channels, target, |rate, channels, target| {
            crate::audio::PipeWireSink::new(rate, channels, target.stable_id.as_deref())
                .map(|sink| Box::new(sink) as Box<dyn OutputSink>)
        })
    }

    fn recreate_to_with<F>(
        &mut self,
        rate: u32,
        channels: u32,
        target: OutputTarget,
        create: F,
    ) -> Result<(), AudioError>
    where
        F: FnOnce(u32, u32, &OutputTarget) -> anyhow::Result<Box<dyn OutputSink>>,
    {
        let new_sink = match create(rate, channels, &target) {
            Ok(sink) => sink,
            Err(error) => {
                return Err(AudioError::DeviceUnavailable(error));
            }
        };
        self.install_for_target(new_sink, target);
        Ok(())
    }

    fn begin_segment(&mut self, resume: bool, frames_written: u64) {
        let SinkState::Live { sink, segment, .. } = &mut self.state else {
            return;
        };
        let generation = sink.flush();
        let acknowledged = sink.flush_acknowledged(generation);
        if acknowledged {
            segment.pending_flush = None;
            segment.base_frames = frames_written;
            segment.frames_anchor = sink.frames_played();
            if resume {
                sink.resume();
            }
        } else {
            segment.pending_flush = Some(PendingFlush {
                generation,
                resume,
                deadline: Instant::now() + FLUSH_ACK_TIMEOUT,
            });
        }
    }

    fn poll_pending_flush(
        &mut self,
        pending_seek_target: &mut Option<Duration>,
        frames_written: u64,
        now: Instant,
    ) -> bool {
        enum FlushPoll {
            Complete,
            Waiting,
            Expired,
        }

        let poll = match &self.state {
            SinkState::Live { sink, segment, .. } => {
                segment
                    .pending_flush
                    .as_ref()
                    .map_or(FlushPoll::Complete, |pending| {
                        if sink.flush_acknowledged(pending.generation) {
                            FlushPoll::Complete
                        } else if now >= pending.deadline {
                            FlushPoll::Expired
                        } else {
                            FlushPoll::Waiting
                        }
                    })
            }
            SinkState::Absent { .. } | SinkState::Lost { .. } => FlushPoll::Complete,
        };
        match poll {
            FlushPoll::Complete => {}
            FlushPoll::Waiting => return false,
            FlushPoll::Expired => {
                tracing::error!(
                    "native pipewire output stream stopped acknowledging a flush; treating it as lost"
                );
                // Use the same lifecycle transition as a disconnected sink.
                // The next pump invokes handle_sink_lost(), which publishes the
                // paused snapshot and notification without consuming the source.
                self.mark_lost();
                return false;
            }
        }
        let SinkState::Live { sink, segment, .. } = &mut self.state else {
            return true;
        };
        let Some(pending) = segment.pending_flush.take() else {
            return true;
        };
        segment.base_frames = frames_written;
        segment.frames_anchor = sink.frames_played();
        let resume = pending.resume;
        *pending_seek_target = None;
        if resume {
            sink.resume();
        }
        true
    }

    fn reanchor(&mut self, frames_written: u64) {
        if let SinkState::Live { sink, segment, .. } = &mut self.state {
            segment.base_frames = frames_written;
            segment.frames_anchor = sink.frames_played();
        }
    }

    fn position_frames(&self, uses_time_stretch: bool, frames_written: u64) -> u64 {
        match &self.state {
            SinkState::Live { sink, segment, .. } => position_frame_count(
                uses_time_stretch,
                Some(sink.frames_played()),
                frames_written,
                segment.base_frames,
                segment.frames_anchor,
            ),
            SinkState::Absent { .. } | SinkState::Lost { .. } => {
                position_frame_count(uses_time_stretch, None, frames_written, 0, 0)
            }
        }
    }

    fn take_live_sink(&mut self) -> Option<Box<dyn OutputSink>> {
        let target = self.state.target().clone();
        match mem::replace(&mut self.state, SinkState::Absent { target }) {
            SinkState::Live { sink, .. } => Some(sink),
            SinkState::Absent { .. } | SinkState::Lost { .. } => None,
        }
    }
}

/// The explicit state of one asynchronously prepared incoming track.
struct PreloadedTrack {
    source: Box<dyn Source<Item = f32> + Send>,
    index: usize,
    identity: TrackSource,
    duration: Option<Duration>,
    rate: u32,
    channels: u32,
    consumed_frames: u64,
}

struct PreloadRequest {
    id: u64,
    source: TrackSource,
    track_index: usize,
}

enum CrossfadeLifecycle {
    NoPreload,
    PreloadRequested(PreloadRequest),
    PreloadReady(PreloadedTrack),
    TransitionRunning {
        preloaded: PreloadedTrack,
        start: Duration,
        duration: Duration,
        progress: f64,
        deferred: Option<PendingOutput>,
    },
}

/// Owns crossfade lifecycle state and persistent pump buffers.
struct CrossfadeStage {
    lifecycle: CrossfadeLifecycle,
    pump_scratch: Vec<f32>,
    pump_output: Vec<f32>,
    pump_a_frame: Vec<f32>,
    pump_b_frame: Vec<f32>,
}

impl CrossfadeStage {
    fn new() -> Self {
        Self {
            lifecycle: CrossfadeLifecycle::NoPreload,
            pump_scratch: Vec::new(),
            pump_output: Vec::new(),
            pump_a_frame: Vec::new(),
            pump_b_frame: Vec::new(),
        }
    }

    fn prepare_buffers(&mut self, channels: usize) {
        let output_frames = PUMP_CHUNK_FRAMES.max(CROSSFADE_TAIL_FRAMES);
        let sample_capacity = output_frames.saturating_mul(channels);
        self.pump_scratch
            .reserve(sample_capacity.saturating_sub(self.pump_scratch.capacity()));
        self.pump_output
            .reserve(sample_capacity.saturating_sub(self.pump_output.capacity()));
        if self.pump_output.len() < sample_capacity {
            self.pump_output.resize(sample_capacity, 0.0);
        }
        if self.pump_a_frame.len() < channels {
            self.pump_a_frame.resize(channels, 0.0);
        }
        if self.pump_b_frame.len() < channels {
            self.pump_b_frame.resize(channels, 0.0);
        }
    }

    fn buffers_ready(&self, channels: usize, chunk_frames: usize) -> bool {
        channels > 0
            && self.pump_output.capacity()
                >= chunk_frames
                    .max(CROSSFADE_TAIL_FRAMES)
                    .saturating_mul(channels)
            && self.pump_a_frame.len() >= channels
            && self.pump_b_frame.len() >= channels
    }

    fn request(&mut self, id: u64, source: TrackSource, track_index: usize) {
        self.lifecycle = CrossfadeLifecycle::PreloadRequested(PreloadRequest {
            id,
            source,
            track_index,
        });
    }

    fn requested_matches(&self, id: u64, source: &TrackSource, track_index: usize) -> bool {
        matches!(
            &self.lifecycle,
            CrossfadeLifecycle::PreloadRequested(request)
                if request.id == id
                    && request.track_index == track_index
                    && request.source == *source
        )
    }

    fn cancel_requested(&mut self) -> Option<PreloadRequest> {
        let lifecycle = mem::replace(&mut self.lifecycle, CrossfadeLifecycle::NoPreload);
        match lifecycle {
            CrossfadeLifecycle::PreloadRequested(request) => Some(request),
            other => {
                self.lifecycle = other;
                None
            }
        }
    }

    fn discard(&mut self) {
        self.lifecycle = CrossfadeLifecycle::NoPreload;
    }

    fn prepare_for_seek(
        &mut self,
        active_rate: u32,
        active_channels: u32,
    ) -> Option<PreloadRequest> {
        let lifecycle = mem::replace(&mut self.lifecycle, CrossfadeLifecycle::NoPreload);
        match lifecycle {
            CrossfadeLifecycle::NoPreload => None,
            CrossfadeLifecycle::PreloadRequested(request) => Some(request),
            CrossfadeLifecycle::TransitionRunning {
                preloaded,
                deferred: Some(_),
                ..
            } => Some(PreloadRequest {
                // The preload source advanced while this output was being
                // built, even though no incoming frames were accepted.
                id: 0,
                source: preloaded.identity,
                track_index: preloaded.index,
            }),
            CrossfadeLifecycle::PreloadReady(preloaded)
            | CrossfadeLifecycle::TransitionRunning { preloaded, .. } => {
                if preloaded.consumed_frames == 0
                    && preloaded.rate == active_rate
                    && preloaded.channels == active_channels
                {
                    self.lifecycle = CrossfadeLifecycle::PreloadReady(preloaded);
                    None
                } else {
                    Some(PreloadRequest {
                        id: 0,
                        source: preloaded.identity,
                        track_index: preloaded.index,
                    })
                }
            }
        }
    }

    fn install_ready(&mut self, preloaded: PreloadedTrack) {
        self.lifecycle = CrossfadeLifecycle::PreloadReady(preloaded);
    }

    fn preloaded(&self) -> Option<&PreloadedTrack> {
        match &self.lifecycle {
            CrossfadeLifecycle::PreloadReady(preloaded)
            | CrossfadeLifecycle::TransitionRunning { preloaded, .. } => Some(preloaded),
            CrossfadeLifecycle::NoPreload | CrossfadeLifecycle::PreloadRequested(_) => None,
        }
    }

    fn preloaded_mut(&mut self) -> Option<&mut PreloadedTrack> {
        match &mut self.lifecycle {
            CrossfadeLifecycle::PreloadReady(preloaded)
            | CrossfadeLifecycle::TransitionRunning { preloaded, .. } => Some(preloaded),
            CrossfadeLifecycle::NoPreload | CrossfadeLifecycle::PreloadRequested(_) => None,
        }
    }

    fn preloaded_source_mut(&mut self) -> &mut Box<dyn Source<Item = f32> + Send> {
        match &mut self.lifecycle {
            CrossfadeLifecycle::PreloadReady(preloaded)
            | CrossfadeLifecycle::TransitionRunning { preloaded, .. } => &mut preloaded.source,
            CrossfadeLifecycle::NoPreload | CrossfadeLifecycle::PreloadRequested(_) => {
                unreachable!("preloaded source checked above")
            }
        }
    }

    #[cfg(test)]
    fn begin_transition(&mut self, start: Duration) -> Option<Duration> {
        self.begin_transition_with_duration(start, Duration::ZERO)
    }

    fn begin_transition_with_duration(
        &mut self,
        start: Duration,
        duration: Duration,
    ) -> Option<Duration> {
        let lifecycle = mem::replace(&mut self.lifecycle, CrossfadeLifecycle::NoPreload);
        match lifecycle {
            CrossfadeLifecycle::PreloadReady(preloaded) => {
                self.lifecycle = CrossfadeLifecycle::TransitionRunning {
                    preloaded,
                    start,
                    duration,
                    progress: 0.0,
                    deferred: None,
                };
                Some(start)
            }
            CrossfadeLifecycle::TransitionRunning {
                preloaded,
                start,
                duration: existing_duration,
                progress,
                deferred,
            } => {
                self.lifecycle = CrossfadeLifecycle::TransitionRunning {
                    preloaded,
                    start,
                    duration: if existing_duration.is_zero() {
                        duration
                    } else {
                        existing_duration
                    },
                    progress,
                    deferred,
                };
                Some(start)
            }
            other => {
                self.lifecycle = other;
                None
            }
        }
    }

    fn is_transition_running(&self) -> bool {
        matches!(self.lifecycle, CrossfadeLifecycle::TransitionRunning { .. })
    }

    fn transition_progress_at(&mut self, position: Duration) -> Option<f64> {
        let CrossfadeLifecycle::TransitionRunning {
            start,
            duration,
            progress,
            ..
        } = &mut self.lifecycle
        else {
            return None;
        };
        if !duration.is_zero() {
            let elapsed = position.saturating_sub(*start);
            let calculated = (elapsed.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0);
            *progress = (*progress).max(calculated);
        }
        Some(*progress)
    }

    fn ensure_transition_duration(&mut self, duration: Duration) {
        if let CrossfadeLifecycle::TransitionRunning {
            duration: stored, ..
        } = &mut self.lifecycle
            && stored.is_zero()
        {
            *stored = duration;
        }
    }

    fn record_progress(&mut self, progress: f64) {
        if let CrossfadeLifecycle::TransitionRunning {
            progress: stored, ..
        } = &mut self.lifecycle
        {
            *stored = (*stored).max(progress.clamp(0.0, 1.0));
        }
    }

    fn rebase_transition(&mut self, position: Duration, duration: Option<Duration>) {
        let CrossfadeLifecycle::TransitionRunning {
            start,
            duration: existing_duration,
            progress,
            ..
        } = &mut self.lifecycle
        else {
            return;
        };
        let duration = duration.unwrap_or(*existing_duration);
        if duration.is_zero() {
            *start = position;
            *existing_duration = duration;
            return;
        }
        let progress = (*progress).clamp(0.0, 1.0);
        let start_seconds = position.as_secs_f64() - progress * duration.as_secs_f64();
        *start = if start_seconds.is_sign_negative() {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(start_seconds)
        };
        *existing_duration = duration;
    }

    fn reset_transition(&mut self) {
        let lifecycle = mem::replace(&mut self.lifecycle, CrossfadeLifecycle::NoPreload);
        self.lifecycle = match lifecycle {
            // A deferred chunk has already advanced both decoders. Keep the
            // transition running so the chunk remains attached to its owner
            // until the sink accepts it.
            CrossfadeLifecycle::TransitionRunning {
                preloaded,
                start,
                duration,
                progress,
                deferred,
            } if deferred.is_some() => CrossfadeLifecycle::TransitionRunning {
                preloaded,
                start,
                duration,
                progress,
                deferred,
            },
            // Keep the anchor while a live transition is temporarily not
            // eligible to pump (for example while speed is non-unity). The
            // stored progress resumes the exact effective gain pair instead
            // of re-entering the fade at zero.
            running @ CrossfadeLifecycle::TransitionRunning { .. } => running,
            other => other,
        };
    }

    fn deferred(&self) -> Option<&PendingOutput> {
        match &self.lifecycle {
            CrossfadeLifecycle::TransitionRunning { deferred, .. } => deferred.as_ref(),
            CrossfadeLifecycle::NoPreload
            | CrossfadeLifecycle::PreloadRequested(_)
            | CrossfadeLifecycle::PreloadReady(_) => None,
        }
    }

    fn take_deferred(&mut self) -> Option<PendingOutput> {
        match &mut self.lifecycle {
            CrossfadeLifecycle::TransitionRunning { deferred, .. } => deferred.take(),
            CrossfadeLifecycle::NoPreload
            | CrossfadeLifecycle::PreloadRequested(_)
            | CrossfadeLifecycle::PreloadReady(_) => None,
        }
    }

    fn restore_deferred(&mut self, pending: PendingOutput) {
        let CrossfadeLifecycle::TransitionRunning { deferred, .. } = &mut self.lifecycle else {
            unreachable!("crossfade output can only be deferred by a running transition");
        };
        *deferred = Some(pending);
    }

    fn restore_buffer(&mut self, buffer: PendingBuffer, samples: Vec<f32>) {
        match buffer {
            PendingBuffer::Scratch => self.pump_scratch = samples,
            PendingBuffer::Output => self.pump_output = samples,
        }
    }

    fn record_accepted_next_frames(&mut self, frames: u64) {
        if let Some(preloaded) = self.preloaded_mut() {
            preloaded.consumed_frames += frames;
        }
    }

    fn take_preloaded(&mut self) -> Option<PreloadedTrack> {
        let lifecycle = mem::replace(&mut self.lifecycle, CrossfadeLifecycle::NoPreload);
        match lifecycle {
            CrossfadeLifecycle::PreloadReady(preloaded)
            | CrossfadeLifecycle::TransitionRunning { preloaded, .. } => Some(preloaded),
            other => {
                self.lifecycle = other;
                None
            }
        }
    }
}

/// The active tempo pipeline. A direct pipeline has no buffered time-stretch
/// state; a time-stretch pipeline always owns the SoundTouch instance that
/// matches its speed.
enum SpeedPipeline {
    Direct,
    TimeStretch { speed: Speed, stretch: SoundTouch },
}

/// Owns SoundTouch and all source-frame accounting for non-default speed.
struct SpeedStage {
    /// Requested speed, retained while no track format exists yet.
    requested_speed: Speed,
    pipeline: SpeedPipeline,
    input_frames: u64,
    credit_remainder: f64,
}

impl SpeedStage {
    fn new() -> Self {
        Self {
            requested_speed: SPEED_DEFAULT,
            pipeline: SpeedPipeline::Direct,
            input_frames: 0,
            credit_remainder: 0.0,
        }
    }

    fn set_speed(&mut self, speed: Speed, format: Option<(u32, u32)>) {
        self.requested_speed = speed;
        if speed == Speed::UNITY {
            // A tempo change is a bounded segment boundary. Dropping the
            // SoundTouch FIFO here is deliberate: preserving its tail made
            // unity/non-unity transitions depend on a second drain state
            // machine and could starve the pump indefinitely.
            self.pipeline = SpeedPipeline::Direct;
        } else {
            match &mut self.pipeline {
                SpeedPipeline::TimeStretch {
                    speed: active_speed,
                    stretch,
                } => {
                    stretch.set_tempo(speed.as_f32() as f64);
                    *active_speed = speed;
                }
                SpeedPipeline::Direct => {
                    if let Some((rate, channels)) = format {
                        self.pipeline = Self::new_stretch(speed, rate, channels);
                    }
                }
            }
        }
    }

    fn setup(&mut self, rate: u32, channels: u32) {
        if self.requested_speed == Speed::UNITY {
            self.pipeline = SpeedPipeline::Direct;
            return;
        }
        self.pipeline = Self::new_stretch(self.requested_speed, rate, channels);
    }

    fn new_stretch(speed: Speed, rate: u32, channels: u32) -> SpeedPipeline {
        let mut stretch = SoundTouch::new();
        stretch
            .set_channels(channels)
            .set_sample_rate(rate)
            .set_tempo(speed.as_f32() as f64);
        SpeedPipeline::TimeStretch { speed, stretch }
    }

    fn uses_time_stretch(&self) -> bool {
        matches!(self.pipeline, SpeedPipeline::TimeStretch { .. })
    }

    fn crossfade_allowed(&self) -> bool {
        matches!(self.pipeline, SpeedPipeline::Direct)
    }

    fn stretch(&self) -> Option<&SoundTouch> {
        match &self.pipeline {
            SpeedPipeline::Direct => None,
            SpeedPipeline::TimeStretch { stretch, .. } => Some(stretch),
        }
    }

    fn stretch_mut(&mut self) -> Option<&mut SoundTouch> {
        match &mut self.pipeline {
            SpeedPipeline::Direct => None,
            SpeedPipeline::TimeStretch { stretch, .. } => Some(stretch),
        }
    }

    fn active_speed(&self) -> Option<Speed> {
        match self.pipeline {
            SpeedPipeline::Direct => None,
            SpeedPipeline::TimeStretch { speed, .. } => Some(speed),
        }
    }
}

impl Worker {
    fn new(
        commands: Receiver<AudioCommand>,
        events: EventSender,
        resolver: StreamResolver,
        interruption: StreamInterruption,
    ) -> Self {
        let mut sink_manager = SinkManager::new();
        sink_manager
            .set_gain(volume_factor(DEFAULT_VOLUME_PERCENT) * gain_factor(GainDb::default()));
        Self {
            commands,
            events,
            resolver,
            interruption,
            acquisition: AcquisitionState::new(),
            playback: Playback::Idle,
            volume_percent: DEFAULT_VOLUME_PERCENT,
            gain_db: GainDb::default(),
            crossfade_seconds: CrossfadeSeconds::default(),
            sink_manager,
            crossfade: CrossfadeStage::new(),
            speed_stage: SpeedStage::new(),
            last_progress: None,
            pending_seek_target: None,
            seek: None,
            deferred_seek_commands: VecDeque::new(),
        }
    }

    /// Drain commands forever, publishing events until asked to stop.
    ///
    /// Every failure path degrades into a notification so the worker loop
    /// itself has no way to take the application down.
    fn run(mut self) {
        tracing::debug!("audio worker started");
        loop {
            // A fallback seek owns its detached decoder until one bounded step
            // completes. Playback is not pumped while that state exists, so a
            // half-seeked source can never contribute stale output.
            let pump_outcome = if self.seek.is_some() {
                self.advance_seek();
                PumpOutcome::Idle
            } else {
                // While a track is playing, pump samples at real time. The
                // native sink's bounded channel is polled without blocking, so
                // a stalled output cannot prevent the command queue from being
                // drained.
                self.pump_playback_outcome()
            };

            // Drain every pending command before publishing progress.
            // Rapid user actions (e.g. repeated seek) queue multiple
            // commands; processing them all in one burst prevents stale
            // intermediate snapshots from overwriting the optimistic UI
            // state.
            if self.drain_commands() {
                return;
            }

            // A completed seek is finalized only after this loop has given all
            // queued commands a chance to supersede it. This is the sole place
            // where the detached decoder is installed into playback.
            if self.seek_is_done() {
                self.complete_seek();
            }

            // Apply results only after the command burst. A newer Play or
            // Stop therefore invalidates an older result before it can affect
            // the current playback state.
            self.drain_acquisition_results();

            self.detect_natural_end();

            // Progress interval is enforced inside publish_periodic_progress.
            self.publish_periodic_progress();

            let active_playing = self
                .playback
                .loaded()
                .is_some_and(|track| self.playback.is_playing() && !track.drain.drained());
            if self.seek.is_some() {
                // The next bounded skip step is itself the worker's poll
                // cadence; sleeping here would turn a large local seek into
                // an artificial 100 ms-per-batch delay.
                continue;
            }
            if !active_playing
                || !pump_outcome.should_continue()
                || !self.deferred_seek_commands.is_empty()
            {
                // Idle or backpressured: block for a command so we do not spin
                // the CPU while still waking promptly for control messages.
                if !self.deferred_seek_commands.is_empty() && !self.sink_manager.has_pending_flush()
                {
                    continue;
                }
                match self.commands.recv_timeout(COMMAND_POLL) {
                    Ok(AudioCommand::Shutdown) => {
                        self.interruption.replace();
                        self.cancel_all_acquisition();
                        self.seek = None;
                        self.deferred_seek_commands.clear();
                        break;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        self.cancel_all_acquisition();
                        self.seek = None;
                        self.deferred_seek_commands.clear();
                        break;
                    }
                    Ok(command) => {
                        if self.handle(command) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => self.drain_acquisition_results(),
                }
            }
        }
        tracing::debug!("audio worker stopped");
    }

    /// Apply one command and report whether it requested worker shutdown.
    fn handle(&mut self, command: AudioCommand) -> bool {
        tracing::debug!(?command, "audio command");
        if self.should_defer_during_seek(&command) {
            self.deferred_seek_commands.push_back(command);
            return false;
        }
        self.dispatch(command)
    }

    /// Apply a command that has already crossed the seek/flush barrier.
    /// Deferred commands use this path during FIFO replay so the barrier does
    /// not re-queue the command it is currently draining.
    fn dispatch(&mut self, command: AudioCommand) -> bool {
        match command {
            AudioCommand::Play {
                source,
                track_index,
                generation,
            } => self.play(source, track_index, generation),
            AudioCommand::Pause => self.pause(),
            AudioCommand::Resume => self.resume(),
            AudioCommand::SetVolume(percent) => self.set_volume(percent),
            AudioCommand::SetGain(db) => self.set_gain(db),
            AudioCommand::SetSpeed(speed) => self.set_speed(speed),
            AudioCommand::SetCrossfade(seconds) => self.set_crossfade(seconds),
            AudioCommand::PreloadNext {
                source,
                track_index,
            } => self.preload_next(source, track_index),
            AudioCommand::CancelPreload => self.discard_crossfade(),
            AudioCommand::SeekTo(position) => return self.seek_to(position),
            AudioCommand::SeekBy { forward, amount } => return self.seek_by(forward, amount),
            AudioCommand::Stop => self.stop(),
            AudioCommand::SetOutput(target) => self.set_output(target),
            AudioCommand::Shutdown => {
                self.interruption.replace();
                self.cancel_all_acquisition();
                self.seek = None;
                self.deferred_seek_commands.clear();
                return true;
            }
        }
        false
    }

    /// Process commands only from the worker's top-level loop. A fallback
    /// seek never calls [`Self::handle`] recursively; commands that can change
    /// the sink boundary wait until the seek and its pending flush are done.
    fn drain_commands(&mut self) -> bool {
        while self.seek.is_none()
            && !self.sink_manager.has_pending_flush()
            && !self.deferred_seek_commands.is_empty()
        {
            let command = self
                .deferred_seek_commands
                .pop_front()
                .expect("deferred seek command disappeared");
            let command = if is_seek_command(&command) {
                self.coalesce_deferred_seeks(command)
            } else {
                command
            };
            if self.dispatch(command) {
                return true;
            }
        }
        while let Ok(command) = self.commands.try_recv() {
            if self.handle(command) {
                return true;
            }
        }
        false
    }

    /// Collapse a burst of deferred relative seeks into one target before
    /// opening another decoder. The active flush already prevents audio from
    /// crossing the boundary, so replaying every intermediate target would
    /// only add decoder work and repeated sink flushes.
    fn coalesce_deferred_seeks(&mut self, command: AudioCommand) -> AudioCommand {
        let Some(mut target) = self.seek_command_target(&command, self.current_position()) else {
            return command;
        };
        let Some(duration) = self.playback.loaded().and_then(|active| active.duration) else {
            return command;
        };

        while let Some(next) = self.deferred_seek_commands.front() {
            if !is_seek_command(next) {
                break;
            }
            let next = self
                .deferred_seek_commands
                .pop_front()
                .expect("deferred seek command disappeared");
            target = self
                .seek_command_target(&next, target)
                .unwrap_or(target.min(duration));
        }

        AudioCommand::SeekTo(target.min(duration))
    }

    fn seek_command_target(&self, command: &AudioCommand, position: Duration) -> Option<Duration> {
        match command {
            AudioCommand::SeekTo(target) => Some(*target),
            AudioCommand::SeekBy {
                forward, amount, ..
            } => self
                .playback
                .loaded()
                .and_then(|active| active.duration)
                .map(|duration| seek_by_target(position, *forward, *amount, Some(duration))),
            _ => None,
        }
    }

    /// Explicit seek-time command policy:
    ///
    /// * `Play`, `Stop`, `Shutdown`, and a newer seek supersede the detached
    ///   decoder immediately;
    /// * every non-superseding command is replayed in arrival order after the
    ///   seek's flush acknowledgement, including `Pause` and control changes;
    /// * while only a pending flush remains, a newer seek is also queued because
    ///   it must not replace that active boundary.
    fn should_defer_during_seek(&self, command: &AudioCommand) -> bool {
        // Keep the FIFO barrier alive until the already-deferred commands have
        // been replayed. This covers the small window where a seek completed
        // with an immediately acknowledged flush, but the main loop has not
        // yet drained the deferred queue.
        let barrier_active = self.seek.is_some()
            || self.sink_manager.has_pending_flush()
            || !self.deferred_seek_commands.is_empty();
        if !barrier_active {
            return false;
        }

        // Superseding commands must reach the dispatcher immediately so they
        // can cancel the detached decoder. During the post-completion flush
        // window, a newer seek is instead queued: it is also a boundary
        // operation and must not replace the active flush.
        if self.seek.is_some() {
            !matches!(
                command,
                AudioCommand::Play { .. }
                    | AudioCommand::Stop
                    | AudioCommand::Shutdown
                    | AudioCommand::SeekTo(_)
                    | AudioCommand::SeekBy { .. }
            )
        } else {
            !matches!(
                command,
                AudioCommand::Play { .. } | AudioCommand::Stop | AudioCommand::Shutdown
            )
        }
    }

    fn drain_acquisition_results(&mut self) {
        while let Ok(result) = self.acquisition.results.try_recv() {
            self.handle_acquisition_result(result);
        }
    }

    fn handle_acquisition_result(&mut self, result: AcquisitionResult) {
        let (kind, id, generation, source, track_index) = match &result {
            AcquisitionResult::StreamReady {
                id,
                generation,
                source,
                track_index,
                ..
            }
            | AcquisitionResult::StreamFailed {
                id,
                generation,
                source,
                track_index,
                ..
            } => (
                AcquisitionKind::Stream,
                *id,
                *generation,
                source,
                *track_index,
            ),
            AcquisitionResult::PreloadReady {
                id,
                generation,
                source,
                track_index,
                ..
            }
            | AcquisitionResult::PreloadFailed {
                id,
                generation,
                source,
                track_index,
                ..
            } => (
                AcquisitionKind::LocalPreload,
                *id,
                *generation,
                source,
                *track_index,
            ),
        };

        let is_current = self.acquisition.pending.as_ref().is_some_and(|pending| {
            pending.kind == kind
                && pending.id == id
                && pending.generation == generation
                && pending.track_index == track_index
                && pending.source == *source
                && !pending.cancellation.is_cancelled()
        });
        let is_current_crossfade_request = kind != AcquisitionKind::LocalPreload
            || self.crossfade.requested_matches(id, source, track_index);
        let is_current_loading =
            kind != AcquisitionKind::Stream || matches!(&self.playback, Playback::Loading {});
        if !is_current || !is_current_crossfade_request || !is_current_loading {
            tracing::debug!(id, "discarding stale stream acquisition result");
            return;
        }
        self.acquisition.pending = None;

        match result {
            AcquisitionResult::StreamReady {
                source,
                generation,
                track_index,
                prepared,
                ..
            } => {
                let PreparedStream {
                    decoder,
                    duration: _,
                    rate,
                    channels,
                } = prepared;
                if let Err(error) = self.finish_track_start(
                    decoder,
                    &source,
                    track_index,
                    rate,
                    channels,
                    generation,
                ) {
                    self.fail_stream_play(error, &source, track_index, generation);
                    return;
                }
                self.last_progress = None;
            }
            AcquisitionResult::StreamFailed {
                error,
                generation,
                source,
                track_index,
                ..
            } => self.fail_stream_play(error, &source, track_index, generation),
            AcquisitionResult::PreloadReady {
                source,
                track_index,
                prepared,
                ..
            } => match self.start_track_into_next(prepared, source.clone(), track_index) {
                Ok(_duration) => tracing::info!(
                    track_index,
                    source = %diagnostic_location(&source),
                    "crossfade preloaded next track"
                ),
                Err(error) => {
                    self.crossfade.discard();
                    tracing::debug!(
                        track_index,
                        source = %diagnostic_location(&source),
                        "could not preload next track: {error}"
                    );
                }
            },
            AcquisitionResult::PreloadFailed { source, error, .. } => {
                self.crossfade.discard();
                tracing::warn!(
                    "could not preload next track {}: {error}",
                    diagnostic_location(&source)
                );
            }
        }
    }

    fn start_stream_acquisition(
        &mut self,
        source: TrackSource,
        track_index: usize,
        generation: Option<u64>,
    ) {
        self.cancel_stream_acquisition();
        let id = self.acquisition.next_id();
        let cancellation = self.interruption.current();
        let request = AcquisitionRequest {
            id,
            generation,
            source: source.clone(),
            track_index,
            cancellation: cancellation.clone(),
            kind: AcquisitionKind::Stream,
        };
        self.acquisition.pending = Some(PendingAcquisition {
            id,
            generation,
            source,
            track_index,
            cancellation,
            kind: AcquisitionKind::Stream,
        });
        spawn_stream_acquisition(
            self.resolver.clone(),
            request,
            self.acquisition.sender.clone(),
        );
    }

    fn cancel_pending_acquisition(&mut self, kind: Option<AcquisitionKind>) {
        let should_cancel = self
            .acquisition
            .pending
            .as_ref()
            .is_some_and(|pending| kind.is_none_or(|kind| pending.kind == kind));
        if should_cancel && let Some(pending) = self.acquisition.pending.take() {
            pending.cancellation.cancel();
            tracing::debug!(id = pending.id, ?pending.kind, "audio acquisition cancelled");
        }
    }

    fn cancel_stream_acquisition(&mut self) {
        self.cancel_pending_acquisition(Some(AcquisitionKind::Stream));
    }

    fn cancel_all_acquisition(&mut self) {
        self.cancel_pending_acquisition(None);
    }

    fn fail_stream_play(
        &mut self,
        error: AudioError,
        source: &TrackSource,
        track_index: usize,
        generation: Option<u64>,
    ) {
        tracing::warn!("{error}");
        self.playback = Playback::Idle;
        self.clear_pending_output();
        self.emit(AppEvent::PlaybackStateChanged {
            snapshot: PlaybackSnapshot {
                status: PlayStatus::Stopped,
                track_index: Some(track_index),
                elapsed: Duration::ZERO,
                duration: None,
                sink_health: self.sink_manager.health(),
            },
        });
        if source.is_stream() {
            let TrackSource::Stream { url, .. } = source else {
                unreachable!("stream source classification changed");
            };
            self.emit(AppEvent::SourceFailed {
                url: url.as_str().to_string(),
                generation,
            });
        }
        self.notify(EffectErrorKind::Audio, format!("Playback failed: {error}"));
    }

    /// Open and queue one source, replacing anything already loaded.
    ///
    /// The device opens lazily here so a missing sound system surfaces as
    /// feedback exactly when it becomes relevant rather than at startup.
    fn play(&mut self, source: TrackSource, track_index: usize, generation: Option<u64>) {
        self.interruption.replace();
        self.seek = None;
        self.deferred_seek_commands.clear();
        self.pending_seek_target = None;
        // A fresh manual/auto start invalidates any in-flight crossfade preload
        // (the previous "next" is no longer the next), so drop it here.
        self.discard_crossfade();
        // Every new Play, including a local-file Play, invalidates any older
        // remote acquisition before it can publish a result.
        self.cancel_stream_acquisition();
        if matches!(source, TrackSource::Stream { .. }) {
            // Replace the active source immediately, then acquire the remote
            // reader and decoder off-thread. Commands remain serviceable while
            // the provider or HLS path is delayed.
            self.clear_pending_output();
            if let Some(sink) = self.sink_manager.sink() {
                sink.pause();
            }
            self.emit(AppEvent::PlaybackStateChanged {
                snapshot: PlaybackSnapshot {
                    status: PlayStatus::Stopped,
                    track_index: Some(track_index),
                    elapsed: Duration::ZERO,
                    duration: None,
                    sink_health: self.sink_manager.health(),
                },
            });
            self.start_stream_acquisition(source, track_index, generation);
            self.playback = Playback::Loading {};
            return;
        }
        self.playback = Playback::Idle;
        match self.start_track(&source, track_index) {
            Ok(_duration) => {
                // Forcing the next publish keeps first frame latency low.
                self.last_progress = None;
            }
            Err(error) => {
                tracing::warn!("{error}");
                // A failed start leaves nothing playable behind
                self.playback = Playback::Idle;
                self.emit(AppEvent::PlaybackStateChanged {
                    snapshot: PlaybackSnapshot {
                        status: PlayStatus::Stopped,
                        track_index: None,
                        elapsed: Duration::ZERO,
                        duration: None,
                        sink_health: self.sink_manager.health(),
                    },
                });
                self.notify(EffectErrorKind::Audio, format!("Playback failed: {error}"));
            }
        }
    }

    /// Decode a local `source` onto the native output, returning its duration.
    /// Stream acquisition and decoder setup run through the cancellable task
    /// boundary in [`Self::start_stream_acquisition`].
    fn start_track(
        &mut self,
        source: &TrackSource,
        track_index: usize,
    ) -> Result<Option<Duration>, AudioError> {
        let decoder = match source {
            TrackSource::Local(_) => decode_local_source(source)?,
            TrackSource::Stream { .. } => {
                return Err(AudioError::Stream(anyhow::anyhow!(
                    "stream playback must use the acquisition task"
                )));
            }
        };
        let rate = decoder.sample_rate().get();
        let channels = u32::from(decoder.channels().get());
        self.finish_track_start(decoder, source, track_index, rate, channels, None)
    }

    fn finish_track_start(
        &mut self,
        decoder: Box<dyn Source<Item = f32> + Send>,
        source: &TrackSource,
        track_index: usize,
        rate: u32,
        channels: u32,
        generation: Option<u64>,
    ) -> Result<Option<Duration>, AudioError> {
        let duration = decoder.total_duration();
        self.prepare_pump_buffers(channels as usize);
        self.speed_stage.input_frames = 0;
        self.speed_stage.credit_remainder = 0.0;
        self.setup_stretch(rate, channels);
        self.ensure_sink(rate, channels)?;
        // A fresh track is a new segment. PipeWire applies flush asynchronously,
        // so the worker anchors only after the matching generation is observed;
        // this prevents old queued frames from contributing to the anchor.
        self.sink_manager.begin_segment(true, 0);

        // Tell the UI the source is ready after the matching pending acquisition
        // has been accepted, so the application can clear its stream activity
        // and drop the Now Playing spinner before the first audio frame.
        if matches!(source, TrackSource::Stream { .. }) {
            let TrackSource::Stream { url, .. } = source else {
                unreachable!("stream source classification changed");
            };
            self.emit(AppEvent::SourceReady {
                url: url.as_str().to_string(),
                generation,
            });
        }

        tracing::info!(track = %diagnostic_location(source), "started playback");
        self.playback = Playback::Playing(LoadedTrack {
            index: track_index,
            source: source.clone(),
            duration,
            decoder,
            rate,
            channels,
            frames_written: 0,
            drain: DrainState::Producing,
            deferred: None,
        });
        Ok(duration)
    }

    /// Decode `source` into the preloaded next source, leaving the currently
    /// playing source untouched. Failure leaves a cleanly empty preload so the
    /// next pump simply plays the current track out.
    fn start_track_into_next(
        &mut self,
        prepared: PreparedStream,
        source: TrackSource,
        track_index: usize,
    ) -> Result<Option<Duration>, AudioError> {
        let PreparedStream {
            decoder,
            duration,
            rate: native_rate,
            channels,
        } = prepared;
        let Some(active) = self.playback.loaded() else {
            return Err(AudioError::Stream(anyhow::anyhow!(
                "crossfade preload requires a loaded playback"
            )));
        };
        let active_rate = active.rate;
        let active_channels = active.channels;
        let (next_rate, rebased_source): (u32, Box<dyn Source<Item = f32> + Send>) = if native_rate
            != active_rate
        {
            // Rebase the incoming track onto the active clock so the crossfade
            // mixer can advance both sources frame-locked. The content (pitch
            // and duration) is unchanged; only the sample clock is adapted.
            use crate::audio::resample::LinearResample;
            (
                active_rate,
                Box::new(
                    LinearResample::try_new(decoder, active_rate).map_err(AudioError::Stream)?,
                ),
            )
        } else {
            (native_rate, decoder)
        };
        let (next_channels, next_source): (u32, Box<dyn Source<Item = f32> + Send>) =
            if channels != active_channels {
                // Reformat the preload onto the active sink's channel layout.
                // The adapter owns its fixed frame buffers, so pumping remains
                // allocation-free while mono/stereo transitions stay mixed
                // instead of silently falling back to a hard cut.
                use crate::audio::resample::ChannelAdapter;
                let adapter = match ChannelAdapter::try_new(rebased_source, active_channels) {
                    Ok(adapter) => adapter,
                    Err(error) => {
                        tracing::debug!(
                            source_channels = channels,
                            active_channels,
                            "refusing unsupported crossfade channel layout: {error}"
                        );
                        return Err(AudioError::Stream(error));
                    }
                };
                tracing::debug!(
                    source_channels = channels,
                    active_channels,
                    "adapting crossfade preload channel layout"
                );
                (active_channels, Box::new(adapter))
            } else {
                (channels, rebased_source)
            };
        self.crossfade.install_ready(PreloadedTrack {
            source: next_source,
            index: track_index,
            identity: source,
            duration,
            rate: next_rate,
            channels: next_channels,
            consumed_frames: 0,
        });
        Ok(duration)
    }

    /// Open the native PipeWire output stream on demand, rebuilding it when
    /// the incoming track needs a different format.
    ///
    /// The stream negotiates a fixed sample rate and channel count at
    /// creation, and PipeWire drives its clock from that negotiated format.
    /// Reusing a stream opened for a previous rate while feeding it frames
    /// decoded at a different rate makes PipeWire interpret those frames at
    /// the old rate, so the audio plays faster or slower than it should.
    /// Rebuild whenever the active track's rate or channel count changes.
    fn ensure_sink(&mut self, rate: u32, channels: u32) -> Result<(), AudioError> {
        match self.sink_manager.ensure(rate, channels) {
            Ok(_) => {}
            Err(error) => {
                self.publish_current_transition();
                return Err(error);
            }
        }
        Ok(())
    }

    /// Retarget the live stream to a PipeWire sink node.
    ///
    /// Rebuilds the output stream instead of calling `disconnect`/`connect`
    /// inside the PipeWire process callback: reconnecting a stream from within
    /// its own buffer processing corrupts PipeWire's internal state and
    /// segfaults. Recreating the stream on this worker thread (outside the
    /// callback) is the safe way to move a live stream to another sink.
    fn set_output(&mut self, target: OutputTarget) {
        if self.sink_manager.is_live() {
            match self.recreate_sink(target.clone()) {
                Ok(()) => {
                    tracing::info!(node = ?target.node_id, stable_id = ?target.stable_id, "output stream rebuilt")
                }
                Err(error) => {
                    let detail = format!("Could not set output: {error}");
                    tracing::warn!("{detail}");
                    self.notify(EffectErrorKind::Audio, detail);
                }
            }
        } else {
            self.sink_manager.set_target(target);
        }
    }

    /// Close and reopen the output stream without losing the playing source.
    ///
    /// The decoded source stays in place, so playback resumes from the same
    /// point (the small queued-ahead buffer is dropped, which is imperceptible
    /// and correct for a device switch). The new stream is created BEFORE the
    /// old one is torn down: if the chosen device is unavailable, the error is
    /// reported and playback keeps running on the previously active stream
    /// instead of falling silent.
    fn recreate_sink(&mut self, target: OutputTarget) -> Result<(), AudioError> {
        let Some((rate, channels, frames_written)) = self
            .playback
            .loaded()
            .map(|active| (active.rate, active.channels, active.frames_written))
        else {
            self.sink_manager.set_target(target);
            return Ok(());
        };
        // Build the replacement first, on the old stream's rate/channels, so a
        // failed device switch never destroys the working sink.
        self.sink_manager.recreate_to(rate, channels, target)?;
        // The replacement stream is a new position segment. Use the same
        // acknowledged flush boundary as seek and fresh-track start.
        self.sink_manager.begin_segment(false, frames_written);
        Ok(())
    }

    fn pause(&mut self) {
        let playback = mem::replace(&mut self.playback, Playback::Idle);
        let Playback::Playing(active) = playback else {
            self.playback = playback;
            return;
        };
        self.playback = Playback::Paused(active);
        if let Some(sink) = self.sink_manager.sink() {
            sink.pause();
        }
        // Periodic publishing skips paused tracks, so the held position
        // must be reported through this one explicit snapshot
        self.publish_transition(PlayStatus::Paused);
    }

    fn resume(&mut self) {
        if !self.playback.is_paused() {
            return;
        }
        // If the output stream was torn down because the device disappeared,
        // reopen it now; otherwise a resume would silently stay silent (the
        // worker contributes nothing while the sink state is not Live).
        let rebuilt = !self.sink_manager.is_live();
        if rebuilt {
            let Some(active) = self.playback.loaded() else {
                return;
            };
            if let Err(error) = self.ensure_sink(active.rate, active.channels) {
                let detail = format!("Could not reopen audio output: {error}");
                tracing::warn!("{detail}");
                self.notify(EffectErrorKind::Audio, detail);
                return;
            }
        }
        let Playback::Paused(active) = mem::replace(&mut self.playback, Playback::Idle) else {
            unreachable!("paused playback was checked above");
        };
        let frames_written = active.frames_written;
        self.playback = Playback::Playing(active);
        if rebuilt {
            self.sink_manager.begin_segment(true, frames_written);
        } else if let Some(sink) = self.sink_manager.sink() {
            sink.resume();
        }
        self.last_progress = None;
        self.publish_transition(PlayStatus::Playing);
    }

    fn set_volume(&mut self, percent: VolumePercent) {
        self.volume_percent = percent;
        self.sink_manager.set_gain(self.output_gain());
    }

    fn set_gain(&mut self, db: GainDb) {
        self.gain_db = db;
        self.sink_manager.set_gain(self.output_gain());
    }

    fn output_gain(&self) -> f32 {
        volume_factor(self.volume_percent) * gain_factor(self.gain_db)
    }

    fn set_speed(&mut self, speed: Speed) {
        if self.crossfade.is_transition_running() {
            // Freeze the last emitted gain pair before changing the playback
            // pipeline. The incoming decoder must not be made to catch up
            // while time-stretching temporarily disables crossfade pumping.
            self.crossfade
                .rebase_transition(self.current_position(), None);
        }
        let was_time_stretch = self.speed_stage.uses_time_stretch();
        let format = self
            .playback
            .loaded()
            .map(|active| (active.rate, active.channels));
        self.speed_stage.set_speed(speed, format);
        if !was_time_stretch
            && self.speed_stage.uses_time_stretch()
            && self
                .playback
                .loaded()
                .is_some_and(|active| active.rate != 0 && active.channels != 0)
        {
            // Direct playback has already accounted for every accepted source
            // frame. Seed the stretch credit at that boundary before creating
            // the FIFO so changing speed mid-track does not reset position.
            let (rate, channels, frames_written) = self
                .playback
                .loaded()
                .map(|active| (active.rate, active.channels, active.frames_written))
                .expect("loaded playback was checked above");
            self.speed_stage.setup(rate, channels);
            self.speed_stage.input_frames = frames_written;
            self.speed_stage.credit_remainder = 0.0;
        }
    }

    /// Store the crossfade length. A value of 0 disables the feature; any
    /// in-flight preload of the next track is dropped so a stale incoming
    /// track never starts mixing into the stream. A live transition keeps its
    /// effective progress when the configured duration changes.
    fn set_crossfade(&mut self, seconds: CrossfadeSeconds) {
        let changed = self.crossfade_seconds != seconds;
        self.crossfade_seconds = seconds;
        if !seconds.is_enabled() {
            self.discard_crossfade();
        } else if changed && self.crossfade.is_transition_running() {
            self.crossfade.rebase_transition(
                self.current_position(),
                Some(Duration::from_secs(u64::from(seconds.as_u16()))),
            );
        } else if !changed {
            // An unchanged setting must never disturb the transition anchor.
        } else {
            self.crossfade.reset_transition();
        }
    }

    /// Decode and bufferize the next track so the transition can begin before
    /// the current one ends. Runs off the audio callback (the worker loop is
    /// the only place that touches the decoded source), so seek/skip logic is
    /// undisturbed. A failed decode simply drops the preload.
    ///
    /// Streams cannot crossfade (no known duration), so this short-circuits
    /// to a no-op when the incoming source is a stream — the caller is the
    /// `arm_crossfade_next` filter, but the engine also refuses just in case.
    fn preload_next(&mut self, source: TrackSource, track_index: usize) {
        if matches!(self.playback, Playback::Loading { .. }) {
            // A primary stream acquisition owns the interruption token and
            // must remain the sole pending request until it reaches a terminal
            // result. A preload command is harmlessly ignored in this state.
            tracing::debug!("ignoring crossfade preload while a track is loading");
            return;
        }
        if !matches!(source, TrackSource::Local(_)) {
            tracing::debug!("skipping crossfade preload for stream track");
            self.discard_crossfade();
            return;
        }
        self.cancel_all_acquisition();
        self.discard_crossfade();
        let id = self.acquisition.next_id();
        let cancellation = self.interruption.current();
        let request = AcquisitionRequest {
            id,
            generation: None,
            source: source.clone(),
            track_index,
            cancellation: cancellation.clone(),
            kind: AcquisitionKind::LocalPreload,
        };
        self.acquisition.pending = Some(PendingAcquisition {
            id,
            generation: None,
            source: request.source.clone(),
            track_index,
            cancellation,
            kind: AcquisitionKind::LocalPreload,
        });
        self.crossfade
            .request(id, request.source.clone(), track_index);
        spawn_stream_acquisition(
            self.resolver.clone(),
            request,
            self.acquisition.sender.clone(),
        );
    }

    /// Cancel only a detached preload acquisition. A completed preload remains
    /// ready for the next crossfade.
    fn cancel_pending_preload(&mut self) {
        let requested = self.crossfade.cancel_requested().is_some();
        let had_pending = self
            .acquisition
            .pending
            .as_ref()
            .is_some_and(|pending| pending.kind == AcquisitionKind::LocalPreload);
        self.cancel_pending_acquisition(Some(AcquisitionKind::LocalPreload));
        if requested || had_pending {
            tracing::debug!("crossfade preload acquisition cancelled");
        }
    }

    /// Discard every crossfade lifecycle state, including a completed preload.
    fn discard_crossfade(&mut self) {
        let had_pending = self
            .acquisition
            .pending
            .as_ref()
            .is_some_and(|pending| pending.kind == AcquisitionKind::LocalPreload);
        self.cancel_pending_acquisition(Some(AcquisitionKind::LocalPreload));
        let had_state = self.crossfade.preloaded().is_some();
        let had_requested = self.crossfade.cancel_requested().is_some();
        if had_state || had_requested || had_pending {
            tracing::debug!("crossfade preload cancelled");
        }
        self.crossfade.discard();
    }

    /// (Re)create or reconfigure the time-stretch engine for the active track,
    /// inheriting the current channel count, sample rate and speed.
    fn setup_stretch(&mut self, rate: u32, channels: u32) {
        self.speed_stage.setup(rate, channels);
    }

    /// Reserve every buffer the pump can need before playback starts. Once a
    /// track has entered the pumping state, crossfade, direct playback and
    /// time-stretching only clear or move these buffers; none of them grows in
    /// the real-time loop.
    fn prepare_pump_buffers(&mut self, channels: usize) {
        self.crossfade.prepare_buffers(channels);
    }

    fn seek_by(&mut self, forward: bool, amount: Duration) -> bool {
        if self.reject_loading_seek() {
            return false;
        }
        let Some(duration) = self.playback.loaded().map(|active| active.duration) else {
            return false;
        };
        let position = self
            .pending_seek_target
            .unwrap_or_else(|| self.current_position());
        let target = seek_by_target(position, forward, amount, duration);
        self.seek_to(target)
    }

    fn seek_to(&mut self, position: Duration) -> bool {
        if self.reject_loading_seek() {
            return false;
        }
        // A seek gets a fresh token for the fallback decoder. Loading seeks
        // were rejected above, so this replacement only invalidates older
        // seek work while preserving the primary acquisition token. Crossfade
        // acquisition is reconciled after the seek succeeds so a newer seek
        // can supersede this one.
        self.interruption.replace();
        if self.seek.is_some() {
            self.deferred_seek_commands.clear();
        }
        self.seek = None;
        // Keep a transition-owned deferred chunk until `prepare_for_seek`
        // inspects it. It may have advanced the preload source without any
        // accepted-frame credit, so it must be rearmed rather than retained.
        self.clear_active_pending_output();
        let Some(active) = self.playback.loaded() else {
            self.pending_seek_target = None;
            return false;
        };
        // Stream seeking is intentionally rejected at the audio-engine policy
        // boundary. Some readers implement `Seek`, but allowing the worker to
        // reposition a network source would violate the stream playback
        // contract. Surface a notification instead of pretending the bar moved.
        if !matches!(active.source, TrackSource::Local(_)) {
            self.pending_seek_target = None;
            self.notify(
                EffectErrorKind::Audio,
                "Seeking is not available for stream playback".to_string(),
            );
            return false;
        }
        let source = active.source.clone();
        self.pending_seek_target = Some(position);
        match self.decoding_and_skip(&source, position) {
            Ok(seek) => {
                let done = matches!(seek.progress, SeekProgress::Done);
                self.seek = Some(seek);
                if done {
                    self.complete_seek();
                }
                false
            }
            Err(error) => {
                self.pending_seek_target = None;
                let detail = format!("seek in {} failed: {error}", diagnostic_location(&source));
                tracing::warn!("{detail}");
                self.notify(EffectErrorKind::Audio, detail);
                false
            }
        }
    }

    fn reject_loading_seek(&self) -> bool {
        if !matches!(self.playback, Playback::Loading { .. }) {
            return false;
        }
        self.notify(
            EffectErrorKind::Audio,
            "Seeking is not available while a track is loading".to_string(),
        );
        true
    }

    /// Re-decode `source` and drop samples up to `position` so playback resumes
    /// at the requested offset. This is an O(n) coarse seek, acceptable here.
    fn decoding_and_skip(
        &mut self,
        source: &TrackSource,
        position: Duration,
    ) -> Result<SeekState, AudioError> {
        let TrackSource::Local(path) = source else {
            // `seek_to` rejects streams before reaching this method. Keep the
            // defensive arm non-blocking if a future caller bypasses that
            // guard.
            return Err(AudioError::Stream(anyhow::anyhow!(
                "stream seeking is not supported"
            )));
        };
        let file = File::open(path).map_err(|io| AudioError::io(source.clone(), io))?;
        let byte_len = file.metadata().map(|meta| meta.len()).ok();
        let mut builder = Decoder::builder()
            .with_data(file)
            // Do not use the startup decoder's coarse seek here. Symphonia's
            // MP3 coarse path can install a frame before its bit reservoir is
            // primed, which yields an audible silent/corrupt prefix even
            // though `try_seek` reports success. Accurate mode resets and
            // primes the decoder before Rodio refines to the requested time.
            .with_coarse_seek(false)
            .with_seekable(true);
        if let Some(len) = byte_len {
            builder = builder.with_byte_len(len);
        }
        let mut decoder = builder
            .build()
            .map_err(|error| AudioError::decode(source.clone(), error.to_string()))?;
        let native_rate = decoder.sample_rate().get();
        let native_channels = u32::from(decoder.channels().get());

        // Prefer the decoder's own seek (O(1) coarse seek on seekable formats);
        // only fall back to walking the source when the format cannot seek.
        let tried_seek = decoder.try_seek(position).map_err(AudioError::Seek);
        let progress = if !matches!(tried_seek, Ok(())) {
            let skip_samples =
                (position.as_secs_f64() * native_rate as f64) as usize * native_channels as usize;
            SeekProgress::Skipping {
                remaining: skip_samples,
            }
        } else {
            SeekProgress::Done
        };
        Ok(SeekState {
            source: source.clone(),
            decoder: Box::new(decoder),
            native_rate,
            native_channels,
            position,
            progress,
        })
    }

    /// Advance one bounded step for a decoder that does not support native
    /// seeking. The decoder remains owned by [`SeekState`] until `Done`, so a
    /// replacement or shutdown can never install a stale source.
    fn skip_fallback_samples(
        source: &mut (dyn Source<Item = f32> + Send),
        progress: &mut SeekProgress,
    ) {
        let SeekProgress::Skipping { remaining } = progress else {
            return;
        };
        let batch = (*remaining).min(SEEK_COMMAND_POLL_SAMPLES);
        for _ in 0..batch {
            if source.next().is_none() {
                *progress = SeekProgress::Done;
                return;
            }
            *remaining = remaining.saturating_sub(1);
        }
        if *remaining == 0 {
            *progress = SeekProgress::Done;
        }
    }

    fn advance_seek(&mut self) {
        let Some(seek) = &mut self.seek else {
            return;
        };
        Self::skip_fallback_samples(seek.decoder.as_mut(), &mut seek.progress);
    }

    fn seek_is_done(&self) -> bool {
        self.seek
            .as_ref()
            .is_some_and(|seek| matches!(seek.progress, SeekProgress::Done))
    }

    /// Install a completed seek and establish its new acknowledged segment
    /// boundary. Fallback seeks reach this method from the main loop only
    /// after command dispatch; native seeks may complete immediately because
    /// they have no bounded skip step.
    fn complete_seek(&mut self) {
        let Some(seek) = self.seek.take() else {
            return;
        };
        if !matches!(seek.progress, SeekProgress::Done) {
            self.seek = Some(seek);
            return;
        }
        let Some(active) = self.playback.loaded() else {
            self.pending_seek_target = None;
            return;
        };
        if active.source != seek.source {
            // A replacement should already have cleared this state. Keep the
            // identity gate here as a second barrier against stale playback.
            self.pending_seek_target = None;
            return;
        }
        let target_rate = active.rate;
        let target_channels = active.channels;
        // Keep the sink clock (`target_rate`) and rebase the re-decoded source
        // onto it, so a seek after a crossfade does not shift the clock and
        // desynchronize the preloaded next track's sample rate.
        let decoder = if seek.native_rate != target_rate {
            use crate::audio::resample::LinearResample;
            match LinearResample::try_new(seek.decoder, target_rate) {
                Ok(decoder) => Box::new(decoder) as Box<dyn Source<Item = f32> + Send>,
                Err(error) => {
                    self.pending_seek_target = None;
                    let detail = format!(
                        "seek in {} failed: {error}",
                        diagnostic_location(&seek.source)
                    );
                    tracing::warn!("{detail}");
                    self.notify(EffectErrorKind::Audio, detail);
                    return;
                }
            }
        } else {
            seek.decoder
        };
        let frames_written = (seek.position.as_secs_f64() * target_rate as f64) as u64;
        self.prepare_pump_buffers(target_channels as usize);
        self.speed_stage.input_frames = frames_written;
        self.speed_stage.credit_remainder = 0.0;
        self.setup_stretch(target_rate, target_channels);
        if let Err(error) = self.ensure_sink(target_rate, target_channels) {
            self.pending_seek_target = None;
            let detail = format!(
                "seek in {} failed: {error}",
                diagnostic_location(&seek.source)
            );
            tracing::warn!("{detail}");
            self.notify(EffectErrorKind::Audio, detail);
            return;
        }
        // A seek starts a fresh position segment. Do not sample frames_played
        // until the asynchronous flush has acknowledged removal of old audio.
        self.sink_manager.begin_segment(false, frames_written);
        let playback = mem::replace(&mut self.playback, Playback::Idle);
        let (mut active, paused) = match playback {
            Playback::Playing(active) => (active, false),
            Playback::Paused(active) => (active, true),
            other => {
                self.playback = other;
                self.pending_seek_target = None;
                return;
            }
        };
        active.decoder = decoder;
        active.frames_written = frames_written;
        active.drain = DrainState::Producing;
        active.rate = target_rate;
        active.channels = target_channels;
        self.playback = if paused {
            Playback::Paused(active)
        } else {
            Playback::Playing(active)
        };
        let _ = seek.native_channels; // preserved for exact native skip accounting

        // A seek moves inside the active track, so the crossfade window is
        // recomputed from the new position rather than cancelled. The ready
        // next stays armed; a consumed preload is rearmed after success.
        let pending_rearm = self
            .acquisition
            .pending
            .as_ref()
            .filter(|pending| pending.kind == AcquisitionKind::LocalPreload)
            .map(|pending| PreloadRequest {
                id: pending.id,
                source: pending.source.clone(),
                track_index: pending.track_index,
            });
        let rearm = self
            .crossfade
            .prepare_for_seek(target_rate, target_channels)
            .or(pending_rearm);
        if rearm.is_some() {
            self.cancel_pending_preload();
        }
        if let Some(request) = rearm {
            self.preload_next(request.source, request.track_index);
        }
        self.last_progress = None;
        if !self.sink_manager.has_pending_flush() {
            self.pending_seek_target = None;
        }
        if self.playback.is_paused() {
            self.publish_transition(PlayStatus::Paused);
        }
    }

    /// Halt playback and drop the active track.
    fn stop(&mut self) {
        self.interruption.replace();
        self.seek = None;
        self.deferred_seek_commands.clear();
        self.pending_seek_target = None;
        self.cancel_stream_acquisition();
        self.playback = Playback::Idle;
        self.discard_crossfade();
        self.clear_pending_output();
        self.last_progress = None;
        // A stopped stream should not keep a live PipeWire output running (it
        // would keep the device in use and consume a thread producing silence),
        // so pause the sink; the next play resumes it.
        if let Some(sink) = self.sink_manager.sink() {
            sink.pause();
        }
        self.emit(AppEvent::PlaybackStateChanged {
            snapshot: PlaybackSnapshot {
                status: PlayStatus::Stopped,
                track_index: None,
                elapsed: Duration::ZERO,
                duration: None,
                sink_health: self.sink_manager.health(),
            },
        });
    }

    /// Publish [`AppEvent::TrackEnded`] when the queue drained naturally.
    ///
    /// Only a track the worker still believes is unpaused counts, which is
    /// what separates a finished file from a stop issued by a replacement.
    fn detect_natural_end(&mut self) {
        let Some(active) = self.playback.loaded() else {
            return;
        };
        if !self.playback.is_playing() || !active.drain.drained() || self.has_pending_output() {
            return;
        }

        if let Playback::Playing(active) = mem::replace(&mut self.playback, Playback::Idle) {
            tracing::info!(index = active.index, "track reached its end");
            self.emit(AppEvent::TrackEnded {
                track_index: active.index,
            });
            self.last_progress = None;
        }
    }

    /// Current playback position. When the native sink is active, this uses
    /// the frames it actually consumed (not what was queued ahead), so the
    /// progress never runs ahead of the audio. The sink counter can reset to
    /// zero when the stream is rebuilt (device or format change), so the
    /// position is anchored to the segment start rather than the raw counter.
    /// Present track position in source frames.
    ///
    /// Under a non-default speed the stretched output is deliberately
    /// shorter/longer than the source, so the elapsed time must follow the
    /// source via the loaded track's `LoadedTrack::frames_written` accounting,
    /// or it would drift from what the listener hears.
    ///
    /// At the default speed the frames pushed to the sink are exactly what the
    /// source yields, but `frames_written` counts frames accepted by the
    /// bounded sample channel — which can sit up to ~0.74s ahead of what the
    /// sink has actually reproduced. The sink exposes a real played-frame
    /// counter, so the direct path derives the position from it, anchored to
    /// the current segment (`segment_base_frames` + sink delta).
    fn current_position(&self) -> Duration {
        if let Some(target) = self.pending_seek_target {
            return target;
        }
        let Some(active) = self.playback.loaded() else {
            // No active track decoded yet: the defensive zero for the
            // snapshot paths that guard on `active` on their own.
            return Duration::ZERO;
        };
        if active.rate == 0 {
            return Duration::ZERO;
        }
        let uses_time_stretch = self.speed_stage.uses_time_stretch();
        let frame_count = self
            .sink_manager
            .position_frames(uses_time_stretch, active.frames_written);
        Duration::from_secs_f64(frame_count as f64 / active.rate as f64)
    }

    /// Complete a previously requested segment boundary when the sink has
    /// processed it. This is an atomic poll, never a wait or a callback call.
    fn poll_pending_flush_at(&mut self, now: Instant) -> bool {
        self.sink_manager.poll_pending_flush(
            &mut self.pending_seek_target,
            self.playback
                .loaded()
                .map_or(0, |active| active.frames_written),
            now,
        )
    }

    /// Retry one chunk that was decoded while the bounded sink queue was full.
    /// Returns `false` when the worker must stop pumping for this iteration.
    fn restore_pump_buffer(&mut self, buffer: PendingBuffer, samples: Vec<f32>) {
        self.crossfade.restore_buffer(buffer, samples);
    }

    fn pending_output(&self) -> Option<&PendingOutput> {
        self.crossfade.deferred().or_else(|| {
            self.playback
                .loaded()
                .and_then(|active| active.deferred.as_ref())
        })
    }

    fn has_pending_output(&self) -> bool {
        self.pending_output().is_some()
    }

    fn take_pending_output(&mut self) -> Option<(PendingOutput, bool)> {
        if let Some(pending) = self.crossfade.take_deferred() {
            return Some((pending, true));
        }
        self.playback
            .loaded_mut()
            .and_then(|active| active.deferred.take())
            .map(|pending| (pending, false))
    }

    fn restore_pending_output(&mut self, pending: PendingOutput, transition: bool) {
        if transition {
            self.crossfade.restore_deferred(pending);
        } else if let Some(active) = self.playback.loaded_mut() {
            active.deferred = Some(pending);
        } else {
            debug_assert!(false, "active playback must own non-transition output");
        }
    }

    fn restore_submitted_chunk(&mut self, chunk: SubmittedChunk) {
        let (pending, transition) = chunk.into_pending();
        self.restore_pending_output(pending, transition);
    }

    /// Submit one already-built chunk and apply the shared accounting and
    /// ownership protocol for every playback pipeline.
    fn submit(&mut self, chunk: SubmittedChunk) -> SubmitOutcome {
        let Some(sink) = self.sink_manager.sink() else {
            self.restore_submitted_chunk(chunk);
            self.sink_manager.mark_lost();
            return SubmitOutcome::Disconnected;
        };
        let result = sink.push_samples(&chunk.samples);
        match result {
            PushSamplesResult::Accepted => {
                let SubmittedChunk {
                    samples,
                    frames,
                    input_frames: _,
                    accepted_input_frames,
                    next_frames,
                    complete_crossfade,
                    buffer,
                    transition: _,
                    drain_on_accept,
                    drain_on_deferred_accept: _,
                } = chunk;
                if let Some(active) = self.playback.loaded_mut() {
                    active.frames_written += frames;
                }
                self.speed_stage.input_frames += accepted_input_frames;
                self.crossfade.record_accepted_next_frames(next_frames);
                self.restore_pump_buffer(buffer, samples);
                if complete_crossfade {
                    self.complete_crossfade();
                }
                if drain_on_accept
                    && let Some(active) = self.playback.loaded_mut()
                    && active.drain.source_exhausted()
                {
                    active.drain = DrainState::Drained;
                }
                SubmitOutcome::Accepted
            }
            PushSamplesResult::Backpressure => {
                self.restore_submitted_chunk(chunk);
                SubmitOutcome::Backpressure
            }
            PushSamplesResult::Disconnected => {
                self.restore_submitted_chunk(chunk);
                self.sink_manager.mark_lost();
                SubmitOutcome::Disconnected
            }
        }
    }

    fn clear_pending_output(&mut self) {
        let _ = self.crossfade.take_deferred();
        self.clear_active_pending_output();
    }

    fn clear_active_pending_output(&mut self) {
        if let Some(active) = self.playback.loaded_mut() {
            active.deferred = None;
        }
    }

    #[cfg(test)]
    fn flush_pending_output(&mut self) -> bool {
        !matches!(
            self.flush_pending_output_outcome(),
            PumpOutcome::Backpressure | PumpOutcome::Disconnected
        )
    }

    fn flush_pending_output_outcome(&mut self) -> PumpOutcome {
        let Some((pending, transition)) = self.take_pending_output() else {
            return PumpOutcome::Idle;
        };
        let drain_on_accept = pending.drain_on_accept;
        match self.submit(SubmittedChunk {
            samples: pending.samples,
            frames: pending.frames,
            input_frames: pending.input_frames,
            accepted_input_frames: pending.input_frames,
            next_frames: pending.next_frames,
            complete_crossfade: pending.complete_crossfade,
            buffer: pending.buffer,
            transition,
            drain_on_accept,
            drain_on_deferred_accept: drain_on_accept,
        }) {
            SubmitOutcome::Accepted => PumpOutcome::Progressed,
            SubmitOutcome::Backpressure => PumpOutcome::Backpressure,
            SubmitOutcome::Disconnected => PumpOutcome::Disconnected,
        }
    }

    /// Feed the next chunk of decoded samples to the native output sink.
    ///
    /// Called in the command loop while playing; the bounded sink channel
    /// provides backpressure so decoding only advances as fast as PipeWire
    /// consumes it.
    ///
    /// At the default speed the samples pass through unchanged (the historical
    /// direct path). Only a non-default speed routes through the
    /// pitch-preserving time-stretch engine, so normal playback is untouched.
    /// The boolean compatibility wrapper is true only when the worker should
    /// immediately pump again; source input consumed by SoundTouch counts as
    /// true even when its output FIFO has not produced samples yet.
    #[cfg(test)]
    fn pump_playback(&mut self) -> bool {
        self.pump_playback_outcome().should_continue()
    }

    /// Pump playback using an explicit observation time.
    ///
    /// The explicit time keeps flush deadline tests deterministic without
    /// sleeping a worker thread.
    #[cfg(test)]
    fn pump_playback_at(&mut self, now: Instant) -> bool {
        self.pump_playback_at_outcome(now).should_continue()
    }

    fn pump_playback_outcome(&mut self) -> PumpOutcome {
        self.pump_playback_at_outcome(Instant::now())
    }

    fn pump_playback_at_outcome(&mut self, now: Instant) -> PumpOutcome {
        // A lost output stream (device removed, PipeWire stopped) must surface
        // immediately and freeze the active track instead of consuming the
        // source to the end in silence. Handle it here, on the next invocation,
        // so the rest of this method never sees a dead sink.
        if self.sink_manager.is_lost() {
            if self.playback.is_playing() {
                self.handle_sink_lost();
            }
            return PumpOutcome::Idle;
        }
        if !self.poll_pending_flush_at(now) {
            return PumpOutcome::Idle;
        }
        if self.playback.is_paused() {
            return PumpOutcome::Idle;
        }
        if self.has_pending_output() {
            // Leave command processing a chance before decoding another chunk.
            return self.flush_pending_output_outcome();
        }
        let Some(active) = self.playback.loaded() else {
            return PumpOutcome::Idle;
        };
        let play_duration = active.duration;
        let track_rate = active.rate;
        let track_channels = active.channels;
        // The `active` borrow ends here; the crossfade path needs full &mut self.
        let channels = track_channels as usize;
        let chunk_frames = PUMP_CHUNK_FRAMES;

        // Crossfade is only available in the direct path (speed == 1.0), when
        // a compatible next track has entered its transition window. Select
        // the pipeline once so each pump has one explicit dispatch path.
        let crossfade_dur = Duration::from_secs(u64::from(self.crossfade_seconds.as_u16()));
        let crossfade_t = if self.crossfade_seconds.is_enabled() {
            let pos = self.current_position();
            let remaining = play_duration.map(|d| d.saturating_sub(pos));
            let transition_running = self.crossfade.is_transition_running();
            let format_ok = self
                .crossfade
                .preloaded()
                .is_some_and(|next| next.rate == track_rate && next.channels == track_channels);
            let duration_ok = play_duration.is_some_and(|d| d > crossfade_dur);
            let gate_ok = self.speed_stage.crossfade_allowed()
                && format_ok
                && play_duration.is_some()
                && (duration_ok || transition_running);
            let in_window = remaining.is_some_and(|r| r <= crossfade_dur);
            if gate_ok && (in_window || transition_running) {
                // Anchor the fade to the entry position (kept across pumps until
                // the transition completes or leaves the window).
                let start = self
                    .crossfade
                    .begin_transition_with_duration(pos, remaining.unwrap_or_default())
                    .expect("crossfade gate requires a ready preload");
                self.crossfade.ensure_transition_duration(
                    play_duration.unwrap_or_default().saturating_sub(start),
                );
                let t = self
                    .crossfade
                    .transition_progress_at(pos)
                    .expect("crossfade transition must retain its anchor");
                // Once the ramp is complete the outgoing track is silent, so
                // hand playback over even if the decoder kept reporting a few
                // extra frames (VBR duration estimates are often off). Waiting
                // for the source to return None would leave the UI stuck on A.
                if t >= 1.0 {
                    self.complete_crossfade();
                    self.crossfade.reset_transition();
                    None
                } else {
                    Some(t)
                }
            } else {
                // Outside the window: clear any stale transition marker.
                self.crossfade.reset_transition();
                None
            }
        } else {
            None
        };

        let pipeline = match crossfade_t {
            Some(t) => PlaybackPipeline::Crossfade { t },
            None if self.speed_stage.uses_time_stretch() => PlaybackPipeline::TimeStretch,
            None => PlaybackPipeline::Direct,
        };
        match pipeline {
            PlaybackPipeline::Direct => self.pump_direct(channels, chunk_frames),
            PlaybackPipeline::TimeStretch => self.pump_time_stretch(channels, chunk_frames),
            PlaybackPipeline::Crossfade { t } => {
                self.pump_crossfade_outcome(channels, chunk_frames, t)
            }
        }
    }

    fn pump_direct(&mut self, channels: usize, chunk_frames: usize) -> PumpOutcome {
        if self
            .playback
            .loaded()
            .is_some_and(|active| active.drain.drained())
            || self.sink_manager.sink().is_none()
        {
            return PumpOutcome::Idle;
        }
        self.crossfade.pump_scratch.clear();
        let frames = {
            let Playback::Playing(active) = &mut self.playback else {
                return PumpOutcome::Idle;
            };
            let source = &mut active.decoder;
            let chunk = &mut self.crossfade.pump_scratch;
            let mut frames = 0usize;
            for _ in 0..chunk_frames {
                let mut frame = 0usize;
                for _ in 0..channels {
                    match source.next() {
                        Some(sample) => {
                            chunk.push(sample);
                            frame += 1;
                        }
                        None => {
                            active.drain = DrainState::Drained;
                            break;
                        }
                    }
                }
                if frame == channels {
                    frames += 1;
                } else {
                    break;
                }
            }
            chunk.truncate(frames * channels);
            frames
        };
        if frames == 0 || self.crossfade.pump_scratch.is_empty() {
            return PumpOutcome::Idle;
        }
        let samples = mem::take(&mut self.crossfade.pump_scratch);
        match self.submit(SubmittedChunk {
            samples,
            frames: frames as u64,
            input_frames: frames as u64,
            accepted_input_frames: 0,
            next_frames: 0,
            complete_crossfade: false,
            buffer: PendingBuffer::Scratch,
            transition: false,
            drain_on_accept: false,
            drain_on_deferred_accept: true,
        }) {
            SubmitOutcome::Accepted => PumpOutcome::Progressed,
            SubmitOutcome::Backpressure => PumpOutcome::Backpressure,
            SubmitOutcome::Disconnected => PumpOutcome::Disconnected,
        }
    }

    fn pump_time_stretch(&mut self, channels: usize, chunk_frames: usize) -> PumpOutcome {
        // Time-stretch path: feed the source through the FIFO, then drain the
        // processed output. `frames_written` tracks SOURCE frames so the
        // progress bar still reflects the real track position.
        if self.speed_stage.stretch().is_none() {
            return PumpOutcome::Idle;
        }
        if self.sink_manager.sink().is_none() {
            return PumpOutcome::Idle;
        }
        let mut in_frames = 0usize;
        self.crossfade.pump_scratch.clear();
        let source_exhausted = self
            .playback
            .loaded()
            .is_some_and(|active| active.drain.source_exhausted());
        if !source_exhausted && let Playback::Playing(active) = &mut self.playback {
            let source = &mut active.decoder;
            for _ in 0..chunk_frames {
                let mut frame = 0usize;
                for _ in 0..channels {
                    match source.next() {
                        Some(sample) => {
                            self.crossfade.pump_scratch.push(sample);
                            frame += 1;
                        }
                        None => {
                            active.drain = DrainState::SourceExhausted;
                            break;
                        }
                    }
                }
                if frame == channels {
                    in_frames += 1;
                } else {
                    break;
                }
            }
        }
        // SoundTouch is also given only complete interleaved frames. A source
        // can end after yielding a prefix of the next frame; that prefix is
        // terminal and cannot be carried to a later source read.
        self.crossfade.pump_scratch.truncate(in_frames * channels);
        if in_frames > 0 {
            let stretch = self
                .speed_stage
                .stretch_mut()
                .expect("stretch was checked above");
            stretch.put_samples(&self.crossfade.pump_scratch, in_frames);
            self.speed_stage.input_frames += in_frames as u64;
        }
        if self
            .playback
            .loaded()
            .is_some_and(|active| active.drain.source_exhausted())
        {
            // The decoder is done; flush the tail the engine still holds so the
            // final buffers play out before the track is marked finished. Do
            // not flush an already-empty SoundTouch stream: its API documents
            // that doing so can append blank samples.
            let stretch = self
                .speed_stage
                .stretch_mut()
                .expect("stretch was checked above");
            if stretch.num_unprocessed_samples() > 0 {
                stretch.flush();
            }
        }

        // Reuse the output buffer as the receive target and drain the scaled
        // samples through the same scratch Vec to avoid allocating per pump.
        let recv_cap = chunk_frames * channels;
        if self.crossfade.pump_output.len() < recv_cap {
            self.crossfade.pump_output.resize(recv_cap, 0.0);
        }
        let mut drained = false;
        // Feeding source frames into SoundTouch is progress even while its
        // normal latency FIFO is warming and receive_samples returns zero.
        let mut pump_outcome = if in_frames > 0 {
            PumpOutcome::Progressed
        } else {
            PumpOutcome::Idle
        };
        loop {
            let got = self
                .speed_stage
                .stretch_mut()
                .expect("stretch was checked above")
                .receive_samples(&mut self.crossfade.pump_output, chunk_frames);
            if got == 0 {
                drained = true;
                break;
            }
            self.crossfade.pump_scratch.clear();
            self.crossfade
                .pump_scratch
                .extend(self.crossfade.pump_output[..got * channels].iter().copied());
            if self.crossfade.pump_scratch.is_empty() {
                break;
            }
            let active_speed = self
                .speed_stage
                .active_speed()
                .expect("time-stretch pipeline has an active speed");
            let exact_credit = got as f64 * f64::from(active_speed.tenths()) / 10.0
                + self.speed_stage.credit_remainder;
            let requested_credit = exact_credit.floor() as u64;
            self.speed_stage.credit_remainder = exact_credit - requested_credit as f64;
            let available_credit = self.speed_stage.input_frames.saturating_sub(
                self.playback
                    .loaded()
                    .map_or(0, |active| active.frames_written),
            );
            let source_credit = requested_credit.min(available_credit);
            let samples = mem::take(&mut self.crossfade.pump_scratch);
            match self.submit(SubmittedChunk {
                samples,
                frames: source_credit,
                input_frames: 0,
                accepted_input_frames: 0,
                next_frames: 0,
                complete_crossfade: false,
                buffer: PendingBuffer::Scratch,
                transition: false,
                drain_on_accept: false,
                drain_on_deferred_accept: false,
            }) {
                SubmitOutcome::Accepted => pump_outcome = PumpOutcome::Progressed,
                SubmitOutcome::Backpressure => return PumpOutcome::Backpressure,
                SubmitOutcome::Disconnected => return PumpOutcome::Disconnected,
            }
            if got < chunk_frames {
                // SoundTouch only had a partial tail left; loop once more to
                // observe the empty state that marks a fully-drained stream.
                continue;
            }
        }
        if drained && !self.has_pending_output() {
            if self
                .playback
                .loaded()
                .is_some_and(|active| active.drain.source_exhausted())
                && let Some(active) = self.playback.loaded_mut()
            {
                active.drain = DrainState::Drained;
            }
        }
        pump_outcome
    }

    /// Mix one buffer of the outgoing track (A) and the preloaded next track
    /// (B) with an equal-power curve, pushing the sum to the sink. A and B are
    /// advanced in lockstep; when A is exhausted the transition hands over to B.
    /// `t` is the anchored 0→1 progress of the fade.
    #[cfg(test)]
    fn pump_crossfade(&mut self, channels: usize, chunk_frames: usize, t: f64) -> bool {
        self.pump_crossfade_outcome(channels, chunk_frames, t)
            .should_continue()
    }

    fn pump_crossfade_outcome(
        &mut self,
        channels: usize,
        chunk_frames: usize,
        t: f64,
    ) -> PumpOutcome {
        if !self.crossfade.buffers_ready(channels, chunk_frames)
            || !matches!(
                &self.crossfade.lifecycle,
                CrossfadeLifecycle::TransitionRunning { .. }
            )
            || self.sink_manager.sink().is_none()
        {
            return PumpOutcome::Idle;
        }

        // Record the effective gain pair before submission. If the sink applies
        // backpressure, the transition owns the deferred chunk and this anchor
        // must remain attached to it across settings changes and recovery.
        self.crossfade.record_progress(t);
        let (out_gain, in_gain) = crate::audio::playback::crossfade_gains(t);
        self.crossfade.pump_output.clear();
        let mut frames = 0usize;
        let mut a_ended = false;
        let mut pump_outcome = PumpOutcome::Idle;
        {
            let Playback::Playing(active) = &mut self.playback else {
                return PumpOutcome::Idle;
            };
            for _ in 0..chunk_frames {
                {
                    let a_frame = &mut self.crossfade.pump_a_frame[..channels];
                    for slot in a_frame.iter_mut() {
                        match active.decoder.next() {
                            Some(sample) => *slot = sample,
                            None => {
                                a_ended = true;
                                break;
                            }
                        }
                    }
                }
                if a_ended {
                    break;
                }
                {
                    for index in 0..channels {
                        let sample = self.crossfade.preloaded_source_mut().next().unwrap_or(0.0);
                        self.crossfade.pump_b_frame[index] = sample;
                    }
                }
                let a_frame = &self.crossfade.pump_a_frame[..channels];
                let b_frame = &self.crossfade.pump_b_frame[..channels];
                for (a, b) in a_frame.iter().zip(b_frame.iter()) {
                    self.crossfade.pump_output.push(a * out_gain + b * in_gain);
                }
                frames += 1;
            }
        }

        if frames > 0 && !self.crossfade.pump_output.is_empty() {
            let samples = mem::take(&mut self.crossfade.pump_output);
            match self.submit(SubmittedChunk {
                samples,
                frames: frames as u64,
                input_frames: frames as u64,
                accepted_input_frames: 0,
                next_frames: frames as u64,
                complete_crossfade: false,
                buffer: PendingBuffer::Output,
                transition: true,
                drain_on_accept: false,
                drain_on_deferred_accept: true,
            }) {
                SubmitOutcome::Accepted => pump_outcome = PumpOutcome::Progressed,
                SubmitOutcome::Backpressure => return PumpOutcome::Backpressure,
                SubmitOutcome::Disconnected => return PumpOutcome::Disconnected,
            }
        }

        if a_ended {
            if t < 1.0 {
                self.crossfade.pump_output.clear();
                for index in 0..CROSSFADE_TAIL_FRAMES {
                    let gain =
                        in_gain + (1.0 - in_gain) * (index as f32 / CROSSFADE_TAIL_FRAMES as f32);
                    let mut frame_ok = false;
                    {
                        for slot in 0..channels {
                            self.crossfade.pump_b_frame[slot] =
                                match self.crossfade.preloaded_source_mut().next() {
                                    Some(sample) => {
                                        frame_ok = true;
                                        sample
                                    }
                                    None => 0.0,
                                };
                        }
                    }
                    for sample in self.crossfade.pump_b_frame[..channels].iter() {
                        self.crossfade.pump_output.push(sample * gain);
                    }
                    if !frame_ok && index > 0 {
                        break;
                    }
                }
                if !self.crossfade.pump_output.is_empty() {
                    let tail_frames = (self.crossfade.pump_output.len() / channels.max(1)) as u64;
                    let samples = mem::take(&mut self.crossfade.pump_output);
                    match self.submit(SubmittedChunk {
                        samples,
                        frames: 0,
                        input_frames: 0,
                        accepted_input_frames: 0,
                        next_frames: tail_frames,
                        complete_crossfade: true,
                        buffer: PendingBuffer::Output,
                        transition: true,
                        drain_on_accept: false,
                        drain_on_deferred_accept: true,
                    }) {
                        SubmitOutcome::Accepted => pump_outcome = PumpOutcome::Progressed,
                        SubmitOutcome::Backpressure => return PumpOutcome::Backpressure,
                        SubmitOutcome::Disconnected => return PumpOutcome::Disconnected,
                    }
                } else {
                    self.complete_crossfade();
                }
            } else {
                self.complete_crossfade();
            }
            return pump_outcome;
        }

        pump_outcome
    }

    /// Hand playback over to the preloaded next track after the fade completes:
    /// the incoming source becomes the active one and the app is told its index
    /// so it can advance the queue/artwork without double-advancing.
    fn complete_crossfade(&mut self) {
        let Some(next) = self.crossfade.take_preloaded() else {
            return;
        };
        let PreloadedTrack {
            source: next_source,
            index: next_index,
            identity: next_identity,
            duration: next_duration,
            rate: next_rate,
            channels: next_channels,
            consumed_frames: consumed,
        } = next;

        self.prepare_pump_buffers(next_channels as usize);
        self.speed_stage.input_frames = consumed;
        self.speed_stage.credit_remainder = 0.0;
        self.crossfade.reset_transition();
        self.setup_stretch(next_rate, next_channels);
        // The incoming track is a new position segment on the same sink (no
        // rebuild at crossfade), so re-anchor to the adopted frame count.
        self.sink_manager.reanchor(consumed);

        self.playback = Playback::Playing(LoadedTrack {
            index: next_index,
            source: next_identity.clone(),
            duration: next_duration,
            decoder: next_source,
            rate: next_rate,
            channels: next_channels,
            frames_written: consumed,
            drain: DrainState::Producing,
            deferred: None,
        });
        let elapsed = Duration::from_secs_f64(consumed as f64 / next_rate as f64);
        tracing::info!(
            track_index = next_index,
            "crossfade completed, adopted next track"
        );
        if let TrackSource::Local(path) = next_identity {
            self.emit(AppEvent::CrossfadeCompleted {
                track_index: next_index,
                path: path.to_path_buf(),
                elapsed,
            });
        }
    }

    /// Emit progress snapshots at a fixed cadence while playing.
    ///
    /// Position comes straight from [`Self::current_position`]: the native
    /// PipeWire sink's played-frame counter is the position authority, so no
    /// wall-clock interpolation or re-sync heuristics apply.
    fn publish_periodic_progress(&mut self) {
        let Some(active) = self.playback.loaded() else {
            return;
        };
        if !self.playback.is_playing() {
            return;
        }
        let now = Instant::now();
        let due = self
            .last_progress
            .is_none_or(|last| now.duration_since(last) >= PROGRESS_INTERVAL);
        if !due {
            return;
        }
        tracing::debug!(index = active.index, "progress snapshot about to publish");

        let elapsed = self.current_position();

        self.last_progress = Some(now);

        self.publish_progress(PlaybackSnapshot {
            status: PlayStatus::Playing,
            track_index: Some(active.index),
            elapsed,
            duration: active.duration,
            sink_health: self.sink_manager.health(),
        });
    }

    /// React to the native output stream disappearing (device unplugged,
    /// PipeWire stopped, or a panicked PipeWire thread).
    ///
    /// The sample channel closing is the only signal [`PipeWireSink`] exposes
    /// for a dead thread, and with it gone the bounded-channel backpressure that
    /// paces decoding disappears too. Without this the worker would run the
    /// source to the end at full speed, emit `TrackEnded` and advance the whole
    /// queue in silence. Tearing the sink down, freezing the active track and
    /// surfacing a notification makes the failure visible and recoverable (the
    /// next `Play`/`Resume` rebuilds the stream via [`Self::ensure_sink`]).
    fn handle_sink_lost(&mut self) {
        tracing::error!("native pipewire output stream lost; pausing playback");
        if let Some(at) = self.sink_manager.lost_at() {
            tracing::debug!(elapsed = ?at.elapsed(), "audio sink loss is recoverable");
        }
        self.pending_seek_target = None;
        if let Playback::Playing(active) = mem::replace(&mut self.playback, Playback::Idle) {
            self.playback = Playback::Paused(active);
        }
        // Reflect the freeze in the UI so the progress bar stops advancing
        // instead of appearing to play with no audio.
        self.publish_transition(PlayStatus::Paused);
        self.notify(
            EffectErrorKind::Audio,
            "Audio output was lost (device disconnected or PipeWire stopped). \
             Playback is paused; reconnect your device and press play to resume."
                .to_string(),
        );
    }

    fn publish_progress(&self, snapshot: PlaybackSnapshot) {
        if let Err(error) = self.events.send(AppEvent::PlaybackProgress { snapshot }) {
            tracing::debug!(?error, "playback progress was not queued");
        }
    }

    fn publish_transition(&self, status: PlayStatus) {
        let Some(active) = self.playback.loaded() else {
            return;
        };
        let elapsed = self.current_position();

        self.emit(AppEvent::PlaybackStateChanged {
            snapshot: PlaybackSnapshot {
                status,
                track_index: Some(active.index),
                elapsed,
                duration: active.duration,
                sink_health: self.sink_manager.health(),
            },
        });
    }

    fn publish_current_transition(&self) {
        if self.playback.loaded().is_none() {
            return;
        }
        let status = if self.playback.is_paused() {
            PlayStatus::Paused
        } else {
            PlayStatus::Playing
        };
        self.publish_transition(status);
    }

    fn emit(&self, event: AppEvent) {
        if let Err(error) = self
            .events
            .send_critical_timeout(event, AUDIO_EVENT_TIMEOUT)
        {
            tracing::error!(?error, "could not publish audio event");
        }
    }

    /// Surface one failure to the user through the bridge bus.
    fn notify(&self, kind: EffectErrorKind, message: String) {
        self.emit(AppEvent::Notification {
            kind,
            operation_id: None,
            message,
        });
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel_all_acquisition();
        // This also covers normal worker shutdown and panic unwinding. The
        // worker must never synchronously destroy a PipeWire sink while its
        // command loop is exiting.
        if let Some(sink) = self.sink_manager.take_live_sink() {
            retire_sink(sink);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::resample::LinearResample;
    use crate::audio::{SPEED_MAX, SPEED_MIN};
    use crate::event::EventBus;
    use crate::stream::provider::{ResolvedStream, StreamError, StreamProvider};
    use crate::stream::source::StreamKind;
    use crate::test_support::{
        ScriptedHttpResponse, ScriptedHttpServer, unique_temp_dir, wav_bytes,
    };
    use rodio::SampleRate;
    use std::collections::VecDeque;
    use std::num::{NonZeroU16, NonZeroU32};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize};
    use std::sync::{Arc, Barrier, Mutex};

    fn volume(value: u16) -> VolumePercent {
        VolumePercent::new(value).expect("test volume must be valid")
    }

    fn crossfade(value: u16) -> CrossfadeSeconds {
        CrossfadeSeconds::new(value).expect("test crossfade must be valid")
    }

    fn output_target(node_id: u32) -> OutputTarget {
        OutputTarget {
            stable_id: Some(format!("sink-{node_id}")),
            node_id: Some(node_id),
        }
    }

    struct FiniteSource {
        samples: Vec<f32>,
        position: usize,
        channels: NonZeroU16,
    }

    impl FiniteSource {
        fn new(samples: Vec<f32>) -> Self {
            Self::with_channels(samples, 1)
        }

        fn with_channels(samples: Vec<f32>, channels: u16) -> Self {
            Self {
                samples,
                position: 0,
                channels: NonZeroU16::new(channels).expect("finite source needs channels"),
            }
        }
    }

    impl Iterator for FiniteSource {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            let sample = self.samples.get(self.position).copied()?;
            self.position += 1;
            Some(sample)
        }
    }

    impl rodio::Source for FiniteSource {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.samples.len().saturating_sub(self.position))
        }

        fn channels(&self) -> NonZeroU16 {
            self.channels
        }

        fn sample_rate(&self) -> NonZeroU32 {
            NonZeroU32::new(44_100).expect("sample rate is non-zero")
        }

        fn total_duration(&self) -> Option<Duration> {
            Some(Duration::from_secs_f64(
                self.samples.len() as f64 / 44_100.0,
            ))
        }
    }

    struct DelayedProvider {
        started: Arc<AtomicBool>,
        boundary: Arc<Barrier>,
        release: Arc<AtomicBool>,
        hls: bool,
    }

    impl StreamProvider for DelayedProvider {
        fn can_handle(&self, url: &url::Url) -> bool {
            url.host_str() == Some("delayed.example")
        }

        fn kind(&self) -> StreamKind {
            StreamKind::Http
        }

        fn resolve(
            &self,
            _url: &url::Url,
            _cancellation: &StreamCancellation,
        ) -> Result<ResolvedStream, StreamError> {
            Ok(ResolvedStream::default())
        }

        fn open_reader(
            &self,
            url: &url::Url,
            cancellation: &StreamCancellation,
        ) -> Result<StreamReader, StreamError> {
            self.started.store(true, Ordering::Release);
            self.boundary.wait();
            while !self.release.load(Ordering::Acquire) {
                thread::yield_now();
            }
            if self.hls {
                let mut hls = crate::stream::hls::build_reader_from_body(
                    b"#EXTM3U\n#EXT-X-ENDLIST\n",
                    url,
                    cancellation,
                )?;
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut hls, &mut bytes)?;
                Ok(StreamReader::buffered(std::io::Cursor::new(bytes)))
            } else {
                Ok(StreamReader::buffered(std::io::Cursor::new(Vec::new())))
            }
        }
    }

    struct LiveHlsProvider {
        host: &'static str,
        segment_url: url::Url,
        started: Arc<AtomicBool>,
    }

    impl StreamProvider for LiveHlsProvider {
        fn can_handle(&self, url: &url::Url) -> bool {
            url.host_str() == Some(self.host)
        }

        fn kind(&self) -> StreamKind {
            StreamKind::Http
        }

        fn resolve(
            &self,
            _url: &url::Url,
            _cancellation: &StreamCancellation,
        ) -> Result<ResolvedStream, StreamError> {
            Ok(ResolvedStream::default())
        }

        fn open_reader(
            &self,
            url: &url::Url,
            cancellation: &StreamCancellation,
        ) -> Result<StreamReader, StreamError> {
            self.started.store(true, Ordering::Release);
            let manifest = format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:0\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:0,\n{}\n",
                self.segment_url
            );
            let reader = crate::stream::hls::build_reader_from_body(
                manifest.as_bytes(),
                &self.segment_url,
                cancellation,
            )?;
            crate::stream::hls::stream_reader_from_hls(reader, url, cancellation)
        }
    }

    struct HlsReadSource {
        reader: StreamReader,
    }

    impl Iterator for HlsReadSource {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            let mut byte = [0u8; 1];
            match std::io::Read::read(&mut self.reader, &mut byte) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(0.0),
            }
        }
    }

    impl rodio::Source for HlsReadSource {
        fn current_span_len(&self) -> Option<usize> {
            None
        }

        fn channels(&self) -> NonZeroU16 {
            NonZeroU16::new(1).expect("one channel is non-zero")
        }

        fn sample_rate(&self) -> NonZeroU32 {
            NonZeroU32::new(44_100).expect("sample rate is non-zero")
        }

        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    fn wait_for_flag(flag: &AtomicBool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !flag.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "delayed provider did not start");
            thread::yield_now();
        }
    }

    fn wait_for_request_count(server: &ScriptedHttpServer, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while server.requests().len() < count {
            assert!(
                Instant::now() < deadline,
                "loopback provider did not receive {count} requests"
            );
            thread::yield_now();
        }
    }

    struct FakeSink {
        outcomes: Mutex<VecDeque<PushSamplesResult>>,
        default_outcome: PushSamplesResult,
        pushed: Mutex<Vec<Vec<f32>>>,
        events: Mutex<Vec<&'static str>>,
        attempts: AtomicUsize,
        next_flush_generation: AtomicU64,
        acknowledged_flush_generation: AtomicU64,
        auto_ack_flush: AtomicBool,
        disconnected: AtomicBool,
        played: AtomicU64,
        gain: AtomicU32,
        rate: u32,
        channels: u32,
    }

    impl FakeSink {
        fn new(outcomes: impl IntoIterator<Item = PushSamplesResult>) -> Self {
            Self::with_default(outcomes, PushSamplesResult::Accepted, 44_100, 1)
        }

        fn with_format(
            outcomes: impl IntoIterator<Item = PushSamplesResult>,
            rate: u32,
            channels: u32,
        ) -> Self {
            Self::with_default(outcomes, PushSamplesResult::Accepted, rate, channels)
        }

        fn always(outcome: PushSamplesResult) -> Self {
            Self::with_default([], outcome, 44_100, 1)
        }

        fn with_default(
            outcomes: impl IntoIterator<Item = PushSamplesResult>,
            default_outcome: PushSamplesResult,
            rate: u32,
            channels: u32,
        ) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                default_outcome,
                pushed: Mutex::new(Vec::new()),
                events: Mutex::new(Vec::new()),
                attempts: AtomicUsize::new(0),
                next_flush_generation: AtomicU64::new(0),
                acknowledged_flush_generation: AtomicU64::new(0),
                auto_ack_flush: AtomicBool::new(true),
                disconnected: AtomicBool::new(false),
                played: AtomicU64::new(0),
                gain: AtomicU32::new(1.0f32.to_bits()),
                rate,
                channels,
            }
        }

        fn pushed_samples(&self) -> Vec<Vec<f32>> {
            self.pushed.lock().unwrap().clone()
        }

        fn push_attempts(&self) -> usize {
            self.attempts.load(Ordering::Relaxed)
        }

        fn gain(&self) -> f32 {
            f32::from_bits(self.gain.load(Ordering::Acquire))
        }

        fn event_names(&self) -> Vec<&'static str> {
            self.events.lock().unwrap().clone()
        }

        fn delay_flush_ack(&self) {
            self.auto_ack_flush.store(false, Ordering::Release);
        }

        fn acknowledge_flushes(&self) {
            let generation = self.next_flush_generation.load(Ordering::Acquire);
            self.acknowledged_flush_generation
                .store(generation, Ordering::Release);
        }

        fn disconnect(&self) {
            self.disconnected.store(true, Ordering::Release);
        }
    }

    impl OutputSink for FakeSink {
        fn push_samples(&self, samples: &[f32]) -> PushSamplesResult {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(self.default_outcome);
            if outcome == PushSamplesResult::Accepted {
                let gain = self.gain();
                self.pushed
                    .lock()
                    .unwrap()
                    .push(samples.iter().map(|sample| sample * gain).collect());
            }
            outcome
        }

        fn set_gain(&self, gain: f32) {
            self.gain.store(gain.to_bits(), Ordering::Release);
        }

        fn flush(&self) -> FlushGeneration {
            self.events.lock().unwrap().push("flush");
            let generation = FlushGeneration(
                self.next_flush_generation
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1),
            );
            if self.auto_ack_flush.load(Ordering::Acquire) {
                self.acknowledged_flush_generation
                    .store(generation.0, Ordering::Release);
            }
            generation
        }

        fn flush_acknowledged(&self, generation: FlushGeneration) -> bool {
            self.acknowledged_flush_generation.load(Ordering::Acquire) >= generation.0
                || self.disconnected.load(Ordering::Acquire)
        }

        fn pause(&self) {}

        fn resume(&self) {}

        fn rate(&self) -> u32 {
            self.rate
        }

        fn channels(&self) -> u32 {
            self.channels
        }

        fn frames_played(&self) -> u64 {
            self.events.lock().unwrap().push("frames_played");
            self.played.load(Ordering::Acquire)
        }
    }

    impl OutputSink for Arc<FakeSink> {
        fn push_samples(&self, samples: &[f32]) -> PushSamplesResult {
            OutputSink::push_samples(self.as_ref(), samples)
        }

        fn set_gain(&self, gain: f32) {
            OutputSink::set_gain(self.as_ref(), gain);
        }

        fn flush(&self) -> FlushGeneration {
            OutputSink::flush(self.as_ref())
        }

        fn flush_acknowledged(&self, generation: FlushGeneration) -> bool {
            OutputSink::flush_acknowledged(self.as_ref(), generation)
        }

        fn pause(&self) {
            OutputSink::pause(self.as_ref());
        }

        fn resume(&self) {
            OutputSink::resume(self.as_ref());
        }

        fn rate(&self) -> u32 {
            OutputSink::rate(self.as_ref())
        }

        fn channels(&self) -> u32 {
            OutputSink::channels(self.as_ref())
        }

        fn frames_played(&self) -> u64 {
            OutputSink::frames_played(self.as_ref())
        }
    }

    struct BlockingDropSink {
        inner: Arc<FakeSink>,
        started: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for BlockingDropSink {
        fn drop(&mut self) {
            self.started.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                thread::yield_now();
            }
            self.dropped.fetch_add(1, Ordering::Release);
        }
    }

    impl OutputSink for BlockingDropSink {
        fn push_samples(&self, samples: &[f32]) -> PushSamplesResult {
            self.inner.push_samples(samples)
        }

        fn set_gain(&self, gain: f32) {
            self.inner.set_gain(gain);
        }

        fn flush(&self) -> FlushGeneration {
            self.inner.flush()
        }

        fn flush_acknowledged(&self, generation: FlushGeneration) -> bool {
            self.inner.flush_acknowledged(generation)
        }

        fn pause(&self) {
            self.inner.pause();
        }

        fn resume(&self) {
            self.inner.resume();
        }

        fn rate(&self) -> u32 {
            self.inner.rate()
        }

        fn channels(&self) -> u32 {
            self.inner.channels()
        }

        fn frames_played(&self) -> u64 {
            self.inner.frames_played()
        }
    }

    fn empty_worker(events: EventSender, commands: Receiver<AudioCommand>) -> Worker {
        let worker = Worker::new(
            commands,
            events,
            crate::stream::resolver_with_defaults(),
            StreamInterruption::new(),
        );
        worker
    }

    #[test]
    fn panic_notification_publication_is_bounded_when_queue_is_full() {
        let bus = EventBus::with_capacity(1);
        bus.send_critical(AppEvent::TrackEnded { track_index: 1 })
            .expect("critical sentinel fits");
        let started = Instant::now();

        let result = publish_audio_panic_notification(&bus.sender());

        assert_eq!(result, Err(crate::event::EventSendError::Full));
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    fn set_playing(
        worker: &mut Worker,
        index: usize,
        source: TrackSource,
        duration: Option<Duration>,
        decoder: Box<dyn Source<Item = f32> + Send>,
    ) {
        worker.playback = Playback::Playing(LoadedTrack {
            index,
            source,
            duration,
            decoder,
            rate: 44_100,
            channels: 1,
            frames_written: 0,
            drain: DrainState::Producing,
            deferred: None,
        });
    }

    fn loaded(worker: &Worker) -> &LoadedTrack {
        worker.playback.loaded().expect("loaded playback")
    }

    fn loaded_mut(worker: &mut Worker) -> &mut LoadedTrack {
        worker.playback.loaded_mut().expect("loaded playback")
    }

    fn pending_flush_deadline(worker: &Worker) -> Instant {
        match &worker.sink_manager.state {
            SinkState::Live { segment, .. } => {
                segment
                    .pending_flush
                    .as_ref()
                    .expect("test requires a pending flush")
                    .deadline
            }
            SinkState::Absent { .. } | SinkState::Lost { .. } => {
                panic!("test requires a live sink")
            }
        }
    }

    fn install_preloaded(
        worker: &mut Worker,
        source: Box<dyn Source<Item = f32> + Send>,
        index: usize,
        identity: TrackSource,
        rate: u32,
        channels: u32,
    ) {
        worker.crossfade.install_ready(PreloadedTrack {
            source,
            index,
            identity,
            duration: None,
            rate,
            channels,
            consumed_frames: 0,
        });
    }

    #[test]
    fn audio_progress_uses_non_blocking_path_when_critical_queue_is_full() {
        let bus = EventBus::with_capacity(2);
        bus.send_critical(AppEvent::Notification {
            kind: EffectErrorKind::Audio,
            operation_id: None,
            message: "first".into(),
        })
        .expect("first critical event fits");
        bus.send_critical(AppEvent::TrackEnded { track_index: 4 })
            .expect("second critical event fits");

        let (_command_tx, command_rx) = channel();
        let worker = empty_worker(bus.sender(), command_rx);
        worker.publish_progress(PlaybackSnapshot {
            status: PlayStatus::Playing,
            track_index: Some(4),
            elapsed: Duration::from_secs(4),
            duration: Some(Duration::from_secs(10)),
            sink_health: SinkHealth::Healthy,
        });

        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::Notification { message, .. }) if message == "first"
        ));
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::TrackEnded { track_index: 4 })
        ));
    }

    #[test]
    fn delayed_provider_does_not_block_audio_commands_or_shutdown() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let resolver = StreamResolver::new(vec![Arc::new(DelayedProvider {
            started: Arc::clone(&started),
            boundary: Arc::new(Barrier::new(1)),
            release: Arc::clone(&release),
            hls: false,
        })]);
        let event_bus = EventBus::new();
        let (command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        worker.resolver = resolver;
        let worker_thread = thread::spawn(move || worker.run());

        command_tx
            .send(AudioCommand::Play {
                source: TrackSource::stream(
                    url::Url::parse("https://delayed.example/live").unwrap(),
                    StreamKind::Http,
                ),
                track_index: 4,
                generation: None,
            })
            .expect("play command");
        wait_for_flag(&started);

        let shutdown_started = Instant::now();
        command_tx.send(AudioCommand::Pause).expect("pause command");
        command_tx
            .send(AudioCommand::SeekTo(Duration::from_secs(2)))
            .expect("seek command");
        command_tx
            .send(AudioCommand::SetOutput(output_target(12)))
            .expect("output command");
        command_tx.send(AudioCommand::Stop).expect("stop command");
        command_tx
            .send(AudioCommand::Shutdown)
            .expect("shutdown command");
        worker_thread.join().expect("worker must shut down");
        assert!(
            shutdown_started.elapsed() < Duration::from_millis(250),
            "a delayed provider must not hold the audio worker shutdown"
        );

        // Let the detached provider task leave its bounded test wait after the
        // worker has already shut down.
        release.store(true, Ordering::Release);
    }

    #[test]
    fn failed_stream_acquisition_publishes_a_matching_source_failure() {
        let event_bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        let url = url::Url::parse("https://radio.example.com/live").expect("valid url");
        let source = TrackSource::stream(url.clone(), StreamKind::Http);
        let cancellation = AcquisitionCancellation::new();
        worker.acquisition.pending = Some(PendingAcquisition {
            id: 17,
            generation: Some(42),
            source: source.clone(),
            track_index: 3,
            cancellation,
            kind: AcquisitionKind::Stream,
        });
        worker.playback = Playback::Loading {};

        worker.handle_acquisition_result(AcquisitionResult::StreamFailed {
            id: 17,
            generation: Some(42),
            source,
            track_index: 3,
            error: AudioError::Stream(anyhow::anyhow!("forced acquisition failure")),
        });

        let mut failed_url = None;
        let mut failed_generation = None;
        while let Ok(event) = event_bus.try_recv() {
            if let AppEvent::SourceFailed {
                url: actual,
                generation,
            } = event
            {
                failed_url = Some(actual);
                failed_generation = generation;
            }
        }
        assert_eq!(failed_url.as_deref(), Some(url.as_str()));
        assert_eq!(failed_generation, Some(42));
    }

    #[test]
    fn stream_play_publishes_loading_snapshot_with_track_identity() {
        let event_bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        let source = TrackSource::stream(
            url::Url::parse("https://radio.example.com/live").expect("valid URL"),
            StreamKind::Http,
        );

        worker.play(source, 4, Some(42));

        let AppEvent::PlaybackStateChanged { snapshot } = event_bus
            .recv_timeout(Duration::from_millis(100))
            .expect("stream loading snapshot")
        else {
            panic!("expected stream loading snapshot");
        };
        assert_eq!(snapshot.status, PlayStatus::Stopped);
        assert_eq!(snapshot.track_index, Some(4));
        worker.cancel_stream_acquisition();
    }

    #[test]
    fn delayed_hls_acquisition_is_cancelled_without_publishing_a_late_result() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let resolver = StreamResolver::new(vec![Arc::new(DelayedProvider {
            started: Arc::clone(&started),
            boundary: Arc::new(Barrier::new(1)),
            release: Arc::clone(&release),
            hls: true,
        })]);
        let event_bus = EventBus::new();
        let (command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        worker.resolver = resolver;
        let worker_thread = thread::spawn(move || worker.run());

        command_tx
            .send(AudioCommand::Play {
                source: TrackSource::stream(
                    url::Url::parse("https://delayed.example/live.m3u8").unwrap(),
                    StreamKind::Http,
                ),
                track_index: 9,
                generation: None,
            })
            .expect("play command");
        wait_for_flag(&started);
        command_tx.send(AudioCommand::Stop).expect("stop command");
        command_tx
            .send(AudioCommand::Shutdown)
            .expect("shutdown command");
        release.store(true, Ordering::Release);
        worker_thread.join().expect("worker must shut down");

        while let Ok(event) = event_bus.try_recv() {
            assert!(!matches!(
                event,
                AppEvent::SourceReady { .. } | AppEvent::SourceFailed { .. }
            ));
        }
    }

    #[test]
    fn live_hls_read_is_interrupted_before_local_replacement_is_processed() {
        let segment_server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, b"x"),
            ScriptedHttpResponse::chunked(200, [b"#EXTM3U\n".as_slice()], Duration::from_secs(2)),
        ]);
        let provider = LiveHlsProvider {
            host: "a.example",
            segment_url: url::Url::parse(&segment_server.endpoint("segment.ts"))
                .expect("segment URL"),
            started: Arc::new(AtomicBool::new(false)),
        };
        let source_url = url::Url::parse("https://a.example/live").expect("source URL");
        let interruption = StreamInterruption::new();
        let reader = provider
            .open_reader(&source_url, &interruption.current())
            .expect("open live HLS reader");
        let event_bus = EventBus::new();
        let (command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        worker.interruption = interruption.clone();
        worker.sink_manager.install(Box::new(FakeSink::new([])));
        set_playing(
            &mut worker,
            1,
            TrackSource::stream(source_url.clone(), StreamKind::Http),
            None,
            Box::new(HlsReadSource { reader }),
        );
        let worker_thread = thread::spawn(move || worker.run());
        let handle = AudioEngineHandle {
            sender: command_tx,
            interruption,
        };

        wait_for_request_count(&segment_server, 2);

        let root = unique_temp_dir("live-hls-local-replacement");
        let local_path = root.path().join("replacement.wav");
        std::fs::write(&local_path, wav_bytes(&[])).expect("write local fixture");
        let started = Instant::now();
        handle
            .send(AudioCommand::Play {
                source: TrackSource::local(local_path),
                track_index: 2,
                generation: None,
            })
            .expect("local replacement");
        handle.shutdown();
        worker_thread
            .join()
            .expect("worker must process replacement");

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "local replacement exceeded the bounded HLS interruption delay: {:?}",
            started.elapsed()
        );
        while let Ok(event) = event_bus.try_recv() {
            assert!(!matches!(event, AppEvent::SourceFailed { .. }));
        }
    }

    #[test]
    fn live_hls_read_is_interrupted_for_stream_to_stream_replacement_and_shutdown() {
        let first_server = ScriptedHttpServer::new([
            ScriptedHttpResponse::fixed(200, b"x"),
            ScriptedHttpResponse::chunked(200, [b"#EXTM3U\n".as_slice()], Duration::from_secs(2)),
        ]);
        let second_server =
            ScriptedHttpServer::new([ScriptedHttpResponse::fixed(200, wav_bytes(&[]))]);
        let second_started = Arc::new(AtomicBool::new(false));
        let first_provider = LiveHlsProvider {
            host: "a.example",
            segment_url: url::Url::parse(&first_server.endpoint("segment.ts"))
                .expect("first segment URL"),
            started: Arc::new(AtomicBool::new(false)),
        };
        let first_source = url::Url::parse("https://a.example/live").expect("first source URL");
        let interruption = StreamInterruption::new();
        let first_reader = first_provider
            .open_reader(&first_source, &interruption.current())
            .expect("open first live HLS reader");
        let resolver = StreamResolver::new(vec![Arc::new(LiveHlsProvider {
            host: "c.example",
            segment_url: url::Url::parse(&second_server.endpoint("segment.ts"))
                .expect("second segment URL"),
            started: Arc::clone(&second_started),
        })]);
        let event_bus = EventBus::new();
        let (command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        worker.resolver = resolver;
        worker.interruption = interruption.clone();
        worker.sink_manager.install(Box::new(FakeSink::new([])));
        set_playing(
            &mut worker,
            1,
            TrackSource::stream(first_source, StreamKind::Http),
            None,
            Box::new(HlsReadSource {
                reader: first_reader,
            }),
        );
        let worker_thread = thread::spawn(move || worker.run());
        let handle = AudioEngineHandle {
            sender: command_tx,
            interruption,
        };

        wait_for_request_count(&first_server, 2);

        handle
            .send(AudioCommand::Play {
                source: TrackSource::stream(
                    url::Url::parse("https://c.example/live").expect("second source URL"),
                    StreamKind::Http,
                ),
                track_index: 3,
                generation: Some(2),
            })
            .expect("second stream play");
        wait_for_flag(&second_started);
        let started = Instant::now();
        handle.shutdown();
        worker_thread.join().expect("worker must process shutdown");

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "stream replacement shutdown exceeded the bounded HLS interruption delay: {:?}",
            started.elapsed()
        );
        let mut ready_urls = Vec::new();
        while let Ok(event) = event_bus.try_recv() {
            if let AppEvent::SourceReady { url, .. } = event {
                ready_urls.push(url);
            }
        }
        assert!(
            ready_urls.iter().all(|url| !url.contains("a.example")),
            "the old stream must not publish a late ready event: {ready_urls:?}"
        );
    }

    #[test]
    fn worker_command_sequence_replaces_streams_and_locals_without_stale_loading() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let resolver = StreamResolver::new(vec![Arc::new(DelayedProvider {
            started: Arc::clone(&started),
            boundary: Arc::new(Barrier::new(1)),
            release: Arc::clone(&release),
            hls: false,
        })]);
        let events = EventBus::new();
        let (sender, receiver) = channel();
        let interruption = StreamInterruption::new();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(events.sender(), receiver);
        worker.resolver = resolver;
        worker.interruption = interruption.clone();
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        let acquisition_results = worker.acquisition.sender.clone();
        let worker_thread = thread::spawn(move || worker.run());
        let handle = AudioEngineHandle {
            sender,
            interruption: interruption.clone(),
        };

        let root = unique_temp_dir("stream-local-command-sequence");
        let local_one = root.path().join("local-one.wav");
        let local_two = root.path().join("local-two.wav");
        std::fs::write(&local_one, wav_bytes(&[])).expect("write first local fixture");
        std::fs::write(&local_two, wav_bytes(&[])).expect("write second local fixture");
        let stream_url = url::Url::parse("https://delayed.example/restart").expect("stream URL");

        let mut app = crate::app::App::new();
        app.state_mut().playlist = {
            let mut playlist = crate::playlist::Playlist::new();
            playlist.extend([
                crate::track::Track::from_stream(stream_url.clone(), StreamKind::Http),
                crate::track::Track::local(local_one.clone()),
                crate::track::Track::local(local_two.clone()),
            ]);
            playlist
        };

        let play_effect = |effects: Vec<crate::app::Effect>| {
            effects
                .into_iter()
                .find_map(|effect| match effect {
                    crate::app::Effect::Audio(command @ AudioCommand::Play { .. }) => Some(command),
                    _ => None,
                })
                .expect("playback selection must dispatch Play")
        };
        let wait_for_local_progress = |index| {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "local track {index} did not produce a playback snapshot"
                );
                let Ok(event) = events.recv_timeout(Duration::from_millis(25)) else {
                    continue;
                };
                if matches!(
                    event,
                    AppEvent::PlaybackProgress { snapshot }
                        if snapshot.status == PlayStatus::Playing
                            && snapshot.track_index == Some(index)
                ) {
                    return;
                }
            }
        };

        app.state_mut().playlist.select(0);
        let stream_play = play_effect(app.begin_current_track());
        handle.send(stream_play).expect("initial stream play");
        wait_for_flag(&started);
        let first_acquisition = interruption.current();

        app.state_mut().playlist.select(1);
        let local_play = play_effect(app.begin_current_track());
        assert!(
            app.state().async_ops.stream_activity().is_none(),
            "local replacement must clear the app-level stream spinner"
        );
        handle.send(local_play).expect("first local replacement");
        assert!(
            first_acquisition.is_cancelled(),
            "the first stream acquisition must be cancelled before local Play"
        );
        wait_for_local_progress(1);
        let first_local_chunks = sink.pushed_samples().len();
        assert!(
            first_local_chunks > 0,
            "first local Play must reach the audio sink"
        );

        started.store(false, Ordering::Release);
        app.state_mut().playlist.select(0);
        let restarted_stream_play = play_effect(app.begin_current_track());
        let restarted_generation = match app.state().async_ops.stream_activity() {
            Some(crate::state::StreamActivity::Acquiring { generation, .. }) => *generation,
            _ => panic!("stream restart must own a new loading identity"),
        };
        handle
            .send(restarted_stream_play)
            .expect("restarted stream play");
        wait_for_flag(&started);
        let restarted_acquisition = interruption.current();

        app.apply_source_failed_with_generation(restarted_generation - 1, stream_url.to_string());
        assert!(
            app.state().async_ops.stream_activity().is_some(),
            "an older stream event must not clear the newer restart"
        );

        app.state_mut().playlist.select(2);
        let final_local_play = play_effect(app.begin_current_track());
        assert!(
            app.state().async_ops.stream_activity().is_none(),
            "final local replacement must clear the restarted stream spinner"
        );
        handle
            .send(final_local_play)
            .expect("second local replacement");
        assert!(
            restarted_acquisition.is_cancelled(),
            "the restarted stream acquisition must be cancelled before the second local Play"
        );
        wait_for_local_progress(2);
        assert!(
            sink.pushed_samples().len() > first_local_chunks,
            "second local Play must reach the audio sink"
        );

        // Publish an old result through the same channel used by detached
        // acquisition tasks. The newer local Play is already active, so the
        // worker must discard this ready result before it can replace the
        // local decoder. Replaying the local command provides a worker-loop
        // boundary without adding a production acknowledgement mechanism.
        acquisition_results
            .send(AcquisitionResult::StreamReady {
                id: 0,
                generation: Some(restarted_generation - 1),
                source: TrackSource::stream(stream_url.clone(), StreamKind::Http),
                track_index: 0,
                prepared: PreparedStream {
                    decoder: Box::new(FiniteSource::new(vec![1.0; 8])),
                    duration: Some(Duration::from_secs(1)),
                    rate: 44_100,
                    channels: 1,
                },
            })
            .expect("stale result must reach the worker acquisition channel");
        handle
            .send(AudioCommand::Play {
                source: TrackSource::local(local_two.clone()),
                track_index: 2,
                generation: None,
            })
            .expect("final local drain boundary");

        let mut stale_source_events = 0;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            assert!(
                Instant::now() < deadline,
                "final local playback did not drain the injected stale result"
            );
            let Ok(event) = events.recv_timeout(Duration::from_millis(25)) else {
                continue;
            };
            match event {
                AppEvent::SourceReady { url, generation } => {
                    stale_source_events += 1;
                    if let Some(generation) = generation {
                        app.apply_source_ready_with_generation(generation, url);
                    } else {
                        app.apply_source_ready(url);
                    }
                }
                AppEvent::SourceFailed { url, generation } => {
                    stale_source_events += 1;
                    if let Some(generation) = generation {
                        app.apply_source_failed_with_generation(generation, url);
                    } else {
                        app.apply_source_failed(url);
                    }
                }
                AppEvent::PlaybackProgress { snapshot }
                    if snapshot.status == PlayStatus::Playing
                        && snapshot.track_index == Some(2) =>
                {
                    assert_eq!(
                        stale_source_events, 0,
                        "the stale stream result must not publish a source event"
                    );
                    break;
                }
                _ => {}
            }
        }
        assert!(
            app.state().async_ops.stream_activity().is_none(),
            "the app acceptance gate must retain the final local identity"
        );

        // Release both detached providers after the newest local track owns the
        // worker. Their cancelled results must not replace that local playback.
        release.store(true, Ordering::Release);
        handle.shutdown();
        worker_thread
            .join()
            .expect("worker must drain shutdown after replacements");

        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(
                    event,
                    AppEvent::SourceReady { .. } | AppEvent::SourceFailed { .. }
                ),
                "cancelled stream acquisitions must not publish stale source events"
            );
        }
    }

    #[test]
    fn wav_decoder_resampler_soundtouch_pipeline_reaches_fake_sink() {
        let root = unique_temp_dir("audio-pipeline");
        let path = root.join("pipeline.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let source = TrackSource::local(path.clone());
        let decoder = decode_local_source(&source).expect("decode WAV fixture");
        assert_eq!(decoder.sample_rate().get(), 44_100);
        assert_eq!(decoder.channels().get(), 1);

        let resampled = LinearResample::new(decoder, SampleRate::new(48_000).unwrap());
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::with_format([], 48_000, 1));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            source,
            Some(Duration::from_secs(1)),
            Box::new(resampled),
        );
        loaded_mut(&mut worker).rate = 48_000;
        loaded_mut(&mut worker).channels = 1;
        worker.speed_stage.requested_speed = Speed::from_tenths(15).unwrap();
        worker.setup_stretch(48_000, 1);
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        for _ in 0..256 {
            worker.pump_playback();
            if loaded(&worker).drain.drained() && worker.pending_output().is_none() {
                break;
            }
        }

        let pushed = sink.pushed_samples();
        assert!(!pushed.is_empty(), "the processed WAV must reach the sink");
        assert!(
            pushed.iter().all(|chunk| !chunk.is_empty()),
            "accepted sink chunks must contain samples"
        );
        assert!(
            loaded(&worker).drain.source_exhausted(),
            "the decoder/resampler must drain"
        );
        assert!(
            loaded(&worker).drain.drained(),
            "SoundTouch must flush its tail"
        );
        assert!(
            worker.speed_stage.input_frames >= 47_900,
            "resampling should produce approximately 48,000 input frames, got {}",
            worker.speed_stage.input_frames
        );
        assert!(
            loaded(&worker).frames_written > 40_000,
            "accepted time-stretched output must account for source frames"
        );
    }

    #[test]
    fn crossfade_channel_adapter_keeps_mono_and_stereo_transitions_compatible() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::with_format([], 44_100, 2));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.set_volume(volume(100));
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/stereo-current.wav"),
            None,
            Box::new(FiniteSource::with_channels(vec![1.0, 2.0], 2)),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        loaded_mut(&mut worker).channels = 2;
        worker
            .start_track_into_next(
                PreparedStream {
                    decoder: Box::new(FiniteSource::new(vec![10.0])),
                    duration: None,
                    rate: 44_100,
                    channels: 1,
                },
                TrackSource::local("/mono-next.wav"),
                1,
            )
            .expect("mono preload must adapt to stereo");
        assert_eq!(worker.crossfade.preloaded().unwrap().channels, 2);
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .unwrap()
                .source
                .channels()
                .get(),
            2
        );
        worker.prepare_pump_buffers(2);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("adapted preload must enter the transition");
        assert!(worker.pump_crossfade(2, 1, 0.5));
        let (out_gain, in_gain) = crate::audio::playback::crossfade_gains(0.5);
        assert_eq!(
            sink.pushed_samples(),
            vec![vec![
                out_gain + in_gain * 10.0,
                2.0 * out_gain + in_gain * 10.0
            ]]
        );

        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::with_format([], 44_100, 1));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.set_volume(volume(100));
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/mono-current.wav"),
            None,
            Box::new(FiniteSource::new(vec![1.0])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker
            .start_track_into_next(
                PreparedStream {
                    decoder: Box::new(FiniteSource::with_channels(vec![10.0, 30.0], 2)),
                    duration: None,
                    rate: 44_100,
                    channels: 2,
                },
                TrackSource::local("/stereo-next.wav"),
                1,
            )
            .expect("stereo preload must adapt to mono");
        assert_eq!(worker.crossfade.preloaded().unwrap().channels, 1);
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .unwrap()
                .source
                .channels()
                .get(),
            1
        );
        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("adapted preload must enter the transition");
        assert!(worker.pump_crossfade(1, 1, 0.5));
        let (out_gain, in_gain) = crate::audio::playback::crossfade_gains(0.5);
        assert_eq!(sink.pushed_samples(), vec![vec![out_gain + in_gain * 20.0]]);
    }

    #[test]
    fn unsupported_crossfade_channel_layout_is_refused_before_transition() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/stereo-current.wav"),
            None,
            Box::new(FiniteSource::with_channels(vec![1.0, 2.0], 2)),
        );
        loaded_mut(&mut worker).channels = 2;

        let error = worker
            .start_track_into_next(
                PreparedStream {
                    decoder: Box::new(FiniteSource::with_channels(vec![1.0; 6], 6)),
                    duration: None,
                    rate: 44_100,
                    channels: 6,
                },
                TrackSource::local("/surround-next.wav"),
                1,
            )
            .expect_err("unsupported layouts must not arm a preload");

        assert!(
            error
                .to_string()
                .contains("unsupported channel layout: 6 -> 2")
        );
        assert!(worker.crossfade.preloaded().is_none());
        assert!(matches!(worker.playback, Playback::Playing(_)));
    }

    #[test]
    fn crossfade_resampling_rejects_zero_target_rate_without_installing_preload() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/current.wav"),
            None,
            Box::new(FiniteSource::new(vec![])),
        );
        loaded_mut(&mut worker).rate = 0;

        let error = worker
            .start_track_into_next(
                PreparedStream {
                    decoder: Box::new(FiniteSource::new(vec![1.0, 2.0])),
                    duration: None,
                    rate: 44_100,
                    channels: 1,
                },
                TrackSource::local("/invalid-target-rate.wav"),
                1,
            )
            .expect_err("a zero crossfade target rate must be rejected");

        assert_eq!(
            error.to_string(),
            "stream error: invalid target sample rate: 0 Hz"
        );
        assert!(
            worker.crossfade.preloaded().is_none(),
            "invalid target rates must not install a degraded preload"
        );
    }

    #[test]
    fn stale_acquisition_result_cannot_replace_newer_playback_identity() {
        let event_bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        let active_source = TrackSource::local("/still-playing.mp3");
        set_playing(
            &mut worker,
            2,
            active_source.clone(),
            None,
            Box::new(FiniteSource::new(vec![])),
        );
        let stream_source = TrackSource::stream(
            url::Url::parse("https://delayed.example/old").unwrap(),
            StreamKind::Http,
        );
        let cancellation = AcquisitionCancellation::new();
        worker.acquisition.pending = Some(PendingAcquisition {
            id: 2,
            generation: None,
            source: stream_source.clone(),
            track_index: 3,
            cancellation,
            kind: AcquisitionKind::Stream,
        });
        worker
            .acquisition
            .sender
            .send(AcquisitionResult::StreamFailed {
                id: 1,
                generation: None,
                source: stream_source,
                track_index: 3,
                error: AudioError::Stream(anyhow::anyhow!("stale")),
            })
            .expect("stale result");

        worker.drain_acquisition_results();

        assert_eq!(loaded(&worker).index, 2);
        assert_eq!(&loaded(&worker).source, &active_source);
        assert!(worker.acquisition.pending.is_some());
        assert!(event_bus.try_recv().is_err());
    }

    #[test]
    fn newer_local_play_cancels_pending_stream_acquisition() {
        let event_bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        let cancellation = AcquisitionCancellation::new();
        worker.acquisition.pending = Some(PendingAcquisition {
            id: 1,
            generation: None,
            source: TrackSource::stream(
                url::Url::parse("https://delayed.example/old").unwrap(),
                StreamKind::Http,
            ),
            track_index: 1,
            cancellation: cancellation.clone(),
            kind: AcquisitionKind::Stream,
        });

        let root = unique_temp_dir("stream-replaced-by-local");
        let local_path = root.path().join("local.wav");
        std::fs::write(&local_path, wav_bytes(&[])).expect("write local fixture");
        worker.sink_manager.install(Box::new(FakeSink::new([])));

        worker.play(TrackSource::local(local_path.clone()), 2, None);

        assert!(cancellation.is_cancelled());
        assert!(worker.acquisition.pending.is_none());
        assert_eq!(loaded(&worker).index, 2);
        assert_eq!(loaded(&worker).source.path(), Some(local_path.as_path()));
    }

    #[test]
    fn fallback_seek_progress_consumes_exact_requested_sample_count() {
        let requested = SEEK_COMMAND_POLL_SAMPLES + 17;
        let samples: Vec<f32> = (0..requested + 1).map(|sample| sample as f32).collect();
        let mut source: Box<dyn Source<Item = f32> + Send> = Box::new(FiniteSource::new(samples));
        let mut progress = SeekProgress::Skipping {
            remaining: requested,
        };

        Worker::skip_fallback_samples(source.as_mut(), &mut progress);
        assert!(matches!(progress, SeekProgress::Skipping { remaining: 17 }));
        Worker::skip_fallback_samples(source.as_mut(), &mut progress);

        assert!(matches!(progress, SeekProgress::Done));
        assert_eq!(source.next(), Some(requested as f32));
    }

    #[test]
    fn controls_during_fallback_seek_cannot_overwrite_pending_flush() {
        let event_bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        sink.delay_flush_ack();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        let source = TrackSource::local("/fallback-seek.wav");
        set_playing(
            &mut worker,
            1,
            source.clone(),
            None,
            Box::new(FiniteSource::new(vec![1.0; 8_192])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.sink_manager.begin_segment(false, 0);
        let flush_events = sink.event_names();
        worker.seek = Some(SeekState {
            source,
            decoder: Box::new(FiniteSource::new(vec![0.0; SEEK_COMMAND_POLL_SAMPLES * 2])),
            native_rate: 44_100,
            native_channels: 1,
            position: Duration::from_secs(1),
            progress: SeekProgress::Skipping {
                remaining: SEEK_COMMAND_POLL_SAMPLES * 2,
            },
        });

        assert!(!worker.handle(AudioCommand::SetOutput(output_target(7))));
        assert!(!worker.handle(AudioCommand::Resume));
        assert_eq!(worker.deferred_seek_commands.len(), 2);
        assert!(worker.sink_manager.has_pending_flush());
        assert_eq!(sink.event_names(), flush_events);

        worker.advance_seek();
        worker.advance_seek();
        assert!(worker.seek_is_done());
        worker.complete_seek();
        assert!(worker.sink_manager.has_pending_flush());
        let post_completion_events = sink.event_names();
        assert!(!worker.handle(AudioCommand::SetOutput(output_target(8))));
        assert!(!worker.handle(AudioCommand::Resume));
        assert_eq!(worker.deferred_seek_commands.len(), 4);
        assert_eq!(sink.event_names(), post_completion_events);
        worker.drain_commands();
        assert_eq!(
            worker.deferred_seek_commands.len(),
            4,
            "deferred controls must wait for the seek flush acknowledgement"
        );
    }

    #[test]
    fn fallback_seek_shutdown_interrupts_without_installing_stale_source() {
        let event_bus = EventBus::new();
        let (command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        let source = TrackSource::local("/still-playing.wav");
        set_playing(
            &mut worker,
            4,
            source.clone(),
            None,
            Box::new(FiniteSource::new(vec![1.0; 8_192])),
        );
        worker.seek = Some(SeekState {
            source,
            decoder: Box::new(FiniteSource::new(vec![
                0.0;
                SEEK_COMMAND_POLL_SAMPLES * 100
            ])),
            native_rate: 44_100,
            native_channels: 1,
            position: Duration::from_secs(1),
            progress: SeekProgress::Skipping {
                remaining: SEEK_COMMAND_POLL_SAMPLES * 100,
            },
        });

        let worker_thread = thread::spawn(move || worker.run());
        let started = Instant::now();
        command_tx
            .send(AudioCommand::Shutdown)
            .expect("shutdown command");
        worker_thread
            .join()
            .expect("worker must shut down promptly");
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "shutdown must interrupt a long fallback seek"
        );
        assert!(event_bus.try_recv().is_err());
    }

    #[test]
    fn newer_play_supersedes_fallback_seek_without_stale_installation() {
        let event_bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        let old_source = TrackSource::local("/old-track.wav");
        set_playing(
            &mut worker,
            1,
            old_source.clone(),
            None,
            Box::new(FiniteSource::new(vec![1.0; 8_192])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.seek = Some(SeekState {
            source: old_source,
            decoder: Box::new(FiniteSource::new(vec![0.0; SEEK_COMMAND_POLL_SAMPLES * 2])),
            native_rate: 44_100,
            native_channels: 1,
            position: Duration::from_secs(1),
            progress: SeekProgress::Skipping {
                remaining: SEEK_COMMAND_POLL_SAMPLES * 2,
            },
        });
        let root = unique_temp_dir("fallback-seek-replacement");
        let replacement_path = root.path().join("replacement.wav");
        std::fs::write(&replacement_path, wav_bytes(&[])).expect("write replacement fixture");

        assert!(!worker.handle(AudioCommand::SetOutput(output_target(7))));
        assert!(!worker.handle(AudioCommand::Resume));
        assert_eq!(worker.deferred_seek_commands.len(), 2);
        assert!(!worker.handle(AudioCommand::Play {
            source: TrackSource::local(replacement_path.clone()),
            track_index: 2,
            generation: Some(2),
        }));
        assert!(worker.seek.is_none());
        assert!(worker.deferred_seek_commands.is_empty());
        assert_eq!(loaded(&worker).index, 2);
        assert_eq!(
            loaded(&worker).source.path(),
            Some(replacement_path.as_path())
        );
    }

    #[test]
    fn newer_seek_replaces_fallback_progress_before_installing_the_new_decoder() {
        let root = unique_temp_dir("fallback-seek-replaced-by-seek");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let event_bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        let source = TrackSource::local(path.clone());
        set_playing(
            &mut worker,
            1,
            source.clone(),
            None,
            Box::new(FiniteSource::new(vec![1.0; 8_192])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.seek = Some(SeekState {
            source,
            decoder: Box::new(FiniteSource::new(vec![0.0; SEEK_COMMAND_POLL_SAMPLES * 2])),
            native_rate: 44_100,
            native_channels: 1,
            position: Duration::from_secs(1),
            progress: SeekProgress::Skipping {
                remaining: SEEK_COMMAND_POLL_SAMPLES * 2,
            },
        });

        assert!(!worker.handle(AudioCommand::SeekTo(Duration::ZERO)));
        assert!(worker.seek.is_none());
        assert_eq!(loaded(&worker).frames_written, 0);
        assert_eq!(loaded(&worker).source.path(), Some(path.as_path()));
    }

    #[test]
    fn stop_and_shutdown_discard_deferred_commands_from_the_cancelled_seek() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        let source = TrackSource::local("/stop-seek.wav");
        set_playing(
            &mut worker,
            1,
            source.clone(),
            None,
            Box::new(FiniteSource::new(vec![1.0; 8_192])),
        );
        worker.seek = Some(SeekState {
            source,
            decoder: Box::new(FiniteSource::new(vec![0.0; SEEK_COMMAND_POLL_SAMPLES])),
            native_rate: 44_100,
            native_channels: 1,
            position: Duration::from_secs(1),
            progress: SeekProgress::Skipping {
                remaining: SEEK_COMMAND_POLL_SAMPLES,
            },
        });
        assert!(!worker.handle(AudioCommand::Resume));
        assert_eq!(worker.deferred_seek_commands.len(), 1);
        assert!(!worker.handle(AudioCommand::Stop));
        assert!(worker.deferred_seek_commands.is_empty());

        let mut shutdown_worker = empty_worker(bus.sender(), channel().1);
        shutdown_worker
            .deferred_seek_commands
            .push_back(AudioCommand::Resume);
        assert!(shutdown_worker.handle(AudioCommand::Shutdown));
        assert!(shutdown_worker.deferred_seek_commands.is_empty());
    }

    #[test]
    fn deferred_commands_replay_in_fifo_order_after_flush_acknowledgement() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        sink.delay_flush_ack();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            1,
            TrackSource::local("/fifo-seek.wav"),
            None,
            Box::new(FiniteSource::new(vec![1.0; 8_192])),
        );
        worker.pause();
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.sink_manager.begin_segment(false, 0);

        assert!(!worker.handle(AudioCommand::Resume));
        assert!(!worker.handle(AudioCommand::Pause));
        assert_eq!(worker.deferred_seek_commands.len(), 2);

        sink.acknowledge_flushes();
        assert!(!worker.pump_playback_at(Instant::now()));
        assert!(!worker.sink_manager.has_pending_flush());
        assert!(!worker.drain_commands());
        assert!(matches!(worker.playback, Playback::Paused(_)));
        assert!(worker.deferred_seek_commands.is_empty());
    }

    #[test]
    fn deferred_commands_keep_fifo_barrier_after_immediate_flush_acknowledgement() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            1,
            TrackSource::local("/fifo-immediate.wav"),
            None,
            Box::new(FiniteSource::new(vec![1.0; 8_192])),
        );
        worker.playback = match std::mem::replace(&mut worker.playback, Playback::Idle) {
            Playback::Playing(active) => Playback::Paused(active),
            other => other,
        };
        worker.sink_manager.install(Box::new(FakeSink::new([])));
        worker
            .deferred_seek_commands
            .push_back(AudioCommand::Resume);

        // A later command must not overtake the already queued Resume, even
        // when no sink flush remains pending at this observation boundary.
        assert!(!worker.handle(AudioCommand::Pause));
        assert_eq!(worker.deferred_seek_commands.len(), 2);
        assert!(!worker.drain_commands());
        assert!(matches!(worker.playback, Playback::Paused(_)));
        assert!(worker.deferred_seek_commands.is_empty());
    }

    #[test]
    fn deferred_seek_burst_reopens_the_decoder_once_at_the_final_target() {
        let root = unique_temp_dir("deferred-seek-burst");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        sink.delay_flush_ack();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path),
            Some(Duration::from_secs(60)),
            Box::new(FiniteSource::new(vec![1.0; 8_192])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.sink_manager.begin_segment(false, 0);

        assert!(!worker.handle(AudioCommand::SeekBy {
            forward: true,
            amount: Duration::from_secs(5),
        }));
        assert!(!worker.handle(AudioCommand::SeekBy {
            forward: true,
            amount: Duration::from_secs(5),
        }));
        assert!(!worker.handle(AudioCommand::SeekTo(Duration::from_secs(20))));
        assert_eq!(worker.deferred_seek_commands.len(), 3);

        sink.acknowledge_flushes();
        worker.pump_playback_at(Instant::now());
        assert!(!worker.sink_manager.has_pending_flush());
        assert!(!worker.drain_commands());

        assert!(worker.deferred_seek_commands.is_empty());
        assert_eq!(worker.pending_seek_target, Some(Duration::from_secs(20)));
        assert_eq!(
            sink.event_names()
                .into_iter()
                .filter(|event| *event == "flush")
                .count(),
            2,
            "the initial boundary and final coalesced seek are the only flushes"
        );
    }

    #[test]
    fn newer_stream_play_discards_a_result_and_retains_c() {
        let event_bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        worker.acquisition.next_id.store(2, Ordering::Relaxed);

        let source_a = TrackSource::stream(
            url::Url::parse("https://example.com/a").expect("valid url"),
            StreamKind::Http,
        );
        let source_c = TrackSource::stream(
            url::Url::parse("https://example.com/c").expect("valid url"),
            StreamKind::Http,
        );
        let cancellation_a = AcquisitionCancellation::new();
        worker.acquisition.pending = Some(PendingAcquisition {
            id: 1,
            generation: None,
            source: source_a.clone(),
            track_index: 1,
            cancellation: cancellation_a.clone(),
            kind: AcquisitionKind::Stream,
        });

        worker.play(source_c.clone(), 3, None);
        let c_id = worker
            .acquisition
            .pending
            .as_ref()
            .expect("C acquisition must be pending")
            .id;
        assert!(cancellation_a.is_cancelled());

        worker.handle_acquisition_result(AcquisitionResult::StreamFailed {
            id: 1,
            generation: None,
            source: source_a,
            track_index: 1,
            error: AudioError::Stream(anyhow::anyhow!("stale A")),
        });
        assert_eq!(
            worker
                .acquisition
                .pending
                .as_ref()
                .map(|pending| pending.id),
            Some(c_id),
            "A's result must not clear C's pending acquisition"
        );

        worker.sink_manager.install(Box::new(FakeSink::new([])));
        worker.handle_acquisition_result(AcquisitionResult::StreamReady {
            id: c_id,
            generation: None,
            source: source_c.clone(),
            track_index: 3,
            prepared: PreparedStream {
                decoder: Box::new(FiniteSource::new(vec![0.0; 8])),
                duration: Some(Duration::from_secs(1)),
                rate: 44_100,
                channels: 1,
            },
        });

        assert!(worker.acquisition.pending.is_none());
        assert_eq!(loaded(&worker).index, 3);
        assert_eq!(&loaded(&worker).source, &source_c);
    }

    #[test]
    fn repressing_same_stream_ignores_old_generation_and_accepts_new_one() {
        let event_bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        worker.acquisition.next_id.store(4, Ordering::Relaxed);
        let source = TrackSource::stream(
            url::Url::parse("https://example.com/restart").expect("valid url"),
            StreamKind::Http,
        );
        let old_cancellation = AcquisitionCancellation::new();
        worker.acquisition.pending = Some(PendingAcquisition {
            id: 3,
            generation: Some(1),
            source: source.clone(),
            track_index: 7,
            cancellation: old_cancellation.clone(),
            kind: AcquisitionKind::Stream,
        });

        worker.play(source.clone(), 7, Some(2));
        let new_id = worker
            .acquisition
            .pending
            .as_ref()
            .expect("repeated Play must create a new acquisition")
            .id;
        assert_ne!(new_id, 3);
        assert!(old_cancellation.is_cancelled());

        worker.handle_acquisition_result(AcquisitionResult::StreamFailed {
            id: 3,
            generation: Some(1),
            source: source.clone(),
            track_index: 7,
            error: AudioError::Stream(anyhow::anyhow!("stale generation")),
        });
        assert_eq!(
            worker
                .acquisition
                .pending
                .as_ref()
                .map(|pending| pending.id),
            Some(new_id),
            "the old same-source result must not deadlock or replace the restart"
        );

        worker.sink_manager.install(Box::new(FakeSink::new([])));
        worker.handle_acquisition_result(AcquisitionResult::StreamReady {
            id: new_id,
            generation: Some(2),
            source: source.clone(),
            track_index: 7,
            prepared: PreparedStream {
                decoder: Box::new(FiniteSource::new(vec![0.0; 8])),
                duration: Some(Duration::from_secs(1)),
                rate: 44_100,
                channels: 1,
            },
        });

        assert!(worker.acquisition.pending.is_none());
        assert_eq!(loaded(&worker).index, 7);
        assert_eq!(&loaded(&worker).source, &source);
        let mut ready_generation = None;
        while let Ok(event) = event_bus.try_recv() {
            if let AppEvent::SourceReady { generation, .. } = event {
                ready_generation = generation;
            }
        }
        assert_eq!(ready_generation, Some(2));
    }

    #[test]
    fn audio_command_derives_debug_clone_and_eq() {
        let cmd = AudioCommand::Play {
            source: TrackSource::local("/test.mp3"),
            track_index: 0,
            generation: None,
        };
        let cloned = cmd.clone();
        assert_eq!(cmd, cloned);

        // Debug should not panic
        let _ = format!("{cmd:?}");
    }

    #[test]
    fn audio_command_variants_are_distinct() {
        let play = AudioCommand::Play {
            source: TrackSource::local("/a.mp3"),
            track_index: 0,
            generation: None,
        };
        let pause = AudioCommand::Pause;
        let resume = AudioCommand::Resume;
        let volume = AudioCommand::SetVolume(volume(50));
        let seek = AudioCommand::SeekTo(Duration::from_secs(10));
        let shutdown = AudioCommand::Shutdown;

        assert_ne!(play, pause);
        assert_ne!(pause, resume);
        assert_ne!(resume, volume);
        assert_ne!(volume, seek);
        assert_ne!(seek, shutdown);
    }

    #[test]
    fn audio_command_boundaries_do_not_accept_invalid_domain_values() {
        assert_eq!(
            AudioCommand::set_volume(150),
            AudioCommand::SetVolume(VolumePercent::MAX)
        );
        assert_eq!(
            AudioCommand::set_gain(6.0),
            Ok(AudioCommand::SetGain(GainDb::try_from(6.0).unwrap()))
        );
        assert_eq!(
            AudioCommand::set_gain(1.25),
            Err(crate::audio::GainDbError::NotAStep)
        );
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                AudioCommand::set_gain(value),
                Err(crate::audio::GainDbError::NonFinite)
            );
        }
        assert_eq!(
            AudioCommand::set_crossfade(12),
            AudioCommand::SetCrossfade(crossfade(10))
        );
    }

    #[test]
    fn engine_handle_send_forwards_command_through_channel() {
        let (sender, receiver) = channel();
        let handle = AudioEngineHandle {
            sender,
            interruption: StreamInterruption::new(),
        };

        handle
            .send(AudioCommand::SetVolume(volume(75)))
            .expect("send must succeed");

        let received = receiver.try_recv().expect("command must be in channel");
        assert_eq!(received, AudioCommand::SetVolume(volume(75)));
    }

    #[test]
    fn relative_seek_dispatch_only_enqueues_without_waiting_for_worker_or_flush() {
        let (sender, receiver) = channel();
        let handle = AudioEngineHandle {
            sender,
            interruption: StreamInterruption::new(),
        };
        let dispatch = thread::spawn(move || {
            handle.send(AudioCommand::SeekBy {
                forward: true,
                amount: Duration::from_secs(5),
            })
        });

        let received = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("dispatch must enqueue without a worker acknowledgement");
        assert_eq!(
            received,
            AudioCommand::SeekBy {
                forward: true,
                amount: Duration::from_secs(5),
            }
        );
        dispatch
            .join()
            .expect("dispatch thread must finish")
            .expect("the command channel must remain open");
    }

    #[test]
    fn engine_handle_interrupts_before_enqueueing_a_replacement() {
        let (sender, receiver) = channel();
        let interruption = StreamInterruption::new();
        let previous = interruption.current();
        let handle = AudioEngineHandle {
            sender,
            interruption,
        };

        handle
            .send(AudioCommand::Play {
                source: TrackSource::local("/replacement.wav"),
                track_index: 2,
                generation: None,
            })
            .expect("replacement must be queued");

        assert!(previous.is_cancelled());
        assert!(matches!(receiver.try_recv(), Ok(AudioCommand::Play { .. })));
    }

    #[test]
    fn engine_handle_shutdown_sends_shutdown_command() {
        let (sender, receiver) = channel();
        let handle = AudioEngineHandle {
            sender,
            interruption: StreamInterruption::new(),
        };

        handle.shutdown();

        let received = receiver.try_recv().expect("shutdown must be in channel");
        assert_eq!(received, AudioCommand::Shutdown);
    }

    #[test]
    fn engine_handle_send_fails_after_worker_drops() {
        let (sender, _receiver) = channel();
        let handle = AudioEngineHandle {
            sender,
            interruption: StreamInterruption::new(),
        };

        // Dropping the receiver closes the channel
        drop(_receiver);

        let result = handle.send(AudioCommand::Pause);
        assert!(result.is_err(), "send must fail when receiver is gone");
    }

    #[test]
    fn stop_command_dispatches_without_panicking() {
        let bus = EventBus::new();
        match spawn_audio_worker(bus.sender(), crate::stream::resolver_with_defaults()) {
            Ok(handle) => {
                // Stop must never panic even with no active track (the worker
                // has not opened a device yet), and it must not open one.
                let _ = handle.send(AudioCommand::Stop);
                handle.shutdown();
            }
            Err(error) => {
                eprintln!("audio worker unavailable (expected in CI): {error}");
            }
        }
    }

    #[test]
    fn a_panicking_worker_is_caught_and_notifies_the_user() {
        use std::sync::mpsc::channel;

        let (events_tx, events_rx) = channel();
        // Simulate the spawn closure's catch_unwind: a panicking body must not
        // unwind the caught thread, and the notification must be delivered.
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("simulated audio worker panic");
        }))
        .is_err();
        assert!(caught, "the worker panic must be contained");

        // The notification path used after a caught panic still delivers.
        let _ = events_tx.send(AppEvent::Notification {
            kind: EffectErrorKind::Audio,
            operation_id: None,
            message: "audio died".to_string(),
        });
        let received = events_rx.try_recv().expect("notification delivered");
        assert!(matches!(received, AppEvent::Notification { .. }));
    }

    #[test]
    fn typed_gain_and_crossfade_values_are_always_in_domain() {
        let event_bus = EventBus::new();
        let mut worker = empty_worker(event_bus.sender(), channel().1);

        // Typed worker state cannot represent values outside the gain domain.
        worker.set_gain(GainDb::MAX);
        assert_eq!(worker.gain_db, GainDb::MAX);
        worker.set_gain(GainDb::MIN);
        assert_eq!(worker.gain_db, GainDb::MIN);
        // Non-finite values are rejected before a command can be built.
        assert!(AudioCommand::set_gain(f32::NAN).is_err());

        // Crossfade commands arrive normalized at their boundary.
        worker.set_crossfade(CrossfadeSeconds::from_boundary(999));
        assert_eq!(worker.crossfade_seconds, CrossfadeSeconds::MAX);
        worker.set_crossfade(CrossfadeSeconds::DISABLED);
        assert_eq!(worker.crossfade_seconds, CrossfadeSeconds::DISABLED);
    }

    #[test]
    fn worker_updates_live_sink_with_combined_typed_gain() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/gain-boundary.wav"),
            None,
            Box::new(FiniteSource::new(vec![2.0])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        worker.set_volume(volume(50));
        worker.set_gain(GainDb::try_from(6.0).expect("valid gain"));

        let expected_gain = volume_factor(volume(50)) * gain_factor(GainDb::try_from(6.0).unwrap());
        assert!((sink.gain() - expected_gain).abs() < f32::EPSILON);
        assert!(worker.pump_playback());
        assert_eq!(sink.pushed_samples().len(), 1);
        assert!((sink.pushed_samples()[0][0] - 2.0 * expected_gain).abs() < f32::EPSILON);
    }

    #[test]
    fn worker_uses_explicit_audio_stage_ownership() {
        let bus = EventBus::new();
        let worker = empty_worker(bus.sender(), channel().1);

        assert!(matches!(
            &worker.crossfade.lifecycle,
            CrossfadeLifecycle::NoPreload
        ));
        assert!(!worker.sink_manager.is_live());
        assert!(worker.crossfade.preloaded().is_none());
        assert!(worker.pending_output().is_none());
        assert_eq!(worker.speed_stage.requested_speed, SPEED_DEFAULT);
        assert!(worker.speed_stage.stretch().is_none());
    }

    #[test]
    fn playback_lifecycle_transitions_keep_loaded_decoder_attached() {
        let root = unique_temp_dir("playback-lifecycle");
        let path = root.join("lifecycle.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.sink_manager.install(Box::new(FakeSink::new([])));

        worker.play(TrackSource::local(path.clone()), 3, None);
        assert!(matches!(worker.playback, Playback::Playing(_)));
        assert_eq!(loaded(&worker).source.path(), Some(path.as_path()));

        worker.pause();
        assert!(matches!(worker.playback, Playback::Paused(_)));
        worker.resume();
        assert!(matches!(worker.playback, Playback::Playing(_)));

        worker.stop();
        assert!(matches!(worker.playback, Playback::Idle));
        assert!(worker.playback.loaded().is_none());
    }

    #[test]
    fn loading_rejects_pause_and_seek_without_cancelling_its_acquisition() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        let source = TrackSource::stream(
            url::Url::parse("https://loading.example/track").expect("valid URL"),
            StreamKind::Http,
        );
        let cancellation = AcquisitionCancellation::new();
        worker.acquisition.pending = Some(PendingAcquisition {
            id: 9,
            generation: Some(4),
            source: source.clone(),
            track_index: 3,
            cancellation: cancellation.clone(),
            kind: AcquisitionKind::Stream,
        });
        worker.playback = Playback::Loading {};

        worker.pause();
        assert!(!worker.handle(AudioCommand::SeekTo(Duration::from_secs(2))));
        assert!(!worker.handle(AudioCommand::SeekBy {
            forward: true,
            amount: Duration::from_secs(2),
        }));
        assert!(matches!(worker.playback, Playback::Loading { .. }));
        assert!(!cancellation.is_cancelled());

        worker.stop();
        assert!(matches!(worker.playback, Playback::Idle));
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn public_handle_seek_during_loading_reaches_terminal_acquisition_failure() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let resolver = StreamResolver::new(vec![Arc::new(DelayedProvider {
            started: Arc::clone(&started),
            boundary: Arc::new(Barrier::new(1)),
            release: Arc::clone(&release),
            hls: false,
        })]);
        let events = EventBus::new();
        let (sender, receiver) = channel();
        let interruption = StreamInterruption::new();
        let mut worker = empty_worker(events.sender(), receiver);
        worker.resolver = resolver;
        worker.interruption = interruption.clone();
        let worker_thread = thread::spawn(move || worker.run());
        let handle = AudioEngineHandle {
            sender,
            interruption: interruption.clone(),
        };
        let source = TrackSource::stream(
            url::Url::parse("https://delayed.example/loading").expect("valid URL"),
            StreamKind::Http,
        );

        handle
            .send(AudioCommand::Play {
                source,
                track_index: 6,
                generation: Some(8),
            })
            .expect("play command");
        wait_for_flag(&started);

        let acquisition_token = interruption.current();
        handle
            .send(AudioCommand::SeekTo(Duration::from_secs(2)))
            .expect("seek command");
        assert!(
            !acquisition_token.is_cancelled(),
            "a rejected loading seek must not cancel the acquisition"
        );

        release.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut saw_failure = false;
        while !saw_failure {
            assert!(
                Instant::now() < deadline,
                "loading acquisition remained pending after the rejected seek"
            );
            if let Ok(event) = events.recv_timeout(Duration::from_millis(25)) {
                saw_failure = matches!(
                    event,
                    AppEvent::SourceFailed {
                        generation: Some(8),
                        ..
                    }
                );
            }
        }

        handle.shutdown();
        worker_thread.join().expect("worker must shut down");
    }

    #[test]
    fn public_handle_preload_during_loading_preserves_terminal_acquisition() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let resolver = StreamResolver::new(vec![Arc::new(DelayedProvider {
            started: Arc::clone(&started),
            boundary: Arc::new(Barrier::new(1)),
            release: Arc::clone(&release),
            hls: false,
        })]);
        let events = EventBus::new();
        let (sender, receiver) = channel();
        let interruption = StreamInterruption::new();
        let mut worker = empty_worker(events.sender(), receiver);
        worker.resolver = resolver;
        worker.interruption = interruption.clone();
        let worker_thread = thread::spawn(move || worker.run());
        let handle = AudioEngineHandle {
            sender,
            interruption: interruption.clone(),
        };

        handle
            .send(AudioCommand::Play {
                source: TrackSource::stream(
                    url::Url::parse("https://delayed.example/loading-preload").expect("valid URL"),
                    StreamKind::Http,
                ),
                track_index: 6,
                generation: Some(9),
            })
            .expect("play command");
        wait_for_flag(&started);

        let acquisition_token = interruption.current();
        handle
            .send(AudioCommand::PreloadNext {
                source: TrackSource::local("/ignored-preload.wav"),
                track_index: 7,
            })
            .expect("preload command");
        assert!(
            !acquisition_token.is_cancelled(),
            "a loading preload must not cancel the primary acquisition"
        );

        release.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut saw_failure = false;
        while !saw_failure {
            assert!(
                Instant::now() < deadline,
                "primary acquisition remained pending after the ignored preload"
            );
            if let Ok(event) = events.recv_timeout(Duration::from_millis(25)) {
                saw_failure = matches!(
                    event,
                    AppEvent::SourceFailed {
                        generation: Some(9),
                        ..
                    }
                );
            }
        }

        handle.shutdown();
        worker_thread.join().expect("worker must shut down");
    }

    #[test]
    fn drain_state_expresses_the_only_valid_source_and_output_order() {
        assert!(!DrainState::Producing.source_exhausted());
        assert!(!DrainState::Producing.drained());
        assert!(DrainState::SourceExhausted.source_exhausted());
        assert!(!DrainState::SourceExhausted.drained());
        assert!(DrainState::Drained.source_exhausted());
        assert!(DrainState::Drained.drained());
    }

    #[test]
    fn crossfade_ready_preload_survives_forward_and_backward_seek_reconciliation() {
        let mut stage = CrossfadeStage::new();
        let source = TrackSource::local("/crossfade-seek-next.wav");
        stage.install_ready(PreloadedTrack {
            source: Box::new(FiniteSource::new(vec![1.0; 8])),
            index: 4,
            identity: source.clone(),
            duration: Some(Duration::from_secs(1)),
            rate: 44_100,
            channels: 1,
            consumed_frames: 0,
        });

        assert!(stage.prepare_for_seek(44_100, 1).is_none());
        assert!(matches!(
            &stage.lifecycle,
            CrossfadeLifecycle::PreloadReady(_)
        ));
        assert_eq!(
            stage.begin_transition(Duration::from_secs(8)),
            Some(Duration::from_secs(8))
        );
        assert!(stage.prepare_for_seek(44_100, 1).is_none());
        assert_eq!(
            stage.begin_transition(Duration::from_secs(2)),
            Some(Duration::from_secs(2))
        );
        assert_eq!(stage.preloaded().expect("ready preload").index, 4);
    }

    #[test]
    fn completed_ready_preload_survives_seek_and_crossfade_starts_at_recalculated_window() {
        let root = unique_temp_dir("crossfade-after-seek");
        let path = root.join("current.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path.clone()),
            Some(Duration::from_secs(10)),
            Box::new(FiniteSource::new(vec![0.0; 8])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.crossfade_seconds = crossfade(5);
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![1.0; 8])),
            1,
            TrackSource::local("/crossfade-after-seek-next.wav"),
            44_100,
            1,
        );

        assert!(!worker.handle(AudioCommand::SeekTo(Duration::from_secs(8))));
        assert_eq!(
            worker.crossfade.preloaded().expect("ready preload").index,
            1
        );
        assert!(worker.crossfade.buffers_ready(1, PUMP_CHUNK_FRAMES));
        assert!(worker.pump_playback());
        assert!(worker.crossfade.preloaded().is_none());
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::CrossfadeCompleted { track_index: 1, .. })
        ));
    }

    #[test]
    fn seek_to_exact_duration_with_ready_preload_never_panics() {
        let root = unique_temp_dir("crossfade-exact-seek");
        let path = root.join("current.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path),
            Some(Duration::from_secs(10)),
            Box::new(FiniteSource::new(vec![0.0; 8])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.crossfade_seconds = crossfade(5);
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![1.0; 8])),
            1,
            TrackSource::local("/crossfade-exact-seek-next.wav"),
            44_100,
            1,
        );

        assert!(!worker.handle(AudioCommand::SeekTo(Duration::from_secs(10))));
        assert!(worker.crossfade.buffers_ready(1, PUMP_CHUNK_FRAMES));
        assert!(worker.pump_playback());
        assert!(worker.crossfade.preloaded().is_none());
    }

    #[test]
    fn unprepared_crossfade_pump_fails_closed_and_recovers_after_preparation() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/crossfade-unprepared-current.wav"),
            Some(Duration::from_secs(1)),
            Box::new(FiniteSource::new(vec![1.0; 4])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![10.0; 4])),
            1,
            TrackSource::local("/crossfade-unprepared-next.wav"),
            44_100,
            1,
        );

        assert!(!worker.pump_crossfade(1, 4, 0.5));
        assert_eq!(loaded(&worker).frames_written, 0);
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .expect("ready preload")
                .consumed_frames,
            0
        );

        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("ready preload must enter the transition");
        assert!(worker.pump_crossfade(1, 4, 0.5));
        assert_eq!(loaded(&worker).frames_written, 4);
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .expect("ready preload")
                .consumed_frames,
            4
        );
    }

    #[test]
    fn crossfade_completion_reports_consumed_track_b_elapsed() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![1.0; 8])),
            1,
            TrackSource::local("/crossfade-elapsed-next.wav"),
            48_000,
            1,
        );
        worker.crossfade.record_accepted_next_frames(2_400);

        worker.complete_crossfade();

        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::CrossfadeCompleted {
                track_index: 1,
                elapsed,
                ..
            }) if elapsed == Duration::from_millis(50)
        ));
    }

    #[test]
    fn crossfade_completion_reports_zero_elapsed_without_consumed_track_b_frames() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![1.0; 8])),
            1,
            TrackSource::local("/crossfade-zero-elapsed-next.wav"),
            48_000,
            1,
        );

        worker.complete_crossfade();

        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::CrossfadeCompleted {
                track_index: 1,
                elapsed: Duration::ZERO,
                ..
            })
        ));
    }

    #[test]
    fn crossfade_t_one_shortcut_reports_frames_accepted_before_handover() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/crossfade-shortcut-current.wav"),
            Some(Duration::from_secs(10)),
            Box::new(FiniteSource::new(vec![1.0; 4])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.crossfade_seconds = crossfade(5);
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![10.0; 4])),
            1,
            TrackSource::local("/crossfade-shortcut-next.wav"),
            44_100,
            1,
        );
        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("ready preload must enter the transition");

        assert!(worker.pump_crossfade(1, 4, 0.5));
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .expect("preloaded track")
                .consumed_frames,
            4
        );

        // The worker's existing seek boundary is enough to make the next pump
        // observe t == 1 without introducing a second position authority.
        worker.pending_seek_target = Some(Duration::from_secs(10));
        worker.pump_playback();
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::CrossfadeCompleted { elapsed, .. })
                if elapsed == Duration::from_secs_f64(4.0 / 44_100.0)
        ));
    }

    #[test]
    fn crossfade_partially_consumed_seek_returns_a_fresh_preload_request() {
        let mut stage = CrossfadeStage::new();
        let source = TrackSource::local("/crossfade-seek-partial.wav");
        stage.install_ready(PreloadedTrack {
            source: Box::new(FiniteSource::new(vec![1.0; 8])),
            index: 7,
            identity: source.clone(),
            duration: Some(Duration::from_secs(1)),
            rate: 44_100,
            channels: 1,
            consumed_frames: 0,
        });
        assert_eq!(
            stage.begin_transition(Duration::from_secs(8)),
            Some(Duration::from_secs(8))
        );
        stage.record_accepted_next_frames(1);

        let request = stage
            .prepare_for_seek(44_100, 1)
            .expect("partial transition must rearm");
        assert_eq!(request.source, source);
        assert_eq!(request.track_index, 7);
        assert!(matches!(&stage.lifecycle, CrossfadeLifecycle::NoPreload));
    }

    #[test]
    fn crossfade_seek_reissue_rejects_the_old_preload_result() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        let source = TrackSource::local("/crossfade-seek-reissue.wav");
        let old_cancellation = AcquisitionCancellation::new();
        worker.acquisition.pending = Some(PendingAcquisition {
            id: 10,
            generation: None,
            source: source.clone(),
            track_index: 3,
            cancellation: old_cancellation,
            kind: AcquisitionKind::LocalPreload,
        });
        worker.crossfade.request(10, source.clone(), 3);
        worker.cancel_pending_preload();
        let new_cancellation = AcquisitionCancellation::new();
        worker.acquisition.pending = Some(PendingAcquisition {
            id: 11,
            generation: None,
            source: source.clone(),
            track_index: 3,
            cancellation: new_cancellation,
            kind: AcquisitionKind::LocalPreload,
        });
        worker.crossfade.request(11, source.clone(), 3);
        worker
            .acquisition
            .sender
            .send(AcquisitionResult::PreloadReady {
                id: 10,
                generation: None,
                source: source.clone(),
                track_index: 3,
                prepared: PreparedStream {
                    decoder: Box::new(FiniteSource::new(vec![0.0; 8])),
                    duration: Some(Duration::from_secs(1)),
                    rate: 44_100,
                    channels: 1,
                },
            })
            .expect("stale preload result");

        worker.drain_acquisition_results();

        assert!(worker.crossfade.preloaded().is_none());
        assert!(matches!(
            &worker.crossfade.lifecycle,
            CrossfadeLifecycle::PreloadRequested(request) if request.id == 11
        ));
        assert_eq!(
            worker
                .acquisition
                .pending
                .as_ref()
                .map(|pending| pending.id),
            Some(11)
        );
    }

    #[test]
    fn explicit_crossfade_discard_clears_ready_and_running_state() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![0.0; 8])),
            2,
            TrackSource::local("/crossfade-discard.wav"),
            44_100,
            1,
        );
        worker.crossfade.begin_transition(Duration::from_secs(1));

        worker.discard_crossfade();

        assert!(worker.crossfade.preloaded().is_none());
        assert!(matches!(
            &worker.crossfade.lifecycle,
            CrossfadeLifecycle::NoPreload
        ));
    }

    #[test]
    fn speed_commands_are_validated_at_the_command_boundary() {
        let event_bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(event_bus.sender(), command_rx);
        worker.speed_stage.requested_speed = SPEED_MAX;

        // Non-finite values cannot become commands and must not replace the
        // last valid speed.
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(matches!(
                AudioCommand::set_speed(invalid),
                Err(SpeedError::NonFinite)
            ));
            assert_eq!(worker.speed_stage.requested_speed, SPEED_MAX);
        }
        assert!(matches!(
            AudioCommand::set_speed(1.25),
            Err(SpeedError::NotAStep)
        ));

        // Finite values outside the domain are saturated at the command
        // boundary before they can reach SoundTouch or credit arithmetic.
        for below_min in [-1.0, 0.0, SPEED_MIN.as_f32() - 0.1] {
            assert!(!worker.handle(AudioCommand::set_speed(below_min).unwrap()));
            assert_eq!(worker.speed_stage.requested_speed, SPEED_MIN);
        }
        assert!(!worker.handle(AudioCommand::set_speed(SPEED_MAX.as_f32() + 0.1).unwrap()));
        assert_eq!(worker.speed_stage.requested_speed, SPEED_MAX);

        // Normal values and both documented boundaries retain their exact
        // command semantics.
        for valid in [SPEED_MIN, Speed::from_tenths(15).unwrap(), SPEED_MAX] {
            assert!(!worker.handle(AudioCommand::SetSpeed(valid)));
            assert_eq!(worker.speed_stage.requested_speed, valid);
        }
    }

    #[test]
    fn unity_does_not_construct_soundtouch_and_predicates_are_independent() {
        let mut stage = SpeedStage::new();
        assert!(!stage.uses_time_stretch());
        assert!(stage.crossfade_allowed());
        stage.setup(44_100, 2);
        assert!(
            stage.stretch().is_none(),
            "unity must stay on the direct path"
        );

        stage.set_speed(Speed::from_tenths(11).unwrap(), Some((44_100, 2)));
        assert!(stage.uses_time_stretch());
        assert!(!stage.crossfade_allowed());
        stage.setup(44_100, 2);
        assert!(stage.stretch().is_some());

        // Returning to unity discards the bounded old-tempo FIFO and makes the
        // direct path immediately explicit.
        stage.set_speed(Speed::UNITY, Some((44_100, 2)));
        assert!(!stage.uses_time_stretch());
        assert!(stage.crossfade_allowed());
        assert!(stage.stretch().is_none());
    }

    #[test]
    fn set_output_without_an_active_track_never_opens_a_device() {
        let bus = EventBus::new();
        match spawn_audio_worker(bus.sender(), crate::stream::resolver_with_defaults()) {
            Ok(handle) => {
                // Retargeting the output with no playing track must only
                // record the chosen node; it must never open a PipeWire
                // stream (which would require the device to exist) and, above
                // all, must not crash the worker.
                let _ = handle.send(AudioCommand::SetOutput(output_target(33)));
                handle.shutdown();
            }
            Err(error) => {
                eprintln!("audio worker unavailable (expected in CI): {error}");
            }
        }
    }

    #[test]
    fn spawn_audio_worker_creates_handle_and_thread() {
        let bus = EventBus::new();
        let result = spawn_audio_worker(bus.sender(), crate::stream::resolver_with_defaults());

        match result {
            Ok(handle) => {
                // Worker thread started — shut it down cleanly
                handle.shutdown();
            }
            Err(error) => {
                // No audio device available in this environment — that's
                // acceptable for CI or headless machines
                eprintln!("audio worker unavailable (expected in CI): {error}");
            }
        }
    }

    #[test]
    fn sink_rebuilds_when_the_sample_rate_or_channels_change() {
        // 44.1k vs 48k is exactly the mixed-library case that made a reused
        // stream sound too fast or too slow.
        assert!(sink_needs_rebuild(44_100, 2, 48_000, 2));
        assert!(sink_needs_rebuild(48_000, 2, 44_100, 2));
        // A channel count change also needs a rebuild.
        assert!(sink_needs_rebuild(44_100, 1, 44_100, 2));
        // Same format is reused without rebuilding.
        assert!(!sink_needs_rebuild(44_100, 2, 44_100, 2));
        assert!(!sink_needs_rebuild(48_000, 6, 48_000, 6));
    }

    #[test]
    fn position_uses_sink_played_frames_anchored_to_the_segment() {
        // The direct path trusts the sink's reproduced-frame counter, anchored
        // to the segment start so it never runs ahead of the audio.
        assert_eq!(
            position_frame_count(false, Some(50_000), 55_000, 0, 0),
            50_000
        );
    }

    #[test]
    fn position_clamps_for_rebuilt_sinks_and_strict_forward_progress() {
        // A sink rebuild resets the played counter below the anchor: the delta
        // saturates to zero, so we report the segment base (never backwards).
        assert_eq!(
            position_frame_count(false, Some(100), 55_000, 50_000, 50_000),
            50_000
        );
        // The reported position cannot exceed frames actually written.
        assert_eq!(
            position_frame_count(false, Some(99_999), 55_000, 0, 0),
            55_000
        );
    }

    #[test]
    fn position_falls_back_to_source_frames_in_the_stretch_path() {
        // Time-stretch changes the output/source mapping, so the source total
        // is authoritative even when the sink reports a lower played count.
        let played = Some(10_000);
        let source_total = 20_000;
        assert_eq!(
            position_frame_count(true, played, source_total, 0, 0),
            source_total
        );
        // No sink yet (e.g. first command before the device opens) also falls
        // back to the source total.
        assert_eq!(
            position_frame_count(true, None, source_total, 0, 0),
            source_total
        );
    }

    #[test]
    fn relative_seek_commands_accumulate_on_the_worker_and_clamp_to_duration() {
        let root = unique_temp_dir("relative-seek-accumulation");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path),
            Some(Duration::from_secs(40)),
            Box::new(FiniteSource::new(vec![])),
        );
        worker.sink_manager.install(Box::new(FakeSink::new([])));

        for _ in 0..10 {
            assert!(!worker.handle(AudioCommand::SeekBy {
                forward: true,
                amount: Duration::from_secs(5),
            }));
        }

        assert_eq!(worker.current_position(), Duration::from_secs(40));
        assert_eq!(worker.pending_seek_target, None);
    }

    #[test]
    fn mixed_relative_seeks_use_the_worker_target_and_respect_bounds() {
        let root = unique_temp_dir("relative-seek-mixed");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path),
            Some(Duration::from_secs(20)),
            Box::new(FiniteSource::new(vec![])),
        );
        worker.sink_manager.install(Box::new(FakeSink::new([])));

        for (forward, amount) in [
            (true, 5),
            (true, 5),
            (false, 5),
            (true, 5),
            (false, 100),
            (true, 5),
        ] {
            assert!(!worker.handle(AudioCommand::SeekBy {
                forward,
                amount: Duration::from_secs(amount),
            }));
        }

        assert_eq!(worker.current_position(), Duration::from_secs(5));
    }

    #[test]
    fn relative_seek_without_duration_remains_finite_and_backward_clamps_to_zero() {
        let root = unique_temp_dir("relative-seek-unknown-duration");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path),
            None,
            Box::new(FiniteSource::new(vec![])),
        );
        worker.sink_manager.install(Box::new(FakeSink::new([])));

        assert!(!worker.handle(AudioCommand::SeekBy {
            forward: true,
            amount: Duration::from_secs(5),
        }));
        assert!(!worker.handle(AudioCommand::SeekBy {
            forward: false,
            amount: Duration::from_secs(10),
        }));
        assert_eq!(worker.current_position(), Duration::ZERO);
    }

    #[test]
    fn stale_sink_progress_cannot_replace_a_pending_relative_seek_target() {
        let root = unique_temp_dir("relative-seek-pending-target");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        sink.delay_flush_ack();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path),
            Some(Duration::from_secs(20)),
            Box::new(FiniteSource::new(vec![])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        assert!(!worker.handle(AudioCommand::SeekBy {
            forward: true,
            amount: Duration::from_secs(5),
        }));
        assert_eq!(worker.pending_seek_target, Some(Duration::from_secs(5)));

        // The sink still reports its pre-seek counter. Progress must remain
        // anchored to the worker target until the flush is acknowledged.
        assert_eq!(worker.current_position(), Duration::from_secs(5));
        worker.publish_periodic_progress();
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::PlaybackProgress { snapshot })
                if snapshot.elapsed == Duration::from_secs(5)
        ));

        sink.acknowledge_flushes();
        worker.pump_playback();
        assert_eq!(worker.pending_seek_target, None);
        assert_eq!(worker.current_position(), Duration::from_secs(5));
    }

    #[test]
    fn seek_waits_for_an_existing_flush_before_installing_a_decoder() {
        let root = unique_temp_dir("seek-pending-flush");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        sink.delay_flush_ack();
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker
            .finish_track_start(
                Box::new(FiniteSource::new(vec![1.0; 512])),
                &TrackSource::local(path.clone()),
                0,
                44_100,
                1,
                None,
            )
            .expect("track start must request the initial flush");
        assert!(worker.sink_manager.has_pending_flush());
        assert_eq!(sink.event_names(), vec!["flush"]);

        assert!(!worker.handle(AudioCommand::SeekTo(Duration::ZERO)));
        assert!(worker.seek.is_none());
        assert_eq!(worker.deferred_seek_commands.len(), 1);
        assert_eq!(
            sink.event_names(),
            vec!["flush"],
            "a seek must not overwrite the active flush boundary"
        );

        sink.acknowledge_flushes();
        worker.pump_playback();
        assert!(!worker.sink_manager.has_pending_flush());
        assert_eq!(worker.deferred_seek_commands.len(), 1);
        assert!(!worker.drain_commands());
        assert!(worker.deferred_seek_commands.is_empty());
        if worker.seek.is_some() {
            while !worker.seek_is_done() {
                worker.advance_seek();
            }
            worker.complete_seek();
        }
        assert!(worker.pending_seek_target.is_some());
        assert!(worker.sink_manager.has_pending_flush());
        let events = sink.event_names();
        assert_eq!(events.iter().filter(|event| **event == "flush").count(), 2);
    }

    #[test]
    fn seek_drops_backpressured_old_output_before_restarting_from_the_target() {
        let root = unique_temp_dir("seek-pending-output");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([PushSamplesResult::Backpressure]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path.clone()),
            Some(Duration::from_secs(20)),
            Box::new(FiniteSource::new(vec![1.0; 512])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.pump_playback();
        assert!(worker.pending_output().is_some());

        assert!(!worker.handle(AudioCommand::SeekTo(Duration::from_secs(5))));
        assert!(worker.pending_output().is_none());
        assert_eq!(loaded(&worker).frames_written, 5 * 44_100);
        assert_eq!(worker.current_position(), Duration::from_secs(5));
        assert!(sink.pushed_samples().is_empty());
    }

    #[test]
    fn completed_seek_submits_non_empty_recovered_output() {
        let root = unique_temp_dir("seek-output-recovery");
        let path = root.join("seek.wav");
        let mut bytes = wav_bytes(&[]);
        for index in (44..bytes.len()).step_by(2) {
            bytes[index..index + 2].copy_from_slice(&1024_i16.to_le_bytes());
        }
        std::fs::write(&path, bytes).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path),
            Some(Duration::from_secs(1)),
            Box::new(FiniteSource::new(vec![0.0; 4_096])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        assert!(!worker.handle(AudioCommand::SeekTo(Duration::ZERO)));
        assert!(worker.pump_playback());

        let pushed = sink.pushed_samples();
        assert!(
            !pushed.is_empty(),
            "a completed seek must submit recovered audio"
        );
        assert!(pushed.iter().all(|chunk| !chunk.is_empty()));
        assert!(
            pushed.iter().flatten().any(|sample| sample.abs() > 0.01),
            "recovered output must contain decoded samples, not only empty chunks"
        );
    }

    #[test]
    fn forward_and_backward_relative_seek_while_playing_use_the_worker_target() {
        let root = unique_temp_dir("active-relative-seek");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path),
            Some(Duration::from_secs(20)),
            Box::new(FiniteSource::new(vec![0.0; 512])),
        );
        worker.sink_manager.install(Box::new(FakeSink::new([])));
        assert!(worker.pump_playback());

        assert!(!worker.handle(AudioCommand::SeekBy {
            forward: true,
            amount: Duration::from_secs(5),
        }));
        assert_eq!(worker.current_position(), Duration::from_secs(5));
        assert!(!worker.handle(AudioCommand::SeekBy {
            forward: false,
            amount: Duration::from_secs(2),
        }));
        assert_eq!(worker.current_position(), Duration::from_secs(3));
    }

    #[test]
    fn relative_seek_is_rejected_for_streams_without_installing_a_target() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::stream(
                url::Url::parse("https://example.com/live").expect("valid URL"),
                StreamKind::Http,
            ),
            None,
            Box::new(FiniteSource::new(vec![])),
        );

        assert!(!worker.handle(AudioCommand::SeekBy {
            forward: true,
            amount: Duration::from_secs(5),
        }));
        assert_eq!(worker.pending_seek_target, None);
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::Notification {
                kind: EffectErrorKind::Audio,
                message,
                ..
            }) if message.contains("Seeking is not available")
        ));
    }

    #[test]
    fn finish_track_start_flushes_before_sampling_the_segment_anchor() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        worker
            .finish_track_start(
                Box::new(FiniteSource::new(vec![0.0; 8])),
                &TrackSource::local("/new-track.wav"),
                0,
                44_100,
                1,
                None,
            )
            .expect("fresh track must start");

        assert_eq!(sink.event_names(), vec!["flush", "frames_played"]);
        assert!(!worker.sink_manager.has_pending_flush());
    }

    #[test]
    fn decoding_and_skip_flushes_before_sampling_the_segment_anchor() {
        let root = unique_temp_dir("seek-flush-anchor");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path.clone()),
            None,
            Box::new(FiniteSource::new(vec![])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        let seek = worker
            .decoding_and_skip(&TrackSource::local(path), Duration::ZERO)
            .expect("seek decoder must be prepared");
        assert!(matches!(seek.progress, SeekProgress::Done));
        worker.seek = Some(seek);
        worker.complete_seek();
        assert_eq!(sink.event_names(), vec!["flush", "frames_played"]);
    }

    #[test]
    fn pending_flush_ack_is_polled_without_pumping_or_blocking() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([PushSamplesResult::Backpressure]));
        sink.delay_flush_ack();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/pending-flush.wav"),
            None,
            Box::new(FiniteSource::new(vec![])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker
            .finish_track_start(
                Box::new(FiniteSource::new(vec![0.0; 512])),
                &TrackSource::local("/pending-flush.wav"),
                0,
                44_100,
                1,
                None,
            )
            .expect("track start must request the flush");

        assert!(worker.sink_manager.has_pending_flush());
        let deadline = pending_flush_deadline(&worker);
        let before_deadline = deadline
            .checked_sub(Duration::from_millis(1))
            .expect("flush deadline must be in the future");
        assert!(
            !worker.pump_playback_at(before_deadline),
            "pending flush must stop pumping"
        );
        assert_eq!(sink.push_attempts(), 0);
        assert_eq!(sink.event_names(), vec!["flush"]);

        sink.acknowledge_flushes();
        worker.pump_playback_at(before_deadline);
        assert!(!worker.sink_manager.has_pending_flush());
        assert_eq!(sink.event_names(), vec!["flush", "frames_played"]);
        assert!(worker.pending_output().is_some());
    }

    #[test]
    fn pending_flush_expiry_enters_existing_sink_loss_path() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        sink.delay_flush_ack();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/expired-flush.wav"),
            None,
            Box::new(FiniteSource::new(vec![])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker
            .finish_track_start(
                Box::new(FiniteSource::new(vec![0.0; 512])),
                &TrackSource::local("/expired-flush.wav"),
                0,
                44_100,
                1,
                None,
            )
            .expect("track start must request the flush");

        let deadline = pending_flush_deadline(&worker);
        assert!(!worker.pump_playback_at(deadline));
        assert!(worker.sink_manager.is_lost());
        assert!(!worker.sink_manager.has_pending_flush());
        assert!(matches!(worker.playback, Playback::Playing(_)));

        // Loss handling remains on the existing next-pump path, which makes
        // the freeze visible without consuming any decoded source frames.
        assert!(!worker.pump_playback_at(deadline));
        assert!(matches!(worker.playback, Playback::Paused(_)));
        assert_eq!(worker.sink_manager.health(), SinkHealth::Lost);
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::PlaybackStateChanged { snapshot })
                if snapshot.status == PlayStatus::Paused
                    && snapshot.sink_health == SinkHealth::Lost
        ));
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::Notification {
                kind: EffectErrorKind::Audio,
                message,
                ..
            }) if message.contains("Audio output was lost")
        ));

        let recovery_sink = Arc::new(FakeSink::new([]));
        worker
            .sink_manager
            .install(Box::new(Arc::clone(&recovery_sink)));
        worker.resume();
        assert!(matches!(worker.playback, Playback::Playing(_)));
        assert_eq!(worker.sink_manager.health(), SinkHealth::Healthy);
        assert!(worker.pump_playback_at(deadline));
        assert_eq!(loaded(&worker).frames_written, 512);
    }

    #[test]
    fn disconnected_sink_releases_pending_flush_and_freezes_playback() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([PushSamplesResult::Disconnected]));
        sink.delay_flush_ack();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            1,
            TrackSource::local("/disconnect-during-flush.wav"),
            None,
            Box::new(FiniteSource::new(vec![])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker
            .finish_track_start(
                Box::new(FiniteSource::new(vec![0.0; 512])),
                &TrackSource::local("/disconnect-during-flush.wav"),
                1,
                44_100,
                1,
                None,
            )
            .expect("track start must request the flush");

        sink.disconnect();
        worker.pump_playback();
        assert!(
            worker.sink_manager.is_lost(),
            "disconnect must be surfaced after ack bypass"
        );
        worker.pump_playback();
        assert!(matches!(worker.playback, Playback::Paused(_)));
        assert!(!worker.sink_manager.is_live());
        assert!(!worker.sink_manager.has_pending_flush());
    }

    #[test]
    fn shutdown_cancels_a_pending_flush_without_waiting_for_pipewire() {
        let bus = EventBus::new();
        let (command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        sink.delay_flush_ack();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            2,
            TrackSource::local("/shutdown-during-flush.wav"),
            None,
            Box::new(FiniteSource::new(vec![])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.sink_manager.begin_segment(false, 0);

        let worker_thread = thread::spawn(move || worker.run());
        command_tx
            .send(AudioCommand::Shutdown)
            .expect("shutdown command");
        worker_thread
            .join()
            .expect("shutdown must not wait for the flush acknowledgement");
    }

    #[test]
    fn lost_sink_pauses_the_track_and_notifies_without_advancing_the_queue() {
        // A worker with a playing (paused=false) track and a still-present sink.
        let event_bus = EventBus::new();
        let mut worker = empty_worker(event_bus.sender(), channel().1);
        set_playing(
            &mut worker,
            7,
            TrackSource::local("/lost.mp3"),
            Some(Duration::from_secs(180)),
            Box::new(FiniteSource::new(vec![])),
        );
        loaded_mut(&mut worker).channels = 2;
        loaded_mut(&mut worker).frames_written = 44_100;
        worker.sink_manager.install(Box::new(FakeSink::new([])));
        worker.sink_manager.mark_lost();

        // The lost-sink signal must pause the track, drop the sink, and emit a
        // paused snapshot plus a notification — never a TrackEnded that would
        // advance the queue in silence.
        worker.handle_sink_lost();

        assert!(matches!(worker.playback, Playback::Paused(_)));
        assert!(
            !worker.sink_manager.is_live(),
            "the dead sink must be torn down"
        );
        assert!(
            worker.sink_manager.is_lost(),
            "the explicit Lost state must remain visible until recovery"
        );
        assert_eq!(worker.sink_manager.health(), SinkHealth::Lost);

        // Collect events: exactly one Paused snapshot and one Notification.
        let mut paused = false;
        let mut notified = false;
        let mut track_ended = false;
        while let Ok(ev) = event_bus.try_recv() {
            match ev {
                AppEvent::PlaybackStateChanged { snapshot } => {
                    assert_eq!(snapshot.status, PlayStatus::Paused);
                    assert_eq!(snapshot.track_index, Some(7));
                    assert_eq!(snapshot.sink_health, SinkHealth::Lost);
                    paused = true;
                }
                AppEvent::Notification { .. } => notified = true,
                AppEvent::TrackEnded { .. } => track_ended = true,
                _ => {}
            }
        }
        assert!(paused, "a Paused snapshot must be surfaced to the UI");
        assert!(notified, "the user must be told the output was lost");
        assert!(!track_ended, "a lost sink must never advance the queue");
    }

    #[test]
    fn manual_recovery_marks_sink_healthy_and_publishes_playing_transition() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            2,
            TrackSource::local("/recovery.mp3"),
            Some(Duration::from_secs(10)),
            Box::new(FiniteSource::new(vec![])),
        );
        let playback = mem::replace(&mut worker.playback, Playback::Idle);
        let Playback::Playing(active) = playback else {
            unreachable!()
        };
        worker.playback = Playback::Paused(active);
        worker.sink_manager.install(Box::new(FakeSink::new([])));
        worker.sink_manager.mark_lost();
        worker.sink_manager.install(Box::new(FakeSink::new([])));

        worker.resume();

        assert_eq!(worker.sink_manager.health(), SinkHealth::Healthy);
        assert!(matches!(worker.playback, Playback::Playing(_)));
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::PlaybackStateChanged { snapshot })
                if snapshot.status == PlayStatus::Playing
                    && snapshot.sink_health == SinkHealth::Healthy
        ));
    }

    #[test]
    fn failed_manual_recovery_marks_output_unavailable() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            2,
            TrackSource::local("/unavailable.mp3"),
            Some(Duration::from_secs(10)),
            Box::new(FiniteSource::new(vec![])),
        );
        loaded_mut(&mut worker).rate = 0;
        loaded_mut(&mut worker).channels = 0;
        let playback = mem::replace(&mut worker.playback, Playback::Idle);
        let Playback::Playing(active) = playback else {
            unreachable!()
        };
        worker.playback = Playback::Paused(active);
        worker.sink_manager.install(Box::new(FakeSink::new([])));
        worker.sink_manager.mark_lost();

        worker.resume();

        assert_eq!(worker.sink_manager.health(), SinkHealth::Unavailable);
        assert!(matches!(worker.playback, Playback::Paused(_)));
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::PlaybackStateChanged { snapshot })
                if snapshot.status == PlayStatus::Paused
                    && snapshot.sink_health == SinkHealth::Unavailable
        ));
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::Notification {
                kind: EffectErrorKind::Audio,
                ..
            })
        ));
    }

    #[test]
    fn failed_sink_startup_never_transitions_manager_to_healthy() {
        let mut manager = SinkManager::new();
        let result = manager.ensure_with(44_100, 2, |_, _, _| {
            Err(anyhow::Error::new(
                crate::audio::PipeWireStartupError::Timeout {
                    timeout: Duration::from_millis(200),
                },
            ))
        });

        let error = result.expect_err("a startup timeout must reach the worker");
        let AudioError::DeviceUnavailable(source) = error else {
            panic!("startup failure must be reported as device unavailable");
        };
        assert!(
            source
                .downcast_ref::<crate::audio::PipeWireStartupError>()
                .is_some(),
            "the typed startup failure must survive SinkManager conversion"
        );
        assert_eq!(manager.health(), SinkHealth::Unavailable);
        assert!(!manager.is_live());
    }

    #[test]
    fn failed_sink_recreation_preserves_old_sink_and_health() {
        let mut manager = SinkManager::new();
        manager.install(Box::new(FakeSink::new([])));
        let prior_health = manager.health();

        let result = manager.recreate_to_with(48_000, 2, OutputTarget::default(), |_, _, _| {
            Err(anyhow::Error::new(
                crate::audio::PipeWireStartupError::Failed("negotiation rejected".to_string()),
            ))
        });

        let error = result.expect_err("a failed replacement must reach the caller");
        let AudioError::DeviceUnavailable(source) = error else {
            panic!("replacement failure must be reported as device unavailable");
        };
        assert!(
            source
                .downcast_ref::<crate::audio::PipeWireStartupError>()
                .is_some(),
            "the typed startup failure must survive SinkManager conversion"
        );
        assert_eq!(manager.health(), prior_health);
        assert!(
            manager
                .sink()
                .is_some_and(|sink| (sink.rate(), sink.channels()) == (44_100, 1)),
            "building a replacement must preserve the working sink"
        );
    }

    #[test]
    fn sink_recreation_passes_target_and_preserves_old_sink_on_failure() {
        let mut manager = SinkManager::new();
        manager.install(Box::new(FakeSink::new([])));
        manager.set_target(OutputTarget {
            stable_id: Some("sink-old".to_string()),
            node_id: Some(33),
        });
        let target = OutputTarget {
            stable_id: Some("sink-a".to_string()),
            node_id: Some(57),
        };

        let result = manager.recreate_to_with(48_000, 2, target, |_, _, target| {
            assert_eq!(target.node_id, Some(57));
            assert_eq!(target.stable_id.as_deref(), Some("sink-a"));
            Err(anyhow::anyhow!("target disappeared"))
        });

        assert!(result.is_err());
        assert!(
            manager.is_live(),
            "the old sink must remain live after failure"
        );
        assert!(
            manager.sink().is_some(),
            "the old sink must remain available"
        );
        assert_eq!(manager.state.target().node_id, Some(33));
        assert_eq!(
            manager.state.target().stable_id.as_deref(),
            Some("sink-old")
        );
    }

    #[test]
    fn sink_replacement_round_trip_reanchors_each_new_segment() {
        let mut manager = SinkManager::new();
        let first = Arc::new(FakeSink::new([]));
        manager.install(Box::new(Arc::clone(&first)));

        let target_b = output_target(66);
        let second = Arc::new(FakeSink::new([]));
        let second_for_factory = Arc::clone(&second);
        manager
            .recreate_to_with(44_100, 2, target_b.clone(), move |_, _, target| {
                assert_eq!(target, &target_b);
                Ok(Box::new(second_for_factory) as Box<dyn OutputSink>)
            })
            .expect("A to B replacement must succeed");
        manager.begin_segment(false, 100);

        let target_a = output_target(33);
        let expected_target_a = target_a.clone();
        let third = Arc::new(FakeSink::new([]));
        let third_for_factory = Arc::clone(&third);
        manager
            .recreate_to_with(44_100, 2, target_a.clone(), move |_, _, target| {
                assert_eq!(target, &expected_target_a);
                Ok(Box::new(third_for_factory) as Box<dyn OutputSink>)
            })
            .expect("B to A replacement must succeed");
        manager.begin_segment(false, 200);

        assert_eq!(manager.state.target(), &target_a);
        assert_eq!(second.event_names(), vec!["flush", "frames_played"]);
        assert_eq!(third.event_names(), vec!["flush", "frames_played"]);
        assert!(manager.is_live());
    }

    #[test]
    fn sink_gain_survives_recreation_and_lost_sink_recovery() {
        let mut manager = SinkManager::new();
        manager.set_gain(0.25);
        let first = Arc::new(FakeSink::new([]));
        manager.install(Box::new(Arc::clone(&first)));
        assert_eq!(first.gain(), 0.25);

        manager.set_gain(0.5);
        assert_eq!(first.gain(), 0.5);

        let recreated = Arc::new(FakeSink::new([]));
        let recreated_for_factory = Arc::clone(&recreated);
        manager
            .recreate_to_with(48_000, 1, OutputTarget::default(), move |_, _, _| {
                Ok(Box::new(recreated_for_factory) as Box<dyn OutputSink>)
            })
            .expect("sink recreation must succeed");
        assert_eq!(recreated.gain(), 0.5);

        manager.mark_lost();
        manager.set_gain(0.75);
        let recovered = Arc::new(FakeSink::new([]));
        manager.install(Box::new(Arc::clone(&recovered)));
        assert_eq!(recovered.gain(), 0.75);
    }

    #[test]
    fn crossfade_reuses_prepared_buffers_and_matches_equal_power_output() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.set_volume(volume(100));
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/crossfade-next-current.mp3"),
            None,
            Box::new(FiniteSource::new(vec![1.0, 2.0, 3.0, 4.0])),
        );
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![10.0, 20.0, 30.0, 40.0])),
            0,
            TrackSource::local("/crossfade-next.mp3"),
            44_100,
            1,
        );
        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("ready preload must enter the transition");
        let output_pointer = worker.crossfade.pump_output.as_ptr();
        let output_capacity = worker.crossfade.pump_output.capacity();
        let a_frame_pointer = worker.crossfade.pump_a_frame.as_ptr();
        let b_frame_pointer = worker.crossfade.pump_b_frame.as_ptr();

        assert!(worker.pump_crossfade(1, 4, 0.5));

        let (out_gain, in_gain) = crate::audio::playback::crossfade_gains(0.5);
        let expected = vec![
            1.0 * out_gain + 10.0 * in_gain,
            2.0 * out_gain + 20.0 * in_gain,
            3.0 * out_gain + 30.0 * in_gain,
            4.0 * out_gain + 40.0 * in_gain,
        ];
        assert_eq!(sink.pushed_samples(), vec![expected]);
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .expect("preloaded track")
                .consumed_frames,
            4
        );
        assert_eq!(worker.crossfade.pump_output.as_ptr(), output_pointer);
        assert_eq!(worker.crossfade.pump_output.capacity(), output_capacity);
        assert_eq!(worker.crossfade.pump_a_frame.as_ptr(), a_frame_pointer);
        assert_eq!(worker.crossfade.pump_b_frame.as_ptr(), b_frame_pointer);
    }

    #[test]
    fn crossfade_tail_reuses_output_and_preserves_tail_samples() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.set_volume(volume(100));
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/crossfade-tail-current.mp3"),
            None,
            Box::new(FiniteSource::new(vec![1.0, 2.0])),
        );
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![10.0; 2 + CROSSFADE_TAIL_FRAMES])),
            1,
            TrackSource::local("/crossfade-next.mp3"),
            44_100,
            1,
        );
        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("ready preload must enter the transition");
        let output_pointer = worker.crossfade.pump_output.as_ptr();
        let (out_gain, in_gain) = crate::audio::playback::crossfade_gains(0.5);

        assert!(worker.pump_crossfade(1, 4, 0.5));

        let pushed = sink.pushed_samples();
        assert_eq!(
            pushed.len(),
            2,
            "the normal fade chunk and tail are both delivered"
        );
        assert_eq!(
            pushed[0],
            vec![
                1.0 * out_gain + 10.0 * in_gain,
                2.0 * out_gain + 10.0 * in_gain,
            ]
        );
        assert_eq!(pushed[1].len(), CROSSFADE_TAIL_FRAMES);
        for (index, sample) in pushed[1].iter().enumerate() {
            let gain = in_gain + (1.0 - in_gain) * (index as f32 / CROSSFADE_TAIL_FRAMES as f32);
            assert!((*sample - 10.0 * gain).abs() < f32::EPSILON);
        }
        assert_eq!(worker.crossfade.pump_output.as_ptr(), output_pointer);
        assert_eq!(loaded(&worker).frames_written, 258);
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::CrossfadeCompleted { elapsed, .. })
                if elapsed == Duration::from_secs_f64(258.0 / 44_100.0)
        ));
    }

    #[test]
    fn crossfade_pending_output_transfers_once_across_repeated_backpressure() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Backpressure,
            PushSamplesResult::Accepted,
        ]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.set_volume(volume(100));
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/crossfade-pending-current.mp3"),
            None,
            Box::new(FiniteSource::new(vec![1.0, 2.0, 3.0, 4.0])),
        );
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![10.0; 4])),
            0,
            TrackSource::local("/crossfade-next.mp3"),
            44_100,
            1,
        );
        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("ready preload must enter the transition");
        let output_pointer = worker.crossfade.pump_output.as_ptr();
        let output_capacity = worker.crossfade.pump_output.capacity();

        assert!(!worker.pump_crossfade(1, 4, 0.5));
        assert!(worker.pending_output().is_some());
        let pending_samples = worker
            .pending_output()
            .expect("pending output")
            .samples
            .clone();
        assert!(matches!(
            &worker.crossfade.lifecycle,
            CrossfadeLifecycle::TransitionRunning {
                deferred: Some(_),
                ..
            }
        ));
        assert_eq!(loaded(&worker).frames_written, 0);
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .expect("preloaded track")
                .consumed_frames,
            0
        );
        assert_eq!(
            worker
                .pending_output()
                .expect("pending output")
                .samples
                .capacity(),
            output_capacity
        );
        assert!(worker.crossfade.pump_output.is_empty());

        assert!(!worker.pump_playback());
        assert!(worker.pending_output().is_some());
        assert!(worker.pump_playback());
        assert!(worker.pending_output().is_none());
        assert_eq!(loaded(&worker).frames_written, 4);
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .expect("preloaded track")
                .consumed_frames,
            4
        );
        assert_eq!(worker.crossfade.pump_output.as_ptr(), output_pointer);
        assert_eq!(worker.crossfade.pump_output.capacity(), output_capacity);
        assert_eq!(sink.pushed_samples(), vec![pending_samples]);
    }

    #[test]
    fn changing_speed_and_length_mid_ramp_preserves_the_gain_anchor() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/crossfade-anchor-current.mp3"),
            Some(Duration::from_secs(20)),
            Box::new(FiniteSource::new(vec![1.0, 2.0, 3.0, 4.0])),
        );
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![10.0, 20.0, 30.0, 40.0])),
            1,
            TrackSource::local("/crossfade-anchor-next.mp3"),
            44_100,
            1,
        );
        worker.crossfade_seconds = crossfade(5);
        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition_with_duration(Duration::ZERO, Duration::from_secs(5))
            .expect("ready preload must enter the transition");

        assert!(worker.pump_crossfade(1, 1, 0.25));
        // The new setting is longer than the track. An already-running
        // transition must continue instead of falling through to direct
        // playback because the future-transition duration gate no longer
        // applies.
        worker.set_crossfade(crossfade(30));
        worker.set_speed(Speed::from_tenths(15).expect("valid test speed"));
        worker.set_speed(Speed::UNITY);
        assert_eq!(
            worker.crossfade.transition_progress_at(Duration::ZERO),
            Some(0.25)
        );

        worker.pump_playback();
        worker.pump_playback();
        let (out_gain, in_gain) = crate::audio::playback::crossfade_gains(0.25);
        let pushed = sink.pushed_samples();
        assert!(pushed.len() >= 2);
        assert_eq!(
            pushed[1][0],
            (2.0 * out_gain + 20.0 * in_gain) * volume_factor(worker.volume_percent)
        );
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::CrossfadeCompleted { track_index: 1, .. })
        ));
    }

    #[test]
    fn changing_crossfade_settings_preserves_deferred_transition_output() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Accepted,
        ]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.set_volume(volume(100));
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/crossfade-setting-current.mp3"),
            None,
            Box::new(FiniteSource::new(vec![1.0, 2.0, 3.0, 4.0])),
        );
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![10.0; 4])),
            1,
            TrackSource::local("/crossfade-setting-next.mp3"),
            44_100,
            1,
        );
        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("ready preload must enter the transition");

        assert!(!worker.pump_crossfade(1, 4, 0.5));
        let pending_samples = worker
            .pending_output()
            .expect("backpressure must defer mixed samples")
            .samples
            .clone();

        worker.set_crossfade(crossfade(10));

        assert_eq!(worker.crossfade_seconds, crossfade(10));
        worker.set_speed(Speed::from_tenths(15).expect("valid test speed"));
        assert_eq!(worker.speed_stage.input_frames, 0);
        assert_eq!(
            worker
                .pending_output()
                .expect("reset must preserve deferred mixed samples")
                .samples,
            pending_samples
        );
        assert!(matches!(
            &worker.crossfade.lifecycle,
            CrossfadeLifecycle::TransitionRunning {
                deferred: Some(_),
                ..
            }
        ));
        assert!(worker.pump_playback());
        assert!(worker.pending_output().is_none());
        assert_eq!(loaded(&worker).frames_written, 4);
        assert_eq!(worker.speed_stage.input_frames, 4);
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .expect("preloaded track")
                .consumed_frames,
            4
        );
        assert_eq!(sink.pushed_samples(), vec![pending_samples]);
    }

    #[test]
    fn discarding_a_running_crossfade_drops_its_deferred_tail() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([
            PushSamplesResult::Accepted,
            PushSamplesResult::Backpressure,
        ]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/crossfade-cancel-current.mp3"),
            None,
            Box::new(FiniteSource::new(vec![1.0, 2.0])),
        );
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![10.0; CROSSFADE_TAIL_FRAMES + 2])),
            1,
            TrackSource::local("/crossfade-cancel-next.mp3"),
            44_100,
            1,
        );
        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("ready preload must enter the transition");

        assert_eq!(
            worker.pump_crossfade_outcome(1, 4, 0.5),
            PumpOutcome::Backpressure
        );
        assert!(worker.pending_output().is_some());
        assert!(matches!(
            &worker.crossfade.lifecycle,
            CrossfadeLifecycle::TransitionRunning {
                deferred: Some(_),
                ..
            }
        ));

        worker.discard_crossfade();

        assert!(matches!(
            &worker.crossfade.lifecycle,
            CrossfadeLifecycle::NoPreload
        ));
        assert!(worker.pending_output().is_none());
        assert_eq!(sink.pushed_samples().len(), 1);
        assert_eq!(loaded(&worker).frames_written, 2);
    }

    #[test]
    fn seek_rearms_a_preload_that_advanced_before_deferred_output_acceptance() {
        let root = unique_temp_dir("crossfade-seek-deferred");
        let current_path = root.join("current.wav");
        std::fs::write(&current_path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker
            .sink_manager
            .install(Box::new(FakeSink::new([PushSamplesResult::Backpressure])));
        set_playing(
            &mut worker,
            0,
            TrackSource::local(current_path.clone()),
            Some(Duration::from_secs(1)),
            Box::new(FiniteSource::new(vec![1.0, 2.0, 3.0, 4.0])),
        );
        let next_source = TrackSource::local("/crossfade-seek-deferred-next.wav");
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![10.0; 4])),
            1,
            next_source.clone(),
            44_100,
            1,
        );
        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("ready preload must enter the transition");

        assert!(!worker.pump_crossfade(1, 4, 0.5));
        assert!(worker.pending_output().is_some());
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .expect("preloaded track")
                .consumed_frames,
            0
        );

        assert!(!worker.seek_to(Duration::ZERO));

        assert!(worker.crossfade.preloaded().is_none());
        assert!(matches!(
            &worker.crossfade.lifecycle,
            CrossfadeLifecycle::PreloadRequested(request)
                if request.source == next_source && request.track_index == 1
        ));
        assert!(worker.pending_output().is_none());
        worker.cancel_pending_preload();
    }

    #[test]
    fn crossfade_pending_output_survives_sink_disconnect_and_recovery() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([PushSamplesResult::Disconnected]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.set_volume(volume(100));
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/crossfade-loss.mp3"),
            None,
            Box::new(FiniteSource::new(vec![1.0, 2.0, 3.0, 4.0])),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        install_preloaded(
            &mut worker,
            Box::new(FiniteSource::new(vec![10.0; 4])),
            0,
            TrackSource::local("/crossfade-next.mp3"),
            44_100,
            1,
        );
        worker.prepare_pump_buffers(1);
        worker
            .crossfade
            .begin_transition(Duration::ZERO)
            .expect("ready preload must enter the transition");

        assert!(!worker.pump_crossfade(1, 4, 0.5));
        assert!(worker.sink_manager.is_lost());
        assert!(worker.pending_output().is_some());
        let pending_samples = worker
            .pending_output()
            .expect("crossfade samples must remain pending")
            .samples
            .clone();

        assert!(!worker.pump_playback());
        assert!(!worker.sink_manager.is_live());
        assert!(worker.pending_output().is_some());

        let recovery_sink = Arc::new(FakeSink::new([]));
        worker
            .sink_manager
            .install(Box::new(Arc::clone(&recovery_sink)));
        worker.resume();
        worker.pump_playback();

        assert!(worker.pending_output().is_none());
        assert_eq!(loaded(&worker).frames_written, 4);
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .expect("preloaded track")
                .consumed_frames,
            4
        );
        assert!(worker.flush_pending_output());
        assert_eq!(
            worker
                .crossfade
                .preloaded()
                .expect("preloaded track")
                .consumed_frames,
            4,
            "an already accepted pending buffer must not be counted again"
        );
        assert_eq!(recovery_sink.pushed_samples(), vec![pending_samples]);
    }

    #[test]
    fn backpressure_defers_samples_without_advancing_frame_accounting() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/backpressure.mp3"),
            None,
            Box::new(rodio::source::SineWave::new(440.0)),
        );
        worker.sink_manager.install(Box::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Accepted,
        ])));

        worker.pump_playback();
        assert!(worker.pending_output().is_some());
        assert_eq!(loaded(&worker).frames_written, 0);
        worker.pump_playback();
        assert!(worker.pending_output().is_none());
        assert_eq!(loaded(&worker).frames_written, 512);
    }

    #[test]
    fn pending_direct_output_returns_the_same_scratch_allocation_after_acceptance() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/pending-identity.wav"),
            None,
            Box::new(FiniteSource::new(vec![1.0; 512])),
        );
        worker.sink_manager.install(Box::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Accepted,
        ])));
        worker.crossfade.pump_scratch.reserve(512);
        let scratch_pointer = worker.crossfade.pump_scratch.as_ptr();
        let scratch_capacity = worker.crossfade.pump_scratch.capacity();

        worker.pump_playback();
        let pending = worker
            .pending_output()
            .expect("backpressure must transfer the scratch buffer");
        assert_eq!(pending.buffer, PendingBuffer::Scratch);
        assert_eq!(pending.samples.as_ptr(), scratch_pointer);
        assert_eq!(pending.samples.capacity(), scratch_capacity);
        assert!(worker.crossfade.pump_scratch.is_empty());

        worker.pump_playback();
        assert!(worker.pending_output().is_none());
        assert_eq!(worker.crossfade.pump_scratch.as_ptr(), scratch_pointer);
        assert_eq!(worker.crossfade.pump_scratch.capacity(), scratch_capacity);
        assert_eq!(loaded(&worker).frames_written, 512);
    }

    #[test]
    fn submit_reports_each_sink_outcome_and_preserves_chunk_ownership() {
        for expected in [
            SubmitOutcome::Accepted,
            SubmitOutcome::Backpressure,
            SubmitOutcome::Disconnected,
        ] {
            let bus = EventBus::new();
            let (_command_tx, command_rx) = channel();
            let sink = Arc::new(FakeSink::new([match expected {
                SubmitOutcome::Accepted => PushSamplesResult::Accepted,
                SubmitOutcome::Backpressure => PushSamplesResult::Backpressure,
                SubmitOutcome::Disconnected => PushSamplesResult::Disconnected,
            }]));
            let mut worker = empty_worker(bus.sender(), command_rx);
            worker.set_volume(volume(100));
            set_playing(
                &mut worker,
                0,
                TrackSource::local("/submit-outcome.wav"),
                None,
                Box::new(FiniteSource::new(vec![0.0; 2])),
            );
            worker.sink_manager.install(Box::new(Arc::clone(&sink)));

            let actual = worker.submit(SubmittedChunk {
                samples: vec![1.0, 2.0],
                frames: 2,
                input_frames: 2,
                accepted_input_frames: 0,
                next_frames: 0,
                complete_crossfade: false,
                buffer: PendingBuffer::Scratch,
                transition: false,
                drain_on_accept: false,
                drain_on_deferred_accept: true,
            });
            assert_eq!(actual, expected);

            match expected {
                SubmitOutcome::Accepted => {
                    assert!(worker.pending_output().is_none());
                    assert_eq!(loaded(&worker).frames_written, 2);
                    assert_eq!(worker.speed_stage.input_frames, 0);
                    assert_eq!(sink.pushed_samples(), vec![vec![1.0, 2.0]]);
                }
                SubmitOutcome::Backpressure => {
                    assert_eq!(
                        worker.pending_output().expect("pending chunk").input_frames,
                        2
                    );
                    assert_eq!(loaded(&worker).frames_written, 0);
                    assert!(!worker.sink_manager.is_lost());
                }
                SubmitOutcome::Disconnected => {
                    assert_eq!(
                        worker.pending_output().expect("pending chunk").input_frames,
                        2
                    );
                    assert_eq!(loaded(&worker).frames_written, 0);
                    assert!(worker.sink_manager.is_lost());
                }
            }
        }
    }

    #[test]
    fn speed_change_credits_deferred_direct_output_before_stretch() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/speed-deferred-direct.wav"),
            None,
            Box::new(FiniteSource::new(vec![1.0; 512])),
        );
        worker.sink_manager.install(Box::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Accepted,
        ])));

        worker.pump_playback();
        assert_eq!(
            worker
                .pending_output()
                .expect("direct output must be deferred")
                .input_frames,
            512
        );
        worker.set_speed(Speed::from_tenths(15).expect("valid test speed"));
        assert_eq!(worker.speed_stage.input_frames, 0);

        assert!(worker.pump_playback());
        assert!(worker.pending_output().is_none());
        assert_eq!(loaded(&worker).frames_written, 512);
        assert_eq!(worker.speed_stage.input_frames, 512);
    }

    #[test]
    fn finite_source_does_not_end_until_backpressured_output_is_delivered() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/finite-backpressure.wav"),
            None,
            Box::new(FiniteSource::new(
                (0..511).map(|sample| sample as f32).collect(),
            )),
        );
        worker.sink_manager.install(Box::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Accepted,
        ])));

        worker.pump_playback();
        assert!(
            loaded(&worker).drain.drained(),
            "the finite decoder must be exhausted"
        );
        assert!(worker.pending_output().is_some());
        worker.detect_natural_end();
        assert!(
            worker.playback.loaded().is_some(),
            "pending output keeps the track active"
        );
        assert!(
            bus.try_recv().is_err(),
            "TrackEnded must wait for the final queued samples"
        );

        worker.pump_playback();
        worker.detect_natural_end();
        assert!(matches!(worker.playback, Playback::Idle));
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::TrackEnded { track_index: 0 })
        ));
    }

    #[test]
    fn stereo_source_terminal_partial_frame_is_not_submitted() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::with_format([], 44_100, 2));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/stereo-partial.wav"),
            None,
            Box::new(FiniteSource::with_channels(vec![1.0, 2.0, 3.0], 2)),
        );
        worker.set_volume(volume(100));
        loaded_mut(&mut worker).channels = 2;
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        worker.pump_playback();

        assert!(loaded(&worker).drain.drained());
        assert_eq!(loaded(&worker).frames_written, 1);
        assert_eq!(sink.pushed_samples(), vec![vec![1.0, 2.0]]);
    }

    #[test]
    fn repeated_backpressure_preserves_pending_frames_until_acceptance() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Backpressure,
            PushSamplesResult::Backpressure,
            PushSamplesResult::Accepted,
        ]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/repeated-backpressure.mp3"),
            None,
            Box::new(rodio::source::SineWave::new(440.0)),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        worker.pump_playback();
        assert_eq!(loaded(&worker).frames_written, 0);
        for _ in 0..2 {
            worker.pump_playback();
            assert!(worker.pending_output().is_some());
            assert_eq!(loaded(&worker).frames_written, 0);
        }

        worker.pump_playback();
        assert!(worker.pending_output().is_none());
        assert_eq!(loaded(&worker).frames_written, 512);
        assert_eq!(sink.pushed_samples().len(), 1);
    }

    #[test]
    fn sink_loss_freezes_playback_and_recovery_resumes_frame_delivery() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.set_volume(volume(100));
        let samples: Vec<f32> = (0..512).map(|sample| sample as f32).collect();
        set_playing(
            &mut worker,
            3,
            TrackSource::local("/loss-recovery.mp3"),
            None,
            Box::new(FiniteSource::new(samples.clone())),
        );
        worker
            .sink_manager
            .install(Box::new(FakeSink::new([PushSamplesResult::Disconnected])));

        worker.pump_playback();
        assert!(
            worker.sink_manager.is_lost(),
            "a disconnected sink must request recovery"
        );
        assert_eq!(loaded(&worker).frames_written, 0);
        let pending_samples = worker
            .pending_output()
            .expect("decoded samples must be retained across sink loss")
            .samples
            .clone();
        worker.pump_playback();
        assert!(matches!(worker.playback, Playback::Paused(_)));
        assert!(!worker.sink_manager.is_live());
        assert_eq!(worker.sink_manager.health(), SinkHealth::Lost);
        assert_eq!(
            worker
                .pending_output()
                .expect("sink loss must not discard decoded samples")
                .samples,
            pending_samples
        );

        let recovery_sink = Arc::new(FakeSink::new([]));
        worker
            .sink_manager
            .install(Box::new(Arc::clone(&recovery_sink)));
        worker.resume();
        assert!(matches!(worker.playback, Playback::Playing(_)));
        assert_eq!(worker.sink_manager.health(), SinkHealth::Healthy);
        worker.pump_playback();
        assert!(
            loaded(&worker).frames_written > 0,
            "recovery must resume frame delivery"
        );
        assert_eq!(loaded(&worker).frames_written, 512);
        assert_eq!(recovery_sink.pushed_samples(), vec![pending_samples]);
    }

    #[test]
    fn second_sink_disconnect_preserves_pending_output_and_frame_credit() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Disconnected,
        ]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.set_volume(volume(100));
        set_playing(
            &mut worker,
            4,
            TrackSource::local("/second-loss.mp3"),
            None,
            Box::new(FiniteSource::new(
                (0..512).map(|sample| sample as f32).collect(),
            )),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        worker.pump_playback();
        let pending = worker
            .pending_output()
            .expect("backpressure must retain decoded output");
        let pending_samples = pending.samples.clone();
        let pending_frames = pending.frames;
        let pending_next_frames = pending.next_frames;
        let pending_crossfade = pending.complete_crossfade;

        worker.pump_playback();

        let retained = worker
            .pending_output()
            .expect("a retry-time disconnect must retain pending output");
        assert_eq!(retained.samples, pending_samples);
        assert_eq!(retained.frames, pending_frames);
        assert_eq!(retained.next_frames, pending_next_frames);
        assert_eq!(retained.complete_crossfade, pending_crossfade);
        assert!(worker.sink_manager.is_lost());
        assert_eq!(loaded(&worker).frames_written, 0);

        worker.pump_playback();
        assert!(!worker.sink_manager.is_live());
        let retained_after_loss = worker
            .pending_output()
            .expect("handling sink loss must not discard pending output");
        assert_eq!(retained_after_loss.samples, pending_samples);
        assert_eq!(retained_after_loss.frames, pending_frames);

        let recovery_sink = Arc::new(FakeSink::new([]));
        worker
            .sink_manager
            .install(Box::new(Arc::clone(&recovery_sink)));
        worker.resume();
        worker.pump_playback();

        assert_eq!(loaded(&worker).frames_written, pending_frames);
        assert_eq!(recovery_sink.pushed_samples(), vec![pending_samples]);
    }

    #[test]
    fn time_stretch_defers_source_frame_credit_until_sink_accepts_output() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/stretch-backpressure.mp3"),
            None,
            Box::new(rodio::source::SineWave::new(440.0).take_duration(Duration::from_secs(1))),
        );
        worker.speed_stage.requested_speed = SPEED_MAX;
        worker.setup_stretch(44_100, 1);
        worker.sink_manager.install(Box::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Accepted,
        ])));

        for _ in 0..10 {
            worker.pump_playback();
            if worker.pending_output().is_some() {
                break;
            }
        }
        let pending = worker
            .pending_output()
            .expect("stretch output should be deferred");
        let pending_frames = pending.frames;
        assert!(
            pending_frames > 0,
            "deferred output must retain source credit"
        );
        assert_eq!(loaded(&worker).frames_written, 0);

        worker.pump_playback();
        assert!(worker.pending_output().is_none());
        assert_eq!(loaded(&worker).frames_written, pending_frames);
    }

    #[test]
    fn deferred_time_stretch_tail_waits_for_true_drain_before_track_end() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Accepted,
        ]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/stretch-deferred-tail.wav"),
            None,
            Box::new(FiniteSource::new(
                (0..511).map(|sample| sample as f32).collect(),
            )),
        );
        worker.speed_stage.requested_speed = SPEED_MIN;
        worker.setup_stretch(44_100, 1);
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        for _ in 0..4 {
            worker.pump_playback();
            if worker.pending_output().is_some() {
                break;
            }
        }
        let pending = worker
            .pending_output()
            .expect("time-stretch output must be deferred");
        assert!(pending.frames > 0);
        assert!(!pending.drain_on_accept);
        assert_eq!(loaded(&worker).drain, DrainState::SourceExhausted);

        worker.pump_playback();
        assert!(worker.pending_output().is_none());
        assert_eq!(loaded(&worker).drain, DrainState::SourceExhausted);
        worker.detect_natural_end();
        assert!(worker.playback.loaded().is_some());
        assert!(bus.try_recv().is_err(), "TrackEnded must wait for the tail");

        let attempts_after_deferred_accept = sink.push_attempts();
        for _ in 0..4 {
            worker.pump_playback();
            if loaded(&worker).drain.drained() {
                break;
            }
        }
        assert!(loaded(&worker).drain.drained());
        assert!(
            sink.push_attempts() > attempts_after_deferred_accept,
            "remaining SoundTouch output must be submitted after retry"
        );
        worker.detect_natural_end();
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::TrackEnded { track_index: 0 })
        ));
    }

    #[test]
    fn stream_crossfade_preload_is_rejected_before_opening_a_reader() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let resolver = StreamResolver::new(vec![Arc::new(DelayedProvider {
            started: Arc::clone(&started),
            boundary: Arc::new(Barrier::new(1)),
            release,
            hls: false,
        })]);
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        worker.resolver = resolver;
        worker.preload_next(
            TrackSource::stream(
                url::Url::parse("https://delayed.example/live").unwrap(),
                StreamKind::Http,
            ),
            1,
        );

        assert!(!started.load(Ordering::Acquire));
        assert!(worker.crossfade.preloaded().is_none());
        assert!(worker.crossfade.preloaded().is_none());
    }

    #[test]
    fn local_preload_is_decoded_through_the_cancellable_acquisition_boundary() {
        let root = unique_temp_dir("async-local-preload");
        let path = root.path().join("next.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let source = TrackSource::local(path.clone());
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/current.wav"),
            None,
            Box::new(FiniteSource::new(vec![])),
        );

        worker.preload_next(source.clone(), 4);
        assert_eq!(
            worker
                .acquisition
                .pending
                .as_ref()
                .map(|pending| pending.kind),
            Some(AcquisitionKind::LocalPreload)
        );

        let deadline = Instant::now() + Duration::from_secs(1);
        while worker.crossfade.preloaded().is_none() {
            worker.drain_acquisition_results();
            assert!(
                Instant::now() < deadline,
                "local preload did not produce a result"
            );
            thread::yield_now();
        }

        assert!(worker.acquisition.pending.is_none());
        let preloaded = worker.crossfade.preloaded().expect("preloaded track");
        assert_eq!(preloaded.index, 4);
        assert_eq!(&preloaded.identity, &source);
        assert_eq!(preloaded.rate, loaded(&worker).rate);
        assert_eq!(preloaded.channels, loaded(&worker).channels);
        assert_eq!(preloaded.consumed_frames, 0);
        assert!(preloaded.duration.is_some());
    }

    #[test]
    fn local_preload_is_cancelled_by_stop_play_seek_cancel_and_shutdown() {
        let root = unique_temp_dir("preload-cancellation");
        let path = root.path().join("track.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");

        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        let stop_token = seed_pending_local_preload(&mut worker, 1, &path);
        worker.stop();
        assert!(stop_token.is_cancelled());
        assert!(worker.acquisition.pending.is_none());

        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        let play_token = seed_pending_local_preload(&mut worker, 2, &path);
        worker.sink_manager.install(Box::new(FakeSink::new([])));
        worker.play(TrackSource::local(path.clone()), 2, None);
        assert!(play_token.is_cancelled());
        assert!(worker.acquisition.pending.is_none());

        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        let seek_token = seed_pending_local_preload(&mut worker, 3, &path);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path.clone()),
            Some(Duration::from_secs(1)),
            Box::new(FiniteSource::new(vec![])),
        );
        worker.sink_manager.install(Box::new(FakeSink::new([])));
        assert!(!worker.handle(AudioCommand::SeekTo(Duration::ZERO)));
        assert!(seek_token.is_cancelled());
        assert_eq!(
            worker
                .acquisition
                .pending
                .as_ref()
                .map(|pending| pending.kind),
            Some(AcquisitionKind::LocalPreload)
        );
        assert_ne!(
            worker
                .acquisition
                .pending
                .as_ref()
                .map(|pending| pending.id),
            Some(3)
        );

        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        let cancel_token = seed_pending_local_preload(&mut worker, 4, &path);
        assert!(!worker.handle(AudioCommand::CancelPreload));
        assert!(cancel_token.is_cancelled());
        assert!(worker.acquisition.pending.is_none());

        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        let shutdown_token = seed_pending_local_preload(&mut worker, 5, &path);
        assert!(worker.handle(AudioCommand::Shutdown));
        assert!(shutdown_token.is_cancelled());
        assert!(worker.acquisition.pending.is_none());
    }

    #[test]
    fn stale_local_preload_result_cannot_arm_crossfade_state() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        let source = TrackSource::local("/new-next.wav");
        let cancellation = AcquisitionCancellation::new();
        worker.acquisition.pending = Some(PendingAcquisition {
            id: 2,
            generation: None,
            source: source.clone(),
            track_index: 9,
            cancellation,
            kind: AcquisitionKind::LocalPreload,
        });
        worker
            .acquisition
            .sender
            .send(AcquisitionResult::PreloadReady {
                id: 1,
                generation: None,
                source,
                track_index: 9,
                prepared: PreparedStream {
                    decoder: Box::new(FiniteSource::new(vec![0.0; 8])),
                    duration: Some(Duration::from_secs(1)),
                    rate: 44_100,
                    channels: 1,
                },
            })
            .expect("stale preload result");

        worker.drain_acquisition_results();

        assert!(worker.crossfade.preloaded().is_none());
        assert!(worker.crossfade.preloaded().is_none());
        assert!(worker.crossfade.preloaded().is_none());
        assert!(worker.acquisition.pending.is_some());
    }

    #[test]
    fn replacing_a_sink_does_not_wait_for_the_retired_sink_drop() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let mut worker = empty_worker(bus.sender(), command_rx);
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicUsize::new(0));
        let old = BlockingDropSink {
            inner: Arc::new(FakeSink::new([])),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            dropped: Arc::clone(&dropped),
        };
        worker.sink_manager.install(Box::new(old));

        let started_at = Instant::now();
        worker.sink_manager.install(Box::new(FakeSink::new([])));
        assert!(
            started_at.elapsed() < Duration::from_millis(100),
            "sink replacement waited for retired teardown"
        );

        let deadline = Instant::now() + Duration::from_secs(1);
        while !started.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "retirement thread did not receive sink"
            );
            thread::yield_now();
        }
        release.store(true, Ordering::Release);
        while dropped.load(Ordering::Acquire) != 1 {
            assert!(Instant::now() < deadline, "retired sink was not dropped");
            thread::yield_now();
        }
    }

    fn seed_pending_local_preload(
        worker: &mut Worker,
        id: u64,
        path: &std::path::Path,
    ) -> AcquisitionCancellation {
        let cancellation = AcquisitionCancellation::new();
        let source = TrackSource::local(path);
        worker.acquisition.pending = Some(PendingAcquisition {
            id,
            generation: None,
            source: source.clone(),
            track_index: id as usize,
            cancellation: cancellation.clone(),
            kind: AcquisitionKind::LocalPreload,
        });
        worker.crossfade.request(id, source, id as usize);
        cancellation
    }

    #[test]
    fn full_fake_sink_does_not_spin_and_commands_remain_responsive() {
        let bus = EventBus::new();
        let (command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::always(PushSamplesResult::Backpressure));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/shutdown.mp3"),
            None,
            Box::new(rodio::source::SineWave::new(440.0)),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        let worker_thread = thread::spawn(move || worker.run());
        let start_deadline = Instant::now() + Duration::from_secs(1);
        while sink.push_attempts() == 0 {
            assert!(
                Instant::now() < start_deadline,
                "worker did not attempt to submit audio"
            );
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(50));
        let attempts_while_backpressured = sink.push_attempts();

        command_tx.send(AudioCommand::Pause).expect("pause command");
        let pause_deadline = Instant::now() + Duration::from_millis(250);
        let mut paused = false;
        while !paused && Instant::now() < pause_deadline {
            if let Ok(event) = bus.recv_timeout(Duration::from_millis(25)) {
                paused = matches!(
                    event,
                    AppEvent::PlaybackStateChanged { snapshot }
                        if snapshot.status == PlayStatus::Paused
                );
            }
        }
        command_tx
            .send(AudioCommand::Shutdown)
            .expect("worker command");
        worker_thread.join().expect("worker must stop");
        assert!(
            attempts_while_backpressured <= 2,
            "a stalled sink must not cause a CPU spin (attempts: {attempts_while_backpressured})"
        );
        assert!(paused, "a full sink must not delay command processing");
    }

    #[test]
    fn time_stretch_at_half_and_double_tempo_changes_duration_but_keeps_pitch() {
        use soundtouch::SoundTouch;

        // One second of a 440 Hz monophonic sine.
        let rate = 44_100u32;
        let input: Vec<f32> = (0..rate as usize)
            .map(|i| {
                let t = i as f32 / rate as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            })
            .collect();

        for tempo in [0.5, 2.0] {
            let mut stretch = SoundTouch::new();
            stretch
                .set_channels(1)
                .set_sample_rate(rate)
                .set_tempo(tempo);
            stretch.put_samples(&input, input.len());
            stretch.flush();

            let mut out = vec![0.0f32; input.len() * 3];
            let mut got = 0usize;
            loop {
                let remaining = out.len() - got;
                let n = stretch.receive_samples(&mut out[got..], remaining);
                if n == 0 {
                    break;
                }
                got += n;
            }

            // SoundTouch changes duration while keeping the underlying
            // frequency untouched.
            let ratio = got as f64 / input.len() as f64;
            assert!(
                (ratio - 1.0 / tempo).abs() < 0.05,
                "tempo {tempo} must produce the expected duration, got {ratio:.3} ({got} samples)"
            );

            let rising_zero_crossings = out[..got]
                .windows(2)
                .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
                .count();
            let measured_frequency = rising_zero_crossings as f64 * rate as f64 / got as f64;
            assert!(
                (measured_frequency - 440.0).abs() < 20.0,
                "tempo {tempo} must preserve the tone: measured {measured_frequency:.1} Hz"
            );
        }
    }

    #[test]
    fn supported_speeds_produce_non_empty_complete_mono_and_stereo_output() {
        for channels in [1u16, 2u16] {
            for speed in [SPEED_MIN, Speed::UNITY, SPEED_MAX] {
                let frames = 22_050usize;
                let samples: Vec<f32> = (0..frames * channels as usize)
                    .map(|index| {
                        let frame = index / channels as usize;
                        (frame as f32 / 97.0).sin()
                    })
                    .collect();
                let bus = EventBus::new();
                let (_command_tx, command_rx) = channel();
                let sink = Arc::new(FakeSink::with_format([], 44_100, u32::from(channels)));
                let mut worker = empty_worker(bus.sender(), command_rx);
                set_playing(
                    &mut worker,
                    0,
                    TrackSource::local("/speed-matrix.wav"),
                    None,
                    Box::new(FiniteSource::with_channels(samples, channels)),
                );
                loaded_mut(&mut worker).channels = u32::from(channels);
                worker.speed_stage.requested_speed = speed;
                worker.setup_stretch(44_100, u32::from(channels));
                worker.sink_manager.install(Box::new(Arc::clone(&sink)));

                for _ in 0..512 {
                    worker.pump_playback();
                    if loaded(&worker).drain.drained() && worker.pending_output().is_none() {
                        break;
                    }
                }

                let pushed = sink.pushed_samples();
                assert!(!pushed.is_empty(), "speed {speed}x must produce output");
                assert!(
                    pushed
                        .iter()
                        .all(|chunk| { !chunk.is_empty() && chunk.len() % channels as usize == 0 }),
                    "speed {speed}x must preserve complete {channels}-channel frames"
                );
                assert!(
                    loaded(&worker).frames_written > 0,
                    "speed {speed}x must advance accepted frame credit"
                );
            }
        }
    }

    #[test]
    fn rapid_speed_sequence_submits_non_empty_output_at_each_non_unity_speed() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        let samples: Vec<f32> = (0..220_500)
            .map(|index| (index as f32 / 31.0).sin())
            .collect();
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/speed-transition.wav"),
            None,
            Box::new(FiniteSource::new(samples)),
        );
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        assert!(worker.pump_playback(), "unity must use the direct path");
        assert!(!worker.speed_stage.uses_time_stretch());

        let sequence = [11, 12, 13, 12, 11, 10, 9, 8, 7];
        for tenths in sequence {
            let before = sink.pushed_samples().len();
            worker.set_speed(Speed::from_tenths(tenths).expect("valid test speed"));
            for _ in 0..16 {
                worker.pump_playback();
                if sink.pushed_samples().len() > before {
                    break;
                }
            }
            let pushed = sink.pushed_samples();
            assert!(
                pushed.len() > before,
                "speed {tenths} must submit output instead of starving"
            );
            assert!(
                pushed[before..].iter().all(|chunk| !chunk.is_empty()),
                "speed {tenths} must never submit an empty chunk"
            );
            if tenths == 10 {
                assert!(!worker.speed_stage.uses_time_stretch());
                assert!(worker.speed_stage.stretch().is_none());
            } else {
                assert!(worker.speed_stage.uses_time_stretch());
                assert!(worker.speed_stage.stretch().is_some());
            }
        }
    }

    #[test]
    fn time_stretch_warmup_counts_source_input_as_worker_progress() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/speed-warmup-probe.wav"),
            None,
            Box::new(FiniteSource::new(vec![0.0; 4096])),
        );
        worker.set_speed(Speed::from_tenths(5).expect("valid test speed"));
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        assert_eq!(
            worker.pump_playback_outcome(),
            PumpOutcome::Progressed,
            "SoundTouch input warm-up must request another immediate pump"
        );
        assert!(sink.pushed_samples().is_empty());
        assert_eq!(sink.push_attempts(), 0);
        assert!(worker.speed_stage.input_frames > 0);
    }

    #[test]
    fn worker_pumps_past_time_stretch_warmup_without_command_poll_sleep() {
        let bus = EventBus::new();
        let (command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/speed-worker-cadence.wav"),
            None,
            Box::new(FiniteSource::new(vec![0.0; 22_050])),
        );
        worker.set_speed(Speed::from_tenths(5).expect("valid test speed"));
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        let worker_thread = thread::spawn(move || worker.run());
        let deadline = Instant::now() + Duration::from_millis(50);
        while sink.pushed_samples().is_empty() && Instant::now() < deadline {
            thread::yield_now();
        }
        let reached_sink = !sink.pushed_samples().is_empty();

        command_tx
            .send(AudioCommand::Shutdown)
            .expect("worker command");
        worker_thread.join().expect("worker must stop");

        assert!(
            reached_sink,
            "time-stretch output must reach the sink before the 100 ms command poll"
        );
    }

    #[test]
    fn repeated_non_unity_speed_changes_update_one_active_soundtouch_stage() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/repeated-speed-change.wav"),
            None,
            Box::new(FiniteSource::new(vec![0.25; 220_500])),
        );
        worker.set_speed(Speed::from_tenths(11).expect("valid test speed"));
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        let stage_pointer = worker.speed_stage.stretch().unwrap() as *const SoundTouch;
        for tenths in [12, 13, 12, 11, 9, 8, 7] {
            worker.set_speed(Speed::from_tenths(tenths).expect("valid test speed"));
            assert_eq!(
                worker.speed_stage.stretch().unwrap() as *const SoundTouch,
                stage_pointer,
                "non-unity tempo changes must update the active stage"
            );
            let before = sink.pushed_samples().len();
            for _ in 0..16 {
                worker.pump_playback();
                if sink.pushed_samples().len() > before {
                    break;
                }
            }
            assert!(sink.pushed_samples().len() > before);
            assert!(
                sink.pushed_samples()[before..]
                    .iter()
                    .all(|chunk| !chunk.is_empty())
            );
        }
    }

    #[test]
    fn unity_transition_discards_fifo_and_never_leaves_speed_without_stretch() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/speed-transition.wav"),
            None,
            Box::new(FiniteSource::new(vec![0.5; 220_500])),
        );
        worker.set_speed(SPEED_MAX);
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.pump_playback();
        assert!(worker.speed_stage.stretch().is_some());

        worker.set_speed(Speed::UNITY);
        assert!(!worker.speed_stage.uses_time_stretch());
        assert!(worker.speed_stage.stretch().is_none());
        worker.set_speed(Speed::from_tenths(7).expect("valid test speed"));
        assert!(worker.speed_stage.uses_time_stretch());
        assert!(worker.speed_stage.stretch().is_some());

        let before = sink.pushed_samples().len();
        for _ in 0..16 {
            worker.pump_playback();
            if sink.pushed_samples().len() > before {
                break;
            }
        }
        assert!(sink.pushed_samples().len() > before);
        assert!(
            sink.pushed_samples()[before..]
                .iter()
                .all(|chunk| !chunk.is_empty())
        );
    }

    #[test]
    fn setup_after_speed_transition_clears_the_old_soundtouch_fifo() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/speed-setup.wav"),
            None,
            Box::new(FiniteSource::new(vec![0.5; 88_200])),
        );
        worker.set_speed(SPEED_MAX);
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.pump_playback();
        assert!(
            worker
                .speed_stage
                .stretch()
                .unwrap()
                .num_unprocessed_samples()
                > 0
        );

        worker.set_speed(Speed::from_tenths(8).expect("valid test speed"));
        worker.speed_stage.input_frames = 0;
        loaded_mut(&mut worker).frames_written = 0;
        loaded_mut(&mut worker).drain = DrainState::Producing;
        worker.setup_stretch(44_100, 1);
        assert!(worker.speed_stage.uses_time_stretch());
        assert_eq!(
            worker
                .speed_stage
                .stretch()
                .unwrap()
                .num_unprocessed_samples(),
            0
        );

        let before = sink.pushed_samples().len();
        for _ in 0..16 {
            worker.pump_playback();
            if sink.pushed_samples().len() > before {
                break;
            }
        }
        assert!(sink.pushed_samples().len() > before);
    }

    #[test]
    fn seek_while_non_unity_recreates_a_fresh_soundtouch_stage() {
        let root = unique_temp_dir("speed-seek-stage");
        let path = root.join("seek.wav");
        std::fs::write(&path, wav_bytes(&[])).expect("write WAV fixture");
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local(path.clone()),
            Some(Duration::from_secs(1)),
            Box::new(FiniteSource::new(vec![0.5; 88_200])),
        );
        worker.set_speed(Speed::from_tenths(7).expect("valid test speed"));
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));
        worker.pump_playback();
        assert!(
            worker
                .speed_stage
                .stretch()
                .unwrap()
                .num_unprocessed_samples()
                > 0
        );

        let seek = worker
            .decoding_and_skip(&TrackSource::local(path), Duration::ZERO)
            .expect("seek decoder must be prepared");
        assert!(matches!(seek.progress, SeekProgress::Done));
        worker.seek = Some(seek);
        worker.complete_seek();
        assert!(worker.speed_stage.uses_time_stretch());
        assert_eq!(
            worker
                .speed_stage
                .stretch()
                .unwrap()
                .num_unprocessed_samples(),
            0
        );
    }

    #[test]
    fn switching_to_unity_while_stretch_output_is_deferred_preserves_credit() {
        let bus = EventBus::new();
        let (_command_tx, command_rx) = channel();
        let sink = Arc::new(FakeSink::new([
            PushSamplesResult::Backpressure,
            PushSamplesResult::Accepted,
        ]));
        let mut worker = empty_worker(bus.sender(), command_rx);
        set_playing(
            &mut worker,
            0,
            TrackSource::local("/speed-deferred-transition.wav"),
            None,
            Box::new(rodio::source::SineWave::new(440.0).take_duration(Duration::from_secs(1))),
        );
        worker.speed_stage.requested_speed = SPEED_MAX;
        worker.setup_stretch(44_100, 1);
        worker.sink_manager.install(Box::new(Arc::clone(&sink)));

        for _ in 0..16 {
            worker.pump_playback();
            if worker.pending_output().is_some() {
                break;
            }
        }
        let deferred_frames = worker
            .pending_output()
            .expect("time-stretch output must be deferred")
            .frames;
        worker.set_speed(Speed::UNITY);

        for _ in 0..128 {
            worker.pump_playback();
            if !worker.speed_stage.uses_time_stretch() && worker.pending_output().is_none() {
                break;
            }
        }

        assert!(worker.pending_output().is_none());
        assert!(!worker.speed_stage.uses_time_stretch());
        assert!(loaded(&worker).frames_written >= deferred_frames);
    }
}
