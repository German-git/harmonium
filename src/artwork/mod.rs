//! Album artwork loading, sizing and render state.
//!
//! This module is the boundary where `ratatui-image` and `image` types may
//! appear, mirroring how
//! `audio` hides rodio behind domain handles. Everything here is built for
//! the owner artwork policy: covers are optional, capability tolerant and
//! must never block the event loop, so the loader reports absence and
//! corruption through logs instead of errors at the call sites.

mod backend;

pub mod loader;
pub mod remote;
pub mod ueberzug;
pub mod ueberzugpp;

pub use backend::{ArtworkBackend, ArtworkBackendKind, ArtworkOverlay, ArtworkTeardown};

use std::cell::RefCell;
#[cfg(test)]
use std::cell::RefMut;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui_image::errors::Errors;

use crate::filesystem::persistence::atomic_replace;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{FontSize, Resize, ResizeEncodeRender, StatefulImage};

pub use loader::ArtworkSource;

use crate::config::ArtworkSource as SourceConfig;
use crate::metadata::TrackMetadata;

// The event bus moves decoded artwork from blocking workers to the UI
// thread, so the protocol must be Send for the pipeline to compile
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<StatefulProtocol>();
    assert_send::<ArtworkThreadProtocol>();
};

/// Maximum playlist artwork presentation size in terminal pixels per side.
const PLAYLIST_ARTWORK_MAX_PIXELS: u32 = 144;

/// Maximum pixel edge produced by a resize request.
///
/// Source decoding is bounded separately in `loader`; this second bound keeps
/// a hostile or accidentally huge terminal target from allocating an
/// unbounded output image during protocol encoding.
const MAX_RESIZE_PIXELS: u32 = 1024;

/// Bounded backlog for the shared artwork encoder.
const RESIZE_QUEUE_CAPACITY: usize = 4;

/// Wake interval used when shutdown races with an empty worker queue.
const RESIZE_SHUTDOWN_POLL: Duration = Duration::from_millis(25);

/// Failures the loader can hit while turning a source into an image.
///
/// Kept structured so the log line stays precise, but callers always
/// degrade to no artwork instead of propagating, per the owner policy.
#[derive(Debug, thiserror::Error)]
pub enum ArtworkError {
    /// The image bytes could not be read from disk.
    #[error("reading artwork {} failed: {source}", path.display())]
    Io {
        /// Cover file that failed.
        path: std::path::PathBuf,
        /// Underlying IO error.
        source: std::io::Error,
    },
    /// The image bytes could not be decoded.
    #[error("decoding artwork for {} failed: {message}", track.display())]
    Decode {
        /// Track the artwork belongs to.
        track: std::path::PathBuf,
        /// Decoder diagnostic.
        message: String,
    },
}

/// One request submitted to the process-local artwork resize worker.
struct ResizeRequest {
    generation: u64,
    target: Size,
    resize: Resize,
    protocol: StatefulProtocol,
    response: SyncSender<ResizeResponse>,
    response_failed: Arc<AtomicBool>,
}

/// Result routed back to the protocol that submitted the request.
///
/// The generation and target are intentionally carried outside the image
/// protocol. `ratatui-image`'s `ThreadProtocol` ids are private to that crate,
/// while this boundary must also reject results after artwork replacement.
struct ResizeResponse {
    generation: u64,
    target: Size,
    result: Result<StatefulProtocol, Errors>,
}

enum ResizeCommand {
    Request(ResizeRequest),
    Shutdown,
}

enum SubmitError {
    Full(ResizeRequest),
    Unavailable(ResizeRequest),
}

/// One bounded, long-lived encoder shared by all artwork protocols from a
/// loader. The worker owns no UI state and never holds a state lock while
/// resizing or encoding.
struct ArtworkResizeWorker {
    sender: SyncSender<ResizeCommand>,
    shutdown: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

impl ArtworkResizeWorker {
    fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel(RESIZE_QUEUE_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_finished = Arc::clone(&finished);

        let result = thread::Builder::new()
            .name("harmonium-artwork-resize".to_string())
            .spawn(move || {
                while !worker_shutdown.load(Ordering::Acquire) {
                    let command = match receiver.recv_timeout(RESIZE_SHUTDOWN_POLL) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    match command {
                        ResizeCommand::Shutdown => break,
                        ResizeCommand::Request(request) => {
                            if worker_shutdown.load(Ordering::Acquire) {
                                break;
                            }
                            let ResizeRequest {
                                generation,
                                target,
                                resize,
                                mut protocol,
                                response,
                                response_failed,
                            } = request;
                            protocol.resize_encode(&resize, target);
                            let result = match protocol.last_encoding_result() {
                                Some(result) => result.map(|()| protocol),
                                None => {
                                    tracing::warn!(
                                        "artwork resize produced no encoding result; marking request failed"
                                    );
                                    response_failed.store(true, Ordering::Release);
                                    continue;
                                }
                            };
                            let response = response.try_send(ResizeResponse {
                                generation,
                                target,
                                result,
                            });
                            if response.is_err() {
                                response_failed.store(true, Ordering::Release);
                            }
                        }
                    }
                }
                worker_finished.store(true, Ordering::Release);
            });
        if let Err(error) = result {
            tracing::warn!("could not start shared artwork resize worker: {error}");
            finished.store(true, Ordering::Release);
        }

        Self {
            sender,
            shutdown,
            finished,
        }
    }

    fn submit(&self, request: ResizeRequest) -> Result<(), SubmitError> {
        match self.sender.try_send(ResizeCommand::Request(request)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(ResizeCommand::Request(request))) => {
                Err(SubmitError::Full(request))
            }
            Err(TrySendError::Disconnected(ResizeCommand::Request(request))) => {
                Err(SubmitError::Unavailable(request))
            }
            Err(TrySendError::Full(ResizeCommand::Shutdown))
            | Err(TrySendError::Disconnected(ResizeCommand::Shutdown)) => {
                unreachable!("shutdown is never submitted through submit")
            }
        }
    }

    /// Signal shutdown without waiting for an in-flight encode.
    ///
    /// The request is bounded by the source and target limits above; dropping
    /// the join handle keeps application teardown from waiting on a worker that
    /// is already inside third-party image code.
    fn shutdown(&self) {
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        self.shutdown.store(true, Ordering::Release);
        let _ = self.sender.try_send(ResizeCommand::Shutdown);
    }

    #[cfg(test)]
    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl Drop for ArtworkResizeWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Shared picker wrapper handed to blocking artwork tasks.
///
/// The picker is built once on the main thread because querying the
/// terminal requires exclusive stdio access, then shared by reference
/// count so any worker can encode protocols off the event loop.
#[derive(Clone)]
pub struct ArtworkLoader {
    picker: Arc<Picker>,
    /// Immutable loading policy selected when the renderer is constructed.
    /// Runtime backend transitions belong to the renderer state machine.
    backend: ArtworkBackendKind,
    external_fallback: bool,
    resize_worker: Arc<ArtworkResizeWorker>,
}

impl ArtworkLoader {
    /// Wrap a picker built by the entrypoint.
    pub fn new(picker: Picker) -> Self {
        Self::with_backend(picker, ArtworkBackendKind::RatatuiImage)
    }

    /// Wrap a picker for automatic half-block fallback with an optional
    /// External presentation path for automatic half-block detection.
    pub fn auto(picker: Picker) -> Self {
        let backend = if picker.protocol_type() == ratatui_image::picker::ProtocolType::Halfblocks {
            ArtworkBackendKind::Ueberzugpp
        } else {
            ArtworkBackendKind::RatatuiImage
        };
        Self::with_backend(picker, backend)
    }

    /// Unicode half-block fallback that never queries the terminal.
    ///
    /// Besides the configured unicode mode this is the constructor tests
    /// use, keeping stdio untouched outside the real entrypoint.
    pub fn halfblocks() -> Self {
        Self::with_backend(Picker::halfblocks(), ArtworkBackendKind::Halfblocks)
    }

    /// Wrap a picker with an explicit backend policy.
    pub(crate) fn with_backend(picker: Picker, backend: ArtworkBackendKind) -> Self {
        Self {
            picker: Arc::new(picker),
            backend,
            external_fallback: false,
            resize_worker: Arc::new(ArtworkResizeWorker::new()),
        }
    }

    /// Native automatic mode keeps enough source information to cross into an
    /// external renderer if ratatui-image later reports a render failure.
    pub(crate) fn with_native_fallback(picker: Picker) -> Self {
        Self {
            picker: Arc::new(picker),
            backend: ArtworkBackendKind::RatatuiImage,
            external_fallback: true,
            resize_worker: Arc::new(ArtworkResizeWorker::new()),
        }
    }

    /// Whether this loader may provide paths for the automatic external image
    /// fallback. Explicit Unicode mode deliberately returns false here.
    pub fn uses_external_fallback(&self) -> bool {
        self.backend_kind().uses_external_layer()
    }

    /// Compatibility name for callers that only need to know whether the
    /// automatic external path is active.
    pub fn uses_ueberzug_fallback(&self) -> bool {
        self.uses_external_fallback()
    }

    pub(crate) fn supports_native_fallback(&self) -> bool {
        self.external_fallback
    }

    /// Stop accepting artwork resize work during application teardown.
    pub(crate) fn shutdown(&self) {
        self.resize_worker.shutdown();
    }

    #[cfg(test)]
    fn resize_worker(&self) -> Arc<ArtworkResizeWorker> {
        Arc::clone(&self.resize_worker)
    }

    /// Cell target that keeps playlist artwork within the 144px presentation
    /// bound for this terminal's detected font size.
    pub fn playlist_target(&self) -> Size {
        playlist_target_for_font(self.picker.font_size())
    }

    /// Resolve and decode artwork for an optional local `track_path`, preparing
    /// its render protocol. Streams omit the path and therefore skip embedded
    /// and directory probes while still allowing configured remote lookup.
    ///
    /// Returns `None` when the track has no usable cover, logging every
    /// failure as a warning, because a missing or corrupt cover is a
    /// normal condition that must never surface as an app error. Must run
    /// inside a blocking worker, never on the UI thread.
    pub fn load(
        &self,
        track_path: Option<&Path>,
        source_config: SourceConfig,
        metadata: Option<&TrackMetadata>,
        cache_dir: &Path,
    ) -> Option<LoadedArtwork> {
        let source = loader::resolve_source(track_path, source_config, metadata, cache_dir)?;
        let decode_path = track_path.unwrap_or_else(|| Path::new("<stream>"));
        let image = match loader::decode_source(decode_path, &source) {
            Ok(image) => image,
            Err(error) => {
                tracing::warn!("{error}");
                return None;
            }
        };
        let backend = self.backend_kind();
        let primary_font = match backend {
            ArtworkBackendKind::Halfblocks
            | ArtworkBackendKind::Ueberzugpp
            | ArtworkBackendKind::Ueberzug => Picker::halfblocks().font_size(),
            _ => self.picker.font_size(),
        };
        let fallback_resize_target = (backend.uses_external_layer()
            || (backend == ArtworkBackendKind::RatatuiImage && self.external_fallback))
            .then(|| resize_target_for_font(Picker::halfblocks().font_size()));
        let external_source = if backend.uses_external_layer()
            || (backend == ArtworkBackendKind::RatatuiImage && self.external_fallback)
        {
            match &source {
                ArtworkSource::File(path) => Some(MaterializedArtwork {
                    path: path.clone(),
                    policy: ArtworkPathPolicy::Borrowed,
                }),
                ArtworkSource::Embedded(bytes) | ArtworkSource::Remote(bytes) => {
                    materialize_for_ueberzug(bytes, cache_dir)
                }
            }
        } else {
            None
        };
        let fallback_protocol = (backend.uses_external_layer()
            || (backend == ArtworkBackendKind::RatatuiImage && self.external_fallback))
            .then(|| Picker::halfblocks().new_resize_protocol(image.clone()));
        let protocol = match backend {
            ArtworkBackendKind::Halfblocks
            | ArtworkBackendKind::Ueberzugpp
            | ArtworkBackendKind::Ueberzug => Picker::halfblocks().new_resize_protocol(image),
            _ => self.picker.new_resize_protocol(image),
        };
        Some(LoadedArtwork {
            protocol,
            fallback_protocol,
            external_source,
            resize_worker: Arc::clone(&self.resize_worker),
            resize_target: resize_target_for_font(primary_font),
            fallback_resize_target,
        })
    }

    fn backend_kind(&self) -> ArtworkBackendKind {
        self.backend
    }
}

/// Artwork decoded by a worker, with an optional filesystem representation for
/// the external renderer.
pub struct LoadedArtwork {
    protocol: StatefulProtocol,
    fallback_protocol: Option<StatefulProtocol>,
    external_source: Option<MaterializedArtwork>,
    resize_worker: Arc<ArtworkResizeWorker>,
    resize_target: Size,
    fallback_resize_target: Option<Size>,
}

/// Lifecycle policy for a path supplied to an external artwork renderer.
///
/// Content-addressed materialization is a shared cache entry, not a private
/// allocation belonging to one [`ArtworkState`]. Its lifecycle therefore must
/// not be coupled to replacement or drop of any one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtworkPathPolicy {
    /// An existing local cover that belongs to the user or another subsystem.
    Borrowed,
    /// A stable content-addressed entry shared by concurrent artwork states.
    SharedContentAddressedCache,
    /// A file created exclusively for one session and safe to remove on clear.
    PrivateSessionOwned,
}

impl ArtworkPathPolicy {
    fn cleanup(self, path: PathBuf) {
        if matches!(self, Self::PrivateSessionOwned) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A path paired with the policy that governs its lifecycle.
#[derive(Debug)]
struct MaterializedArtwork {
    path: PathBuf,
    policy: ArtworkPathPolicy,
}

type ResizeResponseQueue = Arc<Mutex<Receiver<ResizeResponse>>>;
type ArtworkProtocolParts = (
    ArtworkThreadProtocol,
    ResizeResponseQueue,
    Option<StatefulProtocol>,
    Option<MaterializedArtwork>,
    Option<Size>,
);

/// Materialize embedded or remote bytes at a stable content-addressed path.
///
/// The feature-owned directory prevents unbounded temporary-file creation and
/// the stable name lets concurrent artwork workers share one cache entry.
fn materialize_for_ueberzug(bytes: &[u8], cache_dir: &Path) -> Option<MaterializedArtwork> {
    use sha2::{Digest, Sha256};

    let format = image::guess_format(bytes).ok()?;
    let extension = match format {
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::Png => "png",
        _ => return None,
    };
    let root = if cache_dir.as_os_str().is_empty() {
        std::env::temp_dir()
            .join("harmonium")
            .join("ueberzugpp-artwork")
    } else {
        cache_dir.join("ueberzugpp-artwork")
    };
    std::fs::create_dir_all(&root).ok()?;
    let digest = Sha256::digest(bytes);
    let digest: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let name = format!("{digest}.{extension}");
    let path = root.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => return None,
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Err(error) = atomic_replace(&path, bytes) {
                tracing::debug!("ueberzugpp artwork materialization skipped: {error}");
                return None;
            }
        }
        Err(_) => return None,
    }
    Some(MaterializedArtwork {
        path,
        policy: ArtworkPathPolicy::SharedContentAddressedCache,
    })
}

/// Artwork payload carried over the event bus.
///
/// The protocol state implements neither `Debug` nor `Clone`, so this
/// newtype keeps [`crate::event::AppEvent`] derivable while documenting
/// the exact moment artwork crosses the worker boundary.
pub struct ArtworkProtocol {
    protocol: ArtworkThreadProtocol,
    responses: ResizeResponseQueue,
    fallback_protocol: Option<StatefulProtocol>,
    external_source: Option<MaterializedArtwork>,
    fallback_resize_target: Option<Size>,
}

impl ArtworkProtocol {
    /// Wrap a freshly decoded protocol with a test/local worker.
    ///
    /// Production artwork uses [`Self::from_loaded`], which attaches the
    /// loader-owned shared worker. Keeping this constructor available makes
    /// protocol fixtures independent and preserves the small crate API used by
    /// existing callers.
    #[cfg(test)]
    pub(crate) fn new(protocol: StatefulProtocol) -> Self {
        Self::with_worker(
            protocol,
            Arc::new(ArtworkResizeWorker::new()),
            resize_target_for_font(Picker::halfblocks().font_size()),
        )
    }

    fn with_worker(
        protocol: StatefulProtocol,
        resize_worker: Arc<ArtworkResizeWorker>,
        resize_target: Size,
    ) -> Self {
        let (response_tx, response_rx) = mpsc::sync_channel(RESIZE_QUEUE_CAPACITY);
        let response_failed = Arc::new(AtomicBool::new(false));
        Self {
            protocol: ArtworkThreadProtocol {
                inner: Some(protocol),
                resize_worker,
                response_tx,
                response_failed,
                generation: 0,
                pending: None,
                last_encoded_target: None,
                resize_target,
                worker_failed: false,
            },
            responses: Arc::new(Mutex::new(response_rx)),
            fallback_protocol: None,
            external_source: None,
            fallback_resize_target: None,
        }
    }

    /// Wrap a worker-loaded protocol and retain its optional external image
    /// source until the artwork is replaced or the application exits.
    pub(crate) fn from_loaded(loaded: LoadedArtwork) -> Self {
        let mut wrapped =
            Self::with_worker(loaded.protocol, loaded.resize_worker, loaded.resize_target);
        wrapped.fallback_protocol = loaded.fallback_protocol;
        wrapped.external_source = loaded.external_source;
        wrapped.fallback_resize_target = loaded.fallback_resize_target;
        wrapped
    }

    /// Consume the wrapper into the render protocol and its completion queue.
    fn into_parts(self) -> ArtworkProtocolParts {
        (
            self.protocol,
            self.responses,
            self.fallback_protocol,
            self.external_source,
            self.fallback_resize_target,
        )
    }
}

/// UI-facing protocol wrapper that multiplexes requests onto the loader's
/// shared bounded worker while retaining per-protocol result routing.
pub(crate) struct ArtworkThreadProtocol {
    inner: Option<StatefulProtocol>,
    resize_worker: Arc<ArtworkResizeWorker>,
    response_tx: SyncSender<ResizeResponse>,
    response_failed: Arc<AtomicBool>,
    generation: u64,
    pending: Option<(u64, Size)>,
    last_encoded_target: Option<Size>,
    resize_target: Size,
    worker_failed: bool,
}

impl ArtworkThreadProtocol {
    fn replace_protocol(&mut self, protocol: StatefulProtocol, resize_target: Size) {
        self.inner = Some(protocol);
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
        self.last_encoded_target = None;
        self.resize_target = resize_target;
        self.worker_failed = false;
    }

    #[cfg(test)]
    fn empty_protocol(&mut self) {
        self.inner = None;
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
    }

    fn update_resized_protocol(&mut self, response: ResizeResponse) -> ResizeOutcome {
        if self.pending != Some((response.generation, response.target))
            || self.generation != response.generation
        {
            return ResizeOutcome::Stale;
        }
        self.pending = None;
        match response.result {
            Ok(protocol) => {
                self.inner = Some(protocol);
                self.last_encoded_target = Some(response.target);
                ResizeOutcome::Applied
            }
            Err(error) => {
                tracing::warn!("artwork resize failed: {error:?}");
                self.inner = None;
                self.worker_failed = true;
                ResizeOutcome::Failed
            }
        }
    }

    fn take_worker_failure(&mut self) -> bool {
        std::mem::take(&mut self.worker_failed)
            || self.response_failed.swap(false, Ordering::Acquire)
    }

    fn size_for(&self, resize: Resize, size: Size) -> Option<Size> {
        self.inner
            .as_ref()
            .map(|protocol| protocol.size_for(resize, size))
    }

    fn bounded_target(&self, target: Size) -> Size {
        Size::new(
            target.width.min(self.resize_target.width).max(1),
            target.height.min(self.resize_target.height).max(1),
        )
    }
}

enum ResizeOutcome {
    Applied,
    Stale,
    Failed,
}

impl ResizeEncodeRender for ArtworkThreadProtocol {
    fn resize_encode(&mut self, resize: &Resize, size: Size) {
        let Some(protocol) = self.inner.take() else {
            return;
        };
        let target = self.bounded_target(size);
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let request = ResizeRequest {
            generation,
            target,
            resize: resize.clone(),
            protocol,
            response: self.response_tx.clone(),
            response_failed: Arc::clone(&self.response_failed),
        };
        match self.resize_worker.submit(request) {
            Ok(()) => self.pending = Some((generation, target)),
            Err(SubmitError::Full(request)) => {
                self.inner = Some(request.protocol);
            }
            Err(SubmitError::Unavailable(request)) => {
                self.inner = Some(request.protocol);
                self.worker_failed = true;
            }
        }
    }

    fn render(&mut self, area: Rect, buffer: &mut ratatui::buffer::Buffer) {
        if let Some(protocol) = self.inner.as_mut() {
            protocol.render(area, buffer);
        }
    }

    fn needs_resize(&self, resize: &Resize, size: Size) -> Option<Size> {
        let protocol = self.inner.as_ref()?;
        let target = self.bounded_target(size);
        let requested = protocol.needs_resize(resize, target)?;
        let bounded = self.bounded_target(requested);
        (self.last_encoded_target != Some(bounded)).then_some(bounded)
    }
}

impl fmt::Debug for ArtworkProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The encoded buffers are huge and uninteresting in diagnostics
        f.write_str("ArtworkProtocol(..)")
    }
}

/// Render side artwork state living inside [`crate::state::AppState`].
///
/// The protocol sits in a `RefCell` because ratatui's stateful widget API
/// needs `&mut` access at render time while the render path only holds
/// `&AppState`. Rendering is single threaded and the borrow lasts exactly
/// one widget draw, so interior mutability is sound here and keeps the
/// diff far smaller than splitting borrows through the whole UI tree.
///
/// # Borrow invariant
///
/// The internal native render accessor must be called **at most once per frame**.
/// A second `borrow_mut` before the first is dropped would panic
/// (`RefCell` double borrow), so any future widget that also draws the cover
/// must go through the same single access point rather than taking another
/// mutable borrow. This is the reason the accessor is the only way to reach
/// the protocol — never use `borrow_mut` directly from a caller.
pub struct ArtworkState {
    protocol: RefCell<Option<ArtworkThreadProtocol>>,
    resize_responses: RefCell<Option<ResizeResponseQueue>>,
    track_index: Option<usize>,
    backend: ArtworkBackendKind,
    enabled: bool,
    visible: bool,
    playlist_target: Size,
    external_source: Option<MaterializedArtwork>,
    fallback_protocol: RefCell<Option<StatefulProtocol>>,
    fallback_resize_target: Option<Size>,
    native_render_failed: bool,
    resize_state: ResizeLifecycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResizeLifecycle {
    Ready,
    Submitted { generation: u64, target: Size },
    Failed,
}

impl Default for ArtworkState {
    fn default() -> Self {
        Self {
            protocol: RefCell::new(None),
            resize_responses: RefCell::new(None),
            track_index: None,
            backend: ArtworkBackendKind::Disabled,
            // Artwork starts disabled and the entrypoint enables it only
            // when configuration and terminal capabilities both allow it
            enabled: false,
            // Artwork is visible by default, toggled by the user at runtime
            visible: true,
            playlist_target: playlist_target_for_font(FontSize::new(10, 20)),
            external_source: None,
            fallback_protocol: RefCell::new(None),
            fallback_resize_target: None,
            native_render_failed: false,
            resize_state: ResizeLifecycle::Ready,
        }
    }
}

impl fmt::Debug for ArtworkState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtworkState")
            .field("has_protocol", &self.protocol.borrow().is_some())
            .field("track_index", &self.track_index)
            .field("backend", &self.backend)
            .field("enabled", &self.enabled)
            .field("visible", &self.visible)
            .finish()
    }
}

impl ArtworkState {
    /// Whether artwork display is active for this session.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Turn artwork display on or off, applied once at startup.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Whether the artwork cell is visible (user toggle).
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Flip the artwork visibility toggle.
    pub fn toggle_visible(&mut self) {
        self.visible = !self.visible;
    }

    /// Set artwork visibility (used when restoring persisted state).
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    /// Queue index the stored protocol belongs to, if any.
    pub fn track_index(&self) -> Option<usize> {
        self.track_index
    }

    /// Select the renderer that owns the next artwork draw.
    pub(crate) fn set_backend(&mut self, backend: ArtworkBackendKind) {
        self.backend = backend;
    }

    /// Whether a decoded cover is ready to render.
    pub fn has_artwork(&self) -> bool {
        let primary_ready = self.protocol.borrow().as_ref().is_some_and(|protocol| {
            protocol
                .size_for(Resize::Fit(None), Size::new(1, 1))
                .is_some()
        });
        if primary_ready {
            return true;
        }
        let fallback_ready = self
            .fallback_protocol
            .borrow_mut()
            .as_mut()
            .is_some_and(|protocol| {
                let size = protocol.size_for(Resize::Fit(None), Size::new(1, 1));
                size.width > 0 && size.height > 0
            });
        fallback_ready || self.external_source.is_some()
    }

    /// Replace the stored cover and its resize completion queue, recording the track it belongs to.
    ///
    /// `None` clears the cell so a track without usable artwork never
    /// keeps the previous track cover on screen.
    pub fn set_artwork(&mut self, track_index: usize, protocol: Option<ArtworkProtocol>) {
        self.track_index = Some(track_index);
        self.resize_state = ResizeLifecycle::Ready;
        let Some(protocol) = protocol else {
            *self.protocol.get_mut() = None;
            *self.resize_responses.get_mut() = None;
            self.clear_external_source();
            *self.fallback_protocol.get_mut() = None;
            self.fallback_resize_target = None;
            self.native_render_failed = false;
            return;
        };
        let (protocol, responses, fallback_protocol, external_source, fallback_resize_target) =
            protocol.into_parts();
        if self
            .external_source
            .as_ref()
            .zip(external_source.as_ref())
            .is_none_or(|(previous, next)| previous.path != next.path)
        {
            self.clear_external_source();
        }
        *self.protocol.get_mut() = Some(protocol);
        *self.resize_responses.get_mut() = Some(responses);
        *self.fallback_protocol.get_mut() = fallback_protocol;
        self.fallback_resize_target = fallback_resize_target;
        self.external_source = external_source;
        self.native_render_failed = false;
    }

    /// Set the terminal-aware playlist presentation target.
    pub fn set_playlist_target(&mut self, target: Size) {
        self.playlist_target = target;
    }

    /// Return the bounded playlist presentation target in terminal cells.
    pub fn playlist_target(&self) -> Size {
        self.playlist_target
    }

    /// Filesystem source available to the optional external renderer.
    pub fn external_source(&self) -> Option<PathBuf> {
        self.external_source
            .as_ref()
            .map(|source| source.path.clone())
    }

    /// Apply completed worker responses without doing resize or encoding work.
    ///
    /// Responses are drained before each frame. The per-protocol channel is
    /// discarded together with the protocol, so replacement and track changes
    /// cannot admit stale artwork.
    pub fn apply_pending_resizes(&mut self) {
        let Some(protocol) = self.protocol.get_mut().as_mut() else {
            return;
        };
        let Some(responses) = self.resize_responses.get_mut().as_mut() else {
            return;
        };
        loop {
            let response = {
                let responses = responses.lock().unwrap_or_else(|error| error.into_inner());
                responses.try_recv()
            };
            match response {
                Ok(response) => match protocol.update_resized_protocol(response) {
                    ResizeOutcome::Applied => {
                        self.resize_state = ResizeLifecycle::Ready;
                    }
                    ResizeOutcome::Stale => {}
                    ResizeOutcome::Failed => {
                        self.resize_state = ResizeLifecycle::Failed;
                        self.native_render_failed = true;
                    }
                },
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if protocol.pending.is_some() {
                        protocol.inner = None;
                        protocol.pending = None;
                        self.resize_state = ResizeLifecycle::Failed;
                        self.native_render_failed = true;
                    }
                    break;
                }
            }
        }
        if protocol.take_worker_failure() {
            protocol.inner = None;
            self.resize_state = ResizeLifecycle::Failed;
            self.native_render_failed = true;
        }
    }

    /// Decide and submit a resize from the frame tick. Rendering only consumes
    /// the resulting protocol and never enters this transition.
    pub fn submit_resize(&mut self, target: Size) {
        let Some(protocol) = self.protocol.get_mut().as_mut() else {
            return;
        };
        if protocol.pending.is_some() {
            return;
        }
        let Some(requested) = protocol.needs_resize(&Resize::Scale(None), target) else {
            return;
        };
        protocol.resize_encode(&Resize::Scale(None), requested);
        if protocol.take_worker_failure() {
            self.resize_state = ResizeLifecycle::Failed;
            self.native_render_failed = true;
        } else if let Some((generation, target)) = protocol.pending {
            self.resize_state = ResizeLifecycle::Submitted { generation, target };
        }
    }

    #[cfg(test)]
    pub(crate) fn resize_state_for_test(&self) -> ResizeLifecycle {
        self.resize_state
    }

    /// Mutable protocol access scoped to a single widget render.
    ///
    /// See the struct docs for why interior mutability lives here. The
    /// returned borrow must not outlive the draw call that uses it, and this
    /// method may only run once per frame: a second concurrent borrow would
    /// panic. Callers never hold `RefMut` across frames or re-render the same
    /// cover from two widgets in one pass.
    #[cfg(test)]
    pub(crate) fn protocol_for_render(&self) -> RefMut<'_, Option<ArtworkThreadProtocol>> {
        self.protocol.borrow_mut()
    }

    #[cfg(test)]
    pub(crate) fn fail_primary_for_test(&self) {
        if let Some(protocol) = self.protocol.borrow_mut().as_mut() {
            protocol.empty_protocol();
        }
    }

    #[cfg(test)]
    pub(crate) fn backend_for_test(&self) -> ArtworkBackendKind {
        self.backend
    }

    /// Report and consume a ratatui-image failure observed by the render path.
    ///
    /// Ratatui-image does not expose a general widget error result. The
    /// supported failure boundary is therefore the resize worker response: a
    /// failed or disconnected response marks the native protocol unusable.
    pub fn take_native_render_failure(&mut self) -> bool {
        std::mem::take(&mut self.native_render_failed)
    }

    /// Replace a failed native protocol with the prepared half-block protocol.
    /// This also removes any external source so a later fallback cannot replay
    /// a stale overlay.
    pub(crate) fn switch_to_halfblocks(&mut self) {
        if let Some(protocol) = self.fallback_protocol.get_mut().take() {
            let fallback_target = self
                .fallback_resize_target
                .take()
                .unwrap_or_else(|| resize_target_for_font(Picker::halfblocks().font_size()));
            if let Some(current) = self.protocol.get_mut().as_mut() {
                current.replace_protocol(protocol, fallback_target);
            }
        }
        self.clear_external_source();
        self.native_render_failed = false;
    }

    fn clear_external_source(&mut self) {
        if let Some(source) = self.external_source.take() {
            source.policy.cleanup(source.path);
        }
    }

    /// Render through the selected native protocol without exposing
    /// ratatui-image types to panel code.
    pub(crate) fn render(&self, frame: &mut Frame, area: Rect) {
        if !matches!(
            self.backend,
            ArtworkBackendKind::RatatuiImage | ArtworkBackendKind::Halfblocks
        ) {
            return;
        }
        let mut protocol = self.protocol.borrow_mut();
        if let Some(protocol) = protocol.as_mut() {
            frame.render_stateful_widget(
                StatefulImage::new().resize(Resize::Scale(None)),
                area,
                protocol,
            );
        }
    }

    /// Return the image-aware size for a terminal-cell target.
    pub(crate) fn image_size(&self, target: Size) -> Option<Size> {
        let native_size = self
            .protocol
            .borrow_mut()
            .as_mut()
            .and_then(|protocol| protocol.size_for(Resize::Scale(None), target));

        native_size
            .or_else(|| {
                self.fallback_protocol
                    .borrow_mut()
                    .as_mut()
                    .map(|protocol| protocol.size_for(Resize::Scale(None), target))
            })
            .filter(|size| size.width > 0 && size.height > 0)
            .or_else(|| self.external_source.as_ref().map(|_| target))
    }
}

impl Drop for ArtworkState {
    fn drop(&mut self) {
        self.clear_external_source();
    }
}

/// Convert the 144px playlist bound into a target expressed in terminal cells.
///
/// Flooring each dimension is intentional: the encoded target must never exceed
/// the presentation requirement, even when a terminal reports a large font.
pub(crate) fn playlist_target_for_font(font_size: FontSize) -> Size {
    let width = u32::from(font_size.width.max(1));
    let height = u32::from(font_size.height.max(1));
    Size::new(
        (PLAYLIST_ARTWORK_MAX_PIXELS / width).max(1) as u16,
        (PLAYLIST_ARTWORK_MAX_PIXELS / height).max(1) as u16,
    )
}

/// Convert the worker's pixel ceiling into a terminal-cell target for a
/// protocol using `font_size`. Flooring keeps every encoded dimension within
/// the bound while still allowing at least one cell in either direction.
fn resize_target_for_font(font_size: FontSize) -> Size {
    let width = u32::from(font_size.width.max(1));
    let height = u32::from(font_size.height.max(1));
    Size::new(
        (MAX_RESIZE_PIXELS / width).max(1) as u16,
        (MAX_RESIZE_PIXELS / height).max(1) as u16,
    )
}

/// Test fixtures hiding the image crates from the rest of the crate.
///
/// Unit tests outside this module need valid covers and protocols without
/// being allowed to name `ratatui-image` or `image` types, so the fixtures
/// live behind these plain functions.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Smallest valid PNG, encoded so the bytes always decode again.
    pub(crate) fn tiny_png() -> Vec<u8> {
        let image = image::DynamicImage::new_rgba8(2, 2);
        let mut buffer = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("png encoding");
        buffer.into_inner()
    }

    /// A real protocol over a tiny image, built with the half-block
    /// picker so tests never query the terminal.
    pub(crate) fn test_protocol() -> StatefulProtocol {
        Picker::halfblocks().new_resize_protocol(image::DynamicImage::new_rgba8(2, 2))
    }

    /// Test-only protocol carrying a local path for the external overlay.
    pub(crate) fn test_protocol_with_source(path: PathBuf) -> ArtworkProtocol {
        let mut protocol = ArtworkProtocol::new(test_protocol());
        protocol.external_source = Some(MaterializedArtwork {
            path,
            policy: ArtworkPathPolicy::Borrowed,
        });
        protocol
    }

    /// Test-only protocol carrying both a native fallback and an external path.
    pub(crate) fn test_protocol_with_fallback_source(path: PathBuf) -> ArtworkProtocol {
        let mut protocol = test_protocol_with_source(path);
        protocol.fallback_protocol = Some(test_protocol());
        protocol
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artwork::testing::test_protocol;
    use ratatui_image::ResizeEncodeRender;

    fn protocol_with_source(path: PathBuf, policy: ArtworkPathPolicy) -> ArtworkProtocol {
        let mut protocol = ArtworkProtocol::new(test_protocol());
        protocol.external_source = Some(MaterializedArtwork { path, policy });
        protocol
    }

    #[test]
    fn default_state_is_disabled_with_no_cover() {
        let state = ArtworkState::default();

        assert!(!state.is_enabled());
        assert_eq!(state.track_index(), None);
        assert!(!state.has_artwork());
        assert_eq!(state.image_size(Size::new(14, 7)), None);
    }

    #[test]
    fn set_artwork_records_the_track_and_clearing_removes_the_cover() {
        let mut state = ArtworkState::default();

        state.set_artwork(3, Some(ArtworkProtocol::new(test_protocol())));

        assert_eq!(state.track_index(), Some(3));
        assert!(state.has_artwork());
        assert!(state.image_size(Size::new(14, 7)).is_some());

        state.set_artwork(4, None);

        assert_eq!(state.track_index(), Some(4));
        assert!(!state.has_artwork(), "a track without art clears the cell");
        assert_eq!(state.image_size(Size::new(14, 7)), None);
    }

    #[test]
    fn image_size_uses_the_native_protocol_when_available() {
        let mut state = ArtworkState::default();
        let target = Size::new(14, 7);
        state.set_artwork(0, Some(ArtworkProtocol::new(test_protocol())));

        let size = state
            .image_size(target)
            .expect("native artwork should provide a size");

        assert!(size.width > 0 && size.height > 0);
        assert_eq!(state.has_artwork(), state.image_size(target).is_some());
    }

    #[test]
    fn image_size_uses_the_native_fallback_when_the_primary_is_absent() {
        let mut state = ArtworkState::default();
        let target = Size::new(14, 7);
        *state.fallback_protocol.get_mut() = Some(test_protocol());

        let size = state
            .image_size(target)
            .expect("fallback artwork should provide a size");

        assert!(size.width > 0 && size.height > 0);
        assert_eq!(state.has_artwork(), state.image_size(target).is_some());
    }

    #[test]
    fn image_size_uses_the_cached_external_source_when_native_protocols_are_absent() {
        let mut state = ArtworkState::default();
        let target = Size::new(14, 7);
        state.external_source = Some(MaterializedArtwork {
            path: PathBuf::from("/cache/cover.png"),
            policy: ArtworkPathPolicy::Borrowed,
        });

        assert_eq!(state.image_size(target), Some(target));
        assert_eq!(state.has_artwork(), state.image_size(target).is_some());
    }

    #[test]
    fn image_size_preserves_fallback_precedence_over_the_external_source() {
        let mut state = ArtworkState::default();
        let target = Size::new(14, 7);
        *state.fallback_protocol.get_mut() = Some(test_protocol());
        state.external_source = Some(MaterializedArtwork {
            path: PathBuf::from("/cache/cover.png"),
            policy: ArtworkPathPolicy::Borrowed,
        });

        let fallback_size = state
            .fallback_protocol
            .get_mut()
            .as_mut()
            .expect("fallback protocol")
            .size_for(Resize::Scale(None), target);

        assert_eq!(state.image_size(target), Some(fallback_size));
        assert_eq!(state.has_artwork(), state.image_size(target).is_some());
    }

    #[test]
    fn invalid_artwork_does_not_create_a_protocol_or_panic() {
        let root = tempfile::tempdir().expect("artwork fixture directory");
        let track = root.path().join("song.mp3");
        std::fs::write(&track, b"audio").expect("track fixture");
        std::fs::write(root.path().join("cover.png"), b"not an image")
            .expect("invalid cover fixture");

        let loader = ArtworkLoader::halfblocks();
        assert!(
            loader
                .load(Some(&track), SourceConfig::All, None, root.path())
                .is_none()
        );

        let state = ArtworkState::default();
        assert!(!state.has_artwork());
        assert_eq!(state.image_size(Size::new(14, 7)), None);
    }

    #[test]
    fn state_reports_cover_presence_in_debug_output() {
        let state = ArtworkState::default();

        let rendered = format!("{state:?}");

        assert!(rendered.contains("has_protocol: false"));
    }

    #[test]
    fn toggle_visible_flips_the_visibility_state() {
        let mut state = ArtworkState::default();
        assert!(state.is_visible());

        state.toggle_visible();
        assert!(!state.is_visible());

        state.toggle_visible();
        assert!(state.is_visible());
    }

    #[test]
    fn set_visible_overwrites_the_visibility_state() {
        let mut state = ArtworkState::default();
        assert!(state.is_visible());

        state.set_visible(false);
        assert!(!state.is_visible());

        state.set_visible(true);
        assert!(state.is_visible());
    }

    #[test]
    fn switching_to_halfblocks_clears_external_source_and_keeps_artwork() {
        let mut state = ArtworkState::default();
        state.set_artwork(
            0,
            Some(testing::test_protocol_with_fallback_source(PathBuf::from(
                "/cache/cover.png",
            ))),
        );

        assert!(state.external_source().is_some());
        state.switch_to_halfblocks();

        assert!(state.external_source().is_none());
        assert!(state.has_artwork());
    }

    #[test]
    fn render_suppresses_native_protocol_for_external_backends() {
        let mut state = ArtworkState::default();
        state.set_artwork(0, Some(ArtworkProtocol::new(test_protocol())));
        let backend = ratatui::backend::TestBackend::new(8, 4);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        for backend in [ArtworkBackendKind::Ueberzugpp, ArtworkBackendKind::Ueberzug] {
            state.set_backend(backend);
            let protocol_borrow = state.protocol.borrow_mut();
            terminal
                .draw(|frame| state.render(frame, frame.area()))
                .expect("external rendering should not borrow the native protocol");
            drop(protocol_borrow);
        }

        state.set_backend(ArtworkBackendKind::Halfblocks);
        assert_eq!(state.backend_for_test(), ArtworkBackendKind::Halfblocks);
        terminal
            .draw(|frame| state.render(frame, frame.area()))
            .expect("halfblock rendering should be restored");
    }

    #[test]
    fn playlist_target_stays_within_the_144_pixel_bound() {
        let target = playlist_target_for_font(FontSize::new(10, 20));

        assert_eq!(target, Size::new(14, 7));
        assert!(u32::from(target.width) * 10 <= PLAYLIST_ARTWORK_MAX_PIXELS);
        assert!(u32::from(target.height) * 20 <= PLAYLIST_ARTWORK_MAX_PIXELS);
    }

    #[test]
    fn automatic_halfblocks_enable_external_fallback_but_explicit_unicode_does_not() {
        assert!(ArtworkLoader::auto(Picker::halfblocks()).uses_ueberzug_fallback());
        assert!(!ArtworkLoader::halfblocks().uses_ueberzug_fallback());
    }

    #[test]
    fn halfblock_transition_stops_external_materialization_for_later_loads() {
        let root = tempfile::tempdir().expect("artwork fixture directory");
        let track = root.path().join("song.mp3");
        std::fs::write(&track, b"audio").expect("track fixture");
        std::fs::write(root.path().join("cover.png"), testing::tiny_png()).expect("cover fixture");

        let loader =
            ArtworkLoader::with_backend(Picker::halfblocks(), ArtworkBackendKind::Halfblocks);

        let loaded = loader
            .load(Some(&track), SourceConfig::All, None, root.path())
            .expect("cover should load");

        assert!(loaded.external_source.is_none());
    }

    #[test]
    fn materialized_external_source_uses_a_stable_feature_cache_path() {
        let root = tempfile::tempdir().expect("cache directory");
        let bytes = testing::tiny_png();
        let first = materialize_for_ueberzug(&bytes, root.path()).expect("materialized cover");
        let second = materialize_for_ueberzug(&bytes, root.path()).expect("cached cover");

        assert_eq!(first.path, second.path);
        assert_eq!(first.policy, ArtworkPathPolicy::SharedContentAddressedCache);
        assert_eq!(
            second.policy,
            ArtworkPathPolicy::SharedContentAddressedCache
        );
        assert!(
            first
                .path
                .starts_with(root.path().join("ueberzugpp-artwork"))
        );
        assert!(
            first
                .path
                .extension()
                .is_some_and(|extension| extension == "png")
        );
    }

    #[test]
    fn shared_cache_path_survives_state_replacement_clear_and_drop_order() {
        let root = tempfile::tempdir().expect("cache directory");
        let first_bytes = testing::tiny_png();
        let second_bytes = {
            let image = image::DynamicImage::new_rgba8(3, 2);
            let mut buffer = std::io::Cursor::new(Vec::new());
            image
                .write_to(&mut buffer, image::ImageFormat::Png)
                .expect("png encoding");
            buffer.into_inner()
        };
        let first =
            materialize_for_ueberzug(&first_bytes, root.path()).expect("first materialized cover");
        let first_path = first.path.clone();
        let cache_hit =
            materialize_for_ueberzug(&first_bytes, root.path()).expect("shared cache hit");
        assert_eq!(cache_hit.path, first_path);
        assert!(first_path.is_file());

        let second = materialize_for_ueberzug(&second_bytes, root.path())
            .expect("second materialized cover");
        let second_path = second.path.clone();
        assert_ne!(first_path, second_path);

        let mut first_state = ArtworkState::default();
        let mut second_state = ArtworkState::default();
        first_state.set_artwork(
            1,
            Some(protocol_with_source(
                first_path.clone(),
                ArtworkPathPolicy::SharedContentAddressedCache,
            )),
        );
        second_state.set_artwork(
            2,
            Some(protocol_with_source(
                first_path.clone(),
                ArtworkPathPolicy::SharedContentAddressedCache,
            )),
        );

        first_state.set_artwork(
            3,
            Some(protocol_with_source(
                second_path.clone(),
                ArtworkPathPolicy::SharedContentAddressedCache,
            )),
        );
        assert!(first_path.is_file(), "replacement must retain shared cache");
        drop(first_state);
        assert!(
            first_path.is_file(),
            "dropping one state must not remove a shared cache hit"
        );
        assert!(
            second_path.is_file(),
            "replacement cache must remain available"
        );

        second_state.set_artwork(4, None);
        assert!(
            first_path.is_file(),
            "clearing the last state must not remove shared cache data"
        );
        drop(second_state);
        assert!(first_path.is_file());
        assert!(second_path.is_file());

        let retained_hit = materialize_for_ueberzug(&first_bytes, root.path())
            .expect("retained cache entry must remain a hit");
        assert_eq!(retained_hit.path, first_path);
    }

    #[test]
    fn private_session_owned_path_is_removed_on_clear() {
        let root = tempfile::tempdir().expect("private artwork directory");
        let path = root.path().join("private.png");
        std::fs::write(&path, b"private artwork").expect("private fixture");

        let mut state = ArtworkState::default();
        state.set_artwork(
            0,
            Some(protocol_with_source(
                path.clone(),
                ArtworkPathPolicy::PrivateSessionOwned,
            )),
        );
        state.set_artwork(1, None);

        assert!(!path.exists(), "private session files remain state-owned");
    }

    #[test]
    fn resize_worker_completes_pending_target_without_render_thread_work() {
        let mut state = ArtworkState::default();
        state.set_artwork(0, Some(ArtworkProtocol::new(test_protocol())));

        {
            let mut protocol = state.protocol_for_render();
            let protocol = protocol.as_mut().expect("protocol is present");
            let target = protocol
                .needs_resize(&Resize::Scale(None), Size::new(14, 7))
                .expect("a new target needs encoding");
            protocol.resize_encode(&Resize::Scale(None), target);
        }

        assert!(
            !state.has_artwork(),
            "pending protocol must not render stale data"
        );
        for _ in 0..100 {
            state.apply_pending_resizes();
            if state.has_artwork() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            state.has_artwork(),
            "worker response should restore renderable art"
        );
    }

    #[test]
    fn artwork_protocols_share_one_bounded_worker_and_shutdown_is_bounded() {
        let loader = ArtworkLoader::halfblocks();
        let worker = loader.resize_worker();
        let root = tempfile::tempdir().expect("artwork fixture directory");
        let track = root.path().join("song.mp3");
        std::fs::write(&track, b"audio").expect("track fixture");
        std::fs::write(root.path().join("cover.png"), testing::tiny_png()).expect("cover fixture");
        let first = ArtworkProtocol::from_loaded(
            loader
                .load(Some(&track), SourceConfig::All, None, root.path())
                .expect("first artwork loads"),
        );
        let second = ArtworkProtocol::from_loaded(
            loader
                .load(Some(&track), SourceConfig::All, None, root.path())
                .expect("second artwork loads"),
        );

        assert!(Arc::ptr_eq(
            &first.protocol.resize_worker,
            &second.protocol.resize_worker
        ));
        loader.shutdown();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !worker.is_finished() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(worker.is_finished(), "shared resize worker did not stop");
    }

    #[test]
    fn shared_worker_routes_concurrent_protocol_requests_to_their_owners() {
        let worker = Arc::new(ArtworkResizeWorker::new());
        let target = resize_target_for_font(Picker::halfblocks().font_size());
        let mut first = ArtworkState::default();
        let mut second = ArtworkState::default();
        first.set_artwork(
            1,
            Some(ArtworkProtocol::with_worker(
                test_protocol(),
                Arc::clone(&worker),
                target,
            )),
        );
        second.set_artwork(
            2,
            Some(ArtworkProtocol::with_worker(
                test_protocol(),
                Arc::clone(&worker),
                target,
            )),
        );

        for state in [&mut first, &mut second] {
            let mut protocol = state.protocol_for_render();
            let protocol = protocol.as_mut().expect("protocol is present");
            protocol.resize_encode(&Resize::Scale(None), Size::new(30, 20));
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while (!first.has_artwork() || !second.has_artwork())
            && std::time::Instant::now() < deadline
        {
            first.apply_pending_resizes();
            second.apply_pending_resizes();
            std::thread::yield_now();
        }
        assert!(first.has_artwork(), "first protocol lost its response");
        assert!(second.has_artwork(), "second protocol lost its response");
        worker.shutdown();
    }

    #[test]
    fn stale_generation_is_rejected_after_protocol_replacement() {
        let worker = Arc::new(ArtworkResizeWorker::new());
        let target = resize_target_for_font(Picker::halfblocks().font_size());
        let mut protocol = ArtworkProtocol::with_worker(test_protocol(), worker.clone(), target);
        protocol
            .protocol
            .resize_encode(&Resize::Scale(None), Size::new(20, 10));
        let (generation, request_target) = protocol.protocol.pending.expect("pending request");

        protocol.protocol.replace_protocol(test_protocol(), target);
        let outcome = protocol.protocol.update_resized_protocol(ResizeResponse {
            generation,
            target: request_target,
            result: Ok(test_protocol()),
        });

        assert!(matches!(outcome, ResizeOutcome::Stale));
        assert!(protocol.protocol.inner.is_some());
        worker.shutdown();
    }

    #[test]
    fn resize_requests_are_pixel_bounded_and_keep_output_renderable() {
        let worker = Arc::new(ArtworkResizeWorker::new());
        let target = resize_target_for_font(Picker::halfblocks().font_size());
        let mut protocol = ArtworkProtocol::with_worker(test_protocol(), worker.clone(), target);
        protocol
            .protocol
            .resize_encode(&Resize::Scale(None), Size::new(u16::MAX, u16::MAX));
        let (_, bounded) = protocol.protocol.pending.expect("bounded request");

        assert!(u32::from(bounded.width) * 10 <= MAX_RESIZE_PIXELS);
        assert!(u32::from(bounded.height) * 20 <= MAX_RESIZE_PIXELS);

        let mut state = ArtworkState::default();
        state.set_artwork(0, Some(protocol));
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !state.has_artwork() && std::time::Instant::now() < deadline {
            state.apply_pending_resizes();
            std::thread::yield_now();
        }
        assert!(state.has_artwork(), "bounded output was not applied");
        worker.shutdown();
    }

    #[test]
    fn unavailable_worker_marks_native_artwork_failed_without_blocking() {
        let worker = Arc::new(ArtworkResizeWorker::new());
        worker.shutdown();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !worker.is_finished() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }

        let mut state = ArtworkState::default();
        state.set_artwork(
            0,
            Some(ArtworkProtocol::with_worker(
                test_protocol(),
                worker,
                resize_target_for_font(Picker::halfblocks().font_size()),
            )),
        );
        {
            let mut protocol = state.protocol_for_render();
            protocol
                .as_mut()
                .expect("protocol is present")
                .resize_encode(&Resize::Scale(None), Size::new(20, 10));
        }
        state.apply_pending_resizes();

        assert_eq!(state.resize_state_for_test(), ResizeLifecycle::Failed);
        assert!(state.take_native_render_failure());
        assert!(!state.has_artwork());
    }

    #[test]
    fn full_resize_queue_keeps_ready_state_and_preserves_generation_progress() {
        let (sender, _receiver) = mpsc::sync_channel(0);
        let worker = Arc::new(ArtworkResizeWorker {
            sender,
            shutdown: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
        });
        let target = resize_target_for_font(Picker::halfblocks().font_size());
        let mut state = ArtworkState::default();
        state.set_artwork(
            0,
            Some(ArtworkProtocol::with_worker(
                test_protocol(),
                worker.clone(),
                target,
            )),
        );

        state.submit_resize(Size::new(20, 10));

        assert_eq!(state.resize_state_for_test(), ResizeLifecycle::Ready);
        assert!(!state.take_native_render_failure());
        let protocol = state.protocol_for_render();
        assert_eq!(
            protocol
                .as_ref()
                .expect("protocol remains available")
                .generation,
            1
        );
        worker.shutdown();
    }

    #[test]
    fn missing_encoding_result_marks_request_failed_and_keeps_worker_running() {
        let worker = Arc::new(ArtworkResizeWorker::new());
        let (response, responses) = mpsc::sync_channel(RESIZE_QUEUE_CAPACITY);
        let response_failed = Arc::new(AtomicBool::new(false));

        assert!(
            worker
                .submit(ResizeRequest {
                    generation: 1,
                    target: Size::new(0, 0),
                    resize: Resize::Scale(None),
                    protocol: test_protocol(),
                    response: response.clone(),
                    response_failed: Arc::clone(&response_failed),
                })
                .is_ok(),
            "failed encoding request should be accepted"
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !response_failed.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }

        assert!(response_failed.load(Ordering::Acquire));
        assert!(matches!(
            responses.try_recv(),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected)
        ));

        assert!(
            worker
                .submit(ResizeRequest {
                    generation: 2,
                    target: Size::new(1, 1),
                    resize: Resize::Scale(None),
                    protocol: test_protocol(),
                    response,
                    response_failed: Arc::clone(&response_failed),
                })
                .is_ok(),
            "shared worker should accept requests after a failed encoding"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let response = loop {
            match responses.try_recv() {
                Ok(response) => break response,
                Err(mpsc::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Err(error) => panic!("worker response was not delivered: {error:?}"),
            }
        };
        assert!(response.result.is_ok());
        assert!(
            !worker.is_finished(),
            "a missing encoding result must not stop the shared worker"
        );
        worker.shutdown();
    }

    #[test]
    fn browser_target_can_use_more_source_than_playlist_target() {
        let protocol =
            Picker::halfblocks().new_resize_protocol(image::DynamicImage::new_rgba8(512, 512));
        let playlist = protocol.size_for(
            Resize::Scale(None),
            playlist_target_for_font(FontSize::new(10, 20)),
        );
        let browser = protocol.size_for(Resize::Scale(None), Size::new(40, 20));

        assert!(browser.width > playlist.width);
        assert!(browser.height > playlist.height);
    }
}
