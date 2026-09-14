//! Artwork backend selection and runtime lifecycle.

use std::io;
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex};

use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use sha2::{Digest, Sha256};

use super::ueberzug::Manager as LegacyManager;
use super::ueberzugpp::Manager as UeberzugppManager;
use super::{ArtworkLoader, ArtworkState};
use crate::config::AlbumArtMode;

/// Process cleanup capability shared with the application's panic hook.
///
/// The hook owns only this bounded, replaceable capability, never a reference
/// to the stack frame that created the artwork backend. Process termination is
/// best effort and panic-free because the hook also runs for an active panic,
/// including release builds that abort immediately afterwards.
#[derive(Clone, Default)]
pub struct ArtworkTeardown {
    active: Arc<Mutex<Option<Arc<ExternalProcess>>>>,
}

impl ArtworkTeardown {
    /// Create an empty artwork teardown capability.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the one currently owned external process cleanup capability.
    ///
    /// Registration is intentionally a single replaceable slot: artwork owns
    /// at most one external layer at a time, so an unbounded global registry is
    /// neither necessary nor safe during panic handling.
    pub(crate) fn register_process(&self, process: Arc<ExternalProcess>) {
        let previous = match self.active.lock() {
            Ok(mut active) => active.replace(process),
            Err(poisoned) => poisoned.into_inner().replace(process),
        };
        if let Some(previous) = previous {
            previous.terminate();
        }
    }

    /// Run and forget the currently registered process cleanup capability.
    ///
    /// Taking the capability before invoking it keeps repeated normal teardown and
    /// a later panic-hook invocation idempotent.
    pub fn cleanup(&self) {
        let cleanup = match self.active.lock() {
            Ok(mut active) => active.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(cleanup) = cleanup {
            cleanup.terminate();
        }
    }
}

/// Owns one external artwork child behind a shared, panic-hook-safe handle.
pub(crate) struct ExternalProcess {
    child: Mutex<Option<Child>>,
}

impl ExternalProcess {
    pub(crate) fn new(child: Child) -> Arc<Self> {
        Arc::new(Self {
            child: Mutex::new(Some(child)),
        })
    }

    pub(crate) fn has_exited(&self) -> io::Result<bool> {
        let mut child = match self.child.lock() {
            Ok(child) => child,
            Err(poisoned) => poisoned.into_inner(),
        };
        match child.as_mut() {
            Some(child) => child.try_wait().map(|status| status.is_some()),
            None => Ok(true),
        }
    }

    /// Terminate and reap the child. The only normal holder is the UI thread,
    /// and child operations themselves do not invoke user code or panic, so a
    /// panic on another thread can wait for this short critical section
    /// without racing the child owner.
    pub(crate) fn terminate(&self) {
        let mut child = match self.child.lock() {
            Ok(child) => child,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(mut child) = child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Backend-neutral lifecycle for an external image layer.
///
/// The adapters deliberately own their process, parser and payload details.
/// The artwork backend only needs to know how to reconcile one desired layer,
/// whether the adapter is still healthy, and when it must be cleaned up.
pub(crate) trait ExternalArtworkLayer {
    fn reconcile(&mut self, desired: Option<ArtworkOverlay>);
    fn is_healthy(&self) -> bool;
    fn cleanup(&mut self);
    fn panic_cleanup(&self) -> Option<Arc<ExternalProcess>>;
}

/// Renderer selected for the current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtworkBackendKind {
    /// Artwork is disabled by configuration or unavailable in explicit image mode.
    Disabled,
    /// `ratatui-image` is rendering through a graphics protocol.
    RatatuiImage,
    /// `ueberzugpp` is the preferred renderer, with half-blocks underneath it.
    Ueberzugpp,
    /// Original `ueberzug` is the second external renderer, when available.
    Ueberzug,
    /// Native Unicode half-block rendering is active.
    Halfblocks,
}

/// Exact terminal-cell placement for one artwork layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtworkOverlay {
    /// Stable identifier used by external backends when replacing an image.
    pub identifier: String,
    /// Image path supplied to the external backend.
    pub path: PathBuf,
    /// Absolute terminal-cell geometry, matching the native artwork rect.
    pub rect: Rect,
}

impl ArtworkOverlay {
    /// Build an overlay identity from the queue track and image path without
    /// exposing either value through logs.
    pub fn new(track_index: usize, path: PathBuf, rect: Rect) -> Self {
        let mut digest = Sha256::new();
        digest.update(track_index.to_le_bytes());
        digest.update(path.as_os_str().to_string_lossy().as_bytes());
        let identifier = format!("harmonium-artwork-{:x}", digest.finalize());
        Self {
            identifier,
            path,
            rect,
        }
    }
}

impl ArtworkBackendKind {
    pub(crate) const fn uses_external_layer(self) -> bool {
        matches!(self, Self::Ueberzugpp | Self::Ueberzug)
    }
}

/// Owns the selected artwork loader and any optional external renderer.
///
/// Source resolution and decoding remain in [`ArtworkLoader`]. This type only
/// decides which renderer is preferred and owns the renderer lifecycle, so a
/// process failure can transition to native half-blocks without affecting
/// loading, geometry, or stale-load handling.
pub struct ArtworkBackend {
    renderer: ArtworkRenderer,
    loader: Option<ArtworkLoader>,
    teardown: ArtworkTeardown,
}

enum ArtworkRenderer {
    Disabled,
    Native(ArtworkBackendKind),
    External {
        kind: ArtworkBackendKind,
        layer: Option<Box<dyn ExternalArtworkLayer>>,
    },
}

impl ArtworkBackend {
    /// Build a backend from the terminal picker result and the configured mode.
    ///
    /// `Auto` keeps a half-block picker as the native safety net while marking
    /// the external backend as the preferred alternative. Startup calls
    /// [`Self::start_external`] after the terminal is attached.
    pub fn from_detected_picker(mode: AlbumArtMode, picker: Option<Picker>) -> Self {
        Self::from_detected_picker_with_teardown(mode, picker, ArtworkTeardown::new())
    }

    /// Build a backend using the panic-hook capability owned by the entrypoint.
    pub fn from_detected_picker_with_teardown(
        mode: AlbumArtMode,
        picker: Option<Picker>,
        teardown: ArtworkTeardown,
    ) -> Self {
        let mut backend = match mode {
            AlbumArtMode::Off => Self::disabled(),
            AlbumArtMode::Unicode => Self::halfblocks(),
            AlbumArtMode::Image => match picker {
                Some(picker) if picker.protocol_type() != ProtocolType::Halfblocks => {
                    Self::native(picker)
                }
                _ => Self::disabled(),
            },
            AlbumArtMode::Auto => match picker {
                Some(picker) if picker.protocol_type() != ProtocolType::Halfblocks => {
                    Self::native_with_fallback(picker)
                }
                Some(_) | None => Self::ueberzug_candidate(),
            },
        };
        backend.teardown = teardown;
        backend
    }

    /// Current renderer selection.
    pub fn kind(&self) -> ArtworkBackendKind {
        match &self.renderer {
            ArtworkRenderer::Disabled => ArtworkBackendKind::Disabled,
            ArtworkRenderer::Native(kind) => *kind,
            ArtworkRenderer::External { kind, .. } => *kind,
        }
    }

    /// Loader shared with background artwork effects.
    pub fn loader(&self) -> Option<&ArtworkLoader> {
        self.loader.as_ref()
    }

    /// Synchronize the selected backend with the render state before startup.
    pub fn synchronize_state(&self, artwork: &mut ArtworkState) {
        artwork.set_backend(self.kind());
    }

    /// Try to start the preferred external renderer.
    ///
    /// A missing executable or unusable IPC pipe tries original `ueberzug`
    /// before changing the selection to native half-blocks.
    pub fn start_external(&mut self, artwork: &mut ArtworkState) {
        if self.kind() != ArtworkBackendKind::Ueberzugpp {
            return;
        }
        self.start_external_with_factories(
            || {
                UeberzugppManager::spawn()
                    .map(|manager| Box::new(manager) as Box<dyn ExternalArtworkLayer>)
            },
            || {
                LegacyManager::spawn()
                    .map(|manager| Box::new(manager) as Box<dyn ExternalArtworkLayer>)
            },
            artwork,
        );
    }

    fn start_external_with_factories<U, L>(
        &mut self,
        spawn_ueberzugpp: U,
        spawn_ueberzug: L,
        artwork: &mut ArtworkState,
    ) where
        U: FnOnce() -> Option<Box<dyn ExternalArtworkLayer>>,
        L: FnOnce() -> Option<Box<dyn ExternalArtworkLayer>>,
    {
        let ueberzugpp = spawn_ueberzugpp();
        let ueberzug = ueberzugpp.is_none().then(spawn_ueberzug).flatten();
        self.start_external_with(ueberzugpp, ueberzug, artwork);
    }

    fn start_external_with(
        &mut self,
        ueberzugpp: Option<Box<dyn ExternalArtworkLayer>>,
        ueberzug: Option<Box<dyn ExternalArtworkLayer>>,
        artwork: &mut ArtworkState,
    ) {
        if self.kind() != ArtworkBackendKind::Ueberzugpp {
            return;
        }
        if let Some(layer) = ueberzugpp {
            self.register_panic_cleanup(layer.as_ref());
            artwork.set_backend(ArtworkBackendKind::Ueberzugpp);
            self.renderer = ArtworkRenderer::External {
                kind: ArtworkBackendKind::Ueberzugpp,
                layer: Some(layer),
            };
        } else if let Some(layer) = ueberzug {
            self.register_panic_cleanup(layer.as_ref());
            self.set_kind(ArtworkBackendKind::Ueberzug);
            artwork.set_backend(ArtworkBackendKind::Ueberzug);
            self.renderer = ArtworkRenderer::External {
                kind: ArtworkBackendKind::Ueberzug,
                layer: Some(layer),
            };
        } else {
            self.fallback_to_halfblocks(artwork);
        }
    }

    /// Report the only native failure currently observable from ratatui-image:
    /// a failed or disconnected resize response. Auto mode then starts the
    /// external candidates in priority order. Other modes deliberately remain
    /// unchanged.
    pub fn report_native_failure(&mut self, artwork: &mut ArtworkState) {
        self.report_native_failure_with_factories(
            || {
                UeberzugppManager::spawn()
                    .map(|manager| Box::new(manager) as Box<dyn ExternalArtworkLayer>)
            },
            || {
                LegacyManager::spawn()
                    .map(|manager| Box::new(manager) as Box<dyn ExternalArtworkLayer>)
            },
            artwork,
        );
    }

    fn report_native_failure_with_factories<U, L>(
        &mut self,
        spawn_ueberzugpp: U,
        spawn_ueberzug: L,
        artwork: &mut ArtworkState,
    ) where
        U: FnOnce() -> Option<Box<dyn ExternalArtworkLayer>>,
        L: FnOnce() -> Option<Box<dyn ExternalArtworkLayer>>,
    {
        if self.kind() != ArtworkBackendKind::RatatuiImage
            || !self
                .loader
                .as_ref()
                .is_some_and(ArtworkLoader::supports_native_fallback)
        {
            return;
        }
        self.set_kind(ArtworkBackendKind::Ueberzugpp);
        artwork.set_backend(ArtworkBackendKind::Ueberzugpp);
        self.start_external_with_factories(spawn_ueberzugpp, spawn_ueberzug, artwork);
    }

    /// Reconcile the selected external layer after the terminal frame draws.
    ///
    /// Native renderers ignore this call. A failed ueberzugpp adapter is
    /// replaced by original ueberzug before native half-blocks are selected.
    pub fn reconcile(&mut self, desired: Option<ArtworkOverlay>, artwork: &mut ArtworkState) {
        self.reconcile_with_fallback(desired, None, artwork);
    }

    fn reconcile_with_fallback(
        &mut self,
        desired: Option<ArtworkOverlay>,
        fallback: Option<Box<dyn ExternalArtworkLayer>>,
        artwork: &mut ArtworkState,
    ) {
        if !self.kind().uses_external_layer() {
            return;
        }
        let healthy = match self.external_mut() {
            Some(manager) => {
                manager.reconcile(desired.clone());
                manager.is_healthy()
            }
            None => false,
        };
        if healthy {
            return;
        }

        let failed_kind = self.kind();
        self.cleanup_external();
        let fallback = if failed_kind == ArtworkBackendKind::Ueberzugpp {
            fallback.or_else(|| {
                LegacyManager::spawn()
                    .map(|manager| Box::new(manager) as Box<dyn ExternalArtworkLayer>)
            })
        } else {
            None
        };
        if failed_kind == ArtworkBackendKind::Ueberzugpp
            && let Some(mut layer) = fallback
        {
            self.register_panic_cleanup(layer.as_ref());
            self.set_kind(ArtworkBackendKind::Ueberzug);
            artwork.set_backend(ArtworkBackendKind::Ueberzug);
            layer.reconcile(desired);
            if layer.is_healthy() {
                self.renderer = ArtworkRenderer::External {
                    kind: ArtworkBackendKind::Ueberzug,
                    layer: Some(layer),
                };
                return;
            }
        }
        self.fallback_to_halfblocks(artwork);
    }

    fn native(picker: Picker) -> Self {
        Self {
            renderer: ArtworkRenderer::Native(ArtworkBackendKind::RatatuiImage),
            loader: Some(ArtworkLoader::new(picker)),
            teardown: ArtworkTeardown::new(),
        }
    }

    fn native_with_fallback(picker: Picker) -> Self {
        Self {
            renderer: ArtworkRenderer::Native(ArtworkBackendKind::RatatuiImage),
            loader: Some(ArtworkLoader::with_native_fallback(picker)),
            teardown: ArtworkTeardown::new(),
        }
    }

    fn halfblocks() -> Self {
        Self {
            renderer: ArtworkRenderer::Native(ArtworkBackendKind::Halfblocks),
            loader: Some(ArtworkLoader::halfblocks()),
            teardown: ArtworkTeardown::new(),
        }
    }

    fn ueberzug_candidate() -> Self {
        Self {
            renderer: ArtworkRenderer::External {
                kind: ArtworkBackendKind::Ueberzugpp,
                layer: None,
            },
            loader: Some(ArtworkLoader::with_backend(
                Picker::halfblocks(),
                ArtworkBackendKind::Ueberzugpp,
            )),
            teardown: ArtworkTeardown::new(),
        }
    }

    fn disabled() -> Self {
        Self {
            renderer: ArtworkRenderer::Disabled,
            loader: None,
            teardown: ArtworkTeardown::new(),
        }
    }

    fn fallback_to_halfblocks(&mut self, artwork: &mut ArtworkState) {
        self.cleanup_external();
        self.set_kind(ArtworkBackendKind::Halfblocks);
        artwork.set_backend(ArtworkBackendKind::Halfblocks);
        artwork.switch_to_halfblocks();
    }

    fn cleanup_external(&mut self) {
        let layer = match &mut self.renderer {
            ArtworkRenderer::External { layer, .. } => layer.take(),
            _ => None,
        };
        if let Some(mut layer) = layer {
            layer.cleanup();
        }
        self.teardown.cleanup();
    }

    fn set_kind(&mut self, kind: ArtworkBackendKind) {
        if let ArtworkRenderer::External { kind: current, .. } = &mut self.renderer {
            *current = kind;
        } else {
            self.renderer = if kind == ArtworkBackendKind::Disabled {
                ArtworkRenderer::Disabled
            } else {
                ArtworkRenderer::Native(kind)
            };
        }
    }

    fn register_panic_cleanup(&self, layer: &dyn ExternalArtworkLayer) {
        if let Some(process) = layer.panic_cleanup() {
            self.teardown.register_process(process);
        }
    }

    fn external_mut(&mut self) -> Option<&mut Box<dyn ExternalArtworkLayer>> {
        match &mut self.renderer {
            ArtworkRenderer::External { layer, .. } => layer.as_mut(),
            _ => None,
        }
    }
}

impl Drop for ArtworkBackend {
    fn drop(&mut self) {
        self.cleanup_external();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn explicit_modes_select_deterministic_backends() {
        assert_eq!(
            ArtworkBackend::from_detected_picker(AlbumArtMode::Off, None).kind(),
            ArtworkBackendKind::Disabled
        );
        assert_eq!(
            ArtworkBackend::from_detected_picker(AlbumArtMode::Unicode, None).kind(),
            ArtworkBackendKind::Halfblocks
        );
        assert!(
            !ArtworkBackend::from_detected_picker(AlbumArtMode::Unicode, None)
                .loader()
                .is_some_and(ArtworkLoader::uses_external_fallback)
        );
        assert_eq!(
            ArtworkBackend::from_detected_picker(AlbumArtMode::Image, Some(Picker::halfblocks()))
                .kind(),
            ArtworkBackendKind::Disabled
        );
        assert_eq!(
            ArtworkBackend::from_detected_picker(AlbumArtMode::Auto, None).kind(),
            ArtworkBackendKind::Ueberzugpp
        );
    }

    #[test]
    fn auto_halfblocks_prefers_ueberzug_then_falls_back_to_halfblocks() {
        let mut backend =
            ArtworkBackend::from_detected_picker(AlbumArtMode::Auto, Some(Picker::halfblocks()));
        assert_eq!(backend.kind(), ArtworkBackendKind::Ueberzugpp);

        let mut artwork = ArtworkState::default();
        backend.start_external_with(None, None, &mut artwork);

        assert_eq!(backend.kind(), ArtworkBackendKind::Halfblocks);
        assert!(backend.loader().is_some());
    }

    #[test]
    fn auto_external_start_uses_ueberzugpp_before_original_ueberzug() {
        let mut backend =
            ArtworkBackend::from_detected_picker(AlbumArtMode::Auto, Some(Picker::halfblocks()));
        let mut artwork = ArtworkState::default();

        backend.start_external_with(
            Some(Box::new(UeberzugppManager::from_writer(Box::new(
                io::sink(),
            )))),
            Some(Box::new(LegacyManager::from_writer(Box::new(io::sink())))),
            &mut artwork,
        );

        assert_eq!(backend.kind(), ArtworkBackendKind::Ueberzugpp);
        assert_eq!(artwork.backend_for_test(), ArtworkBackendKind::Ueberzugpp);

        let mut fallback_backend =
            ArtworkBackend::from_detected_picker(AlbumArtMode::Auto, Some(Picker::halfblocks()));
        fallback_backend.start_external_with(
            None,
            Some(Box::new(LegacyManager::from_writer(Box::new(io::sink())))),
            &mut artwork,
        );
        assert_eq!(fallback_backend.kind(), ArtworkBackendKind::Ueberzug);
        assert_eq!(artwork.backend_for_test(), ArtworkBackendKind::Ueberzug);
    }

    #[test]
    fn external_start_factories_are_lazy_and_ordered() {
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut backend =
            ArtworkBackend::from_detected_picker(AlbumArtMode::Auto, Some(Picker::halfblocks()));
        let mut artwork = ArtworkState::default();
        let ueberzugpp_order = order.clone();
        let ueberzug_order = order.clone();

        backend.start_external_with_factories(
            move || {
                ueberzugpp_order
                    .lock()
                    .expect("factory order lock")
                    .push("ueberzugpp");
                Some(
                    Box::new(UeberzugppManager::from_writer(Box::new(io::sink())))
                        as Box<dyn ExternalArtworkLayer>,
                )
            },
            move || {
                ueberzug_order
                    .lock()
                    .expect("factory order lock")
                    .push("ueberzug");
                Some(Box::new(LegacyManager::from_writer(Box::new(io::sink())))
                    as Box<dyn ExternalArtworkLayer>)
            },
            &mut artwork,
        );

        assert_eq!(backend.kind(), ArtworkBackendKind::Ueberzugpp);
        assert_eq!(artwork.backend_for_test(), ArtworkBackendKind::Ueberzugpp);
        assert_eq!(
            *order.lock().expect("factory order lock"),
            vec!["ueberzugpp"]
        );

        let fallback_order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut fallback_backend =
            ArtworkBackend::from_detected_picker(AlbumArtMode::Auto, Some(Picker::halfblocks()));
        let mut fallback_artwork = ArtworkState::default();
        let ueberzugpp_order = fallback_order.clone();
        let ueberzug_order = fallback_order.clone();

        fallback_backend.start_external_with_factories(
            move || {
                ueberzugpp_order
                    .lock()
                    .expect("factory order lock")
                    .push("ueberzugpp");
                None
            },
            move || {
                ueberzug_order
                    .lock()
                    .expect("factory order lock")
                    .push("ueberzug");
                Some(Box::new(LegacyManager::from_writer(Box::new(io::sink())))
                    as Box<dyn ExternalArtworkLayer>)
            },
            &mut fallback_artwork,
        );

        assert_eq!(fallback_backend.kind(), ArtworkBackendKind::Ueberzug);
        assert_eq!(
            *fallback_order.lock().expect("factory order lock"),
            vec!["ueberzugpp", "ueberzug"]
        );
    }

    #[test]
    fn native_failure_crosses_to_external_without_changing_explicit_modes() {
        let mut backend = ArtworkBackend::native_with_fallback(Picker::halfblocks());
        let mut artwork = ArtworkState::default();
        artwork.set_artwork(
            0,
            Some(crate::artwork::testing::test_protocol_with_fallback_source(
                PathBuf::from("/cache/cover.png"),
            )),
        );

        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ueberzugpp_order = order.clone();
        let ueberzug_order = order.clone();
        backend.report_native_failure_with_factories(
            move || {
                ueberzugpp_order
                    .lock()
                    .expect("factory order lock")
                    .push("ueberzugpp");
                Some(
                    Box::new(UeberzugppManager::from_writer(Box::new(io::sink())))
                        as Box<dyn ExternalArtworkLayer>,
                )
            },
            move || {
                ueberzug_order
                    .lock()
                    .expect("factory order lock")
                    .push("ueberzug");
                Some(Box::new(LegacyManager::from_writer(Box::new(io::sink())))
                    as Box<dyn ExternalArtworkLayer>)
            },
            &mut artwork,
        );

        assert_eq!(backend.kind(), ArtworkBackendKind::Ueberzugpp);
        assert_eq!(
            *order.lock().expect("factory order lock"),
            vec!["ueberzugpp"]
        );
        assert_eq!(backend.kind(), artwork.backend_for_test());
        assert!(artwork.external_source().is_some());
    }

    #[test]
    fn ueberzugpp_ipc_failure_transitions_to_original_ueberzug() {
        struct Broken;

        impl io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut backend = ArtworkBackend::ueberzug_candidate();
        let mut artwork = ArtworkState::default();
        artwork.set_artwork(
            0,
            Some(crate::artwork::testing::test_protocol_with_fallback_source(
                PathBuf::from("/cache/cover.png"),
            )),
        );
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let fallback_writer = CapturingWriter(captured.clone());
        backend.start_external_with(
            Some(Box::new(UeberzugppManager::from_writer(Box::new(Broken)))),
            None,
            &mut artwork,
        );
        backend.reconcile_with_fallback(
            Some(ArtworkOverlay::new(
                1,
                PathBuf::from("/cache/cover.png"),
                Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
            )),
            Some(Box::new(LegacyManager::from_writer(Box::new(
                fallback_writer,
            )))),
            &mut artwork,
        );

        assert_eq!(backend.kind(), ArtworkBackendKind::Ueberzug);
        assert_eq!(artwork.backend_for_test(), ArtworkBackendKind::Ueberzug);
        let payload = String::from_utf8(captured.lock().expect("capture lock").clone())
            .expect("JSON payload");
        assert!(payload.contains("\"scaler\":\"contain\""));
        assert!(artwork.external_source().is_some());
    }

    #[test]
    fn original_ueberzug_ipc_failure_transitions_to_native_halfblocks() {
        struct Broken;

        impl io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut backend = ArtworkBackend::ueberzug_candidate();
        let mut artwork = ArtworkState::default();
        artwork.set_artwork(
            0,
            Some(crate::artwork::testing::test_protocol_with_fallback_source(
                PathBuf::from("/cache/cover.png"),
            )),
        );
        backend.start_external_with(
            None,
            Some(Box::new(LegacyManager::from_writer(Box::new(Broken)))),
            &mut artwork,
        );
        backend.reconcile(
            Some(ArtworkOverlay::new(
                1,
                PathBuf::from("/cache/cover.png"),
                Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
            )),
            &mut artwork,
        );

        assert_eq!(backend.kind(), ArtworkBackendKind::Halfblocks);
        assert_eq!(artwork.backend_for_test(), ArtworkBackendKind::Halfblocks);
        assert_eq!(backend.kind(), artwork.backend_for_test());
        assert!(artwork.external_source().is_none());
    }

    struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl io::Write for CapturingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct CleanupLayer(Arc<ExternalProcess>);

    impl ExternalArtworkLayer for CleanupLayer {
        fn reconcile(&mut self, _desired: Option<ArtworkOverlay>) {}

        fn is_healthy(&self) -> bool {
            true
        }

        fn cleanup(&mut self) {}

        fn panic_cleanup(&self) -> Option<Arc<ExternalProcess>> {
            Some(Arc::clone(&self.0))
        }
    }

    struct UnhealthyLayer;

    impl ExternalArtworkLayer for UnhealthyLayer {
        fn reconcile(&mut self, _desired: Option<ArtworkOverlay>) {}

        fn is_healthy(&self) -> bool {
            false
        }

        fn cleanup(&mut self) {}

        fn panic_cleanup(&self) -> Option<Arc<ExternalProcess>> {
            None
        }
    }

    struct PanicDuringReconcileLayer {
        teardown: ArtworkTeardown,
        process: Arc<ExternalProcess>,
    }

    impl ExternalArtworkLayer for PanicDuringReconcileLayer {
        fn reconcile(&mut self, _desired: Option<ArtworkOverlay>) {
            self.teardown.cleanup();
            panic!("injected fallback reconciliation panic");
        }

        fn is_healthy(&self) -> bool {
            true
        }

        fn cleanup(&mut self) {}

        fn panic_cleanup(&self) -> Option<Arc<ExternalProcess>> {
            Some(Arc::clone(&self.process))
        }
    }

    #[test]
    fn fallback_registers_process_before_reconciliation() {
        let process = ExternalProcess::new(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn fallback test artwork process"),
        );
        let teardown = ArtworkTeardown::new();
        let mut backend = ArtworkBackend::from_detected_picker_with_teardown(
            AlbumArtMode::Auto,
            Some(Picker::halfblocks()),
            teardown.clone(),
        );
        let mut artwork = ArtworkState::default();
        backend.start_external_with(Some(Box::new(UnhealthyLayer)), None, &mut artwork);

        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            backend.reconcile_with_fallback(
                None,
                Some(Box::new(PanicDuringReconcileLayer {
                    teardown: teardown.clone(),
                    process: Arc::clone(&process),
                })),
                &mut artwork,
            );
        }));

        let exited = process.has_exited().expect("fallback test process status");
        process.terminate();
        assert!(panic_result.is_err());
        assert!(
            exited,
            "fallback process was not registered before reconciliation"
        );
    }

    #[test]
    fn dropping_backend_cleans_registered_external_layer_once() {
        let process = ExternalProcess::new(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn test artwork process"),
        );
        let teardown = ArtworkTeardown::new();
        {
            let mut backend = ArtworkBackend::from_detected_picker_with_teardown(
                AlbumArtMode::Auto,
                Some(Picker::halfblocks()),
                teardown.clone(),
            );
            backend.start_external_with(
                Some(Box::new(CleanupLayer(Arc::clone(&process)))),
                None,
                &mut ArtworkState::default(),
            );
        }

        teardown.cleanup();
        assert!(process.has_exited().expect("test process status"));
    }

    #[test]
    fn teardown_reaps_replaced_processes_and_is_idempotent() {
        let first = ExternalProcess::new(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn first test artwork process"),
        );
        let second = ExternalProcess::new(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn second test artwork process"),
        );
        let teardown = ArtworkTeardown::new();

        teardown.register_process(Arc::clone(&first));
        teardown.register_process(Arc::clone(&second));
        assert!(first.has_exited().expect("first test process status"));

        teardown.cleanup();
        assert!(second.has_exited().expect("second test process status"));
        teardown.cleanup();
    }
}
