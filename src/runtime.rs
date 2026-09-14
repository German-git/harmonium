//! Single shared Tokio runtime and the bridge into the synchronous event bus.
//!
//! Architectural directives for async work in Harmonium:
//!
//! - Exactly one multi-threaded Tokio runtime lives for the whole process,
//!   owned by the application entrypoint through this module, and subsystems
//!   receive a [`Handle`] instead of ever building their own runtime.
//! - The TUI loop stays synchronous on crossterm polls and audio remains
//!   decoupled from Tokio, while async tasks serve concurrent IO such as
//!   filesystem scanning and future network or artwork work. CPU bound work
//!   must go through `spawn_blocking` or dedicated workers when it lands.
//! - Bridge pattern: background tasks report results through the existing
//!   bounded standard mpsc-style [`EventBus`]. Safe high-frequency events are
//!   coalesced, synchronous critical callers retain their waiting behavior,
//!   and async critical publishers yield while waiting for capacity. The UI
//!   thread keeps its simple blocking receive model untouched.
//!
//! Tests construct [`AppServices`] directly and observe effects through the
//! bus on plain threads, so no nested runtime juggling is ever required.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio::runtime::{Builder, Handle, Runtime};

use crate::artwork::ArtworkLoader;
use crate::audio::engine::spawn_audio_worker;
use crate::audio::{AudioCommand, AudioEngineHandle};
use crate::error::WorkerError;
use crate::event::{AppEvent, EffectErrorKind, EventBus, EventSender};
use crate::lyrics::LyricsService;
use crate::playlist::PlaylistRepository;
use crate::stream::{StreamResolver, resolver_with_defaults};
use crate::ui::theme::{FileThemeRepository, ThemeRepository};

pub type SpawnFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;
pub type OperationTask = Box<dyn FnOnce(OperationContext) -> SpawnFuture + Send + 'static>;

/// Runtime boundary used by effect dispatch. Tests can provide a deterministic
/// scheduler without constructing a Tokio runtime or an audio worker.
pub trait Spawner: Send + Sync {
    fn spawn_operation(
        &self,
        name: &'static str,
        kind: OperationKind,
        task: OperationTask,
    ) -> Result<OperationHandle, OperationRegistrationError>;

    fn spawn_background(
        &self,
        name: &'static str,
        kind: EffectErrorKind,
        task: SpawnFuture,
    ) -> Result<(), OperationRegistrationError>;

    fn spawn_background_with_context(
        &self,
        name: &'static str,
        kind: EffectErrorKind,
        task: OperationTask,
    ) -> Result<OperationHandle, OperationRegistrationError>;
}

/// Narrow audio boundary used by the effect runner and its tests.
pub trait AudioSink: Send + Sync {
    fn send_audio(
        &self,
        command: AudioCommand,
    ) -> Result<(), std::sync::mpsc::SendError<AudioCommand>>;
}

/// Injectable service aggregate used by effect execution.
///
/// Production [`AppServices`] supplies the process-wide runtime and audio
/// worker, while tests can provide the same boundaries without creating either
/// one. The aggregate keeps effect code independent from the concrete runtime
/// owner without splitting one effect across unrelated parameter lists.
pub trait EffectServices: Spawner {
    fn event_sender(&self) -> EventSender;
    fn audio_sink(&self) -> &dyn AudioSink;
    fn artwork_loader(&self) -> Option<&ArtworkLoader>;
    fn lyrics_service(&self) -> &LyricsService;
    fn playlist_store(&self) -> Option<Arc<dyn PlaylistRepository>>;
    fn stream_resolver(&self) -> &StreamResolver;
    fn config_writer(&self) -> Arc<ConfigWriteCoordinator>;
    fn runtime_state_writer(&self) -> Arc<RuntimeStateWriteCoordinator>;
    fn theme_repository(&self) -> Arc<dyn ThemeRepository>;
    fn set_lyrics_remote_enabled(&self, enabled: bool);
}

impl AudioSink for AudioEngineHandle {
    fn send_audio(
        &self,
        command: AudioCommand,
    ) -> Result<(), std::sync::mpsc::SendError<AudioCommand>> {
        self.send(command)
    }
}

/// Grace period granted to running tasks when the application shuts down.
#[cfg(not(test))]
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(50);
const OPERATION_REGISTRY_CAPACITY: usize = 64;

/// Stable identity attached to one unit of background application work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(u64);

impl OperationId {
    /// Numeric representation useful for tracing and deterministic tests.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Kind of work represented by an operation. New work of the same kind
/// invalidates an older active operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationKind {
    Browser,
    Search,
    OutputEnumeration,
    ResolveStream,
    Artwork,
    Lyrics,
    Playlist,
    ThemeLoad,
    ThemeSave,
    FileRename,
    Generic,
}

impl OperationKind {
    /// Stable bounded label used by tracing and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Search => "search",
            Self::OutputEnumeration => "output-enumeration",
            Self::ResolveStream => "stream",
            Self::Artwork => "artwork",
            Self::Lyrics => "lyrics",
            Self::Playlist => "playlist",
            Self::ThemeLoad => "theme-load",
            Self::ThemeSave => "theme-save",
            Self::FileRename => "file-rename",
            Self::Generic => "generic",
        }
    }
}

/// Terminal category recorded for one background operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationResultCategory {
    /// The task returned successfully without being cancelled.
    Success,
    /// The task returned an error.
    Failure,
    /// The task panicked before returning a result.
    Panic,
    /// The task returned successfully after cancellation was requested.
    Cancelled,
}

impl OperationResultCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Panic => "panic",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Bounded, privacy-safe observation of one completed operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationObservation {
    /// Stable operation identity.
    pub operation_id: OperationId,
    /// Application subsystem that owns the operation.
    pub subsystem: &'static str,
    /// Bounded source category, never a URL or user-provided value.
    pub source_kind: &'static str,
    /// Monotonic elapsed duration measured by the runtime.
    pub elapsed: Duration,
    /// Coarse terminal outcome, without error text.
    pub result_category: OperationResultCategory,
    /// Whether cancellation was observed at settlement time.
    pub cancelled: bool,
}

#[derive(Debug)]
struct OperationTelemetry {
    started: Instant,
    span: tracing::Span,
    observation: Arc<Mutex<Option<OperationObservation>>>,
}

impl OperationTelemetry {
    fn new(operation_id: OperationId, subsystem: &'static str, source_kind: &'static str) -> Self {
        Self {
            started: Instant::now(),
            span: tracing::info_span!(
                "background_operation",
                operation_id = operation_id.get(),
                subsystem,
                source_kind,
                elapsed_ms = tracing::field::Empty,
                result_category = tracing::field::Empty,
                cancelled = tracing::field::Empty,
            ),
            observation: Arc::new(Mutex::new(None)),
        }
    }

    fn finish(
        &self,
        operation_id: OperationId,
        subsystem: &'static str,
        source_kind: &'static str,
        result_category: OperationResultCategory,
        cancelled: bool,
    ) {
        let elapsed = self.started.elapsed();
        let elapsed_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        self.span.record("elapsed_ms", elapsed_ms);
        self.span
            .record("result_category", result_category.as_str());
        self.span.record("cancelled", cancelled);
        *self
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(OperationObservation {
            operation_id,
            subsystem,
            source_kind,
            elapsed,
            result_category,
            cancelled,
        });
        tracing::info!(
            parent: &self.span,
            elapsed_ms,
            result_category = result_category.as_str(),
            cancelled,
            "background operation finished"
        );
    }
}

/// Error returned when a new operation cannot enter the bounded registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationRegistrationError {
    /// The runtime is already shutting down and accepts no new work.
    Closed,
    /// All registry slots contain unsettled operations.
    CapacityExhausted,
}

impl fmt::Display for OperationRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("operation registry is closed"),
            Self::CapacityExhausted => {
                formatter.write_str("operation registry capacity is exhausted")
            }
        }
    }
}

impl std::error::Error for OperationRegistrationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationStatus {
    Active,
    Cancelled,
    Settled,
}

#[derive(Debug)]
struct OperationRecord {
    kind: OperationKind,
    status: OperationStatus,
    valid: bool,
    cancelled: Arc<AtomicBool>,
    queued_events: usize,
}

#[derive(Debug)]
struct RegistryState {
    closed: bool,
    next_id: u64,
    operations: HashMap<OperationId, OperationRecord>,
}

/// Bounded lifecycle registry for application-owned background work.
#[derive(Debug)]
struct OperationRegistry {
    state: Mutex<RegistryState>,
    wake: Condvar,
    capacity: usize,
}

impl OperationRegistry {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(RegistryState {
                closed: false,
                next_id: 0,
                operations: HashMap::new(),
            }),
            wake: Condvar::new(),
            capacity,
        }
    }

    #[cfg(test)]
    fn register(
        self: &Arc<Self>,
        kind: OperationKind,
        supersede: bool,
    ) -> Result<OperationHandle, OperationRegistrationError> {
        self.register_named(kind, supersede, kind.as_str())
    }

    fn register_named(
        self: &Arc<Self>,
        kind: OperationKind,
        supersede: bool,
        subsystem: &'static str,
    ) -> Result<OperationHandle, OperationRegistrationError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err(OperationRegistrationError::Closed);
        }

        while state
            .operations
            .values()
            .filter(|record| record.kind == kind)
            .count()
            >= self.capacity
        {
            let settled = state.operations.iter().find_map(|(id, record)| {
                (record.kind == kind
                    && matches!(
                        record.status,
                        OperationStatus::Cancelled | OperationStatus::Settled
                    )
                    && record.queued_events == 0)
                    .then_some(*id)
            });
            let Some(settled) = settled else {
                return Err(OperationRegistrationError::CapacityExhausted);
            };
            state.operations.remove(&settled);
        }

        if supersede {
            for record in state.operations.values_mut() {
                if record.kind == kind && record.valid {
                    record.valid = false;
                    if record.status == OperationStatus::Active {
                        record.status = OperationStatus::Cancelled;
                        record.cancelled.store(true, Ordering::Release);
                    }
                }
            }
        }

        state.next_id = state.next_id.wrapping_add(1).max(1);
        let id = OperationId(state.next_id);
        let cancelled = Arc::new(AtomicBool::new(false));
        let telemetry = Arc::new(OperationTelemetry::new(id, subsystem, kind.as_str()));
        state.operations.insert(
            id,
            OperationRecord {
                kind,
                status: OperationStatus::Active,
                valid: true,
                cancelled: cancelled.clone(),
                queued_events: 0,
            },
        );
        self.wake.notify_all();
        Ok(OperationHandle {
            id,
            token: OperationToken { cancelled },
            registry: Arc::clone(self),
            telemetry,
        })
    }

    fn cancel(&self, id: OperationId) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(record) = state.operations.get_mut(&id) else {
            return false;
        };
        if record.status != OperationStatus::Active {
            return false;
        }
        record.valid = false;
        record.status = OperationStatus::Cancelled;
        record.cancelled.store(true, Ordering::Release);
        self.wake.notify_all();
        true
    }

    fn settle(&self, id: OperationId) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(record) = state.operations.get_mut(&id) else {
            return false;
        };
        if record.status == OperationStatus::Settled {
            return false;
        }
        record.status = OperationStatus::Settled;
        self.wake.notify_all();
        true
    }

    fn is_active(&self, id: OperationId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .operations
            .get(&id)
            .is_some_and(|record| record.status == OperationStatus::Active)
    }

    fn accepts_completion(&self, id: OperationId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .operations
            .get(&id)
            .is_some_and(|record| {
                record.valid
                    && matches!(
                        record.status,
                        OperationStatus::Active | OperationStatus::Settled
                    )
            })
    }

    async fn publish_event(
        &self,
        sender: &EventSender,
        id: OperationId,
        event: AppEvent,
    ) -> Result<(), crate::event::EventSendError> {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(record) = state.operations.get_mut(&id) else {
                return Ok(());
            };
            if !record.valid || record.status != OperationStatus::Active {
                return Ok(());
            }
            record.queued_events += 1;
        }

        let mut reservation = QueuedEventReservation {
            registry: self,
            id,
            committed: false,
        };
        sender.send_critical_async(event).await?;
        reservation.committed = true;
        Ok(())
    }

    /// Release the registry identity after the UI consumes an operation event.
    fn release_event(&self, id: OperationId) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let remove = if let Some(record) = state.operations.get_mut(&id) {
            record.queued_events = record.queued_events.saturating_sub(1);
            matches!(
                record.status,
                OperationStatus::Cancelled | OperationStatus::Settled
            ) && record.queued_events == 0
        } else {
            false
        };
        if remove {
            state.operations.remove(&id);
        }
        self.wake.notify_all();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .operations
            .len()
    }

    #[cfg(test)]
    fn active_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .operations
            .values()
            .filter(|record| record.status == OperationStatus::Active)
            .count()
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        for record in state.operations.values_mut() {
            if record.status == OperationStatus::Active {
                record.valid = false;
                record.status = OperationStatus::Cancelled;
                record.cancelled.store(true, Ordering::Release);
            }
        }
        self.wake.notify_all();
    }

    fn settle_and_clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        for record in state.operations.values_mut() {
            record.status = OperationStatus::Settled;
        }
        state.operations.clear();
        self.wake.notify_all();
    }
}

struct QueuedEventReservation<'a> {
    registry: &'a OperationRegistry,
    id: OperationId,
    committed: bool,
}

impl Drop for QueuedEventReservation<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.registry.release_event(self.id);
        }
    }
}

/// Cloneable cancellation signal passed into a background operation.
#[derive(Debug, Clone)]
pub struct OperationToken {
    cancelled: Arc<AtomicBool>,
}

impl OperationToken {
    /// Whether the operation was cancelled by a newer request or shutdown.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Controller for one registered background operation.
#[derive(Debug, Clone)]
pub struct OperationHandle {
    id: OperationId,
    token: OperationToken,
    registry: Arc<OperationRegistry>,
    telemetry: Arc<OperationTelemetry>,
}

impl OperationHandle {
    /// Identity carried by completion and failure events.
    pub fn id(&self) -> OperationId {
        self.id
    }

    /// Cancellation token for code that needs to poll while doing work.
    pub fn token(&self) -> OperationToken {
        self.token.clone()
    }

    /// Cancel this operation and invalidate any later completion.
    pub fn cancel(&self) -> bool {
        self.registry.cancel(self.id)
    }

    /// Completed privacy-safe timing and outcome fields, when settlement ran.
    pub fn observation(&self) -> Option<OperationObservation> {
        self.telemetry
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// Context supplied to an operation task.
#[derive(Debug, Clone)]
pub struct OperationContext {
    handle: OperationHandle,
}

impl OperationContext {
    /// Identity carried by events published by this task.
    pub fn id(&self) -> OperationId {
        self.handle.id()
    }

    /// Cancellation token for cooperative work.
    pub fn token(&self) -> OperationToken {
        self.handle.token()
    }

    /// Cancel this operation, suppressing its completion and failure events.
    pub fn cancel(&self) -> bool {
        self.handle.cancel()
    }

    /// Publish one completion while the operation is still current. A
    /// cancelled operation is treated as a successful no-op rather than
    /// surfacing a failure during normal supersession.
    pub async fn publish(&self, sender: &EventSender, event: AppEvent) -> anyhow::Result<()> {
        self.handle
            .registry
            .publish_event(sender, self.id(), event)
            .await
            .context("event bus receiver went away")
    }
}

/// Owns the process wide runtime, the publisher side of the event bridge
/// and the audio worker handle.
pub struct AppServices {
    /// Taken out on the first shutdown so repeated drops stay harmless.
    runtime: Option<Runtime>,
    /// Shared entry point handed to subsystems for spawning async work.
    handle: Handle,
    /// Bus drained by the UI thread and fed by background tasks.
    events: EventBus,
    /// Producer of the dedicated playback worker owned for this process.
    audio: AudioEngineHandle,
    /// Shared artwork encoder, installed by the entrypoint after terminal
    /// capability detection.
    artwork: Option<ArtworkLoader>,
    /// Lyrics resolution chain with its per-track outcome memo.
    lyrics: LyricsService,
    /// Playlist store used for non-blocking autosave, installed by the
    /// entrypoint. `None` when no playlists directory is available.
    playlist_store: Option<Arc<dyn PlaylistRepository>>,
    /// Filesystem-backed theme repository in production; replaceable in
    /// effect tests without touching the user theme directory.
    theme_repository: Arc<dyn ThemeRepository>,
    /// Streaming resolver shared between the UI thread (Add Stream popup)
    /// and the resolver worker spawned by `Effect::ResolveStream`. Cloning
    /// is cheap; providers are stored as `Arc<dyn StreamProvider>`.
    stream_resolver: StreamResolver,
    /// Bounded lifecycle state for application-owned background work.
    operations: Arc<OperationRegistry>,
    /// Serializes config snapshots and rejects older snapshots after a newer
    /// request has reached the writer.
    config_writer: Arc<ConfigWriteCoordinator>,
    /// Serializes runtime-state snapshots and rejects older snapshots after a
    /// newer snapshot has reached the writer.
    runtime_state_writer: Arc<RuntimeStateWriteCoordinator>,
}

/// Single-writer coordinator for `config.toml` snapshots.
#[derive(Debug, Default)]
pub struct ConfigWriteCoordinator {
    latest_request_id: Mutex<u64>,
}

impl ConfigWriteCoordinator {
    /// Write one snapshot while preventing an older request from overwriting
    /// a newer snapshot that has already reached the coordinator.
    pub(crate) fn save(
        &self,
        request_id: u64,
        config: &crate::config::AppConfig,
        config_dir: &std::path::Path,
    ) -> Result<bool, crate::config::ConfigSaveError> {
        let mut latest_request_id = self
            .latest_request_id
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if request_id < *latest_request_id {
            return Ok(false);
        }
        *latest_request_id = request_id;
        config.save(config_dir).map(|()| true)
    }
}

/// Single-writer coordinator for `state.toml` snapshots.
#[derive(Debug, Default)]
pub struct RuntimeStateWriteCoordinator {
    latest_request_id: Mutex<u64>,
}

impl RuntimeStateWriteCoordinator {
    /// Write one runtime-state snapshot while preventing an older request from
    /// overwriting a newer snapshot that has already reached the coordinator.
    pub(crate) fn save(
        &self,
        request_id: u64,
        state: &crate::config::PersistedState,
        data_dir: &std::path::Path,
    ) -> Result<bool, crate::config::StateSaveError> {
        let mut latest_request_id = self
            .latest_request_id
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if request_id < *latest_request_id {
            return Ok(false);
        }
        *latest_request_id = request_id;
        state.save_result(data_dir).map(|()| true)
    }
}

impl AppServices {
    /// Build the single runtime backing the whole application lifetime.
    ///
    /// Construction failures surface as a regular error so the entrypoint can
    /// abort cleanly before any terminal state is touched. The audio worker
    /// starts idle and opens no device until the first play request, so
    /// machines without sound hardware still boot normally here.
    pub fn new() -> anyhow::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building the tokio runtime failed")?;
        let handle = runtime.handle().clone();
        let events = EventBus::new();
        let resolver = resolver_with_defaults();
        let audio = spawn_audio_worker(events.sender(), resolver.clone())
            .context("starting the audio worker failed")?;

        Ok(Self {
            runtime: Some(runtime),
            handle,
            events,
            audio,
            artwork: None,
            lyrics: LyricsService::new(),
            playlist_store: None,
            theme_repository: Arc::new(FileThemeRepository),
            stream_resolver: resolver,
            operations: Arc::new(OperationRegistry::new(OPERATION_REGISTRY_CAPACITY)),
            config_writer: Arc::new(ConfigWriteCoordinator::default()),
            runtime_state_writer: Arc::new(RuntimeStateWriteCoordinator::default()),
        })
    }

    /// Handle for spawning async work on the shared runtime.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Bus feeding user facing events into the main loop drain.
    pub fn events(&self) -> &EventBus {
        &self.events
    }

    /// Producer of the dedicated audio worker owning all backend types.
    pub fn audio(&self) -> &AudioEngineHandle {
        &self.audio
    }

    /// Shared lyrics resolution chain used by the background resolver.
    pub fn lyrics_service(&self) -> &LyricsService {
        &self.lyrics
    }

    /// Applies the remote lyrics preference at runtime.
    pub fn set_lyrics_remote_enabled(&self, enabled: bool) {
        self.lyrics.set_remote_enabled(enabled);
    }

    /// Install the artwork loader built after terminal detection.
    ///
    /// Separate from construction because the picker query needs the
    /// terminal in alternate screen mode, which happens after the runtime
    /// already exists. Called at most once by the entrypoint.
    pub fn set_artwork_loader(&mut self, loader: Option<ArtworkLoader>) {
        self.artwork = loader;
    }

    /// Shared artwork loader, when this session displays covers.
    pub fn artwork_loader(&self) -> Option<&ArtworkLoader> {
        self.artwork.as_ref()
    }

    /// Install the playlist store used for background autosave. Called once by
    /// the entrypoint after the real playlists directory is resolved.
    pub fn set_playlist_store<R>(&mut self, store: Option<R>)
    where
        R: PlaylistRepository + 'static,
    {
        self.playlist_store = store.map(|store| Arc::new(store) as Arc<dyn PlaylistRepository>);
    }

    /// Playlist store for background autosave, if available for this session.
    pub fn playlist_store(&self) -> Option<Arc<dyn PlaylistRepository>> {
        self.playlist_store.clone()
    }

    /// Install a replaceable theme repository for effect execution.
    pub fn set_theme_repository<R>(&mut self, repository: R)
    where
        R: ThemeRepository + 'static,
    {
        self.theme_repository = Arc::new(repository);
    }

    pub fn theme_repository(&self) -> Arc<dyn ThemeRepository> {
        Arc::clone(&self.theme_repository)
    }

    /// Shared streaming resolver used by the Add Stream popup and the
    /// background resolver spawned by `Effect::ResolveStream`.
    pub fn stream_resolver(&self) -> &StreamResolver {
        &self.stream_resolver
    }

    /// Whether a completion may still be applied to the application state.
    /// Settled operations remain acceptable because the worker settles after
    /// publishing its one completion, while cancelled operations are stale.
    pub fn accepts_operation_completion(&self, id: OperationId) -> bool {
        self.operations.accepts_completion(id)
    }

    /// Mark an operation-tagged event as consumed by the UI loop.
    pub fn release_operation_event(&self, id: Option<OperationId>) {
        if let Some(id) = id {
            self.operations.release_event(id);
        }
    }

    /// Spawn a superseding operation whose task receives identity and
    /// cooperative cancellation. Completion events should be sent through
    /// [`OperationContext::publish`] so stale work is rejected centrally.
    pub fn spawn_operation<F, Fut>(
        &self,
        name: &'static str,
        kind: OperationKind,
        task: F,
    ) -> Result<OperationHandle, OperationRegistrationError>
    where
        F: FnOnce(OperationContext) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.spawn_registered(name, kind, true, EffectErrorKind::Task, task)
    }

    /// Spawn a named background task whose failures reach the UI as notifications.
    ///
    /// Background work must degrade gracefully instead of taking the app down,
    /// so errors and panics alike convert into [`AppEvent::Notification`] on
    /// the bridge bus. Panics are caught by awaiting the join handle in a
    /// companion bookkeeping task, because Tokio captures a task panic as a
    /// `JoinError` surfaced there rather than aborting the process. Registry
    /// admission failures are returned directly so reporting them cannot
    /// block the caller on the UI-owned event queue.
    pub fn spawn_background<F>(
        &self,
        name: &'static str,
        kind: EffectErrorKind,
        task: F,
    ) -> Result<(), OperationRegistrationError>
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.spawn_registered(name, OperationKind::Generic, false, kind, move |_| task)
            .map(|_| ())
    }

    /// Spawn non-superseding background work while retaining its operation
    /// identity for safe completion events and observability.
    pub fn spawn_background_with_context<F, Fut>(
        &self,
        name: &'static str,
        kind: EffectErrorKind,
        task: F,
    ) -> Result<OperationHandle, OperationRegistrationError>
    where
        F: FnOnce(OperationContext) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.spawn_registered(name, OperationKind::Generic, false, kind, task)
    }

    fn spawn_registered<F, Fut>(
        &self,
        name: &'static str,
        operation_kind: OperationKind,
        supersede: bool,
        failure_kind: EffectErrorKind,
        task: F,
    ) -> Result<OperationHandle, OperationRegistrationError>
    where
        F: FnOnce(OperationContext) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let sender = self.events.sender();
        let handle = match self
            .operations
            .register_named(operation_kind, supersede, name)
        {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!(
                    operation = name,
                    ?error,
                    "background operation was not registered"
                );
                return Err(error);
            }
        };
        let context = OperationContext {
            handle: handle.clone(),
        };
        let registry = Arc::clone(&self.operations);
        let id = handle.id();
        let telemetry = Arc::clone(&handle.telemetry);
        let operation_span = telemetry.span.clone();
        let joined = self.handle.spawn(async move {
            let _entered = operation_span.enter();
            task(context).await
        });

        let operation_handle = handle.clone();
        self.handle.spawn(async move {
            let result_category = match joined.await {
                Ok(Ok(())) if operation_handle.token().is_cancelled() => {
                    OperationResultCategory::Cancelled
                }
                Ok(Ok(())) => OperationResultCategory::Success,
                Ok(Err(error)) => {
                    let worker_error = WorkerError::from_anyhow(name, error);
                    tracing::warn!(
                        error = %worker_error,
                        "background task failed"
                    );
                    if registry.is_active(id) {
                        notify_bridge_failure(
                            &sender,
                            Some(id),
                            name,
                            failure_kind,
                            worker_error,
                            Some(registry.as_ref()),
                        )
                        .await;
                    }
                    OperationResultCategory::Failure
                }
                Err(join_error) => {
                    let worker_error = WorkerError::new(name, join_error);
                    tracing::warn!(
                        error = %worker_error,
                        "background task crashed"
                    );
                    if registry.is_active(id) {
                        notify_bridge_failure(
                            &sender,
                            Some(id),
                            name,
                            failure_kind,
                            worker_error,
                            Some(registry.as_ref()),
                        )
                        .await;
                    }
                    OperationResultCategory::Panic
                }
            };
            let cancelled = operation_handle.token().is_cancelled();
            telemetry.finish(
                id,
                name,
                operation_kind.as_str(),
                result_category,
                cancelled,
            );
            registry.settle(id);
        });
        Ok(handle)
    }

    /// Stop the runtime, granting running tasks a bounded grace period.
    ///
    /// Consuming self plus the idempotent drop path keeps double shutdown safe.
    /// Must be called after the terminal guard has restored the console.
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    /// Shared shutdown logic used by both explicit shutdown and Drop.
    fn shutdown_inner(&mut self) {
        // Mark every operation cancelled before closing the event bus. Tasks
        // that are still inside detached blocking work cannot publish stale
        // completions after shutdown begins.
        self.operations.close();
        // Wake any producer waiting for critical event capacity before joining
        // the audio worker. The UI is no longer draining events during exit.
        self.events.close();
        // Stop accepting resize work before the async runtime goes away. The
        // shared artwork worker is bounded and shuts down cooperatively
        // without making teardown wait on third-party image encoding.
        if let Some(loader) = self.artwork.as_ref() {
            loader.shutdown();
        }
        // Ask the audio worker to leave first so the backend releases the
        // sound device while the runtime still exists to drain its tasks
        self.audio.shutdown();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);
            tracing::info!("shared async runtime stopped");
        }
        // `spawn_blocking` cannot be forcibly interrupted once it has started.
        // Finalize its registry records without extending the bounded runtime
        // shutdown or waiting for that detached work.
        self.operations.settle_and_clear();
    }
}

impl Spawner for AppServices {
    fn spawn_operation(
        &self,
        name: &'static str,
        kind: OperationKind,
        task: OperationTask,
    ) -> Result<OperationHandle, OperationRegistrationError> {
        AppServices::spawn_operation(self, name, kind, move |context| task(context))
    }

    fn spawn_background(
        &self,
        name: &'static str,
        kind: EffectErrorKind,
        task: SpawnFuture,
    ) -> Result<(), OperationRegistrationError> {
        AppServices::spawn_background(self, name, kind, task)
    }

    fn spawn_background_with_context(
        &self,
        name: &'static str,
        kind: EffectErrorKind,
        task: OperationTask,
    ) -> Result<OperationHandle, OperationRegistrationError> {
        AppServices::spawn_background_with_context(self, name, kind, move |context| task(context))
    }
}

impl EffectServices for AppServices {
    fn event_sender(&self) -> EventSender {
        self.events.sender()
    }

    fn audio_sink(&self) -> &dyn AudioSink {
        &self.audio
    }

    fn artwork_loader(&self) -> Option<&ArtworkLoader> {
        self.artwork.as_ref()
    }

    fn lyrics_service(&self) -> &LyricsService {
        &self.lyrics
    }

    fn playlist_store(&self) -> Option<Arc<dyn PlaylistRepository>> {
        self.playlist_store.clone()
    }

    fn stream_resolver(&self) -> &StreamResolver {
        &self.stream_resolver
    }

    fn config_writer(&self) -> Arc<ConfigWriteCoordinator> {
        Arc::clone(&self.config_writer)
    }

    fn runtime_state_writer(&self) -> Arc<RuntimeStateWriteCoordinator> {
        Arc::clone(&self.runtime_state_writer)
    }

    fn theme_repository(&self) -> Arc<dyn ThemeRepository> {
        Arc::clone(&self.theme_repository)
    }

    fn set_lyrics_remote_enabled(&self, enabled: bool) {
        self.lyrics.set_remote_enabled(enabled);
    }
}

impl Drop for AppServices {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

/// Push a failure notification into the bridge bus, ignoring a closed receiver.
///
/// The UI thread owns the receiver for the whole process lifetime, so a send
/// failure can only happen while the application is already exiting.
async fn notify_bridge_failure(
    sender: &EventSender,
    operation_id: Option<OperationId>,
    name: &'static str,
    kind: EffectErrorKind,
    detail: WorkerError,
    registry: Option<&OperationRegistry>,
) {
    let event = AppEvent::Notification {
        kind,
        operation_id,
        message: format!("{name}: {detail}"),
    };
    let result = match (registry, operation_id) {
        (Some(registry), Some(operation_id)) => {
            registry.publish_event(sender, operation_id, event).await
        }
        _ => sender.send_critical_async(event).await,
    };
    if let Err(error) = result {
        tracing::error!(?error, "could not publish background failure notification");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;

    /// Generous ceiling so slow machines never turn timing into flakes.
    const EVENT_WAIT: Duration = Duration::from_secs(5);
    /// Short window sampling silence after a fully resolved success path.
    const QUIET_WINDOW: Duration = Duration::from_millis(150);

    /// Receive one event or fail the test deterministically after a timeout.
    fn next_event(bus: &EventBus) -> AppEvent {
        match bus.recv_timeout(EVENT_WAIT) {
            Ok(event) => event,
            Err(error) => panic!("timed out waiting for an event: {error}"),
        }
    }

    #[test]
    fn successful_task_stays_silent_on_the_bus() {
        let services = AppServices::new().expect("services construction");
        let (task_done, task_done_rx) = tokio::sync::oneshot::channel::<()>();

        services
            .spawn_background("ok-task", EffectErrorKind::Task, async move {
                let _ = task_done.send(());
                Ok(())
            })
            .expect("success operation registers");

        // Sentinel ordered behind the watched task through the oneshot so the
        // assertions below only start once the success path fully resolved
        let sender = services.events().sender();
        services
            .spawn_background("sentinel", EffectErrorKind::Task, async move {
                let _ = task_done_rx.await;
                let _ = sender.send(AppEvent::Tick);
                Ok(())
            })
            .expect("sentinel operation registers");

        let mut unexpected = Vec::new();
        let mut saw_sentinel = false;
        while !saw_sentinel {
            match next_event(services.events()) {
                AppEvent::Notification { message, .. } => unexpected.push(message),
                AppEvent::Tick => saw_sentinel = true,
                _ => {}
            }
        }
        assert!(
            unexpected.is_empty(),
            "success must not notify, got {unexpected:?}"
        );

        // A successful task has no code path that notifies, so the quiet
        // window is a deterministic check rather than a race
        assert!(
            matches!(
                services.events().recv_timeout(QUIET_WINDOW),
                Err(RecvTimeoutError::Timeout)
            ),
            "nothing may arrive on the bus after a successful task"
        );

        services.shutdown();
    }

    #[test]
    fn operation_observation_contains_bounded_timing_result_and_cancellation_fields() {
        let telemetry = OperationTelemetry::new(OperationId(7), "unit-test", "generic");
        telemetry.finish(
            OperationId(7),
            "unit-test",
            "generic",
            OperationResultCategory::Success,
            false,
        );
        let success = telemetry
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .expect("success observation");
        assert_eq!(success.operation_id.get(), 7);
        assert_eq!(success.subsystem, "unit-test");
        assert_eq!(success.source_kind, "generic");
        assert!(success.elapsed <= Duration::from_secs(1));
        assert_eq!(success.result_category, OperationResultCategory::Success);
        assert!(!success.cancelled);

        let cancelled = OperationTelemetry::new(OperationId(8), "unit-test", "search");
        cancelled.finish(
            OperationId(8),
            "unit-test",
            "search",
            OperationResultCategory::Cancelled,
            true,
        );
        let cancelled = cancelled
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .expect("cancelled observation");
        assert_eq!(
            cancelled.result_category,
            OperationResultCategory::Cancelled
        );
        assert!(cancelled.cancelled);
    }

    #[test]
    fn failing_task_notifies_with_name_prefix() {
        let services = AppServices::new().expect("services construction");

        services
            .spawn_background("failing-task", EffectErrorKind::Task, async {
                Err(anyhow::anyhow!("library scan exploded"))
            })
            .expect("failing operation registers");

        match next_event(services.events()) {
            AppEvent::Notification {
                operation_id,
                kind,
                message,
                ..
            } => {
                assert!(operation_id.is_some(), "failure carries operation identity");
                assert_eq!(kind, EffectErrorKind::Task);
                assert!(message.starts_with("failing-task:"), "got {message}");
                assert!(message.contains("library scan exploded"), "got {message}");
            }
            event => panic!("expected a failure notification, got {event:?}"),
        }

        services.shutdown();
    }

    #[test]
    fn panicking_task_notifies_mentioning_the_task_name() {
        let services = AppServices::new().expect("services construction");

        services
            .spawn_background("panic-task", EffectErrorKind::Task, async {
                panic!("background boom");
            })
            .expect("panicking operation registers");

        match next_event(services.events()) {
            AppEvent::Notification {
                operation_id,
                message,
                ..
            } => {
                assert!(operation_id.is_some(), "panic carries operation identity");
                assert!(message.contains("panic-task"), "got {message}");
                assert!(message.contains("background boom"), "got {message}");
            }
            event => panic!("expected a crash notification, got {event:?}"),
        }

        services.shutdown();
    }

    #[test]
    fn operation_registration_supersedes_and_cancels_older_work() {
        let registry = Arc::new(OperationRegistry::new(4));
        let first = registry
            .register(OperationKind::Search, true)
            .expect("first operation registers");
        let second = registry
            .register(OperationKind::Search, true)
            .expect("second operation registers");

        assert!(first.token().is_cancelled());
        assert!(!registry.is_active(first.id()));
        assert!(registry.is_active(second.id()));
        assert_eq!(registry.active_count(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_completion_is_rejected_without_touching_the_bus() {
        let registry = Arc::new(OperationRegistry::new(4));
        let handle = registry
            .register(OperationKind::Artwork, true)
            .expect("operation registers");
        let context = OperationContext {
            handle: handle.clone(),
        };
        let bus = EventBus::new();

        assert!(handle.cancel());
        context
            .publish(&bus.sender(), AppEvent::Tick)
            .await
            .expect("cancellation is a quiet no-op");
        assert!(bus.try_recv().is_err());
    }

    #[test]
    fn settlement_is_idempotent_and_retains_the_final_state() {
        let registry = Arc::new(OperationRegistry::new(4));
        let handle = registry
            .register(OperationKind::Lyrics, true)
            .expect("operation registers");

        assert!(registry.settle(handle.id()));
        assert!(!registry.settle(handle.id()));
        assert!(!registry.is_active(handle.id()));
        assert!(registry.accepts_completion(handle.id()));
    }

    #[test]
    fn newer_operation_rejects_an_older_queued_completion() {
        let registry = Arc::new(OperationRegistry::new(4));
        let first = registry
            .register(OperationKind::Lyrics, true)
            .expect("first operation registers");
        assert!(registry.settle(first.id()));

        let second = registry
            .register(OperationKind::Lyrics, true)
            .expect("second operation registers");

        assert!(!registry.accepts_completion(first.id()));
        assert!(registry.accepts_completion(second.id()));
    }

    #[test]
    fn shutdown_cancels_and_clears_every_operation() {
        let registry = Arc::new(OperationRegistry::new(4));
        let first = registry
            .register(OperationKind::Generic, false)
            .expect("first operation registers");
        let second = registry
            .register(OperationKind::Generic, false)
            .expect("second operation registers");

        registry.close();

        assert!(first.token().is_cancelled());
        assert!(second.token().is_cancelled());
        assert_eq!(registry.active_count(), 0);
        registry.settle_and_clear();
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn operation_registry_capacity_failure_is_reported_explicitly() {
        let mut services = AppServices::new().expect("services construction");
        services.operations = Arc::new(OperationRegistry::new(1));

        services
            .spawn_background("held-operation", EffectErrorKind::Task, async {
                std::future::pending::<anyhow::Result<()>>().await
            })
            .expect("held operation registers");
        assert_eq!(
            services.spawn_background("capacity-operation", EffectErrorKind::Task, async {
                Ok(())
            }),
            Err(OperationRegistrationError::CapacityExhausted)
        );
        assert!(services.events.try_recv().is_err());

        services.shutdown();
    }

    #[test]
    fn operation_capacity_is_partitioned_by_kind() {
        let registry = Arc::new(OperationRegistry::new(1));
        let search = registry
            .register(OperationKind::Search, false)
            .expect("search operation registers");
        let artwork = registry
            .register(OperationKind::Artwork, false)
            .expect("independent artwork partition registers");

        assert!(matches!(
            registry.register(OperationKind::Search, false),
            Err(OperationRegistrationError::CapacityExhausted)
        ));
        assert!(registry.is_active(search.id()));
        assert!(registry.is_active(artwork.id()));
    }

    #[test]
    fn production_services_expose_replaceable_effect_boundaries() {
        fn assert_spawner<T: Spawner>() {}
        fn assert_audio_sink<T: AudioSink>() {}
        fn assert_playlist_repository<T: PlaylistRepository>() {}
        fn assert_theme_repository<T: ThemeRepository>() {}

        assert_spawner::<AppServices>();
        assert_audio_sink::<AudioEngineHandle>();
        assert_playlist_repository::<crate::playlist::PlaylistStore>();
        assert_theme_repository::<FileThemeRepository>();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_operation_event_prevents_settled_identity_eviction() {
        let registry = Arc::new(OperationRegistry::new(1));
        let first = registry
            .register(OperationKind::Search, true)
            .expect("first operation registers");
        let context = OperationContext {
            handle: first.clone(),
        };
        let bus = EventBus::with_capacity(2);

        context
            .publish(
                &bus.sender(),
                AppEvent::Notification {
                    kind: EffectErrorKind::Task,
                    operation_id: Some(first.id()),
                    message: "queued failure".to_string(),
                },
            )
            .await
            .expect("operation event publishes");
        assert!(registry.settle(first.id()));

        assert!(matches!(
            registry.register(OperationKind::Search, true),
            Err(OperationRegistrationError::CapacityExhausted)
        ));
        assert!(registry.accepts_completion(first.id()));

        let queued = bus.try_recv().expect("operation event remains queued");
        assert!(matches!(
            queued,
            AppEvent::Notification {
                operation_id: Some(id),
                ..
            } if id == first.id()
        ));
        registry.release_event(first.id());

        registry
            .register(OperationKind::Search, true)
            .expect("capacity is released after event consumption");
        assert!(!registry.accepts_completion(first.id()));
    }

    #[test]
    fn non_cooperative_blocking_work_does_not_extend_registry_shutdown() {
        let services = AppServices::new().expect("services construction");
        let registry = Arc::clone(&services.operations);
        let started = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(AtomicBool::new(false));
        let release_worker = Arc::clone(&release);
        let started_worker = Arc::clone(&started);
        let (operation_seen, operation_seen_rx) = tokio::sync::oneshot::channel();
        let operation_seen = Arc::new(Mutex::new(Some(operation_seen)));

        services
            .spawn_operation("non-cooperative", OperationKind::Generic, move |_| {
                let operation_seen = Arc::clone(&operation_seen);
                async move {
                    let _ = operation_seen
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take()
                        .map(|sender| sender.send(()));
                    tokio::task::spawn_blocking(move || {
                        started_worker.wait();
                        while !release_worker.load(Ordering::Acquire) {
                            std::thread::yield_now();
                        }
                    })
                    .await
                    .context("blocking test worker crashed")?;
                    Ok(())
                }
            })
            .expect("operation registers");

        operation_seen_rx.blocking_recv().expect("operation starts");
        started.wait();
        let begin = std::time::Instant::now();
        services.shutdown();
        assert!(begin.elapsed() < Duration::from_secs(1));
        assert_eq!(
            registry.len(),
            0,
            "shutdown clears detached operation records"
        );
        release.store(true, Ordering::Release);
    }
}
