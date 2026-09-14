//! Internal event plumbing between the terminal frontend and future workers.
//!
//! Later phases will route filesystem scans, audio playback progress and
//! metadata lookups through the same bus so the main loop keeps a single
//! receive path.
//!
//! Async workers publish into the same bounded queue. High-frequency progress
//! and terminal wakeups are coalesced, while completion, notification and
//! state-transition events are retained or reported as an explicit overflow.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;

use crate::artwork::ArtworkProtocol;
use crate::audio::playback::PlaybackSnapshot;
use crate::error::WorkerResult;
use crate::filesystem::FileEntry;
use crate::lyrics::LoadOutcome;
use crate::metadata::TrackMetadata;
use crate::runtime::OperationId;
use crate::search::{SearchResult, SearchScope};

/// Events produced by the frontend and by background workers.
///
/// The enum is deliberately not `Clone`: the artwork payload owns encoded
/// buffers that would be expensive to duplicate, and events are consumed
/// exactly once by the drain loop, so nothing needs to clone them.
/// Category of an effect failure carried by [`AppEvent::Notification`].
///
/// The payload keeps the message as a plain string for rendering, while this
/// tag allows the UI to differentiate errors of a specific subsystem later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectErrorKind {
    /// Failure raised by the audio engine worker.
    Audio,
    /// Failure raised by a generic background task.
    Task,
    /// Failure raised by the playlist store.
    Playlist,
}

#[derive(Debug)]
pub enum AppEvent {
    /// A keyboard event forwarded from the terminal.
    Key(KeyEvent),
    /// Terminal resize carrying the new cell dimensions.
    Resize(u16, u16),
    /// Periodic wakeup reserved for playback progress updates.
    Tick,
    /// Message from a failed or crashed background task, shown to the user.
    Notification {
        /// Category of the failed effect, so the UI can differentiate errors.
        kind: EffectErrorKind,
        operation_id: Option<OperationId>,
        /// Human readable message presented to the user.
        message: String,
    },
    /// Finished loading one browser directory level.
    BrowserDirectoryLoaded {
        /// Runtime operation that produced the listing.
        operation_id: OperationId,
        /// UI request identity used to discard stale navigation results.
        request_id: u64,
        /// Directory that was enumerated.
        dir: PathBuf,
        /// Folder name to select after an upward navigation, if any.
        restore_cursor_name: Option<String>,
        /// Sorted entries observed by the worker.
        entries: Vec<FileEntry>,
    },
    /// Finished validating a Settings browser-directory input.
    BrowserDirectoryValidated {
        /// Runtime operation that produced the validation.
        operation_id: OperationId,
        /// UI request identity used to discard cancelled or superseded edits.
        request_id: u64,
        /// Trimmed directory path submitted for validation.
        path: PathBuf,
        /// Validation context retained for the reducer's state transition.
        validation: crate::app::BrowserDirectoryValidation,
        /// Validated path on success, or a bounded worker error on failure.
        result: WorkerResult<PathBuf>,
    },
    /// A finished recursive scan reporting the audio discovered under one
    /// requested directory, committed to the playlist by the main loop.
    ScanCompleted {
        /// Runtime operation that produced the scan result.
        operation_id: OperationId,
        /// Directory the scan was spawned for, used in user messages.
        requested_dir: PathBuf,
        /// Supported audio paths found below it.
        tracks: Vec<PathBuf>,
    },
    /// Finished metadata extraction for one queued batch of paths.
    ///
    /// Failures travel as a count instead of per file errors so a corrupt
    /// download cannot flood the status area. Details stay in the log.
    MetadataCompleted {
        /// Runtime operation that produced the metadata batch.
        operation_id: OperationId,
        /// Successfully extracted snapshots paired with their paths.
        loaded: Vec<(PathBuf, TrackMetadata)>,
        /// How many paths of the batch failed extraction.
        failed: usize,
    },
    /// Finished contextual search, carrying the request identity so stale
    /// completions cannot replace a newer popup.
    SearchCompleted {
        /// Search request that produced the result.
        request_id: u64,
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// Surface searched by the request.
        scope: SearchScope,
        /// Matching files or tracks.
        results: Vec<SearchResult>,
        /// Optional worker detail when the browser root could not be read.
        message: Option<crate::error::WorkerError>,
    },
    /// Finished PipeWire output enumeration for the current Settings visit.
    OutputsEnumerated {
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// Settings visit that requested the enumeration.
        request_id: u64,
        /// Outputs observed by the provider.
        outputs: Vec<crate::audio::AudioOutput>,
    },
    /// Periodic position report for the track being played, published by
    /// the audio worker so the UI never queries audio internals.
    PlaybackProgress {
        /// Engine observation at publish time.
        snapshot: PlaybackSnapshot,
    },
    /// A non-coalescible playback state transition.
    PlaybackStateChanged {
        /// Engine observation at the transition boundary.
        snapshot: PlaybackSnapshot,
    },
    /// The playing track reached its natural end off loop, letting the
    /// application decide the next queue step. This is intentionally generic:
    /// it is emitted by the dedicated audio worker, whose lifecycle identity
    /// is not an application [`OperationContext`].
    TrackEnded {
        /// Queue index of the finished entry.
        track_index: usize,
    },
    /// A crossfade finished and the incoming track is now the one playing,
    /// reported so the application advances its queue/artwork state without
    /// treating the outgoing track as a natural end. The path lets the app
    /// re-locate the entry after a reorder, since the index alone may be stale.
    /// The elapsed value is the Track B position measured by the audio worker
    /// from the frames consumed during the transition.
    /// This remains a generic audio-worker event for the same reason as
    /// [`AppEvent::TrackEnded`].
    CrossfadeCompleted {
        /// Queue index the engine adopted for the new track.
        track_index: usize,
        /// Path of the track now playing, used to find its current index.
        path: PathBuf,
        /// Position already consumed from the incoming track at adoption.
        elapsed: Duration,
    },
    /// Artwork resolution for one track finished off loop.
    ///
    /// A `None` payload is a normal outcome meaning no usable cover was
    /// found or decoding failed, already logged by the worker. The main
    /// loop applies it through the same staleness gate as track events so
    /// a late delivery never paints over a newer track.
    ArtworkLoaded {
        /// Queue index the artwork was resolved for.
        track_index: usize,
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// Threaded render protocol, or nothing usable.
        ///
        /// Boxed because the protocol state is hundreds of bytes while
        /// every other variant is tiny, and the bus copies events by value
        artwork: Option<Box<ArtworkProtocol>>,
    },
    /// Lyrics resolution for one track finished off loop.
    ///
    /// The outcome carries the parsed document (or the degradation reason)
    /// so the panel merges it into state with the same staleness gate as
    /// artwork: a late delivery for a track the user already left is dropped.
    LyricsLoaded {
        /// Queue index the lyrics were resolved for.
        track_index: usize,
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// Parsed result or the short reason for the miss.
        outcome: LoadOutcome,
    },
    /// The ten raw editable tag fields of one file arrived from a worker.
    ///
    /// Published by the `EditMetadataPrefill` effect so the metadata editor
    /// can fill its form without ever touching lofty on the UI thread. The
    /// array follows [`crate::metadata::MetaField::ALL`] order.
    MetadataPrefillReady {
        /// Runtime operation that produced the editable fields.
        operation_id: OperationId,
        /// File the fields were read from, for the staleness gate.
        path: PathBuf,
        /// Raw tag values, empty strings for absent tags.
        fields: [String; 10],
    },
    /// A playlist-name listing finished on a blocking worker.
    PlaylistNamesCompleted {
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// UI request identity used to discard stale completions.
        request_id: u64,
        /// State context captured when the listing was requested.
        request: crate::app::PlaylistNamesRequest,
        /// Names on success, or a bounded worker error on failure.
        result: WorkerResult<Vec<String>>,
    },
    /// A named playlist write finished on a blocking worker.
    PlaylistSaved {
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// UI request identity used to discard stale completions.
        request_id: u64,
        /// Name passed to the worker.
        name: String,
        /// State transition captured when the write was requested.
        action: crate::app::PlaylistSaveAction,
        /// Written path on success, or a bounded worker error on failure.
        result: WorkerResult<PathBuf>,
    },
    /// A saved-playlist rename finished on a blocking worker.
    PlaylistRenamed {
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// UI request identity used to discard stale completions.
        request_id: u64,
        /// Existing playlist name.
        old_name: String,
        /// Requested playlist name.
        new_name: String,
        /// Dialog workflow that owns the completion.
        action: crate::app::PlaylistRenameAction,
        /// Typed worker outcome preserving collision and failure messages.
        result: crate::app::PlaylistRenameResult,
    },
    /// A saved-playlist delete and manager refresh finished on a blocking
    /// worker.
    PlaylistDeleted {
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// UI request identity used to discard stale completions.
        request_id: u64,
        /// Deleted playlist name.
        name: String,
        /// Cursor captured before the confirmation popup opened.
        cursor: Option<usize>,
        /// Whether the deleted playlist owned the current queue.
        was_active: bool,
        /// Delete outcome and names observed after the attempt.
        result: crate::app::PlaylistDeleteResult,
    },
    /// A named playlist load finished on a blocking worker.
    PlaylistLoaded {
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// UI request identity used to discard stale completions.
        request_id: u64,
        /// Playlist name passed to the worker.
        name: String,
        /// Loaded queue, or a bounded worker error.
        result: WorkerResult<crate::playlist::Playlist>,
    },
    /// A configuration snapshot finished on the single config writer.
    ConfigSaved {
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// Snapshot identity used by the writer's stale guard.
        request_id: u64,
        /// `Ok(true)` means written, `Ok(false)` means superseded, and `Err`
        /// carries the typed worker failure.
        result: WorkerResult<bool>,
    },
    /// A runtime-state snapshot finished on the single state writer.
    RuntimeStateSaved {
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// Snapshot identity used by the writer's stale guard.
        request_id: u64,
        /// `Ok(true)` means written, `Ok(false)` means superseded, and `Err`
        /// carries the bounded worker failure without changing UI state.
        result: WorkerResult<bool>,
    },
    /// A theme listing and palette load finished on a blocking worker.
    ThemeLoaded {
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// Settings-owned request identity used to discard stale results.
        request_id: u64,
        /// Settings visit that owns the request.
        visit_id: u64,
        /// Directory used by the worker.
        themes_dir: PathBuf,
        /// Theme name passed to the worker.
        name: String,
        /// Names observed when the Settings popup opened.
        theme_names: Option<Vec<String>>,
        /// Loaded editable colors, or a bounded worker error.
        result: WorkerResult<crate::ui::theme::ThemeColors>,
        /// Consumer of this load result.
        purpose: crate::app::ThemeLoadPurpose,
    },
    /// A theme file write finished on a blocking worker.
    ThemeSaved {
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// Settings-owned request identity used to discard stale results.
        request_id: u64,
        /// Settings visit that owns the request.
        visit_id: u64,
        /// Directory used by the worker.
        themes_dir: PathBuf,
        /// Theme name passed to the worker.
        name: String,
        /// Colors captured before the write started.
        colors: crate::ui::theme::ThemeColors,
        /// Write outcome, or a bounded worker error.
        result: WorkerResult<()>,
    },
    /// A background rename finished, carrying the disk outcome.
    ///
    /// Playlist rewrites ran before the disk rename, so a `Conflict` result
    /// means a file occupied the target before the rename step.
    RenameCompleted {
        /// Runtime operation that produced the rename outcome.
        operation_id: OperationId,
        /// UI request identity used to discard stale or cancelled results.
        request_id: u64,
        /// Path the file had before the rename.
        from: PathBuf,
        /// Path the file should now have.
        to: PathBuf,
        /// Typed worker outcome preserving success, collision, safety and
        /// operational failures for the reducer.
        result: crate::app::RenameFileResult,
    },
    /// A metadata write finished, reporting the persisted outcome.
    MetadataWriteCompleted {
        /// Runtime operation that produced the write outcome.
        operation_id: OperationId,
        /// File whose tags were written.
        path: PathBuf,
        /// Typed write outcome; the UI formats the failure only at its edge.
        result: WorkerResult<()>,
        /// Field values as they were just persisted (in
        /// [`crate::metadata::MetaField::ALL`] order). The completion
        /// handler propagates the new Title into every saved playlist's
        /// EXTINF label, so the persistence is reflected on disk without
        /// waiting for the post-write re-read.
        fields: [String; 10],
    },
    /// Outcome of a streaming URL resolution dispatched from the Add
    /// Stream popup.
    ///
    /// Carries the original URL so the UI can match the event against the
    /// dialog (in case the user opened a second Add Stream while the
    /// first was resolving). On success the queue gains a new track; on
    /// failure the popup surfaces the message and the queue is untouched.
    StreamResolved {
        /// Request identity assigned by the Add Stream dialog.
        request_id: u64,
        /// Runtime operation that produced the result.
        operation_id: OperationId,
        /// URL the user submitted. Echoed back so the UI handler can
        /// decide whether the event is still relevant to the current
        /// dialog state.
        url: url::Url,
        /// Fully resolved track ready to be queued, when the lookup
        /// succeeded. The track carries its resolved metadata so the
        /// playlist row shows the radio station name, not the bare
        /// hostname.
        track: Option<Box<crate::track::Track>>,
        /// Typed failure detail when the resolver could not produce a track.
        message: Option<crate::error::WorkerError>,
    },
    /// Audio engine finished decoding a stream source and is ready to
    /// start pumping samples.
    ///
    /// Published after the worker accepts the matching `PendingAcquisition`
    /// and successfully prepares the stream decoder. The application uses the
    /// URL and generation to clear its stream activity only when this result
    /// still belongs to the current request, so the Now Playing spinner drops
    /// before the first audio frame without accepting a stale completion.
    /// `Playback::Loading` is an identity-free worker marker; the pending
    /// acquisition owns the source identity and cancellation token.
    /// Audio-worker completions remain generic because they have no
    /// application [`OperationContext`].
    SourceReady {
        /// Stable identifier of the track the engine just prepared,
        /// typically the URL for streams.
        url: String,
        /// Playback generation assigned when this acquisition was started.
        /// `None` preserves compatibility with unrelated audio producers.
        generation: Option<u64>,
    },
    /// Audio engine failed while acquiring or decoding a stream source.
    ///
    /// The application uses the URL and playback generation as the staleness
    /// identity so a failed older request cannot clear stream activity for a
    /// newer track. A cancelled acquisition intentionally emits no result;
    /// source-superseding commands transition the worker out of loading, while
    /// rejected seek and preload commands leave the primary acquisition intact.
    SourceFailed {
        /// Stable identifier of the stream that failed.
        url: String,
        /// Playback generation assigned when this acquisition was started.
        generation: Option<u64>,
    },
}

const DEFAULT_EVENT_CAPACITY: usize = 256;

/// Error returned when an event cannot be admitted to the bounded bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSendError {
    /// The queue contains only critical events and cannot accept another one.
    Full,
    /// The receiving event loop has already shut down.
    Disconnected,
}

impl fmt::Display for EventSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("event bus capacity exhausted by critical events"),
            Self::Disconnected => formatter.write_str("event bus receiver went away"),
        }
    }
}

impl Error for EventSendError {}

/// Point-in-time bounded state of the event queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventQueueSnapshot {
    /// Number of events currently waiting for the receiver.
    pub depth: usize,
    /// Maximum number of retained events, including coalescible entries.
    pub capacity: usize,
    /// Whether the receiving event loop is still accepting producers.
    pub receiver_alive: bool,
}

impl EventQueueSnapshot {
    /// Number of slots available before a non-coalescible send is full.
    pub const fn remaining_capacity(self) -> usize {
        self.capacity.saturating_sub(self.depth)
    }
}

#[derive(Debug)]
struct EventQueue {
    queue: Mutex<VecDeque<AppEvent>>,
    wake: Condvar,
    async_wake: tokio::sync::Notify,
    async_critical_publish: tokio::sync::Mutex<()>,
    capacity: usize,
    receiver_alive: AtomicBool,
}

impl EventQueue {
    fn snapshot(&self) -> EventQueueSnapshot {
        let queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        EventQueueSnapshot {
            depth: queue.len(),
            capacity: self.capacity,
            receiver_alive: self.receiver_alive.load(Ordering::Acquire),
        }
    }
}

/// Cloneable producer for the bounded event bus.
#[derive(Debug, Clone)]
pub struct EventSender {
    queue: Arc<EventQueue>,
}

impl EventSender {
    /// Read bounded queue state without exposing event payloads.
    pub fn queue_snapshot(&self) -> EventQueueSnapshot {
        self.queue.snapshot()
    }

    /// Enqueue an event, coalescing only safe high-frequency variants.
    pub fn send(&self, event: AppEvent) -> Result<(), EventSendError> {
        let mut queue = self
            .queue
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !self.queue.receiver_alive.load(Ordering::Acquire) {
            return Err(EventSendError::Disconnected);
        }

        match try_enqueue(&mut queue, event, self.queue.capacity) {
            EnqueueResult::Enqueued => {
                self.queue.wake.notify_one();
                Ok(())
            }
            EnqueueResult::Full(_event) => Err(EventSendError::Full),
        }
    }

    /// Enqueue a completion, notification, error, or state transition without
    /// dropping it when the bounded queue is temporarily full. The sender
    /// sleeps until the receiver drains a slot or closes the bus.
    pub fn send_critical(&self, mut event: AppEvent) -> Result<(), EventSendError> {
        let mut queue = self
            .queue
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        loop {
            if !self.queue.receiver_alive.load(Ordering::Acquire) {
                return Err(EventSendError::Disconnected);
            }

            match try_enqueue(&mut queue, event, self.queue.capacity) {
                EnqueueResult::Enqueued => {
                    self.queue.wake.notify_one();
                    return Ok(());
                }
                EnqueueResult::Full(returned) => {
                    event = *returned;
                    queue = self
                        .queue
                        .wake
                        .wait(queue)
                        .unwrap_or_else(|error| error.into_inner());
                }
            }
        }
    }

    /// Enqueue a critical event with an explicit upper bound on producer
    /// blocking. This is reserved for real-time-adjacent workers such as the
    /// audio thread; ordinary completions continue using `send_critical` so
    /// they are never silently shed.
    pub fn send_critical_timeout(
        &self,
        mut event: AppEvent,
        timeout: Duration,
    ) -> Result<(), EventSendError> {
        let deadline = Instant::now() + timeout;
        let mut queue = self
            .queue
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        loop {
            if !self.queue.receiver_alive.load(Ordering::Acquire) {
                return Err(EventSendError::Disconnected);
            }
            match try_enqueue(&mut queue, event, self.queue.capacity) {
                EnqueueResult::Enqueued => {
                    self.queue.wake.notify_one();
                    return Ok(());
                }
                EnqueueResult::Full(returned) => {
                    event = *returned;
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(EventSendError::Full);
                    }
                    let (next, result) = self
                        .queue
                        .wake
                        .wait_timeout(queue, remaining)
                        .unwrap_or_else(|error| error.into_inner());
                    queue = next;
                    if result.timed_out() {
                        return Err(EventSendError::Full);
                    }
                }
            }
        }
    }

    /// Enqueue a critical event without blocking a Tokio worker while the
    /// bounded queue is full. The standard mutex is held only for the brief
    /// queue inspection/mutation; capacity waits happen through [`Notify`].
    pub async fn send_critical_async(&self, mut event: AppEvent) -> Result<(), EventSendError> {
        // Tokio's mutex serializes async critical publishers so a group of
        // completions cannot overtake one another after a shared capacity
        // wakeup. The synchronous path retains its existing behavior.
        let _publish_turn = self.queue.async_critical_publish.lock().await;
        loop {
            let notified = self.queue.async_wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let returned = {
                let mut queue = self
                    .queue
                    .queue
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if !self.queue.receiver_alive.load(Ordering::Acquire) {
                    return Err(EventSendError::Disconnected);
                }

                match try_enqueue(&mut queue, event, self.queue.capacity) {
                    EnqueueResult::Enqueued => {
                        self.queue.wake.notify_one();
                        return Ok(());
                    }
                    EnqueueResult::Full(returned) => returned,
                }
            };
            event = *returned;
            notified.await;
        }
    }
}

enum EnqueueResult {
    Enqueued,
    Full(Box<AppEvent>),
}

fn try_enqueue(queue: &mut VecDeque<AppEvent>, event: AppEvent, capacity: usize) -> EnqueueResult {
    if let Some(index) = queue
        .iter()
        .position(|queued| same_coalescing_class(queued, &event))
    {
        queue[index] = event;
        return EnqueueResult::Enqueued;
    }

    if queue.len() >= capacity {
        if let Some(index) = queue.iter().position(is_coalescible) {
            queue.remove(index);
        } else {
            return EnqueueResult::Full(Box::new(event));
        }
    }

    queue.push_back(event);
    EnqueueResult::Enqueued
}

fn is_coalescible(event: &AppEvent) -> bool {
    matches!(
        event,
        AppEvent::Tick | AppEvent::Resize(_, _) | AppEvent::PlaybackProgress { .. }
    )
}

fn same_coalescing_class(left: &AppEvent, right: &AppEvent) -> bool {
    matches!(
        (left, right),
        (AppEvent::Tick, AppEvent::Tick)
            | (AppEvent::Resize(_, _), AppEvent::Resize(_, _))
            | (
                AppEvent::PlaybackProgress { .. },
                AppEvent::PlaybackProgress { .. }
            )
    )
}

/// Minimal publish subscribe channel over a bounded queue.
#[derive(Debug)]
pub struct EventBus {
    sender: EventSender,
    queue: Arc<EventQueue>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    /// Create a bus with the default bounded capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_EVENT_CAPACITY)
    }

    /// Create a bus with a deterministic capacity, primarily for tests.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "event bus capacity must be positive");
        let queue = Arc::new(EventQueue {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            wake: Condvar::new(),
            async_wake: tokio::sync::Notify::new(),
            async_critical_publish: tokio::sync::Mutex::new(()),
            capacity,
            receiver_alive: AtomicBool::new(true),
        });
        let sender = EventSender {
            queue: queue.clone(),
        };
        Self { sender, queue }
    }

    /// Return a clone of the producer side for future worker threads.
    pub fn sender(&self) -> EventSender {
        self.sender.clone()
    }

    /// Read bounded queue state without exposing event payloads.
    pub fn queue_snapshot(&self) -> EventQueueSnapshot {
        self.queue.snapshot()
    }

    /// Enqueue an event for the main loop.
    ///
    /// Critical overflow is returned to the caller instead of being silently
    /// dropped. Safe high-frequency events may be coalesced by the sender.
    pub fn send(&self, event: AppEvent) -> Result<(), EventSendError> {
        self.sender.send(event)
    }

    /// Enqueue a critical event, waiting for temporary capacity instead of
    /// dropping a completion, notification, or state transition.
    pub fn send_critical(&self, event: AppEvent) -> Result<(), EventSendError> {
        self.sender.send_critical(event)
    }

    /// Enqueue a critical event while yielding instead of blocking a Tokio
    /// worker if the bounded queue has no immediately usable capacity.
    pub async fn send_critical_async(&self, event: AppEvent) -> Result<(), EventSendError> {
        self.sender.send_critical_async(event).await
    }

    /// Stop producers waiting for capacity during application shutdown.
    pub fn close(&self) {
        // Take the queue lock before changing the state so a sender cannot
        // observe an open bus, release the lock, and then miss this wakeup
        // while entering the condition-variable wait.
        let _queue = self
            .queue
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.queue.receiver_alive.store(false, Ordering::Release);
        self.queue.wake.notify_all();
        self.queue.async_wake.notify_waiters();
    }

    /// Receive without blocking, draining whatever is queued.
    pub fn try_recv(&self) -> std::result::Result<AppEvent, TryRecvError> {
        let event = self
            .queue
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front();
        if event.is_some() {
            self.queue.wake.notify_all();
            self.queue.async_wake.notify_one();
        }
        event.ok_or(TryRecvError::Empty)
    }

    /// Receive with a timeout, useful for idle driven ticks later on.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<AppEvent, RecvTimeoutError> {
        let mut queue = self
            .queue
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(event) = queue.pop_front() {
                self.queue.wake.notify_all();
                self.queue.async_wake.notify_one();
                return Ok(event);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            let (guard, result) = self
                .queue
                .wake
                .wait_timeout(queue, remaining)
                .unwrap_or_else(|error| error.into_inner());
            queue = guard;
            if result.timed_out() && queue.is_empty() {
                return Err(RecvTimeoutError::Timeout);
            }
        }
    }
}

impl Drop for EventBus {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier, mpsc};

    #[test]
    fn event_bus_send_and_recv() {
        let bus = EventBus::new();
        let event = AppEvent::Tick;

        bus.send(event)
            .expect("send should succeed while capacity is free");

        let received = bus
            .try_recv()
            .expect("event should be immediately available");
        assert!(matches!(received, AppEvent::Tick));
    }

    #[test]
    fn event_bus_try_recv_empty() {
        let bus = EventBus::new();

        let result = bus.try_recv();

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TryRecvError::Empty));
    }

    #[test]
    fn event_bus_recv_timeout_expires() {
        let bus = EventBus::new();

        let result = bus.recv_timeout(Duration::from_millis(10));

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RecvTimeoutError::Timeout));
    }

    #[test]
    fn event_bus_multiple_senders() {
        let bus = EventBus::new();
        let sender_b = bus.sender();

        bus.send(AppEvent::Tick).expect("send from original sender");
        sender_b
            .send(AppEvent::Resize(80, 24))
            .expect("send from cloned sender");

        let first = bus.try_recv().expect("first event available");
        let second = bus.try_recv().expect("second event available");

        assert!(matches!(first, AppEvent::Tick));
        assert!(matches!(second, AppEvent::Resize(80, 24)));
    }

    #[test]
    fn event_bus_default_trait() {
        let default_bus = EventBus::default();
        let new_bus = EventBus::new();

        let event = AppEvent::Notification {
            kind: EffectErrorKind::Task,
            operation_id: None,
            message: "test".into(),
        };

        default_bus.send(event).expect("default bus sends");

        assert!(default_bus.try_recv().is_ok());
        assert!(new_bus.try_recv().is_err());
    }

    #[test]
    fn queue_snapshot_reports_depth_capacity_and_shutdown_without_payloads() {
        let bus = EventBus::with_capacity(3);
        assert_eq!(
            bus.queue_snapshot(),
            EventQueueSnapshot {
                depth: 0,
                capacity: 3,
                receiver_alive: true,
            }
        );

        bus.send(AppEvent::Tick).expect("tick fits");
        assert_eq!(bus.queue_snapshot().depth, 1);
        assert_eq!(bus.queue_snapshot().remaining_capacity(), 2);

        bus.close();
        assert!(!bus.queue_snapshot().receiver_alive);
        assert_eq!(bus.queue_snapshot().capacity, 3);
    }

    fn progress(elapsed: u64) -> AppEvent {
        AppEvent::PlaybackProgress {
            snapshot: PlaybackSnapshot {
                status: crate::audio::playback::PlayStatus::Playing,
                track_index: Some(0),
                elapsed: Duration::from_secs(elapsed),
                duration: Some(Duration::from_secs(10)),
                sink_health: crate::audio::playback::SinkHealth::Healthy,
            },
        }
    }

    #[test]
    fn bounded_bus_coalesces_progress_but_keeps_critical_events() {
        let bus = EventBus::with_capacity(2);
        bus.send(progress(1)).expect("first progress fits");
        bus.send(AppEvent::Notification {
            kind: EffectErrorKind::Audio,
            operation_id: None,
            message: "output lost".into(),
        })
        .expect("critical event fits");
        bus.send(progress(2))
            .expect("latest progress replaces old progress");

        assert!(
            matches!(bus.try_recv(), Ok(AppEvent::PlaybackProgress { snapshot }) if snapshot.elapsed == Duration::from_secs(2))
        );
        assert!(matches!(bus.try_recv(), Ok(AppEvent::Notification { .. })));
    }

    #[test]
    fn timed_critical_publish_returns_when_capacity_stays_full() {
        let bus = EventBus::with_capacity(1);
        bus.send_critical(AppEvent::TrackEnded { track_index: 1 })
            .expect("initial critical event fits");
        let started = Instant::now();
        let result = bus.sender().send_critical_timeout(
            AppEvent::TrackEnded { track_index: 2 },
            Duration::from_millis(20),
        );

        assert_eq!(result, Err(EventSendError::Full));
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn progress_drops_without_displacing_a_full_critical_queue() {
        let bus = EventBus::with_capacity(2);
        bus.send_critical(AppEvent::Notification {
            kind: EffectErrorKind::Audio,
            operation_id: None,
            message: "first".into(),
        })
        .expect("first critical event fits");
        bus.send_critical(AppEvent::TrackEnded { track_index: 4 })
            .expect("second critical event fits");

        assert_eq!(bus.send(progress(4)), Err(EventSendError::Full));
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
    fn bounded_bus_coalesces_resize_and_tick_wakeups() {
        let bus = EventBus::with_capacity(3);
        bus.send(AppEvent::Resize(80, 24)).expect("resize fits");
        bus.send(AppEvent::Resize(120, 40))
            .expect("latest resize replaces old resize");
        bus.send(AppEvent::Tick).expect("tick fits");
        bus.send(AppEvent::Tick)
            .expect("duplicate tick is coalesced");

        assert!(matches!(bus.try_recv(), Ok(AppEvent::Resize(120, 40))));
        assert!(matches!(bus.try_recv(), Ok(AppEvent::Tick)));
        assert!(matches!(bus.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn critical_overflow_is_reported_explicitly() {
        let bus = EventBus::with_capacity(1);
        bus.send(AppEvent::Notification {
            kind: EffectErrorKind::Task,
            operation_id: None,
            message: "first".into(),
        })
        .expect("first critical event fits");
        let result = bus.send(AppEvent::Notification {
            kind: EffectErrorKind::Task,
            operation_id: None,
            message: "second".into(),
        });
        assert_eq!(result, Err(EventSendError::Full));
        assert!(
            matches!(bus.try_recv(), Ok(AppEvent::Notification { message, .. }) if message == "first")
        );
    }

    #[test]
    fn critical_send_waits_for_capacity_instead_of_dropping() {
        let bus = Arc::new(EventBus::with_capacity(1));
        bus.send(AppEvent::Notification {
            kind: EffectErrorKind::Task,
            operation_id: None,
            message: "first".into(),
        })
        .expect("first critical event fits");
        let sender = bus.sender();
        let (started_tx, started_rx) = mpsc::channel();
        let completed = std::thread::spawn(move || {
            started_tx.send(()).expect("sender started");
            sender
                .send_critical(AppEvent::Notification {
                    kind: EffectErrorKind::Task,
                    operation_id: None,
                    message: "second".into(),
                })
                .expect("critical sender must eventually succeed");
        });

        started_rx.recv().expect("sender reached the blocking send");
        assert!(!completed.is_finished(), "sender must wait while full");
        assert!(matches!(bus.try_recv(), Ok(AppEvent::Notification { .. })));
        completed.join().expect("critical sender thread");
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::Notification { message, .. }) if message == "second"
        ));
    }

    #[test]
    fn concurrent_progress_flood_retains_critical_events() {
        let bus = Arc::new(EventBus::with_capacity(4));
        let start = Arc::new(Barrier::new(7));
        let critical_completed = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();

        for producer in 0..4 {
            let sender = bus.sender();
            let start = Arc::clone(&start);
            workers.push(std::thread::spawn(move || {
                start.wait();
                for elapsed in 0..256 {
                    let _ = sender.send(progress((producer * 256 + elapsed) as u64));
                }
            }));
        }

        for message in ["first critical", "second critical"] {
            let sender = bus.sender();
            let start = Arc::clone(&start);
            let completed = Arc::clone(&critical_completed);
            workers.push(std::thread::spawn(move || {
                start.wait();
                sender
                    .send_critical(AppEvent::Notification {
                        kind: EffectErrorKind::Task,
                        operation_id: None,
                        message: message.to_string(),
                    })
                    .expect("critical event must survive the progress flood");
                completed.fetch_add(1, Ordering::Release);
            }));
        }

        start.wait();
        for worker in workers {
            worker.join().expect("event producer");
        }
        assert_eq!(critical_completed.load(Ordering::Acquire), 2);

        let mut critical_messages = Vec::new();
        while let Ok(event) = bus.try_recv() {
            if let AppEvent::Notification { message, .. } = event {
                critical_messages.push(message);
            }
        }
        critical_messages.sort();
        assert_eq!(
            critical_messages,
            vec!["first critical", "second critical"],
            "critical events must not be displaced by coalescible progress"
        );
    }

    #[test]
    fn close_wakes_a_critical_sender_waiting_on_a_full_queue() {
        let bus = Arc::new(EventBus::with_capacity(1));
        bus.send_critical(AppEvent::TrackEnded { track_index: 1 })
            .expect("initial critical event fits");

        let sender = bus.sender();
        let (started_tx, started_rx) = mpsc::channel();
        let waiting = std::thread::spawn(move || {
            started_tx.send(()).expect("sender started");
            sender.send_critical(AppEvent::TrackEnded { track_index: 2 })
        });

        started_rx.recv().expect("sender reached the blocking send");
        bus.close();
        assert_eq!(
            waiting.join().expect("waiting sender thread"),
            Err(EventSendError::Disconnected)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_critical_send_yields_when_the_queue_is_full() {
        let bus = EventBus::with_capacity(1);
        bus.send_critical(AppEvent::TrackEnded { track_index: 1 })
            .expect("initial critical event fits");

        let sender = bus.sender();
        let mut waiting =
            Box::pin(sender.send_critical_async(AppEvent::TrackEnded { track_index: 2 }));
        tokio::select! {
            result = &mut waiting => panic!("full queue must suspend, got {result:?}"),
            _ = tokio::task::yield_now() => {}
        }

        let (ran_tx, ran_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            ran_tx.send(()).expect("probe receiver is alive");
        });
        tokio::time::timeout(Duration::from_secs(1), ran_rx)
            .await
            .expect("another future must run while the sender waits")
            .expect("probe task must complete");

        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::TrackEnded { track_index: 1 })
        ));
        assert_eq!(waiting.await, Ok(()));
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::TrackEnded { track_index: 2 })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_critical_publications_preserve_queue_order() {
        let bus = EventBus::with_capacity(1);
        bus.send_critical(AppEvent::TrackEnded { track_index: 1 })
            .expect("initial critical event fits");

        let sender = bus.sender();
        let publishing = tokio::spawn(async move {
            sender
                .send_critical_async(AppEvent::TrackEnded { track_index: 2 })
                .await?;
            sender
                .send_critical_async(AppEvent::TrackEnded { track_index: 3 })
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            !publishing.is_finished(),
            "publication must wait for capacity"
        );

        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::TrackEnded { track_index: 1 })
        ));

        let second = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(event) = bus.try_recv() {
                    break event;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second event must be published promptly");
        assert!(matches!(second, AppEvent::TrackEnded { track_index: 2 }));

        let third = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(event) = bus.try_recv() {
                    break event;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("third event must be published promptly");
        assert!(matches!(third, AppEvent::TrackEnded { track_index: 3 }));
        publishing
            .await
            .expect("publisher task must not panic")
            .expect("third event fits");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_wakes_async_critical_sender_promptly() {
        let bus = EventBus::with_capacity(1);
        bus.send_critical(AppEvent::TrackEnded { track_index: 1 })
            .expect("initial critical event fits");
        let sender = bus.sender();
        let waiting = tokio::spawn(async move {
            sender
                .send_critical_async(AppEvent::TrackEnded { track_index: 2 })
                .await
        });

        tokio::task::yield_now().await;
        bus.close();
        let result = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("close must wake the async sender")
            .expect("sender task must not panic");
        assert_eq!(result, Err(EventSendError::Disconnected));
    }

    #[test]
    fn state_transition_events_are_not_coalesced_with_progress() {
        let bus = EventBus::with_capacity(2);
        let snapshot = PlaybackSnapshot {
            status: crate::audio::playback::PlayStatus::Paused,
            track_index: Some(0),
            elapsed: Duration::ZERO,
            duration: Some(Duration::from_secs(10)),
            sink_health: crate::audio::playback::SinkHealth::Lost,
        };
        bus.send_critical(AppEvent::PlaybackStateChanged { snapshot })
            .expect("transition fits");
        bus.send(progress(3)).expect("progress fits");
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::PlaybackStateChanged { .. })
        ));
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::PlaybackProgress { .. })
        ));
    }
}
