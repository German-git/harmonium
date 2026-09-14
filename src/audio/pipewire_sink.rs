//! Native PipeWire output stream component (Slice 3a).
//!
//! This is a self-contained, compilable `PipeWireSink` that owns a real
//! PipeWire output stream on a dedicated thread and accepts interleaved `f32`
//! samples pushed from a producer. Changing the output device is handled by
//! the owner dropping this stream and opening a fresh one: reconnecting a
//! stream from inside its own process callback is not safe and causes a
//! segfault, so the stream is rebuilt rather than retargeted live.
//!
//! The existing rodio/ALSA playback path is intentionally left as the active
//! engine: this component is meant to be validated on a real PipeWire session
//! before it is switched in as the output sink.

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa;
use thiserror::Error;

/// How long [`PipeWireSink::shutdown`] waits for the PipeWire thread to return
/// before giving up. PipeWire can stop invoking the process callback while the
/// device is suspended or removed, in which case `mainloop.run()` never returns
/// and an unbounded `join()` would hang the audio worker forever.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);

/// Maximum time the constructor waits for PipeWire to report a connected,
/// negotiated stream. This wait belongs to the audio-worker startup boundary,
/// never to the RT process callback or the UI/Tokio paths.
const STARTUP_TIMEOUT: Duration = Duration::from_millis(200);
/// The first process callback can arrive before WirePlumber finishes selecting
/// and linking the requested target. Keep the stream alive for this short
/// settling window so a later `defined target not found` error rejects a
/// replacement before it can displace the working sink.
const STARTUP_STABILITY: Duration = Duration::from_millis(100);

/// Number of fixed sample buffers owned by one sink. The engine produces 512
/// frame chunks, so this preserves the old 32-chunk queue depth while allowing
/// the self-test's larger chunks to be split without allocating in the RT
/// callback.
const SAMPLE_POOL_SLOTS: usize = 32;

/// Maximum number of frames copied into one pool buffer. Every buffer is
/// allocated once, before the PipeWire thread starts, and reused thereafter.
const SAMPLE_BUFFER_FRAMES: usize = 1024;

/// Signals that the PipeWire thread has actually returned, so the owner can
/// `join` without risking an indefinite block.
///
/// `run_stream_thread` sets this on every exit path (including the early
/// `return`s on setup failure). If the thread is stuck in `mainloop.run()`
/// because the callback stopped firing, the flag is never set and the owner
/// stops waiting after [`SHUTDOWN_TIMEOUT`].
#[derive(Default)]
struct ThreadExited {
    signal: Mutex<bool>,
    condvar: Condvar,
}

impl ThreadExited {
    /// Block until the thread returns or the timeout elapses. Returns whether
    /// the thread actually exited (so the caller knows if a `join` is safe).
    fn wait(&self, timeout: Duration) -> bool {
        let exited = self.signal.lock().unwrap_or_else(|e| e.into_inner());
        if *exited {
            return true;
        }
        // Re-check the flag after `wait_timeout` returns, whether it woke early
        // (flag set) or on the timeout (flag still false).
        let (guard, _result) = self
            .condvar
            .wait_timeout(exited, timeout)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }

    /// Mark the thread as exited and wake any waiter.
    fn mark_exited(&self) {
        let mut exited = self.signal.lock().unwrap_or_else(|e| e.into_inner());
        *exited = true;
        self.condvar.notify_all();
    }
}

/// A typed failure raised when a PipeWire stream cannot prove readiness.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PipeWireStartupError {
    #[error("PipeWire stream startup failed: {0}")]
    Failed(String),
    #[error("PipeWire stream did not become connected within {timeout:?}")]
    Timeout { timeout: Duration },
}

#[derive(Debug, Default)]
enum StartupStatus {
    #[default]
    Pending,
    Failed(String),
    TimedOut,
}

#[derive(Debug, PartialEq, Eq)]
enum StartupWait {
    Connected,
    Failed(String),
    TimedOut,
}

/// Synchronization owned by the sink constructor and its PipeWire thread.
///
/// The callback publishes one terminal startup result. The constructor waits
/// only at its controlled startup boundary, while the process callback never
/// touches this mutex or condition variable.
#[derive(Debug, Default)]
struct StartupReadiness {
    status: Mutex<StartupStatus>,
    process_started: AtomicBool,
}

impl StartupReadiness {
    fn mark_failed(&self, message: impl Into<String>) {
        self.complete(StartupStatus::Failed(message.into()));
    }

    fn mark_failed_if_pending(&self, message: &str) {
        self.complete(StartupStatus::Failed(message.to_string()));
    }

    /// Wait for the first real-time process callback. A stream state of
    /// Paused/Streaming only proves that format negotiation completed; an
    /// unlinked stream can reach that state without producing audio.
    fn wait_for_process(&self, timeout: Duration) -> StartupWait {
        self.wait_for_process_stable(timeout, STARTUP_STABILITY)
    }

    fn wait_for_process_stable(&self, timeout: Duration, stability: Duration) -> StartupWait {
        let deadline = Instant::now().checked_add(timeout);
        let mut process_started_at = None;
        loop {
            {
                let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
                match &*status {
                    StartupStatus::Failed(message) => {
                        return StartupWait::Failed(message.clone());
                    }
                    StartupStatus::TimedOut => return StartupWait::TimedOut,
                    StartupStatus::Pending => {}
                }
            }
            if self.process_started.load(Ordering::Acquire) {
                let started_at = *process_started_at.get_or_insert_with(Instant::now);
                if started_at.elapsed() >= stability {
                    return StartupWait::Connected;
                }
            }
            let Some(deadline) = deadline else {
                self.complete(StartupStatus::TimedOut);
                return StartupWait::TimedOut;
            };
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                self.complete(StartupStatus::TimedOut);
                return StartupWait::TimedOut;
            };
            if remaining.is_zero() {
                self.complete(StartupStatus::TimedOut);
                return StartupWait::TimedOut;
            }
            std::thread::sleep(Duration::from_millis(1).min(remaining));
        }
    }

    fn mark_process_started(&self) {
        self.process_started.store(true, Ordering::Release);
    }

    fn complete(&self, next: StartupStatus) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(&*status, StartupStatus::Pending) {
            *status = next;
        }
    }
}

/// Converts an unexpected thread return before readiness into a startup error.
struct StartupExitGuard(Arc<StartupReadiness>);

impl Drop for StartupExitGuard {
    fn drop(&mut self) {
        self.0
            .mark_failed_if_pending("PipeWire startup thread exited before connection was proven");
    }
}

/// RAII guard that flags [`ThreadExited`] when the PipeWire thread returns.
struct ExitGuard(Arc<ThreadExited>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.mark_exited();
    }
}

/// Marks the sample pool disconnected on every PipeWire thread exit. This
/// preserves the old channel-disconnect signal without requiring the RT path
/// to drop an owned channel message.
struct PoolExitGuard(Arc<SamplePool>);

impl Drop for PoolExitGuard {
    fn drop(&mut self) {
        self.0.mark_disconnected();
    }
}

/// Commands sent from the owner to the PipeWire thread.
pub enum Control {
    /// Drop queued samples so a seek is heard immediately.
    Flush {
        /// Monotonic marker published only after all pre-flush queue state is
        /// discarded by the PipeWire thread.
        generation: FlushGeneration,
    },
    /// Deactivate the stream (pause output) until [`Control::Resume`].
    Pause,
    /// Reactivate a paused stream.
    Resume,
    /// Quit the PipeWire thread.
    Shutdown,
}

/// Observable completion marker for an asynchronous sink flush.
///
/// The audio worker never waits for this marker. It temporarily stops
/// submitting samples and resumes only after the PipeWire callback publishes
/// the matching generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushGeneration(pub(crate) u64);

/// Result of submitting one decoded sample chunk to the bounded sink queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushSamplesResult {
    /// The sink copied the sample chunk into its reusable buffer pool.
    Accepted,
    /// The sink is alive but its bounded queue is full. The caller must retry
    /// the same chunk without advancing its source accounting.
    Backpressure,
    /// The sink thread or channel is gone. The caller must freeze playback.
    Disconnected,
}

/// A value transferred through one of the single-producer/single-consumer
/// rings. It contains no owning allocation, so receiving it in the process
/// callback cannot drop or deallocate anything.
#[derive(Clone, Copy, Debug)]
struct ReadySample {
    slot: usize,
    len: usize,
}

/// A bounded lock-free SPSC ring for `Copy` values.
///
/// The worker is the only producer and the PipeWire process callback is the
/// only consumer of `ready`. Acquire/release ordering publishes a completely
/// written buffer before its slot index becomes visible to the other thread.
struct SpscRing<T: Copy, const N: usize> {
    values: [UnsafeCell<MaybeUninit<T>>; N],
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl<T: Copy, const N: usize> SpscRing<T, N> {
    fn new() -> Self {
        assert!(N > 0, "an SPSC ring must have a non-zero capacity");
        Self {
            values: std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    fn push(&self, value: T) -> Result<(), T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= N {
            return Err(value);
        }

        // Only the producer writes this slot until the release store above
        // publishes it; only the consumer reads it after the acquire load.
        unsafe {
            (*self.values[head % N].get()).write(value);
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    fn pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }

        let value = unsafe { (*self.values[tail % N].get()).assume_init_read() };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

// The ring's SPSC protocol, rather than a Rust borrow, provides the exclusive
// access to each UnsafeCell slot. T is Copy, so an occupied ring cell has no
// destructor that could be skipped when the ring is dropped.
unsafe impl<T: Copy + Send, const N: usize> Send for SpscRing<T, N> {}
unsafe impl<T: Copy + Send, const N: usize> Sync for SpscRing<T, N> {}

/// A fixed-node lock-free stack for slot indices.
///
/// The worker is the only consumer of the free list, while both the worker's
/// rollback path and the PipeWire process callback can return slots. A Treiber
/// stack is sufficient here: nodes are removed by one consumer only, so a
/// popped node cannot be reused by a producer while the consumer still has a
/// compare-and-swap in flight. The stack performs no allocation and takes no
/// lock on the process callback path.
struct AtomicFreeStack<const N: usize> {
    next: [AtomicUsize; N],
    head: AtomicUsize,
}

const EMPTY_STACK_INDEX: usize = 0;

impl<const N: usize> AtomicFreeStack<N> {
    fn new() -> Self {
        assert!(N > 0, "a free stack must have a non-zero capacity");
        Self {
            next: std::array::from_fn(|_| AtomicUsize::new(EMPTY_STACK_INDEX)),
            head: AtomicUsize::new(EMPTY_STACK_INDEX),
        }
    }

    fn push(&self, slot: usize) {
        assert!(slot < N, "free-list slot index must be in range");
        let node = slot + 1;
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            self.next[slot].store(head, Ordering::Relaxed);
            match self
                .head
                .compare_exchange_weak(head, node, Ordering::Release, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(observed) => head = observed,
            }
        }
    }

    fn pop(&self) -> Option<usize> {
        let mut head = self.head.load(Ordering::Acquire);
        loop {
            if head == EMPTY_STACK_INDEX {
                return None;
            }
            let slot = head - 1;
            let next = self.next[slot].load(Ordering::Relaxed);
            match self
                .head
                .compare_exchange_weak(head, next, Ordering::Acquire, Ordering::Acquire)
            {
                Ok(_) => return Some(slot),
                Err(observed) => head = observed,
            }
        }
    }
}

unsafe impl<const N: usize> Send for AtomicFreeStack<N> {}
unsafe impl<const N: usize> Sync for AtomicFreeStack<N> {}

struct SampleSlot {
    samples: UnsafeCell<Vec<f32>>,
}

impl SampleSlot {
    fn new(samples: usize) -> Self {
        Self {
            samples: UnsafeCell::new(vec![0.0; samples]),
        }
    }
}

// A slot is accessed by exactly one side of the ownership ring at a time.
unsafe impl Send for SampleSlot {}
unsafe impl Sync for SampleSlot {}

/// Preallocated ownership pool between the engine worker and the RT callback.
///
/// No sample `Vec` crosses the callback boundary. The worker acquires free
/// slots, copies into their already allocated storage, and publishes only
/// `(slot, length)` descriptors. The callback returns descriptors to the free
/// ring after copying their samples into its pending deque.
struct SamplePool {
    slots: Box<[SampleSlot; SAMPLE_POOL_SLOTS]>,
    free: AtomicFreeStack<SAMPLE_POOL_SLOTS>,
    ready: SpscRing<ReadySample, SAMPLE_POOL_SLOTS>,
    samples_per_slot: usize,
    /// `PipeWireSink::push_samples` is normally called by one worker, but the
    /// public sink is shareable. Serialize accidental extra producers here so
    /// the RT-facing rings remain genuinely SPSC; this mutex is never touched
    /// by the process callback.
    producer: Mutex<()>,
    connected: AtomicBool,
    #[cfg(test)]
    rollback_barrier: std::sync::OnceLock<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    publish_barrier: std::sync::OnceLock<Arc<std::sync::Barrier>>,
}

impl SamplePool {
    fn new(channels: u32) -> Self {
        let samples_per_slot = SAMPLE_BUFFER_FRAMES
            .checked_mul(channels as usize)
            .expect("validated channel count must fit the pool size");
        let free = AtomicFreeStack::new();
        let pool = Self {
            slots: Box::new(std::array::from_fn(|_| SampleSlot::new(samples_per_slot))),
            free,
            ready: SpscRing::new(),
            samples_per_slot,
            producer: Mutex::new(()),
            connected: AtomicBool::new(true),
            #[cfg(test)]
            rollback_barrier: std::sync::OnceLock::new(),
            #[cfg(test)]
            publish_barrier: std::sync::OnceLock::new(),
        };
        for slot in 0..SAMPLE_POOL_SLOTS {
            pool.free.push(slot);
        }
        pool
    }

    /// Copy one producer chunk into one or more preallocated slots.
    ///
    /// If the pool cannot reserve every required slot, all reservations are
    /// returned and the original borrowed chunk remains untouched from the
    /// caller's perspective. `Backpressure` is therefore explicit and safe:
    /// the caller retries the same chunk, just as it did with the old bounded
    /// channel.
    fn push_samples(&self, samples: &[f32]) -> PushSamplesResult {
        if samples.is_empty() {
            return PushSamplesResult::Accepted;
        }
        let _producer = self.producer.lock().unwrap_or_else(|e| e.into_inner());
        if !self.connected.load(Ordering::Acquire) {
            return PushSamplesResult::Disconnected;
        }

        let required = samples.len().div_ceil(self.samples_per_slot);
        if required > SAMPLE_POOL_SLOTS {
            // A single API call cannot be represented by the bounded pool.
            // Do not partially accept it: the caller keeps ownership and can
            // retry after using a smaller chunk.
            return PushSamplesResult::Backpressure;
        }

        let mut reserved = [0usize; SAMPLE_POOL_SLOTS];
        for count in 0..required {
            if !self.connected.load(Ordering::Acquire) {
                for slot in reserved.into_iter().take(count) {
                    self.release(slot);
                }
                return PushSamplesResult::Disconnected;
            }
            let Some(slot) = self.free.pop() else {
                #[cfg(test)]
                if let Some(barrier) = self.rollback_barrier.get() {
                    barrier.wait();
                }
                for slot in reserved.into_iter().take(count) {
                    self.release(slot);
                }
                return PushSamplesResult::Backpressure;
            };
            reserved[count] = slot;
        }

        #[cfg(test)]
        if let Some(barrier) = self.publish_barrier.get() {
            barrier.wait();
        }

        for (count, chunk) in samples.chunks(self.samples_per_slot).enumerate() {
            let slot = reserved[count];
            unsafe {
                (&mut *self.slots[slot].samples.get())[..chunk.len()].copy_from_slice(chunk);
            }
            self.ready
                .push(ReadySample {
                    slot,
                    len: chunk.len(),
                })
                .expect("reserved slots must fit in the ready ring");
        }
        PushSamplesResult::Accepted
    }

    fn pop_ready(&self) -> Option<ReadySample> {
        self.ready.pop()
    }

    fn samples(&self, ready: ReadySample) -> &[f32] {
        // The ready ring gives the callback exclusive access to this slot.
        unsafe { &(&*self.slots[ready.slot].samples.get())[..ready.len] }
    }

    fn release(&self, slot: usize) {
        self.free.push(slot);
    }

    fn discard_queued(&self) {
        while let Some(ready) = self.ready.pop() {
            self.release(ready.slot);
        }
    }

    fn mark_disconnected(&self) {
        // This is the lifecycle boundary, not the RT callback. Serializing it
        // with publication gives every push a clear order: it either finishes
        // before disconnect, or observes `connected == false` and publishes
        // nothing. A push that wins the mutex immediately before disconnect
        // can be accepted and then reclaimed below, because `Accepted` means
        // copied before the lifecycle boundary, not guaranteed to be played.
        // The callback never takes this mutex.
        let _producer = self.producer.lock().unwrap_or_else(|e| e.into_inner());
        self.connected.store(false, Ordering::Release);
        // The process callback has already stopped before PoolExitGuard runs,
        // so the ready ring has no consumer left. Reclaim descriptors that a
        // producer published just before the disconnect boundary.
        self.discard_queued();
    }

    #[cfg(test)]
    fn set_rollback_barrier(&self, barrier: Arc<std::sync::Barrier>) {
        assert!(self.rollback_barrier.set(barrier).is_ok());
    }

    #[cfg(test)]
    fn set_publish_barrier(&self, barrier: Arc<std::sync::Barrier>) {
        assert!(self.publish_barrier.set(barrier).is_ok());
    }

    #[cfg(test)]
    fn slot_ptr(&self, slot: usize) -> *const f32 {
        unsafe { (&*self.slots[slot].samples.get()).as_ptr() }
    }
}

/// A live PipeWire output stream fed with interleaved `f32` samples.
pub struct PipeWireSink {
    samples: Arc<SamplePool>,
    control: Sender<Control>,
    handle: Option<JoinHandle<()>>,
    rate: u32,
    channels: u32,
    stream_info: String,
    /// Frames actually written to the PipeWire stream (consumed), shared with
    /// the producer so the reported position tracks real playback, not the
    /// amount queued ahead in the buffer.
    frames_played: Arc<AtomicU64>,
    /// Linear master gain read by the real-time process callback at pack time.
    gain: Arc<AtomicU32>,
    /// Next worker-visible flush generation.
    next_flush_generation: AtomicU64,
    /// Last flush generation fully applied by the PipeWire callback.
    acknowledged_flush_generation: Arc<AtomicU64>,
    /// Signals when the PipeWire thread has actually exited, so `shutdown`
    /// can bound its wait instead of hanging on a dead device.
    exited: Arc<ThreadExited>,
}

impl PipeWireSink {
    /// Open a PipeWire output stream for the given format and target.
    ///
    /// `target` is a stable PipeWire `node.name`; `None` lets the session
    /// default decide. On any PipeWire failure this returns `Err` so the
    /// caller can fall back rather than silently losing audio.
    pub fn new(rate: u32, channels: u32, target: Option<&str>) -> anyhow::Result<Self> {
        if rate == 0 || channels == 0 || channels > spa::param::audio::MAX_CHANNELS as u32 {
            anyhow::bail!("invalid output format: {rate} Hz, {channels} ch");
        }
        let channel_position = channel_positions(channels)?;

        let samples = Arc::new(SamplePool::new(channels));
        let thread_samples = samples.clone();
        let (ctl_tx, ctl_rx) = channel::<Control>();
        let frames_played = Arc::new(AtomicU64::new(0));
        let thread_frames = frames_played.clone();
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let thread_gain = gain.clone();
        let acknowledged_flush_generation = Arc::new(AtomicU64::new(0));
        let thread_flush_ack = acknowledged_flush_generation.clone();
        let exited = Arc::new(ThreadExited::default());
        let thread_exited = exited.clone();
        let startup = Arc::new(StartupReadiness::default());
        let thread_startup = startup.clone();
        let target = target.map(str::to_owned);

        let stream_info = format!("sample format: F32, {rate} Hz, {channels} ch");
        let handle = std::thread::Builder::new()
            .name("harmonium-pipewire".to_string())
            .spawn(move || {
                run_stream_thread(
                    rate,
                    channels,
                    target.as_deref(),
                    channel_position,
                    thread_samples,
                    ctl_rx,
                    thread_frames,
                    thread_gain,
                    thread_flush_ack,
                    thread_exited,
                    thread_startup,
                )
            })
            .map_err(|error| anyhow::anyhow!("spawning pipewire thread failed: {error}"))?;

        match startup.wait_for_process(STARTUP_TIMEOUT) {
            StartupWait::Connected => {}
            StartupWait::Failed(message) => {
                shutdown_thread(&ctl_tx, handle, &exited);
                return Err(anyhow::Error::new(PipeWireStartupError::Failed(message)));
            }
            StartupWait::TimedOut => {
                shutdown_thread(&ctl_tx, handle, &exited);
                return Err(anyhow::Error::new(PipeWireStartupError::Timeout {
                    timeout: STARTUP_TIMEOUT,
                }));
            }
        }

        Ok(Self {
            samples,
            control: ctl_tx,
            handle: Some(handle),
            rate,
            channels,
            stream_info,
            frames_played,
            gain,
            next_flush_generation: AtomicU64::new(0),
            acknowledged_flush_generation,
            exited,
        })
    }

    /// Push interleaved samples without waiting for a non-consuming sink.
    pub fn push_samples(&self, samples: &[f32]) -> PushSamplesResult {
        if samples.is_empty() {
            return PushSamplesResult::Accepted;
        }
        self.samples.push_samples(samples)
    }

    /// Set the linear master gain without rebuilding the stream or touching
    /// the callback's queue and control paths.
    pub fn set_gain(&self, gain: f32) {
        if gain.is_finite() && gain >= 0.0 {
            self.gain.store(gain.to_bits(), Ordering::Relaxed);
        }
    }

    /// Drop all queued (not yet played) samples so a seek is heard instantly.
    pub fn flush(&self) -> FlushGeneration {
        let generation = FlushGeneration(
            self.next_flush_generation
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1),
        );
        if self.control.send(Control::Flush { generation }).is_err() {
            // A dead control thread cannot retain playable stale audio. Mark
            // the request complete so the worker can observe the disconnect
            // through its normal nonblocking push path instead of waiting.
            self.acknowledged_flush_generation
                .store(generation.0, Ordering::Release);
        }
        generation
    }

    /// Return whether a flush has been applied, or the sink has disconnected.
    ///
    /// The disconnect branch is important when PipeWire stops invoking the
    /// process callback: the callback cannot publish an acknowledgement then,
    /// but the exit guard has already reclaimed the queued sample descriptors.
    pub fn flush_acknowledged(&self, generation: FlushGeneration) -> bool {
        flush_acknowledged(
            &self.acknowledged_flush_generation,
            &self.samples.connected,
            generation,
        )
    }

    /// Pause output (the stream stays connected but silent).
    pub fn pause(&self) {
        let _ = self.control.send(Control::Pause);
    }

    /// Resume output.
    pub fn resume(&self) {
        let _ = self.control.send(Control::Resume);
    }

    /// Shut the stream down and join its thread.
    ///
    /// The wait is bounded by [`SHUTDOWN_TIMEOUT`]: if PipeWire stops invoking
    /// the process callback (device suspended or removed) the thread may never
    /// observe the `Shutdown` command, so an unbounded `join()` would hang the
    /// audio worker. On timeout the handle is dropped (detaching the thread)
    /// rather than blocking forever.
    pub fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            shutdown_thread(&self.control, handle, &self.exited);
        }
    }

    /// The negotiated sample rate.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The negotiated channel count.
    pub fn channels(&self) -> u32 {
        self.channels
    }

    /// Human readable stream description (for diagnostics).
    pub fn description(&self) -> &str {
        &self.stream_info
    }

    /// Frames the stream has actually consumed (reproduced), for accurate
    /// position reporting.
    pub fn frames_played(&self) -> u64 {
        self.frames_played.load(Ordering::Acquire)
    }
}

/// Check a flush marker without waiting for the PipeWire callback.
fn flush_acknowledged(
    acknowledged_generation: &AtomicU64,
    connected: &AtomicBool,
    generation: FlushGeneration,
) -> bool {
    acknowledged_generation.load(Ordering::Acquire) >= generation.0
        || !connected.load(Ordering::Acquire)
}

impl Drop for PipeWireSink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Request a bounded shutdown for a thread that may be detached if PipeWire
/// stops dispatching callbacks. Ownership is consumed exactly once by the
/// caller, preventing a second teardown attempt after startup failure.
fn shutdown_thread(control: &Sender<Control>, handle: JoinHandle<()>, exited: &ThreadExited) {
    let _ = control.send(Control::Shutdown);
    if exited.wait(SHUTDOWN_TIMEOUT) {
        let _ = handle.join();
    } else {
        tracing::warn!(
            "pipewire thread did not exit within {:?}; detaching it",
            SHUTDOWN_TIMEOUT
        );
    }
}

/// Target backlog in frames kept in flight to cover any PipeWire quantum
/// without over-buffering (which would make the position drift backwards).
const PENDING_FRAMES: usize = 16384;

/// How many samples the pending deque can hold before it must grow.
fn pending_capacity(channels: u32) -> usize {
    PENDING_FRAMES.saturating_mul(channels as usize)
}

/// State owned by the PipeWire thread and passed to the process callback.
struct SinkData {
    samples: Arc<SamplePool>,
    control: Receiver<Control>,
    startup: Arc<StartupReadiness>,
    /// Shared linear gain; the process callback only performs a relaxed load.
    gain: Arc<AtomicU32>,
    pending: VecDeque<f32>,
    /// One ready slot that did not fit in the remaining preallocated pending
    /// capacity. It stays owned by the callback until a later callback drains
    /// enough samples; it is never dropped or allocated.
    deferred: Option<ReadySample>,
    acknowledged_flush_generation: Arc<AtomicU64>,
    paused: bool,
    // Drives the PipeWire event loop: a `Shutdown` command must quit the
    // loop, otherwise the thread never returns from `mainloop.run()` and the
    // owner's `shutdown()` deadlocks on `join()`.
    mainloop: pw::main_loop::MainLoopRc,
}

impl Drop for SinkData {
    fn drop(&mut self) {
        // A shutdown command can stop the event loop before the deferred slot
        // is consumed. Return every callback-owned descriptor while the
        // callback side still owns this state; PoolExitGuard then reclaims any
        // descriptor published concurrently before the lifecycle boundary.
        flush_samples(&self.samples, &mut self.pending, &mut self.deferred);
    }
}

/// Frame bytes for F32 interleaved samples.
fn frame_bytes(channels: u32) -> usize {
    (channels as usize) * std::mem::size_of::<f32>()
}

/// Number of whole frames that fit into `buffer_bytes`.
fn frames_for(buffer_bytes: usize, frame_bytes: usize) -> usize {
    if frame_bytes == 0 {
        return 0;
    }
    buffer_bytes / frame_bytes
}

/// Pack complete interleaved `f32` frames into a F32LE byte slice, up to
/// `frames`.
///
/// `pending` is a sample stream, not a frame queue: complete frames are
/// removed as they are packed, while a trailing partial frame remains pending
/// so a later chunk can complete it. The engine drops a source-terminal partial
/// frame before submission because no later chunk can complete it. `gain` is
/// loaded once per buffer so control changes affect already queued samples
/// without adding callback synchronization or allocation.
fn pack_f32le(
    out: &mut [u8],
    channels: usize,
    frames: usize,
    pending: &mut VecDeque<f32>,
    gain: &AtomicU32,
) -> usize {
    if channels == 0 {
        return 0;
    }
    let gain = f32::from_bits(gain.load(Ordering::Relaxed));
    let mut written = 0;
    let mut byte = 0;
    while written < frames && pending.len() >= channels {
        for _ in 0..channels {
            let sample = pending
                .pop_front()
                .expect("pending length checked before packing a frame");
            out[byte..byte + 4].copy_from_slice(&(sample * gain).to_le_bytes());
            byte += 4;
        }
        written += 1;
    }
    written
}

/// Pack source samples and account only for complete frames written.
fn pack_f32le_and_count(
    out: &mut [u8],
    channels: usize,
    frames: usize,
    pending: &mut VecDeque<f32>,
    gain: &AtomicU32,
    frames_played: &AtomicU64,
) -> usize {
    let written = pack_f32le(out, channels, frames, pending, gain);
    frames_played.fetch_add(written as u64, Ordering::Relaxed);
    written
}

/// Move ready pooled buffers into `pending` without ever exceeding its fixed
/// capacity. A buffer that does not fit stays deferred until a later callback
/// has packed enough complete frames.
fn append_available_samples(
    samples: &SamplePool,
    pending: &mut VecDeque<f32>,
    deferred: &mut Option<ReadySample>,
    capacity: usize,
) {
    loop {
        let Some(ready) = deferred.take().or_else(|| samples.pop_ready()) else {
            return;
        };
        let remaining = capacity.saturating_sub(pending.len());
        if ready.len > remaining {
            *deferred = Some(ready);
            return;
        }
        pending.extend(samples.samples(ready));
        samples.release(ready.slot);
    }
}

/// Flush all callback-owned sample state and return every queued pool slot.
fn flush_samples(
    samples: &SamplePool,
    pending: &mut VecDeque<f32>,
    deferred: &mut Option<ReadySample>,
) {
    pending.clear();
    if let Some(ready) = deferred.take() {
        samples.release(ready.slot);
    }
    samples.discard_queued();
}

/// Return the explicit SPA channel layout for the interleaved engine order.
///
/// The position at index `n` describes sample `n` in every interleaved frame.
/// Counts above eight are rejected instead of negotiating unknown positions,
/// because a channel count alone cannot safely identify a larger layout.
fn channel_positions(channels: u32) -> anyhow::Result<[u32; spa::param::audio::MAX_CHANNELS]> {
    if channels == 0 || channels > spa::param::audio::MAX_CHANNELS as u32 {
        anyhow::bail!("invalid output channel count: {channels}");
    }

    let layout: &[u32] = match channels {
        1 => &[spa_sys::SPA_AUDIO_CHANNEL_MONO],
        2 => &[spa_sys::SPA_AUDIO_CHANNEL_FL, spa_sys::SPA_AUDIO_CHANNEL_FR],
        3 => &[
            spa_sys::SPA_AUDIO_CHANNEL_FL,
            spa_sys::SPA_AUDIO_CHANNEL_FR,
            spa_sys::SPA_AUDIO_CHANNEL_FC,
        ],
        4 => &[
            spa_sys::SPA_AUDIO_CHANNEL_FL,
            spa_sys::SPA_AUDIO_CHANNEL_FR,
            spa_sys::SPA_AUDIO_CHANNEL_RL,
            spa_sys::SPA_AUDIO_CHANNEL_RR,
        ],
        5 => &[
            spa_sys::SPA_AUDIO_CHANNEL_FL,
            spa_sys::SPA_AUDIO_CHANNEL_FR,
            spa_sys::SPA_AUDIO_CHANNEL_FC,
            spa_sys::SPA_AUDIO_CHANNEL_SL,
            spa_sys::SPA_AUDIO_CHANNEL_SR,
        ],
        6 => &[
            spa_sys::SPA_AUDIO_CHANNEL_FL,
            spa_sys::SPA_AUDIO_CHANNEL_FR,
            spa_sys::SPA_AUDIO_CHANNEL_FC,
            spa_sys::SPA_AUDIO_CHANNEL_LFE,
            spa_sys::SPA_AUDIO_CHANNEL_SL,
            spa_sys::SPA_AUDIO_CHANNEL_SR,
        ],
        7 => &[
            spa_sys::SPA_AUDIO_CHANNEL_FL,
            spa_sys::SPA_AUDIO_CHANNEL_FR,
            spa_sys::SPA_AUDIO_CHANNEL_FC,
            spa_sys::SPA_AUDIO_CHANNEL_LFE,
            spa_sys::SPA_AUDIO_CHANNEL_RC,
            spa_sys::SPA_AUDIO_CHANNEL_SL,
            spa_sys::SPA_AUDIO_CHANNEL_SR,
        ],
        8 => &[
            spa_sys::SPA_AUDIO_CHANNEL_FL,
            spa_sys::SPA_AUDIO_CHANNEL_FR,
            spa_sys::SPA_AUDIO_CHANNEL_FC,
            spa_sys::SPA_AUDIO_CHANNEL_LFE,
            spa_sys::SPA_AUDIO_CHANNEL_RL,
            spa_sys::SPA_AUDIO_CHANNEL_RR,
            spa_sys::SPA_AUDIO_CHANNEL_SL,
            spa_sys::SPA_AUDIO_CHANNEL_SR,
        ],
        _ => anyhow::bail!(
            "unsupported output channel layout: {channels} channels (supported: 1 through 8)"
        ),
    };

    let mut positions = [0u32; spa::param::audio::MAX_CHANNELS];
    positions[..layout.len()].copy_from_slice(layout);
    Ok(positions)
}

/// Connect to PipeWire and run the output stream until shutdown.
const NODE_DONT_FALLBACK: &str = "node.dont-fallback";

fn stream_properties(channels: u32, target: Option<&str>) -> pw::properties::PropertiesBox {
    let mut properties = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::AUDIO_CHANNELS => channels.to_string(),
    };
    if let Some(target) = target {
        // PipeWire's numeric pw_stream_connect target argument is deprecated;
        // WirePlumber selects the requested sink from target.object instead.
        properties.insert("target.object", target);
        properties.insert("node.dont-move", "true");
        // Do not silently route to the default sink when the requested target
        // disappeared between enumeration and stream creation.
        properties.insert(NODE_DONT_FALLBACK, "true");
    }
    properties
}

fn run_stream_thread(
    rate: u32,
    channels: u32,
    target: Option<&str>,
    channel_position: [u32; spa::param::audio::MAX_CHANNELS],
    samples: Arc<SamplePool>,
    ctl_rx: Receiver<Control>,
    frames_played: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
    acknowledged_flush_generation: Arc<AtomicU64>,
    exited: Arc<ThreadExited>,
    startup: Arc<StartupReadiness>,
) {
    // Notify the owner when this thread returns, on every exit path. If it is
    // stuck in `mainloop.run()` the flag is never set and the owner's bounded
    // wait expires — which is exactly the no-callback case we need to survive.
    let _startup_exit_guard = StartupExitGuard(startup.clone());
    let _exit_guard = ExitGuard(exited);
    let _pool_exit_guard = PoolExitGuard(samples.clone());

    pw::init();
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(mainloop) => mainloop,
        Err(error) => {
            let message = format!("could not create the PipeWire main loop: {error}");
            tracing::error!("{message}");
            startup.mark_failed(message);
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(context) => context,
        Err(error) => {
            let message = format!("could not create the PipeWire context: {error}");
            tracing::error!("{message}");
            startup.mark_failed(message);
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(core) => core,
        Err(error) => {
            let message = format!("could not connect to the PipeWire core: {error}");
            tracing::error!("{message}");
            startup.mark_failed(message);
            return;
        }
    };

    let properties = stream_properties(channels, target);
    let stream = match pw::stream::StreamBox::new(&core, "harmonium-output", properties) {
        Ok(stream) => stream,
        Err(error) => {
            let message = format!("could not create the PipeWire output stream: {error}");
            tracing::error!("{message}");
            startup.mark_failed(message);
            return;
        }
    };

    let user = SinkData {
        samples,
        control: ctl_rx,
        startup: startup.clone(),
        gain,
        // Pre-allocate the full target backlog so the process callback (which
        // runs in the real-time audio thread) never has to reallocate the
        // deque while pulling samples.
        pending: VecDeque::with_capacity(pending_capacity(channels)),
        deferred: None,
        acknowledged_flush_generation,
        paused: false,
        mainloop: mainloop.clone(),
    };
    let _listener = match stream
        .add_local_listener_with_user_data(user)
        .state_changed(|_, data, _, state| match state {
            // PipeWire reports Paused/Streaming after format negotiation, but
            // that does not prove the stream has a live target link. Startup
            // readiness is published by the first process callback instead.
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {}
            pw::stream::StreamState::Error(error) => {
                data.startup.mark_failed(if error.is_empty() {
                    "PipeWire reported an unknown stream error".to_string()
                } else {
                    error
                });
                data.mainloop.quit();
            }
            pw::stream::StreamState::Unconnected | pw::stream::StreamState::Connecting => {}
        })
        .process(move |stream, data| {
            fill_buffer(stream, data, channels, &frames_played);
        })
        .register()
    {
        Ok(listener) => listener,
        Err(error) => {
            let message = format!("could not register the PipeWire stream listener: {error}");
            tracing::error!("{message}");
            startup.mark_failed(message);
            return;
        }
    };

    // Build the negotiated format and connect the output stream. The target is
    // selected by the target.object property above; passing a numeric global
    // node id here is deprecated and does not reliably select a sink.
    let mut audio = spa::param::audio::AudioInfoRaw::new();
    audio.set_format(spa::param::audio::AudioFormat::F32LE);
    audio.set_rate(rate);
    audio.set_channels(channels);
    audio.set_position(channel_position);

    let (cursor, _) = match pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: spa_sys::SPA_TYPE_OBJECT_Format,
            id: spa_sys::SPA_PARAM_EnumFormat,
            properties: audio.into(),
        }),
    ) {
        Ok(serialized) => serialized,
        Err(error) => {
            let message = format!("format pod serialization failed: {error}");
            tracing::error!("{message}");
            startup.mark_failed(message);
            return;
        }
    };
    let value = cursor.into_inner();
    let Some(pod) = pw::spa::pod::Pod::from_bytes(&value) else {
        tracing::error!("format pod is invalid");
        startup.mark_failed("format pod is invalid");
        return;
    };

    let mut params = [pod];
    if let Err(error) = stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    ) {
        let message = format!("could not connect the PipeWire output stream: {error}");
        tracing::error!("{message}");
        startup.mark_failed(message);
        return;
    }

    mainloop.run();
}

/// Drain control commands and write samples into the stream buffer.
fn fill_buffer(
    stream: &pw::stream::Stream,
    data: &mut SinkData,
    channels: u32,
    frames_played: &AtomicU64,
) {
    data.startup.mark_process_started();
    // Handle pending control commands first.
    while let Ok(command) = data.control.try_recv() {
        match command {
            Control::Flush { generation } => {
                flush_samples(&data.samples, &mut data.pending, &mut data.deferred);
                data.acknowledged_flush_generation
                    .store(generation.0, Ordering::Release);
            }
            Control::Pause => data.paused = true,
            Control::Resume => data.paused = false,
            Control::Shutdown => {
                // Quit the event loop so `mainloop.run()` returns and the
                // thread can be joined; a plain `return` here only leaves the
                // process callback and the loop keeps spinning.
                data.mainloop.quit();
                return;
            }
        }
    }

    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    if datas.is_empty() {
        return;
    }
    let data_ref = &mut datas[0];
    let Some(slice) = data_ref.data() else {
        return;
    };

    // Pull decoded samples from the lock-free pool if the local queue ran dry.
    // Keep enough in flight (up to PENDING_FRAMES) to satisfy any PipeWire
    // quantum without over-buffering, which would make the position drift
    // backwards. A chunk is admitted only when its complete length fits in the
    // remaining capacity; otherwise it is deferred instead of making `extend`
    // grow the deque in the RT callback.
    append_available_samples(
        &data.samples,
        &mut data.pending,
        &mut data.deferred,
        pending_capacity(channels),
    );

    let channels_usize = channels as usize;
    let stride = frame_bytes(channels);
    let n_frames = frames_for(slice.len(), stride);
    let written = if data.paused {
        0
    } else {
        pack_f32le_and_count(
            slice,
            channels_usize,
            n_frames,
            &mut data.pending,
            &data.gain,
            frames_played,
        )
    };

    // Always fill the whole quantum so PipeWire never sees a short buffer
    // (which would underrun and make the position stutter). Samples beyond the
    // source are silence.
    if written < n_frames {
        let start = written * stride;
        for byte in slice[start..n_frames * stride].iter_mut() {
            *byte = 0;
        }
    }

    let chunk = data_ref.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as i32;
    *chunk.size_mut() = (stride * n_frames) as u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    fn unity_gain() -> AtomicU32 {
        AtomicU32::new(1.0f32.to_bits())
    }

    #[test]
    fn frame_calculation_rounds_down() {
        let stride = frame_bytes(2);
        assert_eq!(stride, 8);
        assert_eq!(frames_for(20, stride), 2);
        assert_eq!(frames_for(0, stride), 0);
    }

    #[test]
    fn channel_positions_match_supported_interleaved_layouts() {
        let layouts = [
            (1, &[spa_sys::SPA_AUDIO_CHANNEL_MONO][..]),
            (
                2,
                &[spa_sys::SPA_AUDIO_CHANNEL_FL, spa_sys::SPA_AUDIO_CHANNEL_FR][..],
            ),
            (
                3,
                &[
                    spa_sys::SPA_AUDIO_CHANNEL_FL,
                    spa_sys::SPA_AUDIO_CHANNEL_FR,
                    spa_sys::SPA_AUDIO_CHANNEL_FC,
                ][..],
            ),
            (
                4,
                &[
                    spa_sys::SPA_AUDIO_CHANNEL_FL,
                    spa_sys::SPA_AUDIO_CHANNEL_FR,
                    spa_sys::SPA_AUDIO_CHANNEL_RL,
                    spa_sys::SPA_AUDIO_CHANNEL_RR,
                ][..],
            ),
            (
                5,
                &[
                    spa_sys::SPA_AUDIO_CHANNEL_FL,
                    spa_sys::SPA_AUDIO_CHANNEL_FR,
                    spa_sys::SPA_AUDIO_CHANNEL_FC,
                    spa_sys::SPA_AUDIO_CHANNEL_SL,
                    spa_sys::SPA_AUDIO_CHANNEL_SR,
                ][..],
            ),
            (
                6,
                &[
                    spa_sys::SPA_AUDIO_CHANNEL_FL,
                    spa_sys::SPA_AUDIO_CHANNEL_FR,
                    spa_sys::SPA_AUDIO_CHANNEL_FC,
                    spa_sys::SPA_AUDIO_CHANNEL_LFE,
                    spa_sys::SPA_AUDIO_CHANNEL_SL,
                    spa_sys::SPA_AUDIO_CHANNEL_SR,
                ][..],
            ),
            (
                7,
                &[
                    spa_sys::SPA_AUDIO_CHANNEL_FL,
                    spa_sys::SPA_AUDIO_CHANNEL_FR,
                    spa_sys::SPA_AUDIO_CHANNEL_FC,
                    spa_sys::SPA_AUDIO_CHANNEL_LFE,
                    spa_sys::SPA_AUDIO_CHANNEL_RC,
                    spa_sys::SPA_AUDIO_CHANNEL_SL,
                    spa_sys::SPA_AUDIO_CHANNEL_SR,
                ][..],
            ),
            (
                8,
                &[
                    spa_sys::SPA_AUDIO_CHANNEL_FL,
                    spa_sys::SPA_AUDIO_CHANNEL_FR,
                    spa_sys::SPA_AUDIO_CHANNEL_FC,
                    spa_sys::SPA_AUDIO_CHANNEL_LFE,
                    spa_sys::SPA_AUDIO_CHANNEL_RL,
                    spa_sys::SPA_AUDIO_CHANNEL_RR,
                    spa_sys::SPA_AUDIO_CHANNEL_SL,
                    spa_sys::SPA_AUDIO_CHANNEL_SR,
                ][..],
            ),
        ];

        for (channels, expected) in layouts {
            let actual = channel_positions(channels).expect("supported layout must be accepted");
            assert_eq!(&actual[..expected.len()], expected);
            assert!(
                actual[expected.len()..]
                    .iter()
                    .all(|position| *position == 0)
            );
        }
    }

    #[test]
    fn concrete_target_is_encoded_as_stable_target_object_without_fallback() {
        let properties = stream_properties(2, Some("alsa_output.pci-0000_13_00.1.hdmi-stereo"));

        assert_eq!(
            properties.get("target.object"),
            Some("alsa_output.pci-0000_13_00.1.hdmi-stereo")
        );
        assert_eq!(properties.get(NODE_DONT_FALLBACK), Some("true"));
        assert_eq!(properties.get("node.dont-move"), Some("true"));
    }

    #[test]
    fn default_target_leaves_session_routing_unconstrained() {
        let properties = stream_properties(2, None);

        assert_eq!(properties.get("target.object"), None);
        assert_eq!(properties.get(NODE_DONT_FALLBACK), None);
        assert_eq!(properties.get("node.dont-move"), None);
    }

    #[test]
    fn unsupported_channel_layout_is_rejected_without_pipewire() {
        let error = channel_positions(9)
            .expect_err("nine channels has no supported explicit layout")
            .to_string();
        assert!(error.contains("unsupported output channel layout"));
        assert!(error.contains("9 channels"));
        assert!(PipeWireSink::new(44_100, 9, None).is_err());
    }

    #[test]
    fn invalid_channel_counts_remain_rejected_without_pipewire() {
        assert!(channel_positions(0).is_err());
        assert!(channel_positions(spa::param::audio::MAX_CHANNELS as u32 + 1).is_err());
        assert!(PipeWireSink::new(44_100, 0, None).is_err());
        assert!(
            PipeWireSink::new(44_100, spa::param::audio::MAX_CHANNELS as u32 + 1, None).is_err()
        );
    }

    #[test]
    fn packs_interleaved_frames() {
        let mut pending = VecDeque::from([1.0f32, -2.0, 3.0, -4.0]);
        let mut out = [0u8; 16];
        let written = pack_f32le(&mut out, 2, 2, &mut pending, &unity_gain());
        assert_eq!(written, 2);
        // Verify frame 0 and 1 round trip.
        assert_eq!(f32::from_le_bytes(out[0..4].try_into().unwrap()), 1.0);
        assert_eq!(f32::from_le_bytes(out[4..8].try_into().unwrap()), -2.0);
        assert_eq!(f32::from_le_bytes(out[8..12].try_into().unwrap()), 3.0);
        assert_eq!(f32::from_le_bytes(out[12..16].try_into().unwrap()), -4.0);
        assert!(pending.is_empty());
    }

    #[test]
    fn packing_reads_the_current_atomic_gain_at_the_sink_boundary() {
        let gain = unity_gain();
        let mut pending = VecDeque::from([2.0f32, -4.0]);
        let mut out = [0u8; 8];

        assert_eq!(pack_f32le(&mut out, 2, 1, &mut pending, &gain), 1);
        assert_eq!(f32::from_le_bytes(out[0..4].try_into().unwrap()), 2.0);
        assert_eq!(f32::from_le_bytes(out[4..8].try_into().unwrap()), -4.0);

        gain.store(0.25f32.to_bits(), Ordering::Relaxed);
        pending.extend([8.0, -12.0]);
        assert_eq!(pack_f32le(&mut out, 2, 1, &mut pending, &gain), 1);
        assert_eq!(f32::from_le_bytes(out[0..4].try_into().unwrap()), 2.0);
        assert_eq!(f32::from_le_bytes(out[4..8].try_into().unwrap()), -3.0);
    }

    #[test]
    fn short_pending_reports_only_frames_actually_written() {
        // A buffer larger than the available source samples must report the
        // frames that carried real audio, never the destination size. The sink
        // counts this return value as `frames_played`, so counting the trailing
        // silence here would make the reported position (and the perceived
        // duration) run ahead of what actually played.
        let mut pending = VecDeque::from([1.0f32, -2.0, 3.0, -4.0]); // 2 stereo frames
        let mut out = [0u8; 32]; // room for 4 stereo frames
        let written = pack_f32le(&mut out, 2, 4, &mut pending, &unity_gain());
        assert_eq!(
            written, 2,
            "the two source frames count, the silence does not"
        );
        assert!(pending.is_empty());
        // The remaining destination bytes are untouched by the writer; the
        // caller is responsible for zeroing them, and must not count them.
        assert_eq!(
            f32::from_le_bytes(out[8..12].try_into().unwrap()),
            3.0,
            "second frame lands right after the first"
        );
    }

    #[test]
    fn partial_pending_frame_is_preserved_and_not_counted_until_completed() {
        let frames_played = AtomicU64::new(0);
        let mut pending = VecDeque::from([1.0f32, -2.0, 3.0]);
        let mut out = [0u8; 16];

        let written =
            pack_f32le_and_count(&mut out, 2, 2, &mut pending, &unity_gain(), &frames_played);

        assert_eq!(written, 1);
        assert_eq!(frames_played.load(Ordering::Relaxed), 1);
        assert_eq!(pending, VecDeque::from([3.0]));

        pending.push_back(4.0);
        let written =
            pack_f32le_and_count(&mut out, 2, 2, &mut pending, &unity_gain(), &frames_played);

        assert_eq!(written, 1);
        assert_eq!(frames_played.load(Ordering::Relaxed), 2);
        assert!(pending.is_empty());
        assert_eq!(f32::from_le_bytes(out[0..4].try_into().unwrap()), 3.0);
        assert_eq!(f32::from_le_bytes(out[4..8].try_into().unwrap()), 4.0);
    }

    #[test]
    fn thread_exited_wait_times_out_when_the_thread_never_returns() {
        // The no-callback case: nothing marks the flag, so the bounded wait
        // must return `false` (not block forever), letting shutdown detach.
        let exited = ThreadExited::default();
        let deadline = Duration::from_millis(50);
        let start = std::time::Instant::now();
        let result = exited.wait(deadline);
        assert!(!result, "a never-marked thread must time out as not exited");
        assert!(
            start.elapsed() >= deadline,
            "the wait must actually observe the timeout"
        );
    }

    #[test]
    fn thread_exited_wait_returns_immediately_when_already_marked() {
        let exited = ThreadExited::default();
        exited.mark_exited();
        // Already exited, so the call returns `true` without waiting.
        assert!(exited.wait(Duration::from_millis(1)));
    }

    #[test]
    fn thread_exited_wait_wakes_when_marked_during_the_wait() {
        let exited = Arc::new(ThreadExited::default());
        let marked = exited.clone();
        // Mark from another thread shortly after the wait starts; the condvar
        // must wake the waiter and return `true` instead of running its timeout.
        let marker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            marked.mark_exited();
        });
        assert!(exited.wait(Duration::from_secs(2)), "must wake on the flag");
        let _ = marker.join();
    }

    #[test]
    fn exit_guard_marks_the_thread_exited_on_drop() {
        let exited = Arc::new(ThreadExited::default());
        {
            let _guard = ExitGuard(exited.clone());
            assert!(!*exited.signal.lock().unwrap());
        }
        // On drop the guard flags the thread as exited.
        assert!(*exited.signal.lock().unwrap());
        assert!(exited.wait(Duration::from_millis(1)));
    }

    #[test]
    fn startup_readiness_requires_the_process_callback_and_stability_window() {
        let readiness = StartupReadiness::default();
        readiness.mark_process_started();
        let started = Instant::now();

        assert_eq!(
            readiness.wait_for_process_stable(Duration::from_millis(20), Duration::from_millis(2)),
            StartupWait::Connected,
            "a process callback is the successful startup proof once the settling window passes"
        );
        assert!(started.elapsed() >= Duration::from_millis(2));
    }

    #[test]
    fn startup_readiness_returns_typed_callback_errors() {
        let readiness = StartupReadiness::default();
        readiness.mark_failed("target node rejected the stream");

        assert_eq!(
            readiness.wait_for_process(Duration::ZERO),
            StartupWait::Failed("target node rejected the stream".to_string())
        );
    }

    #[test]
    fn startup_timeout_is_terminal_and_exit_cleanup_remains_bounded() {
        let readiness = Arc::new(StartupReadiness::default());
        let exited = Arc::new(ThreadExited::default());
        {
            let _startup_guard = StartupExitGuard(readiness.clone());
            let _exit_guard = ExitGuard(exited.clone());
            assert_eq!(
                readiness.wait_for_process(Duration::from_millis(1)),
                StartupWait::TimedOut
            );
        }

        assert_eq!(
            readiness.wait_for_process(Duration::ZERO),
            StartupWait::TimedOut
        );
        assert!(exited.wait(Duration::ZERO));
    }

    #[test]
    fn sample_pool_reports_exhaustion_and_reuses_a_slot() {
        let pool = SamplePool::new(1);
        for value in 0..SAMPLE_POOL_SLOTS {
            assert_eq!(
                pool.push_samples(&[value as f32]),
                PushSamplesResult::Accepted
            );
        }
        assert_eq!(
            pool.push_samples(&[99.0]),
            PushSamplesResult::Backpressure,
            "an exhausted pool must apply bounded backpressure"
        );

        let first = pool.pop_ready().expect("first slot must be ready");
        let first_ptr = pool.slot_ptr(first.slot);
        assert_eq!(pool.samples(first), &[0.0]);
        pool.release(first.slot);
        assert_eq!(pool.push_samples(&[99.0]), PushSamplesResult::Accepted);

        for _ in 1..SAMPLE_POOL_SLOTS {
            pool.pop_ready().expect("queued slots must remain ordered");
        }
        let reused = pool.pop_ready().expect("reused slot must be published");
        assert_eq!(reused.slot, first.slot);
        assert_eq!(pool.slot_ptr(reused.slot), first_ptr);
        assert_eq!(pool.samples(reused), &[99.0]);
    }

    #[test]
    fn concurrent_rollback_and_rt_release_recovers_every_slot() {
        let pool = Arc::new(SamplePool::new(1));
        let rendezvous = Arc::new(Barrier::new(3));
        pool.set_rollback_barrier(rendezvous.clone());

        for round in 0..128 {
            // Leave exactly one free slot. The producer consumes it, then
            // fails its second reservation and rolls the first one back.
            let mut held = Vec::with_capacity(SAMPLE_POOL_SLOTS - 1);
            for _ in 0..SAMPLE_POOL_SLOTS - 1 {
                held.push(pool.free.pop().expect("the pool must contain held slots"));
            }
            let callback_slot = held.pop().expect("one slot must be released by RT");

            let producer_pool = pool.clone();
            let producer_samples = vec![round as f32; SAMPLE_BUFFER_FRAMES + 1];
            let producer =
                std::thread::spawn(move || producer_pool.push_samples(&producer_samples));

            let callback_pool = pool.clone();
            let callback_rendezvous = rendezvous.clone();
            let callback = std::thread::spawn(move || {
                callback_rendezvous.wait();
                callback_pool.release(callback_slot);
            });

            rendezvous.wait();
            assert_eq!(
                producer.join().expect("producer must not panic"),
                PushSamplesResult::Backpressure
            );
            callback.join().expect("RT-style releaser must not panic");

            for slot in held {
                pool.release(slot);
            }

            let mut recovered = Vec::with_capacity(SAMPLE_POOL_SLOTS);
            for _ in 0..SAMPLE_POOL_SLOTS {
                recovered.push(pool.free.pop().expect("every slot must be recoverable"));
            }
            recovered.sort_unstable();
            assert_eq!(recovered, (0..SAMPLE_POOL_SLOTS).collect::<Vec<_>>());
            assert!(pool.free.pop().is_none(), "no slot may be duplicated");
            for slot in recovered {
                pool.release(slot);
            }
        }
    }

    #[test]
    fn sample_pool_flush_returns_queued_and_deferred_slots() {
        let pool = SamplePool::new(1);
        let mut pending = VecDeque::with_capacity(4);
        let mut deferred = None;
        assert_eq!(
            pool.push_samples(&[1.0, 2.0, 3.0]),
            PushSamplesResult::Accepted
        );
        assert_eq!(pool.push_samples(&[4.0, 5.0]), PushSamplesResult::Accepted);
        append_available_samples(&pool, &mut pending, &mut deferred, 4);
        assert!(deferred.is_some(), "the second chunk must be deferred");

        flush_samples(&pool, &mut pending, &mut deferred);
        assert!(pending.is_empty());
        assert!(deferred.is_none());
        for value in 0..SAMPLE_POOL_SLOTS {
            assert_eq!(
                pool.push_samples(&[value as f32]),
                PushSamplesResult::Accepted,
                "flush must return every slot to the free pool"
            );
        }
    }

    #[test]
    fn variable_chunks_preserve_final_partial_samples_without_growing_pending() {
        let pool = SamplePool::new(2);
        let mut pending = VecDeque::with_capacity(4);
        let pending_capacity = pending.capacity();
        let mut deferred = None;
        assert_eq!(
            pool.push_samples(&[1.0, 2.0, 3.0]),
            PushSamplesResult::Accepted
        );
        assert_eq!(pool.push_samples(&[4.0, 5.0]), PushSamplesResult::Accepted);

        append_available_samples(&pool, &mut pending, &mut deferred, pending_capacity);
        assert_eq!(pending, VecDeque::from([1.0, 2.0, 3.0]));
        assert!(deferred.is_some());
        assert_eq!(pending.capacity(), pending_capacity);

        let mut out = [0u8; 8];
        assert_eq!(pack_f32le(&mut out, 2, 1, &mut pending, &unity_gain()), 1);
        append_available_samples(&pool, &mut pending, &mut deferred, pending_capacity);
        assert_eq!(pending, VecDeque::from([3.0, 4.0, 5.0]));
        assert_eq!(pack_f32le(&mut out, 2, 1, &mut pending, &unity_gain()), 1);
        assert_eq!(pending, VecDeque::from([5.0]));
        assert_eq!(pending.capacity(), pending_capacity);
    }

    #[test]
    fn pooled_storage_is_reused_without_reallocation() {
        let pool = SamplePool::new(2);
        assert_eq!(pool.push_samples(&[1.0, 2.0]), PushSamplesResult::Accepted);
        let first = pool.pop_ready().expect("first sample must be ready");
        let pointer = pool.slot_ptr(first.slot);
        pool.release(first.slot);

        // The free list is LIFO: cycle through every slot before the released
        // slot is reused, then verify its backing allocation stayed at the
        // same address.
        for value in 0..SAMPLE_POOL_SLOTS {
            assert_eq!(
                pool.push_samples(&[value as f32, value as f32 + 1.0]),
                PushSamplesResult::Accepted
            );
        }
        let mut second = None;
        for _ in 0..SAMPLE_POOL_SLOTS {
            let ready = pool.pop_ready().expect("cycled slot must be ready");
            if ready.slot == first.slot {
                second = Some(ready);
            }
        }
        let second = second.expect("the released slot must be reused");
        assert_eq!(second.slot, first.slot);
        assert_eq!(pool.slot_ptr(second.slot), pointer);
        assert_eq!(pool.samples(second), &[0.0, 1.0]);
    }

    #[test]
    fn sample_pool_reports_disconnected_sink() {
        let pool = SamplePool::new(1);
        pool.mark_disconnected();
        assert_eq!(pool.push_samples(&[1.0]), PushSamplesResult::Disconnected);
    }

    #[test]
    fn disconnect_serializes_with_publication_and_reclaims_ready_slots() {
        let pool = Arc::new(SamplePool::new(1));
        let rendezvous = Arc::new(Barrier::new(2));
        pool.set_publish_barrier(rendezvous.clone());

        let producer_pool = pool.clone();
        let producer = std::thread::spawn(move || producer_pool.push_samples(&[1.0]));
        rendezvous.wait();

        // The producer owns the publication mutex while it is paused at the
        // final pre-publication check. Disconnect therefore cannot mark the
        // pool until this push has either published or returned.
        let disconnect_pool = pool.clone();
        let disconnector = std::thread::spawn(move || disconnect_pool.mark_disconnected());

        assert_eq!(
            producer.join().expect("producer must not panic"),
            PushSamplesResult::Accepted
        );
        disconnector
            .join()
            .expect("disconnect must not panic or deadlock");

        assert!(!pool.connected.load(Ordering::Acquire));
        assert_eq!(pool.push_samples(&[2.0]), PushSamplesResult::Disconnected);

        let mut recovered = Vec::with_capacity(SAMPLE_POOL_SLOTS);
        for _ in 0..SAMPLE_POOL_SLOTS {
            recovered.push(pool.free.pop().expect("disconnect must not strand slots"));
        }
        recovered.sort_unstable();
        assert_eq!(recovered, (0..SAMPLE_POOL_SLOTS).collect::<Vec<_>>());
        assert!(
            pool.free.pop().is_none(),
            "disconnect must not duplicate slots"
        );
    }

    #[test]
    fn flush_generation_is_acknowledged_only_after_queued_audio_is_discarded() {
        let pool = SamplePool::new(1);
        let mut pending = VecDeque::with_capacity(4);
        let mut deferred = None;
        let acknowledged = AtomicU64::new(0);
        let generation = FlushGeneration(1);

        assert_eq!(pool.push_samples(&[1.0, 2.0]), PushSamplesResult::Accepted);
        append_available_samples(&pool, &mut pending, &mut deferred, 4);
        assert!(!pending.is_empty());
        assert!(!flush_acknowledged(
            &acknowledged,
            &pool.connected,
            generation
        ));

        flush_samples(&pool, &mut pending, &mut deferred);
        acknowledged.store(generation.0, Ordering::Release);
        assert!(flush_acknowledged(
            &acknowledged,
            &pool.connected,
            generation
        ));
        assert!(pending.is_empty());
        assert!(deferred.is_none());
    }

    #[test]
    fn disconnected_pool_completes_flush_poll_without_waiting_for_callback() {
        let pool = SamplePool::new(1);
        let acknowledged = AtomicU64::new(0);
        let generation = FlushGeneration(7);

        pool.mark_disconnected();

        assert!(flush_acknowledged(
            &acknowledged,
            &pool.connected,
            generation
        ));
        assert_eq!(pool.push_samples(&[1.0]), PushSamplesResult::Disconnected);
    }
}
