//! Bounded producer/consumer buffering for synchronous stream readers.
//!
//! A [`ProgressiveReader`] keeps blocking transport reads on a dedicated
//! producer thread. The consumer only waits for bytes when the bounded ring is
//! empty, so decoder reads cannot perform network I/O on the audio worker.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::stream::provider::{ReadSeek, StreamCancellation};

const DEFAULT_CAPACITY: usize = 512 * 1024;
const READ_CHUNK: usize = 16 * 1024;
const CANCELLATION_POLL: Duration = Duration::from_millis(10);

struct RingState {
    buffer: Box<[u8]>,
    head: u64,
    head_index: usize,
    len: usize,
    consumer_position: u64,
    eof: bool,
    error: Option<io::Error>,
    cancelled: bool,
}

struct SharedRing {
    state: Mutex<RingState>,
    not_empty: Condvar,
    not_full: Condvar,
    seek: Mutex<Option<SeekRequest>>,
}

struct SeekRequest {
    target: u64,
    response: SyncSender<io::Result<u64>>,
}

/// A bounded, seekable view over a reader whose blocking reads run elsewhere.
///
/// The ring retains already-read bytes until the producer needs the space and
/// the consumer has advanced past them. This gives backward seeks a bounded
/// window while keeping memory usage fixed. Seeks before the retained window
/// are delegated to the producer's underlying reader.
pub struct ProgressiveReader {
    shared: Arc<SharedRing>,
    cancellation: StreamCancellation,
    content_length: Option<u64>,
    producer: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ProgressiveReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressiveReader").finish_non_exhaustive()
    }
}

impl ProgressiveReader {
    /// Spawn a producer with the default bounded read-ahead capacity.
    pub fn spawn(
        source: Box<dyn ReadSeek + Send + Sync>,
        cancellation: &StreamCancellation,
    ) -> io::Result<Self> {
        Self::spawn_with_content_length(source, cancellation, None)
    }

    pub(crate) fn spawn_with_content_length(
        source: Box<dyn ReadSeek + Send + Sync>,
        cancellation: &StreamCancellation,
        content_length: Option<u64>,
    ) -> io::Result<Self> {
        Self::spawn_with_capacity_and_length(source, cancellation, DEFAULT_CAPACITY, content_length)
    }

    #[cfg(test)]
    fn spawn_with_capacity(
        source: Box<dyn ReadSeek + Send + Sync>,
        cancellation: &StreamCancellation,
        capacity: usize,
    ) -> io::Result<Self> {
        Self::spawn_with_capacity_and_length(source, cancellation, capacity, None)
    }

    fn spawn_with_capacity_and_length(
        source: Box<dyn ReadSeek + Send + Sync>,
        cancellation: &StreamCancellation,
        capacity: usize,
        content_length: Option<u64>,
    ) -> io::Result<Self> {
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "progressive reader capacity must be non-zero",
            ));
        }
        let shared = Arc::new(SharedRing {
            state: Mutex::new(RingState {
                buffer: vec![0; capacity].into_boxed_slice(),
                head: 0,
                head_index: 0,
                len: 0,
                consumer_position: 0,
                eof: false,
                error: None,
                cancelled: false,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            seek: Mutex::new(None),
        });
        let token = cancellation.clone();
        let producer_shared = Arc::clone(&shared);
        let producer = thread::Builder::new()
            .name("harmonium-stream-buffer".to_string())
            .spawn(move || produce(source, producer_shared, token))?;
        Ok(Self {
            shared,
            cancellation: cancellation.clone(),
            content_length,
            producer: Some(producer),
        })
    }

    fn wait_for_data<'a>(
        &self,
        mut state: std::sync::MutexGuard<'a, RingState>,
    ) -> io::Result<std::sync::MutexGuard<'a, RingState>> {
        loop {
            let available = state
                .head
                .checked_add(state.len as u64)
                .unwrap_or(u64::MAX)
                .saturating_sub(state.consumer_position);
            if available > 0 || state.eof || state.error.is_some() || state.cancelled {
                return Ok(state);
            }
            let (next, _) = self
                .shared
                .not_empty
                .wait_timeout(state, CANCELLATION_POLL)
                .map_err(|_| io::Error::other("progressive reader ring was poisoned"))?;
            state = next;
        }
    }

    fn seek_before_window(&self, target: u64) -> io::Result<u64> {
        let (sender, receiver) = mpsc::sync_channel(1);
        {
            let mut request = self
                .shared
                .seek
                .lock()
                .map_err(|_| io::Error::other("progressive reader seek state was poisoned"))?;
            if request.is_some() {
                return Err(io::Error::other("progressive reader seek already pending"));
            }
            *request = Some(SeekRequest {
                target,
                response: sender,
            });
        }
        self.shared.not_full.notify_all();
        loop {
            if let Ok(mut state) = self.shared.state.lock() {
                if state.cancelled {
                    return Err(cancelled_io_error());
                }
                if let Some(error) = state.error.take() {
                    return Err(error);
                }
            }
            match receiver.recv_timeout(CANCELLATION_POLL) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("progressive reader producer stopped"));
                }
            }
        }
    }

    fn skip_forward(&mut self, target: u64) -> io::Result<u64> {
        loop {
            let mut state = self
                .shared
                .state
                .lock()
                .map_err(|_| io::Error::other("progressive reader ring was poisoned"))?;
            if state.cancelled {
                return Err(cancelled_io_error());
            }
            if state.consumer_position >= target {
                return Ok(state.consumer_position);
            }
            let available = state
                .head
                .checked_add(state.len as u64)
                .unwrap_or(u64::MAX)
                .saturating_sub(state.consumer_position);
            if available > 0 {
                let skipped = available.min(target - state.consumer_position);
                state.consumer_position += skipped;
                self.shared.not_full.notify_all();
                continue;
            }
            if let Some(error) = state.error.take() {
                return Err(error);
            }
            if state.eof {
                return Ok(state.consumer_position);
            }
            drop(self.wait_for_data(state)?);
        }
    }
}

impl Read for ProgressiveReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let mut state = self
                .shared
                .state
                .lock()
                .map_err(|_| io::Error::other("progressive reader ring was poisoned"))?;
            state = self.wait_for_data(state)?;
            if state.cancelled {
                return Err(cancelled_io_error());
            }
            let available = state
                .head
                .checked_add(state.len as u64)
                .unwrap_or(u64::MAX)
                .saturating_sub(state.consumer_position);
            if available == 0 {
                if let Some(error) = state.error.take() {
                    return Err(error);
                }
                if state.eof {
                    return Ok(0);
                }
                continue;
            }

            if state.consumer_position < state.head {
                return Err(io::Error::other(
                    "progressive reader consumer fell outside the ring",
                ));
            }
            let offset = (state.consumer_position - state.head) as usize;
            let available = state.len - offset;
            let count = available.min(buf.len());
            copy_from_ring(&state.buffer, state.head_index, offset, &mut buf[..count]);
            state.consumer_position += count as u64;
            self.shared.not_full.notify_all();
            return Ok(count);
        }
    }
}

impl Seek for ProgressiveReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let current = self
            .shared
            .state
            .lock()
            .map_err(|_| io::Error::other("progressive reader ring was poisoned"))?
            .consumer_position;
        let target = match position {
            SeekFrom::Start(target) => target,
            SeekFrom::Current(delta) => checked_seek(current, delta)?,
            SeekFrom::End(delta) => self.content_length.map_or_else(
                || {
                    Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "seek from end requires a Content-Length",
                    ))
                },
                |length| checked_seek(length, delta),
            )?,
        };

        if target == current {
            return Ok(current);
        }
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| io::Error::other("progressive reader ring was poisoned"))?;
        let head = state.head;
        let end = head + state.len as u64;
        if target >= current && (head..=end).contains(&target) {
            state.consumer_position = target;
            return Ok(target);
        }
        drop(state);
        if target > end {
            return self.skip_forward(target);
        }
        self.seek_before_window(target)
    }
}

impl Drop for ProgressiveReader {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.shared.not_empty.notify_all();
        self.shared.not_full.notify_all();
        if let Some(producer) = self.producer.take() {
            let _ = producer.join();
        }
    }
}

fn produce(
    mut source: Box<dyn ReadSeek + Send + Sync>,
    shared: Arc<SharedRing>,
    cancellation: StreamCancellation,
) {
    let mut scratch = vec![0; READ_CHUNK];
    loop {
        if cancellation.is_cancelled() {
            mark_cancelled(&shared);
            return;
        }
        if let Some(request) = take_seek_request(&shared) {
            let result = source.seek(SeekFrom::Start(request.target));
            match result {
                Ok(position) => {
                    if let Ok(mut state) = shared.state.lock() {
                        state.head = position;
                        state.head_index = 0;
                        state.len = 0;
                        state.consumer_position = position;
                        state.eof = false;
                        state.error = None;
                    }
                    let _ = request.response.send(Ok(position));
                    shared.not_empty.notify_all();
                    shared.not_full.notify_all();
                }
                Err(error) => {
                    let _ = request.response.send(Err(error));
                    shared.not_empty.notify_all();
                    shared.not_full.notify_all();
                }
            }
            continue;
        }

        let (write_offset, read_size) = {
            let mut state = match shared.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            loop {
                if cancellation.is_cancelled() {
                    mark_cancelled_locked(&mut state);
                    return;
                }
                let seek_pending = shared
                    .seek
                    .lock()
                    .map(|request| request.is_some())
                    .unwrap_or(true);
                if seek_pending {
                    break (0, 0);
                }
                if state.eof {
                    let (next, _) = match shared.not_full.wait_timeout(state, CANCELLATION_POLL) {
                        Ok(result) => result,
                        Err(_) => return,
                    };
                    state = next;
                    continue;
                }
                let capacity = state.buffer.len();
                if state.len < capacity {
                    let offset = (state.head_index + state.len) % capacity;
                    break (offset, (capacity - state.len).min(READ_CHUNK));
                }
                if state.consumer_position > state.head {
                    let consumed = (state.consumer_position - state.head) as usize;
                    state.head_index = (state.head_index + consumed) % state.buffer.len();
                    state.head = state.consumer_position;
                    state.len -= consumed;
                    continue;
                }
                let (next, _) = match shared.not_full.wait_timeout(state, CANCELLATION_POLL) {
                    Ok(result) => result,
                    Err(_) => return,
                };
                state = next;
            }
        };
        if read_size == 0 {
            continue;
        }

        match source.read(&mut scratch[..read_size]) {
            Ok(0) => {
                if let Ok(mut state) = shared.state.lock() {
                    state.eof = true;
                }
                shared.not_empty.notify_all();
                continue;
            }
            Ok(count) => {
                if let Ok(mut state) = shared.state.lock() {
                    write_to_ring(&mut state, write_offset, &scratch[..count]);
                }
                shared.not_empty.notify_all();
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if cancellation.is_cancelled() {
                    if let Ok(mut state) = shared.state.lock() {
                        state.error = Some(error);
                    }
                    shared.not_empty.notify_all();
                    return;
                }
            }
            Err(error) => {
                if let Ok(mut state) = shared.state.lock() {
                    state.error = Some(error);
                }
                shared.not_empty.notify_all();
                return;
            }
        }
    }
}

fn take_seek_request(shared: &SharedRing) -> Option<SeekRequest> {
    shared.seek.lock().ok()?.take()
}

fn mark_cancelled(shared: &SharedRing) {
    if let Ok(mut state) = shared.state.lock() {
        mark_cancelled_locked(&mut state);
    }
    shared.not_empty.notify_all();
    shared.not_full.notify_all();
}

fn mark_cancelled_locked(state: &mut RingState) {
    state.cancelled = true;
}

fn write_to_ring(state: &mut RingState, offset: usize, bytes: &[u8]) {
    let first = bytes.len().min(state.buffer.len() - offset);
    state.buffer[offset..offset + first].copy_from_slice(&bytes[..first]);
    if first < bytes.len() {
        state.buffer[..bytes.len() - first].copy_from_slice(&bytes[first..]);
    }
    state.len += bytes.len();
}

fn copy_from_ring(buffer: &[u8], head_index: usize, relative: usize, output: &mut [u8]) {
    let offset = (head_index + relative) % buffer.len();
    let first = output.len().min(buffer.len() - offset);
    output[..first].copy_from_slice(&buffer[offset..offset + first]);
    if first < output.len() {
        let remainder = output.len() - first;
        output[first..].copy_from_slice(&buffer[..remainder]);
    }
}

fn checked_seek(current: u64, delta: i64) -> io::Result<u64> {
    match (current as i128).checked_add(delta as i128) {
        Some(target) if target >= 0 && target <= u64::MAX as i128 => Ok(target as u64),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seek before start of progressive stream",
        )),
    }
}

fn cancelled_io_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "progressive stream was cancelled",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;

    fn cancellation() -> StreamCancellation {
        StreamCancellation::new()
    }

    fn wait_until(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !predicate() {
            assert!(Instant::now() < deadline, "condition did not become true");
            thread::yield_now();
        }
    }

    #[derive(Clone)]
    struct CountingReader {
        reads: Arc<AtomicUsize>,
        bytes: Arc<AtomicUsize>,
        next: u8,
    }

    impl Read for CountingReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.reads.fetch_add(1, Ordering::AcqRel);
            let count = output.len().min(1);
            if count == 1 {
                output[0] = self.next;
                self.next = self.next.wrapping_add(1);
                self.bytes.fetch_add(1, Ordering::AcqRel);
            }
            Ok(count)
        }
    }

    impl Seek for CountingReader {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            match position {
                SeekFrom::Start(target) => {
                    self.next = target as u8;
                    Ok(target)
                }
                _ => Err(io::Error::new(io::ErrorKind::Unsupported, "test seek")),
            }
        }
    }

    #[test]
    fn producer_reads_ahead_before_consumer_reads() {
        let reads = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let source = CountingReader {
            reads: Arc::clone(&reads),
            bytes: Arc::clone(&bytes),
            next: 0,
        };
        let token = cancellation();
        let reader = ProgressiveReader::spawn_with_capacity(Box::new(source), &token, 8)
            .expect("spawn producer");
        wait_until(|| bytes.load(Ordering::Acquire) >= 8);
        assert!(reads.load(Ordering::Acquire) > 0);
        drop(reader);
    }

    struct GatedReader {
        started: Arc<AtomicBool>,
        release: Arc<Barrier>,
        released: bool,
    }

    impl Read for GatedReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.started.store(true, Ordering::Release);
            if self.released {
                return Ok(0);
            }
            self.release.wait();
            self.released = true;
            output[0] = 7;
            Ok(1)
        }
    }

    impl Seek for GatedReader {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    #[test]
    fn consumer_blocks_on_starvation_without_returning_zero() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Barrier::new(2));
        let token = cancellation();
        let reader = ProgressiveReader::spawn_with_capacity(
            Box::new(GatedReader {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
                released: false,
            }),
            &token,
            8,
        )
        .expect("spawn producer");
        let consumer = thread::spawn(move || {
            let mut reader = reader;
            let mut byte = [0; 1];
            reader.read(&mut byte).expect("read after release")
        });
        wait_until(|| started.load(Ordering::Acquire));
        assert!(!consumer.is_finished());
        release.wait();
        assert_eq!(consumer.join().expect("consumer join"), 1);
    }

    #[test]
    fn producer_backpressure_keeps_ring_bounded() {
        let reads = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let token = cancellation();
        let mut reader = ProgressiveReader::spawn_with_capacity(
            Box::new(CountingReader {
                reads: Arc::clone(&reads),
                bytes: Arc::clone(&bytes),
                next: 0,
            }),
            &token,
            4,
        )
        .expect("spawn producer");
        wait_until(|| bytes.load(Ordering::Acquire) == 4);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(bytes.load(Ordering::Acquire), 4);
        let mut output = [0; 4];
        reader.read_exact(&mut output).expect("consume ring");
        assert_eq!(output, [0, 1, 2, 3]);
        assert!(reads.load(Ordering::Acquire) >= 4);
    }

    #[test]
    fn cancellation_interrupts_starved_consumer() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Barrier::new(2));
        let token = cancellation();
        let reader = ProgressiveReader::spawn_with_capacity(
            Box::new(GatedReader {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
                released: false,
            }),
            &token,
            8,
        )
        .expect("spawn producer");
        let consumer = thread::spawn(move || {
            let mut reader = reader;
            let mut byte = [0; 1];
            reader.read(&mut byte)
        });
        wait_until(|| started.load(Ordering::Acquire));
        token.cancel();
        release.wait();
        assert_eq!(
            consumer
                .join()
                .expect("consumer join")
                .expect_err("cancelled")
                .kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn eof_is_the_only_empty_success() {
        let token = cancellation();
        let mut reader =
            ProgressiveReader::spawn_with_capacity(Box::new(Cursor::new(vec![1, 2, 3])), &token, 4)
                .expect("spawn producer");
        let mut output = [0; 3];
        reader.read_exact(&mut output).expect("read body");
        assert_eq!(output, [1, 2, 3]);
        let mut empty = [0; 1];
        assert_eq!(reader.read(&mut empty).expect("read EOF"), 0);
    }

    #[test]
    fn known_length_supports_seek_from_end() {
        let token = cancellation();
        let mut reader = ProgressiveReader::spawn_with_capacity_and_length(
            Box::new(Cursor::new(b"abcdef".to_vec())),
            &token,
            4,
            Some(6),
        )
        .expect("spawn producer");
        assert_eq!(reader.seek(SeekFrom::End(-2)).expect("seek from end"), 4);
        let mut suffix = [0; 2];
        reader.read_exact(&mut suffix).expect("read suffix");
        assert_eq!(&suffix, b"ef");
    }

    #[test]
    fn drop_cancels_and_joins_producer() {
        let dropped = Arc::new(AtomicBool::new(false));
        struct DropSource(Arc<AtomicBool>);
        impl Read for DropSource {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                output[0] = 1;
                Ok(1)
            }
        }
        impl Seek for DropSource {
            fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
                Ok(0)
            }
        }
        impl Drop for DropSource {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let token = cancellation();
        let reader = ProgressiveReader::spawn_with_capacity(
            Box::new(DropSource(Arc::clone(&dropped))),
            &token,
            2,
        )
        .expect("spawn producer");
        wait_until(|| !dropped.load(Ordering::Acquire));
        drop(reader);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn seek_supports_ring_rewind_and_producer_reopen() {
        let token = cancellation();
        let mut reader = ProgressiveReader::spawn_with_capacity(
            Box::new(Cursor::new((0..32).collect::<Vec<_>>())),
            &token,
            8,
        )
        .expect("spawn producer");
        let mut first = [0; 2];
        reader.read_exact(&mut first).expect("initial read");
        assert_eq!(reader.seek(SeekFrom::Start(0)).expect("ring rewind"), 0);
        assert_eq!(reader.read(&mut first).expect("rewound read"), 2);
        assert_eq!(first, [0, 1]);

        let mut reader = ProgressiveReader::spawn_with_capacity(
            Box::new(Cursor::new((0..32).collect::<Vec<_>>())),
            &token,
            4,
        )
        .expect("spawn second producer");
        let position = reader.seek(SeekFrom::Start(20)).expect("forward seek");
        assert_eq!(position, 20);
        let mut byte = [0; 1];
        reader.read_exact(&mut byte).expect("forward read");
        assert_eq!(byte[0], 20);
        assert_eq!(reader.seek(SeekFrom::Start(0)).expect("producer reopen"), 0);
        reader.read_exact(&mut byte).expect("reopened read");
        assert_eq!(byte[0], 0);
    }
}
