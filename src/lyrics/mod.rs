//! Lyrics model and the resolution chain.
//!
//! The panel never touches IO. Like artwork, lyrics resolution travels
//! through a background worker: the command layer emits
//! [`crate::app::Effect::LoadLyrics`], the runtime runs
//! [`LyricsService::load`] inside `spawn_blocking` and the outcome lands in
//! the event loop through [`crate::event::AppEvent::LyricsLoaded`].
//!
//! The service owns a priority chain (`LocalSource` -> `MetadataSource` ->
//! `RemoteSource`) plus a memo keyed by audio path, so toggling the panel
//! off and on again for the same track is instant and never repeats network
//! work.

mod cache;
mod karaoke;
mod locator;
mod lrc;
mod metadata;
mod remote;

pub use cache::save_lyrics_file;
pub use karaoke::{
    ANTICIPATION_MS, DEFAULT_LINE_MS, DocumentLayout, FollowDirection, active_line_index,
    active_line_index_from_starts, effective_elapsed, follow_scroll, layout_document,
    line_char_times, next_line_starts, reached_char_count, split_at_chars, wrap_rows_for,
};
pub use locator::find_local_lrc;
pub use lrc::{LrcParsed, format_lrc, parse_lrc};
pub use metadata::read_embedded_lyrics;
pub use remote::{LrcLibProvider, LrclibResult};

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// One word of an Enhanced LRC line, with its own start time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsWord {
    /// Start time in milliseconds (offsets already applied).
    pub start_ms: i64,
    /// Word text without timing tags.
    pub text: String,
}

/// One rendered lyric row.
///
/// `timestamp_ms` is `None` for untimed lines (plain lyrics, headings).
/// `words` holds the Enhanced LRC word timing and is empty for standard
/// LRC or plain text lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsLine {
    /// Timestamp in milliseconds when the line becomes active.
    pub timestamp_ms: Option<i64>,
    /// Body text without any timestamp tags.
    pub text: String,
    /// Word-level timing kept in file order, empty when absent.
    pub words: Vec<LyricsWord>,
}

/// Fully parsed lyrics ready for the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsDocument {
    /// Body lines in file order, including untimed ones.
    pub lines: Vec<LyricsLine>,
    /// Plain body with all timestamps stripped, joined by newlines.
    pub text: String,
}

impl LyricsDocument {
    /// Build a document from raw untimed text.
    pub fn from_plain(text: &str) -> Self {
        let lines = text
            .lines()
            .map(|line| LyricsLine {
                timestamp_ms: None,
                text: line.to_string(),
                words: Vec::new(),
            })
            .collect();
        Self {
            lines,
            text: text.to_string(),
        }
    }
}

/// Where the document was ultimately found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricsOrigin {
    /// A `.lrc` file next to the audio file.
    Local,
    /// Embedded lyrics inside the audio file tags.
    Metadata,
    /// LRCLIB over the network.
    Remote,
}

/// Everything a source needs to resolve lyrics for one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsRequest {
    /// Absolute audio file path, the identity key for the memo.
    pub audio_path: PathBuf,
    /// Title from the track metadata when known.
    pub title: Option<String>,
    /// Artist from the track metadata when known.
    pub artist: Option<String>,
    /// Album from the track metadata when known, used to prefer exact
    /// releases over live or compilation versions.
    pub album: Option<String>,
}

/// Result of one resolution, carrying everything the UI needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOutcome {
    /// Parsed document on success, `None` when nothing was found.
    pub document: Option<LyricsDocument>,
    /// Which link of the chain produced the document.
    pub origin: Option<LyricsOrigin>,
    /// Where the remote lyric was cached on disk, when it was.
    pub saved_path: Option<PathBuf>,
    /// Short human readable reason when no document came out.
    pub error: Option<String>,
}

impl LoadOutcome {
    /// Successful outcome without any cache write.
    fn success(document: LyricsDocument, origin: LyricsOrigin) -> Self {
        Self {
            document: Some(document),
            origin: Some(origin),
            saved_path: None,
            error: None,
        }
    }

    /// Miss outcome: nothing usable was found anywhere.
    fn miss(error: &str) -> Self {
        Self {
            document: None,
            origin: None,
            saved_path: None,
            error: Some(error.to_string()),
        }
    }
}

/// One candidate provider in the priority chain.
pub trait LyricsSource: Send + Sync {
    /// Resolve lyrics for the request, `None` when this source has none.
    fn find(&self, req: &LyricsRequest) -> Option<LyricsDocument>;
}

/// Signature of the cache writer used after a remote resolution.
type CacheWriter = dyn Fn(&Path, &str) -> io::Result<PathBuf> + Send + Sync;

/// Resolves lyrics through the chain, memoizing results per audio path.
///
/// Cloning is cheap (one `Arc` bump) and lets the runtime hand a snapshot of
/// the chain to each background resolution.
#[derive(Clone)]
pub struct LyricsService {
    inner: Arc<ServiceInner>,
}

struct ServiceInner {
    /// Ordered chain of sources with the origin each one covers.
    sources: Vec<(LyricsOrigin, Box<dyn LyricsSource>)>,
    /// Outcome cache and its eviction order, including negative results.
    memo: Mutex<LyricsMemo>,
    /// Per-path coordination for resolutions that have not reached the memo.
    coordination: Mutex<LyricsCoordination>,
    /// Persists remote results next to the audio file, when configured.
    cache_writer: Option<Box<CacheWriter>>,
    /// Gates the remote source, toggled at runtime by the settings screen.
    remote_enabled: AtomicBool,
    /// Changes whenever the remote-source policy changes, preventing an older
    /// resolution from repopulating a memo that was just invalidated.
    remote_generation: AtomicUsize,
}

/// Maximum number of lyrics outcomes kept in memory.
///
/// A long session touching thousands of distinct tracks (and a library that is
/// mostly *without* local lyrics, so misses are cached too) would otherwise
/// grow the memo without bound. A generous cap keeps memory predictable while
/// retaining the common hit-again case.
const MEMO_MAX_ENTRIES: usize = 512;

/// Bounded least-recently-used memo for resolved lyrics outcomes.
///
/// The map and order are kept together so every inspection or update can
/// preserve their invariant while holding one mutex. A cache hit refreshes
/// the key, and inserting an existing key replaces its outcome and refreshes
/// it as well.
struct LyricsMemo {
    entries: HashMap<PathBuf, Arc<LoadOutcome>>,
    order: VecDeque<PathBuf>,
}

impl LyricsMemo {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &Path) -> Option<Arc<LoadOutcome>> {
        let outcome = self.entries.get(key).cloned()?;
        self.refresh(key);
        self.order.push_back(key.to_path_buf());
        Some(outcome)
    }

    fn insert(&mut self, key: PathBuf, outcome: Arc<LoadOutcome>) {
        self.entries.insert(key.clone(), outcome);
        self.refresh(&key);
        self.order.push_back(key);

        while self.order.len() > MEMO_MAX_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    fn refresh(&mut self, key: &Path) {
        if let Some(position) = self.order.iter().position(|entry| entry == key) {
            self.order.remove(position);
        }
    }
}

/// Tracks one resolution per key without holding the memo lock during IO.
struct LyricsCoordination {
    in_flight: HashMap<PathBuf, Arc<InFlightResolution>>,
}

struct InFlightResolution {
    generation: usize,
    state: Mutex<InFlightState>,
    ready: Condvar,
}

#[derive(Default)]
struct InFlightState {
    outcome: Option<Arc<LoadOutcome>>,
}

impl LyricsCoordination {
    fn new() -> Self {
        Self {
            in_flight: HashMap::new(),
        }
    }
}

impl InFlightResolution {
    fn new(generation: usize) -> Self {
        Self {
            generation,
            state: Mutex::new(InFlightState::default()),
            ready: Condvar::new(),
        }
    }
}

impl LyricsService {
    /// Build the production chain: local files, embedded tags, LRCLIB.
    pub fn new() -> Self {
        Self::with_sources(vec![
            (LyricsOrigin::Local, Box::new(LocalSource)),
            (LyricsOrigin::Metadata, Box::new(MetadataSource)),
            (
                LyricsOrigin::Remote,
                Box::new(RemoteSource {
                    provider: LrcLibProvider::new(),
                }),
            ),
        ])
        .with_cache_writer(Box::new(save_lyrics_file))
        .with_remote(true)
    }

    /// Build a service from an explicit chain (used by tests).
    pub fn with_sources(sources: Vec<(LyricsOrigin, Box<dyn LyricsSource>)>) -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                sources,
                memo: Mutex::new(LyricsMemo::new()),
                coordination: Mutex::new(LyricsCoordination::new()),
                cache_writer: None,
                remote_enabled: AtomicBool::new(true),
                remote_generation: AtomicUsize::new(0),
            }),
        }
    }

    /// Attach the cache writer responsible for remote results, if any.
    ///
    /// Meant to be called on a fresh service before any clone: with clones
    /// existing the writer cannot be swapped in, and the call degrades to a
    /// warning instead of failing loudly.
    pub fn with_cache_writer(mut self, writer: Box<CacheWriter>) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.cache_writer = Some(writer),
            None => tracing::warn!("lyrics cache writer not applied: service is shared"),
        }
        self
    }

    /// Set the remote-source gate on a fresh, uncloned service.
    ///
    /// Mirrors [`Self::with_cache_writer`]: safe only before clones exist,
    /// because every clone would keep observing the old flag value.
    pub fn with_remote(mut self, remote_enabled: bool) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner
                .remote_enabled
                .store(remote_enabled, Ordering::Relaxed),
            None => tracing::warn!("lyrics remote flag not applied: service is shared"),
        }
        self
    }

    /// Enable or disable the remote source for all later resolutions.
    ///
    /// Stored in an [`AtomicBool`] so background loads already in flight can
    /// finish without blocking the UI thread that flips the preference. A
    /// real change also drops the memo: a tracked miss cached while the
    /// remote source was off must be re-resolved once it is enabled, or a
    /// user would not get LRCLIB lyrics until the next session.
    pub fn set_remote_enabled(&self, enabled: bool) {
        let changed = self.inner.remote_enabled.swap(enabled, Ordering::Relaxed) != enabled;
        if changed {
            self.inner.remote_generation.fetch_add(1, Ordering::AcqRel);
            if let Ok(mut memo) = self.inner.memo.lock() {
                memo.clear();
            }
        }
    }

    /// Resolve lyrics for `req`, honoring the per-path memo.
    ///
    /// A cached outcome (success or miss) short-circuits the chain, so a
    /// panel toggle for the same track never hits the network twice.
    pub fn load(&self, req: &LyricsRequest) -> LoadOutcome {
        self.load_with_cancellation(req, || false)
            .expect("an uncancelled lyrics load must produce an outcome")
    }

    /// Resolve lyrics while allowing the caller to abandon its wait.
    ///
    /// The source chain has no cooperative cancellation contract, so shared
    /// work is deliberately never aborted when a waiter is cancelled. The
    /// remaining resolution publishes its outcome to all other waiters and
    /// the memo; this keeps filesystem/network resources safe and preserves
    /// negative outcomes. A cancelled leader also lets the resolution finish,
    /// but receives no outcome itself.
    pub(crate) fn load_with_cancellation<F>(
        &self,
        req: &LyricsRequest,
        is_cancelled: F,
    ) -> Option<LoadOutcome>
    where
        F: Fn() -> bool,
    {
        if let Some(cached) = self
            .inner
            .memo
            .lock()
            .ok()
            .and_then(|mut memo| memo.get(&req.audio_path))
        {
            return (!is_cancelled()).then(|| cached.as_ref().clone());
        }

        let generation = self.inner.remote_generation.load(Ordering::Acquire);
        let (flight, leader) = {
            let mut coordination = self
                .inner
                .coordination
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match coordination.in_flight.get(&req.audio_path) {
                Some(existing) if existing.generation == generation => {
                    (Arc::clone(existing), false)
                }
                _ => {
                    let flight = Arc::new(InFlightResolution::new(generation));
                    coordination
                        .in_flight
                        .insert(req.audio_path.clone(), Arc::clone(&flight));
                    (flight, true)
                }
            }
        };
        let result = if leader {
            let outcome = self.resolve_fresh(req);
            let shared = Arc::new(outcome.clone());

            // Do not repopulate a memo invalidated by a remote preference
            // change while this resolution was in progress.
            if let Ok(mut memo) = self.inner.memo.lock()
                && self.inner.remote_generation.load(Ordering::Acquire) == generation
            {
                memo.insert(req.audio_path.clone(), Arc::clone(&shared));
            }

            {
                let mut state = flight
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state.outcome = Some(shared);
                flight.ready.notify_all();
            }

            let mut coordination = self
                .inner
                .coordination
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if coordination
                .in_flight
                .get(&req.audio_path)
                .is_some_and(|current| Arc::ptr_eq(current, &flight))
            {
                coordination.in_flight.remove(&req.audio_path);
            }
            (!is_cancelled()).then_some(outcome)
        } else {
            let mut state = flight
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            loop {
                if is_cancelled() {
                    break None;
                }
                if let Some(outcome) = &state.outcome {
                    break Some(outcome.as_ref().clone());
                }
                let (next, _) = flight
                    .ready
                    .wait_timeout(state, Duration::from_millis(10))
                    .unwrap_or_else(|error| error.into_inner());
                state = next;
            }
        };

        result
    }

    /// Walk the chain, writing remote results through the cache writer.
    fn resolve_fresh(&self, req: &LyricsRequest) -> LoadOutcome {
        for (origin, source) in &self.inner.sources {
            // A disabled remote source is skipped without being consulted, so
            // the user preference never costs a network call.
            if *origin == LyricsOrigin::Remote && !self.inner.remote_enabled.load(Ordering::Relaxed)
            {
                continue;
            }
            let Some(document) = source.find(req) else {
                continue;
            };
            if *origin == LyricsOrigin::Remote
                && let Some(writer) = &self.inner.cache_writer
            {
                let title: Option<String> = req.title.clone().or_else(|| {
                    req.audio_path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                });
                let content = format_lrc(&document, title.as_deref(), req.artist.as_deref());
                match writer(&req.audio_path, &content) {
                    Ok(path) => {
                        tracing::info!(
                            saved = %path.display(),
                            "remote lyrics cached next to the audio file"
                        );
                        return LoadOutcome {
                            document: Some(document),
                            origin: Some(*origin),
                            error: None,
                            saved_path: Some(path),
                        };
                    }
                    Err(error) => {
                        // A failed cache write must never fail the show:
                        // the document is still usable for this session.
                        tracing::warn!("lyrics cache write failed: {error}");
                        return LoadOutcome::success(document, *origin);
                    }
                }
            }
            return LoadOutcome::success(document, *origin);
        }
        LoadOutcome::miss("Lyrics not found")
    }
}

impl Default for LyricsService {
    fn default() -> Self {
        Self::new()
    }
}

/// First link: `.lrc` files beside the audio file.
struct LocalSource;

impl LyricsSource for LocalSource {
    fn find(&self, req: &LyricsRequest) -> Option<LyricsDocument> {
        let path = find_local_lrc(&req.audio_path)?;
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                tracing::debug!(path = %path.display(), "local lyrics file found");
                Some(parse_lrc(&content).into_document())
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "lyrics file unreadable");
                None
            }
        }
    }
}

/// Second link: embedded lyrics read on demand from the audio tags.
struct MetadataSource;

impl LyricsSource for MetadataSource {
    fn find(&self, req: &LyricsRequest) -> Option<LyricsDocument> {
        let lyrics = read_embedded_lyrics(&req.audio_path)?;
        tracing::debug!(path = %req.audio_path.display(), "embedded lyrics found");
        Some(parse_lrc(&lyrics).into_document())
    }
}

/// Third link: LRCLIB remote lookup.
struct RemoteSource {
    provider: LrcLibProvider,
}

impl LyricsSource for RemoteSource {
    fn find(&self, req: &LyricsRequest) -> Option<LyricsDocument> {
        self.provider.resolve(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier, mpsc};
    use std::thread;

    /// Test source counting its invocations so the chain tests can pin
    /// both the order and the memo short-circuiting.
    struct CountingSource {
        result: Option<LyricsDocument>,
        calls: Arc<AtomicUsize>,
    }

    impl LyricsSource for CountingSource {
        fn find(&self, _req: &LyricsRequest) -> Option<LyricsDocument> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    #[derive(Clone)]
    struct ResolutionGate {
        started: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl ResolutionGate {
        fn wait(&self) {
            let (released, signal) = &*self.release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = signal.wait(released).unwrap();
            }
        }

        fn release(&self) {
            let (released, signal) = &*self.release;
            *released.lock().unwrap() = true;
            signal.notify_all();
        }
    }

    struct GatedSource {
        calls: Arc<AtomicUsize>,
        gate: ResolutionGate,
        result: Option<LyricsDocument>,
    }

    impl LyricsSource for GatedSource {
        fn find(&self, _req: &LyricsRequest) -> Option<LyricsDocument> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.gate.started.send(()).unwrap();
            self.gate.wait();
            self.result.clone()
        }
    }

    fn resolution_gate() -> (ResolutionGate, mpsc::Receiver<()>) {
        let (started, started_rx) = mpsc::channel();
        (
            ResolutionGate {
                started,
                release: Arc::new((Mutex::new(false), Condvar::new())),
            },
            started_rx,
        )
    }

    fn plain_doc(text: &str) -> LyricsDocument {
        LyricsDocument::from_plain(text)
    }

    fn request(path: &str) -> LyricsRequest {
        LyricsRequest {
            audio_path: PathBuf::from(path),
            title: Some("Title".to_string()),
            artist: None,
            album: None,
        }
    }

    #[test]
    fn memo_tracks_order_refreshes_repeated_keys_and_evicts_lru() {
        let mut memo = LyricsMemo::new();
        let first = PathBuf::from("/m/first.flac");
        let second = PathBuf::from("/m/second.flac");
        let third = PathBuf::from("/m/third.flac");

        memo.insert(first.clone(), Arc::new(LoadOutcome::miss("first")));
        memo.insert(second.clone(), Arc::new(LoadOutcome::miss("second")));
        assert_eq!(
            memo.order.iter().cloned().collect::<Vec<_>>(),
            vec![first.clone(), second.clone()]
        );

        assert!(memo.get(&first).is_some());
        memo.insert(third.clone(), Arc::new(LoadOutcome::miss("third")));
        assert_eq!(
            memo.order.iter().cloned().collect::<Vec<_>>(),
            vec![second.clone(), first.clone(), third.clone()]
        );

        for index in 0..(MEMO_MAX_ENTRIES - 3) {
            memo.insert(
                PathBuf::from(format!("/m/filler-{index}.flac")),
                Arc::new(LoadOutcome::miss("filler")),
            );
        }
        let newest = PathBuf::from("/m/newest.flac");
        memo.insert(newest.clone(), Arc::new(LoadOutcome::miss("newest")));

        assert!(!memo.entries.contains_key(&second));
        assert!(memo.entries.contains_key(&first));
        assert!(memo.entries.contains_key(&third));
        assert!(memo.entries.contains_key(&newest));
        assert_eq!(memo.entries.len(), MEMO_MAX_ENTRIES);
        assert_eq!(memo.order.len(), MEMO_MAX_ENTRIES);
    }

    #[test]
    fn chain_stops_at_the_first_source_with_content() {
        let calls_a = Arc::new(AtomicUsize::new(0));
        let calls_b = Arc::new(AtomicUsize::new(0));
        let service = LyricsService::with_sources(vec![
            (
                LyricsOrigin::Local,
                Box::new(CountingSource {
                    result: Some(plain_doc("local")),
                    calls: calls_b.clone(),
                }),
            ),
            (
                LyricsOrigin::Metadata,
                Box::new(CountingSource {
                    result: Some(plain_doc("meta")),
                    calls: calls_a.clone(),
                }),
            ),
        ]);

        let outcome = service.load(&request("/m/song.flac"));

        assert_eq!(
            outcome.document.as_ref().map(|d| d.text.as_str()),
            Some("local")
        );
        assert_eq!(outcome.origin, Some(LyricsOrigin::Local));
        assert_eq!(calls_b.load(Ordering::SeqCst), 1);
        assert_eq!(
            calls_a.load(Ordering::SeqCst),
            0,
            "a hit in the first source must skip the rest"
        );
    }

    #[test]
    fn chain_falls_through_on_misses_and_reports_the_reason() {
        let calls_a = Arc::new(AtomicUsize::new(0));
        let calls_b = Arc::new(AtomicUsize::new(0));
        let service = LyricsService::with_sources(vec![
            (
                LyricsOrigin::Local,
                Box::new(CountingSource {
                    result: None,
                    calls: calls_a.clone(),
                }),
            ),
            (
                LyricsOrigin::Metadata,
                Box::new(CountingSource {
                    result: Some(plain_doc("meta")),
                    calls: calls_b.clone(),
                }),
            ),
        ]);

        let outcome = service.load(&request("/m/song.flac"));

        assert_eq!(outcome.origin, Some(LyricsOrigin::Metadata));
        assert_eq!(calls_a.load(Ordering::SeqCst), 1);
        assert_eq!(calls_b.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn total_misses_return_an_error_reason() {
        let service = LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(CountingSource {
                result: None,
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        )]);

        let outcome = service.load(&request("/m/song.flac"));

        assert_eq!(outcome.document, None);
        assert_eq!(outcome.origin, None);
        assert_eq!(outcome.error.as_deref(), Some("Lyrics not found"));
    }

    #[test]
    fn memo_hits_skip_the_sources_entirely() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(CountingSource {
                result: Some(plain_doc("cached")),
                calls: calls.clone(),
            }),
        )]);

        let first = service.load(&request("/m/song.flac"));
        let second = service.load(&request("/m/song.flac"));

        assert_eq!(first.document, second.document);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "sources run exactly once");
    }

    #[test]
    fn memo_caches_negative_results_too() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(CountingSource {
                result: None,
                calls: calls.clone(),
            }),
        )]);

        let first = service.load(&request("/m/song.flac"));
        let second = service.load(&request("/m/song.flac"));

        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "misses are memoized");
    }

    #[test]
    fn different_paths_do_not_share_cache_entries() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(CountingSource {
                result: Some(plain_doc("one")),
                calls: calls.clone(),
            }),
        )]);

        service.load(&request("/m/one.flac"));
        service.load(&request("/m/two.flac"));

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn memo_is_bounded_and_evicts_the_oldest_entry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(CountingSource {
                result: Some(plain_doc("lyrics")),
                calls: calls.clone(),
            }),
        )]);

        // Load more distinct paths than the memo cap, then reload the very
        // first one. The first must have been evicted (re-resolved) while the
        // most recent stays cached, so memory stays bounded.
        service.load(&request("/m/path-0.flac"));
        for i in 1..(MEMO_MAX_ENTRIES + 10) {
            service.load(&request(&format!("/m/path-{i}.flac")));
        }
        let calls_before = calls.load(Ordering::SeqCst);
        service.load(&request("/m/path-0.flac"));
        assert!(
            calls.load(Ordering::SeqCst) > calls_before,
            "the oldest entry must be evicted and re-resolved"
        );
        // Confirm the memo itself respects the cap.
        assert!(service.inner.memo.lock().unwrap().entries.len() <= MEMO_MAX_ENTRIES);
    }

    #[test]
    fn concurrent_loads_keep_memo_entries_and_order_consistent() {
        let service = Arc::new(LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(CountingSource {
                result: Some(plain_doc("lyrics")),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        )]));
        let mut workers = Vec::new();

        for worker in 0..8 {
            let service = Arc::clone(&service);
            workers.push(thread::spawn(move || {
                for item in 0..80 {
                    let outcome =
                        service.load(&request(&format!("/m/concurrent-{worker}-{item}.flac")));
                    assert_eq!(
                        outcome
                            .document
                            .as_ref()
                            .map(|document| document.text.as_str()),
                        Some("lyrics")
                    );
                }
            }));
        }

        for worker in workers {
            worker
                .join()
                .expect("concurrent lyrics load must not panic");
        }

        let memo = service.inner.memo.lock().unwrap();
        assert_eq!(memo.entries.len(), MEMO_MAX_ENTRIES);
        assert_eq!(memo.order.len(), memo.entries.len());
        assert!(
            memo.order.iter().all(|key| memo.entries.contains_key(key)),
            "every eviction-order key must have a matching cached outcome"
        );
    }

    #[test]
    fn same_key_concurrent_loads_share_one_resolution() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (gate, started_rx) = resolution_gate();
        let release = gate.clone();
        let service = Arc::new(LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(GatedSource {
                calls: calls.clone(),
                gate,
                result: Some(plain_doc("shared")),
            }),
        )]));

        let leader_service = Arc::clone(&service);
        let leader = thread::spawn(move || leader_service.load(&request("/m/shared.flac")));
        started_rx.recv().unwrap();

        let (waiter_ready_tx, waiter_ready_rx) = mpsc::channel();
        let waiter_service = Arc::clone(&service);
        let waiter = thread::spawn(move || {
            waiter_service.load_with_cancellation(&request("/m/shared.flac"), || {
                waiter_ready_tx.send(()).is_err()
            })
        });
        waiter_ready_rx.recv().unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.release();

        assert_eq!(leader.join().unwrap().document, Some(plain_doc("shared")));
        assert_eq!(
            waiter.join().unwrap().unwrap().document,
            Some(plain_doc("shared"))
        );
    }

    #[test]
    fn distinct_keys_resolve_in_parallel() {
        let calls = Arc::new(AtomicUsize::new(0));
        let source_barrier = Arc::new(Barrier::new(2));

        struct ParallelSource {
            calls: Arc<AtomicUsize>,
            barrier: Arc<Barrier>,
        }

        impl LyricsSource for ParallelSource {
            fn find(&self, _req: &LyricsRequest) -> Option<LyricsDocument> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.barrier.wait();
                Some(plain_doc("parallel"))
            }
        }

        let service = Arc::new(LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(ParallelSource {
                calls: calls.clone(),
                barrier: source_barrier,
            }),
        )]));
        let first_service = Arc::clone(&service);
        let first = thread::spawn(move || first_service.load(&request("/m/first.flac")));
        let second_service = Arc::clone(&service);
        let second = thread::spawn(move || second_service.load(&request("/m/second.flac")));

        assert_eq!(first.join().unwrap().document, Some(plain_doc("parallel")));
        assert_eq!(second.join().unwrap().document, Some(plain_doc("parallel")));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cancelling_one_waiter_does_not_cancel_shared_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (gate, started_rx) = resolution_gate();
        let release = gate.clone();
        let service = Arc::new(LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(GatedSource {
                calls: calls.clone(),
                gate,
                result: Some(plain_doc("survives")),
            }),
        )]));

        let leader_service = Arc::clone(&service);
        let leader = thread::spawn(move || leader_service.load(&request("/m/cancel.flac")));
        started_rx.recv().unwrap();

        let cancelled = Arc::new(AtomicBool::new(false));
        let (waiter_ready_tx, waiter_ready_rx) = mpsc::channel();
        let (cancel_seen_tx, cancel_seen_rx) = mpsc::channel();
        let waiter_service = Arc::clone(&service);
        let waiter_cancelled = Arc::clone(&cancelled);
        let waiter = thread::spawn(move || {
            waiter_service.load_with_cancellation(&request("/m/cancel.flac"), || {
                waiter_ready_tx.send(()).ok();
                if waiter_cancelled.load(Ordering::Acquire) {
                    cancel_seen_tx.send(()).ok();
                    true
                } else {
                    false
                }
            })
        });
        waiter_ready_rx.recv().unwrap();
        cancelled.store(true, Ordering::Release);
        cancel_seen_rx.recv().unwrap();

        assert!(waiter.join().unwrap().is_none());
        release.release();
        assert_eq!(leader.join().unwrap().document, Some(plain_doc("survives")));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_waiters_share_negative_outcomes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (gate, started_rx) = resolution_gate();
        let release = gate.clone();
        let service = Arc::new(LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(GatedSource {
                calls: calls.clone(),
                gate,
                result: None,
            }),
        )]));

        let leader_service = Arc::clone(&service);
        let leader = thread::spawn(move || leader_service.load(&request("/m/missing.flac")));
        started_rx.recv().unwrap();
        let (waiter_ready_tx, waiter_ready_rx) = mpsc::channel();
        let waiter_service = Arc::clone(&service);
        let waiter = thread::spawn(move || {
            waiter_service.load_with_cancellation(&request("/m/missing.flac"), || {
                waiter_ready_tx.send(()).is_err()
            })
        });
        waiter_ready_rx.recv().unwrap();
        release.release();

        let expected = leader.join().unwrap();
        assert_eq!(waiter.join().unwrap(), Some(expected.clone()));
        assert_eq!(expected.error.as_deref(), Some("Lyrics not found"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn post_completion_load_hits_the_memo() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = LyricsService::with_sources(vec![(
            LyricsOrigin::Local,
            Box::new(CountingSource {
                result: Some(plain_doc("memoized")),
                calls: calls.clone(),
            }),
        )]);

        let first = service.load(&request("/m/memoized.flac"));
        let second = service.load(&request("/m/memoized.flac"));

        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn remote_results_are_written_through_the_cache_writer() {
        use std::sync::Mutex;
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer_sink = written.clone();
        let writer: Box<CacheWriter> = Box::new(move |path: &Path, content: &str| {
            writer_sink
                .lock()
                .expect("cache writer is single threaded in this test")
                .push((path.to_path_buf(), content.to_string()));
            Ok(PathBuf::from("/cache/target.lrc"))
        });

        let service = LyricsService::with_sources(vec![(
            LyricsOrigin::Remote,
            Box::new(CountingSource {
                result: Some(plain_doc("Remote line")),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        )]);
        let service = service.with_cache_writer(writer);

        let outcome = service.load(&request("/m/song.flac"));

        assert_eq!(outcome.origin, Some(LyricsOrigin::Remote));
        assert_eq!(outcome.saved_path, Some(PathBuf::from("/cache/target.lrc")));
        let (path, content) = &written.lock().expect("cache writer lock")[0];
        assert_eq!(path, &PathBuf::from("/m/song.flac"));
        assert!(
            content.contains("[ti:"),
            "cached remote content must carry header metadata: {content}"
        );
    }

    #[test]
    fn local_and_metadata_results_are_never_rewritten() {
        let service = LyricsService::with_sources(vec![
            (
                LyricsOrigin::Local,
                Box::new(CountingSource {
                    result: Some(plain_doc("local")),
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
            ),
            (
                LyricsOrigin::Metadata,
                Box::new(CountingSource {
                    result: Some(plain_doc("meta")),
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
            ),
        ]);

        let outcome = service.load(&request("/m/song.flac"));
        assert_eq!(
            outcome.saved_path, None,
            "non remote results must not be persisted again"
        );

        let second0 = service.load(&request("/m/song.flac"));
        assert_eq!(second0.saved_path, None, "memo hit also skips the writer");
    }

    #[test]
    fn remote_disabled_skips_the_remote_source() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = LyricsService::with_sources(vec![
            (
                LyricsOrigin::Local,
                Box::new(CountingSource {
                    result: None,
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
            ),
            (
                LyricsOrigin::Remote,
                Box::new(CountingSource {
                    result: Some(plain_doc("remote")),
                    calls: calls.clone(),
                }),
            ),
        ])
        .with_remote(false);

        let outcome = service.load(&request("/m/song.flac"));

        assert_eq!(outcome.document, None);
        assert_eq!(outcome.origin, None);
        assert_eq!(outcome.error.as_deref(), Some("Lyrics not found"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a disabled remote source must never be consulted"
        );
    }

    #[test]
    fn remote_can_be_enabled_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = LyricsService::with_sources(vec![(
            LyricsOrigin::Remote,
            Box::new(CountingSource {
                result: Some(plain_doc("remote")),
                calls: calls.clone(),
            }),
        )])
        .with_remote(false);

        // The flag is atomic, so the service can be re-enabled after
        // construction even though every clone still shares it.
        service.set_remote_enabled(true);
        let outcome = service.load(&request("/m/song.flac"));

        assert_eq!(outcome.origin, Some(LyricsOrigin::Remote));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn enabling_remote_after_a_cached_miss_reconsults_the_source() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = LyricsService::with_sources(vec![(
            LyricsOrigin::Remote,
            Box::new(CountingSource {
                result: Some(plain_doc("remote")),
                calls: calls.clone(),
            }),
        )])
        .with_remote(false);

        // A miss with the remote source off is memoized, as every miss is.
        let first = service.load(&request("/m/song.flac"));
        assert_eq!(first.document, None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // Enabling the remote source must drop that stale miss so the same
        // track is re-resolved without restarting the app.
        service.set_remote_enabled(true);
        let second = service.load(&request("/m/song.flac"));

        assert_eq!(second.origin, Some(LyricsOrigin::Remote));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
