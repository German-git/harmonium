//! Background effect dispatch and event publication.

use super::*;
use crate::filesystem::{FilesystemRenameService, RenameService};
use crate::runtime::{
    EffectServices, OperationContext, OperationKind, OperationRegistrationError, Spawner,
};
use std::sync::atomic::AtomicUsize;

/// RAII guard that decrements the in-flight effect counter on drop.
///
/// Moved into every spawned background task so the counter is always
/// decremented when the task body finishes, even when it bails out early or
/// panics. The `Drop` impl runs on every unwind path; the task's success
/// or failure outcome is observable through the existing notification and
/// event channels, never through the counter.
pub(crate) struct EffectGuard(Arc<AtomicUsize>);

/// State transition required when an effect could not enter the runtime.
/// Dispatch admission happens after the command handler has already moved the
/// UI into its loading state, so the failure must carry the same identity back
/// to the application instead of being reduced to a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchCompensation {
    StreamResolution { request_id: u64 },
    BrowserValidation { request_id: u64 },
    Search { request_id: u64 },
    OutputEnumeration { request_id: u64 },
    Playlist { request_id: u64 },
    ThemeLoad { request_id: u64, visit_id: u64 },
    ThemeSave { request_id: u64, visit_id: u64 },
    FileRename { request_id: u64 },
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DispatchFailure {
    pub(crate) operation: &'static str,
    pub(crate) error: OperationRegistrationError,
    pub(crate) compensation: DispatchCompensation,
}

impl EffectGuard {
    /// Increment the counter and return a guard whose drop restores it.
    pub(crate) fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(Arc::clone(counter))
    }
}

impl Drop for EffectGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Spawn a background task wrapped with an [`EffectGuard`] so the in-flight
/// counter tracks exactly one task for the lifetime of the closure.
///
/// Keeping the increment+guard inside the same helper means each effect arm
/// of [`execute_effects`] stays focused on its own work; the counter never
/// drifts from the real set of in-flight tasks because the guard's `Drop`
/// runs whether the task returns `Ok`, an error, or unwinds.
pub(crate) fn dispatch_with_guard<S, F>(
    services: &S,
    counter: &Arc<AtomicUsize>,
    name: &'static str,
    kind: EffectErrorKind,
    task: F,
) -> Result<(), OperationRegistrationError>
where
    S: Spawner + ?Sized,
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let counter = Arc::clone(counter);
    services.spawn_background(
        name,
        kind,
        Box::pin(async move {
            let _guard = EffectGuard::new(&counter);
            task.await
        }),
    )
}

/// Spawn a superseding application operation while keeping the existing
/// pending-effect accounting separate from runtime lifecycle state.
pub(crate) fn dispatch_with_operation<S, F, Fut>(
    services: &S,
    counter: &Arc<AtomicUsize>,
    name: &'static str,
    kind: OperationKind,
    task: F,
) -> Result<(), OperationRegistrationError>
where
    S: Spawner + ?Sized,
    F: FnOnce(OperationContext) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let counter = Arc::clone(counter);
    services
        .spawn_operation(
            name,
            kind,
            Box::new(move |context| {
                Box::pin(async move {
                    let _guard = EffectGuard::new(&counter);
                    task(context).await
                })
            }),
        )
        .map(|_| ())
}

/// Spawn non-superseding background work with an operation context. Completion
/// events use this path when carrying an identity is safe without changing the
/// fire-and-forget ordering of older generic effects.
pub(crate) fn dispatch_with_background_operation<S, F, Fut>(
    services: &S,
    counter: &Arc<AtomicUsize>,
    name: &'static str,
    kind: EffectErrorKind,
    task: F,
) -> Result<(), OperationRegistrationError>
where
    S: Spawner + ?Sized,
    F: FnOnce(OperationContext) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let counter = Arc::clone(counter);
    services
        .spawn_background_with_context(
            name,
            kind,
            Box::new(move |context| {
                Box::pin(async move {
                    let _guard = EffectGuard::new(&counter);
                    task(context).await
                })
            }),
        )
        .map(|_| ())
}

fn report_dispatch_error(
    name: &'static str,
    result: Result<(), OperationRegistrationError>,
    failures: &mut Vec<DispatchFailure>,
    compensation: DispatchCompensation,
) {
    if let Err(error) = result {
        tracing::warn!(
            operation = name,
            ?error,
            "could not dispatch application operation"
        );
        failures.push(DispatchFailure {
            operation: name,
            error,
            compensation,
        });
    }
}

pub(crate) fn worker_panic_error(
    operation: &'static str,
    error: tokio::task::JoinError,
) -> WorkerError {
    WorkerError::new(operation, error)
}

/// Per-effect dispatch boundary owned by the effect handler itself.
pub(crate) trait EffectHandler {
    fn handle<S: EffectServices + ?Sized>(
        &self,
        effect: Effect,
        services: &S,
        pending_effects: &Arc<AtomicUsize>,
    ) -> Vec<DispatchFailure>;
}

struct RuntimeEffectHandler;

/// Execute command effects through the per-effect handler boundary.
pub(crate) fn execute_effects<S: EffectServices + ?Sized>(
    effects: Vec<Effect>,
    services: &S,
    pending_effects: &Arc<AtomicUsize>,
) -> Vec<DispatchFailure> {
    let handler = RuntimeEffectHandler;
    effects
        .into_iter()
        .flat_map(|effect| handler.handle(effect, services, pending_effects))
        .collect()
}

/// Dispatch one effect against the shared services.
///
/// Each directory spawns exactly one named background task whose completion
/// publishes [`AppEvent::ScanCompleted`] on the bridge bus, each
/// metadata batch decodes inside `spawn_blocking` so the async workers are
/// never stalled by tag decoding, and playback requests are forwarded to
/// the dedicated audio worker through its non blocking command channel,
/// while task failures surface as notifications through the standard
/// spawn_background path.
///
/// `pending_effects` is the shared counter incremented per effect before the
/// task is spawned and decremented by an [`EffectGuard`] that lives for the
/// whole lifetime of the spawned task. It is read by the renderer to show
/// a global "Loading" spinner in the status bar while any task is in
/// flight.
impl EffectHandler for RuntimeEffectHandler {
    fn handle<S: EffectServices + ?Sized>(
        &self,
        effect: Effect,
        services: &S,
        pending_effects: &Arc<AtomicUsize>,
    ) -> Vec<DispatchFailure> {
        let mut failures = Vec::new();
        match effect {
            Effect::LoadBrowserDirectory {
                request_id,
                dir,
                show_hidden,
                restore_cursor_name,
            } => {
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "browser-directory",
                    OperationKind::Browser,
                    move |operation| async move {
                        let (dir, entries) = tokio::task::spawn_blocking(move || {
                            crate::filesystem::read_sorted_entries(&dir, show_hidden)
                                .map(|entries| (dir, entries))
                        })
                        .await
                        .context("browser directory worker crashed")??;
                        operation
                            .publish(
                                &sender,
                                AppEvent::BrowserDirectoryLoaded {
                                    operation_id: operation.id(),
                                    request_id,
                                    dir,
                                    restore_cursor_name,
                                    entries,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "browser-directory",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::ValidateBrowserDirectory {
                request_id,
                path,
                validation,
            } => {
                let sender = services.event_sender();
                let event_path = path.clone();
                let event_validation = validation.clone();
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "browser-directory-validation",
                    OperationKind::Browser,
                    move |operation| async move {
                        let validation_result =
                            match tokio::task::spawn_blocking(move || match &validation {
                                BrowserDirectoryValidation::Settings { input, .. } => {
                                    if path.is_dir() {
                                        Ok(path.clone())
                                    } else {
                                        Err(WorkerError::message(
                                            "browser-directory-validation",
                                            format!("Invalid path: {input}"),
                                        ))
                                    }
                                }
                                BrowserDirectoryValidation::Symlink { .. } => {
                                    match std::fs::canonicalize(&path) {
                                        Ok(resolved) if resolved.is_dir() => Ok(resolved),
                                        Ok(_) => Err(WorkerError::message(
                                            "browser-directory-validation",
                                            "symbolic link does not point at a directory",
                                        )),
                                        Err(error) => Err(WorkerError::new(
                                            "browser-directory-validation",
                                            crate::error::HarmoniumError::io(&path, error),
                                        )),
                                    }
                                }
                            })
                            .await
                            {
                                Ok(result) => result,
                                Err(error) => {
                                    Err(worker_panic_error("browser-directory-validation", error))
                                }
                            };
                        operation
                            .publish(
                                &sender,
                                AppEvent::BrowserDirectoryValidated {
                                    operation_id: operation.id(),
                                    request_id,
                                    path: event_path,
                                    validation: event_validation,
                                    result: validation_result,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "browser-directory-validation",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::BrowserValidation { request_id },
                );
            }
            Effect::ScanDirectory(dir) => {
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_background_operation(
                    services,
                    pending_effects,
                    "dir-scan",
                    EffectErrorKind::Task,
                    move |operation| async move {
                        let tracks = scan_directory(&dir).await?;
                        publish_scan_result(&operation, &sender, dir, tracks).await
                    },
                );
                report_dispatch_error(
                    "dir-scan",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::LoadMetadata(paths) => {
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_background_operation(
                    services,
                    pending_effects,
                    "track-metadata",
                    EffectErrorKind::Task,
                    move |operation| async move {
                        // Decoding is CPU bound, so it must not occupy an
                        // async worker thread while it parses
                        let batch = tokio::task::spawn_blocking(move || collect_metadata(paths))
                            .await
                            .context("metadata worker crashed")?;
                        publish_metadata_result(&operation, &sender, batch.loaded, batch.failed)
                            .await
                    },
                );
                report_dispatch_error(
                    "track-metadata",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::Search {
                request_id,
                scope,
                root,
                query,
                tracks,
            } => {
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "contextual-search",
                    OperationKind::Search,
                    move |operation| async move {
                        if operation.token().is_cancelled() {
                            return Ok(());
                        }
                        let (results, message) = match scope {
                            SearchScope::Browser => match scan_directory(&root).await {
                                Ok(paths) if !operation.token().is_cancelled() => {
                                    (search_browser_paths(&query, &root, paths), None)
                                }
                                Ok(_) => return Ok(()),
                                Err(error) => (
                                    Vec::new(),
                                    Some(WorkerError::new("contextual-search", error)),
                                ),
                            },
                            SearchScope::Playlist => {
                                if operation.token().is_cancelled() {
                                    return Ok(());
                                }
                                (search_playlist_tracks(&query, &tracks), None)
                            }
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::SearchCompleted {
                                    request_id,
                                    operation_id: operation.id(),
                                    scope,
                                    results,
                                    message,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "contextual-search",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::Search { request_id },
                );
            }
            Effect::EnumerateOutputs {
                request_id,
                provider,
            } => {
                let provider = provider.provider();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "output-enumeration",
                    OperationKind::OutputEnumeration,
                    move |operation| async move {
                        let outputs = tokio::task::spawn_blocking(move || provider.list_outputs())
                            .await
                            .context("output enumeration worker crashed")?;
                        if operation.token().is_cancelled() {
                            return Ok(());
                        }
                        operation
                            .publish(
                                &sender,
                                AppEvent::OutputsEnumerated {
                                    operation_id: operation.id(),
                                    request_id,
                                    outputs,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "output-enumeration",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::OutputEnumeration { request_id },
                );
            }
            Effect::UpdateExtinfTitle { path, new_title } => {
                let store = match services.playlist_store() {
                    Some(store) => store.clone(),
                    None => {
                        tracing::warn!("playlist store unavailable, skipping EXTINF title update");
                        return failures;
                    }
                };
                let dispatch_result = dispatch_with_guard(
                    services,
                    pending_effects,
                    "extinf-title-update",
                    EffectErrorKind::Task,
                    async move {
                        let (touched, path, new_title) = tokio::task::spawn_blocking(move || {
                            store
                                .update_extinf_title(&path, &new_title)
                                .map(|outcome| (outcome.touched(), path, new_title))
                        })
                        .await
                        .context("extinf update worker crashed")??;
                        tracing::info!(
                            touched,
                            path = ?path,
                            new_title,
                            "extinf rewrite finished"
                        );
                        Ok(())
                    },
                );
                report_dispatch_error(
                    "extinf-title-update",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::UpdateStreamExtinf { url, new_title } => {
                // Stream rename: rewrite the EXTINF label that precedes the
                // given URL across every saved playlist, mirroring the
                // local-file path but keyed on the URL string.
                let store = match services.playlist_store() {
                    Some(store) => store.clone(),
                    None => {
                        tracing::warn!(
                            "playlist store unavailable, skipping stream EXTINF title update"
                        );
                        return failures;
                    }
                };
                let dispatch_result = dispatch_with_guard(
                    services,
                    pending_effects,
                    "stream-extinf-title-update",
                    EffectErrorKind::Task,
                    async move {
                        let (touched, url, new_title) = tokio::task::spawn_blocking(move || {
                            store
                                .update_stream_extinf_title(&url, &new_title)
                                .map(|outcome| (outcome.touched(), url, new_title))
                        })
                        .await
                        .context("stream extinf update worker crashed")??;
                        tracing::info!(
                            touched,
                            url = %crate::net::safe_url(&url),
                            new_title,
                            "stream EXTINF rewrite finished"
                        );
                        Ok(())
                    },
                );
                report_dispatch_error(
                    "stream-extinf-title-update",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::ResolveStream {
                request_id,
                url,
                cancellation,
            } => {
                // Off-thread URL resolution: a slow Radio Browser lookup
                // must not freeze Ratatui. The worker builds the Track
                // (with metadata) and posts the result back to the UI.
                let resolver = services.stream_resolver().clone();
                let sender = services.event_sender();
                let cancellation_for_worker = cancellation.clone();
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "stream-resolve",
                    OperationKind::ResolveStream,
                    move |operation| async move {
                        let url_for_log = url.clone();
                        let operation_token = operation.token();
                        let result = tokio::task::spawn_blocking(move || {
                            if cancellation_for_worker.is_cancelled()
                                || operation_token.is_cancelled()
                            {
                                return None;
                            }
                            let resolved =
                                resolver.resolve_with_cancellation(&url, &cancellation_for_worker);
                            if cancellation_for_worker.is_cancelled()
                                || operation_token.is_cancelled()
                            {
                                None
                            } else {
                                Some(resolved)
                            }
                        })
                        .await;
                        if cancellation.is_cancelled() {
                            let _ = operation.cancel();
                            return Ok(());
                        }
                        if operation.token().is_cancelled() {
                            return Ok(());
                        }
                        let event = match result {
                            Ok(Some(Ok(resolved))) => {
                                let kind = resolved
                                    .station
                                    .clone()
                                    .or_else(|| resolved.title.clone())
                                    .unwrap_or_else(|| {
                                        url_for_log.host_str().unwrap_or("stream").to_string()
                                    });
                                let track =
                                    crate::track::Track::from_stream(url_for_log.clone(), {
                                        crate::stream::classify(&url_for_log)
                                            .unwrap_or(crate::stream::StreamKind::Http)
                                    });
                                let mut track = track;
                                track.attach_prepared_stream(resolved.prepared.clone());
                                track.set_title(kind);
                                // Layer the rest of the metadata as a snapshot
                                // so the playlist row shows the station name,
                                // codec, bitrate, etc. when available.
                                let mut metadata = crate::metadata::TrackMetadata {
                                    title: track.display_name().into_owned(),
                                    title_tagged: true,
                                    ..Default::default()
                                };
                                if let Some(codec) = resolved.codec.clone() {
                                    metadata.codec = codec.clone();
                                    metadata.format = codec;
                                }
                                if let Some(bitrate) = resolved.bitrate {
                                    metadata.bitrate = Some(bitrate);
                                }
                                metadata.duration = resolved.duration.unwrap_or_default();
                                if let Some(artist) = resolved.station {
                                    metadata.artist = artist;
                                }
                                if let Some(genre) = resolved.genre {
                                    metadata.genre = Some(genre);
                                }
                                if let Some(album) = resolved.title.clone() {
                                    metadata.album = album;
                                }
                                track.set_metadata(metadata);
                                AppEvent::StreamResolved {
                                    request_id,
                                    operation_id: operation.id(),
                                    url: url_for_log,
                                    track: Some(Box::new(track)),
                                    message: None,
                                }
                            }
                            Ok(Some(Err(error))) => AppEvent::StreamResolved {
                                request_id,
                                operation_id: operation.id(),
                                url: url_for_log,
                                track: None,
                                message: Some(WorkerError::new("stream-resolve", error)),
                            },
                            Ok(None) => return Ok(()),
                            Err(join) => AppEvent::StreamResolved {
                                request_id,
                                operation_id: operation.id(),
                                url: url_for_log,
                                track: None,
                                message: Some(WorkerError::message(
                                    "stream-resolve",
                                    format!("resolver crashed: {join}"),
                                )),
                            },
                        };
                        if let Err(error) = operation.publish(&sender, event).await {
                            tracing::error!(?error, "could not publish resolver failure event");
                        }
                        Ok(())
                    },
                );
                report_dispatch_error(
                    "stream-resolve",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::StreamResolution { request_id },
                );
            }
            Effect::Audio(command) => {
                // The audio worker is its own thread; sending the command is
                // effectively instantaneous from the dispatcher's perspective
                // and never deserves a global "Loading" indicator, so we
                // skip the in-flight counter for this effect.
                if services.audio_sink().send_audio(command).is_err() {
                    // The worker thread only disappears after a panic or
                    // process teardown, so this path is already terminal
                    tracing::error!("audio worker command channel is closed");
                    if let Err(error) =
                        services
                            .event_sender()
                            .send_critical(AppEvent::Notification {
                                kind: EffectErrorKind::Audio,
                                operation_id: None,
                                message: "Audio worker unavailable".to_string(),
                            })
                    {
                        tracing::error!(
                            ?error,
                            "could not publish audio worker failure notification"
                        );
                    }
                }
            }
            Effect::LoadArtwork {
                track_index,
                path,
                metadata,
                source_config,
                cache_dir,
            } => {
                // A missing loader still completes the request with no art so
                // a previous track's cover cannot survive the attempt boundary.
                let loader = services.artwork_loader().cloned();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "artwork-load",
                    OperationKind::Artwork,
                    move |operation| async move {
                        // Probing and decoding are CPU and IO bound, so the
                        // source pipeline runs inside a blocking worker and
                        // the async runtime stays free
                        let artwork = match loader {
                            Some(loader) => tokio::task::spawn_blocking(move || {
                                loader.load(
                                    path.as_deref(),
                                    source_config,
                                    metadata.as_ref(),
                                    &cache_dir,
                                )
                            })
                            .await
                            .context("artwork worker crashed")?,
                            None => None,
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::ArtworkLoaded {
                                    track_index,
                                    operation_id: operation.id(),
                                    artwork: artwork
                                        .map(ArtworkProtocol::from_loaded)
                                        .map(Box::new),
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "artwork-load",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::LoadLyrics {
                track_index,
                request,
            } => {
                let service = services.lyrics_service().clone();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "lyrics-load",
                    OperationKind::Lyrics,
                    move |operation| async move {
                        // The chain touches the filesystem, decodes tags and may
                        // walk the network, so the whole resolution runs inside a
                        // blocking worker and the async runtime stays free
                        let cancellation = operation.token();
                        let outcome = tokio::task::spawn_blocking(move || {
                            service.load_with_cancellation(&request, || cancellation.is_cancelled())
                        })
                        .await
                        .context("lyrics worker crashed")?;
                        let Some(outcome) = outcome else {
                            return Ok(());
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::LyricsLoaded {
                                    track_index,
                                    operation_id: operation.id(),
                                    outcome,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "lyrics-load",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::SaveActivePlaylist { name, contents } => {
                // The store clone shares the write lock, so every save across
                // the session stays serialized. The rendered text is owned by
                // the effect, so the blocking write cannot observe later edits.
                let Some(store) = services.playlist_store() else {
                    return failures;
                };
                let dispatch_result = dispatch_with_guard(
                    services,
                    pending_effects,
                    "playlist-save",
                    EffectErrorKind::Playlist,
                    async move {
                        let name = crate::playlist::PlaylistName::try_from(name.as_str())?;
                        tokio::task::spawn_blocking(move || store.save_rendered(&name, &contents))
                            .await
                            .context("playlist save worker crashed")??;
                        Ok(())
                    },
                );
                report_dispatch_error(
                    "playlist-save",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::ListPlaylistNames {
                request_id,
                request,
            } => {
                let store = services.playlist_store();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_background_operation(
                    services,
                    pending_effects,
                    "playlist-list",
                    EffectErrorKind::Playlist,
                    move |operation| async move {
                        let result = match store {
                            Some(store) => {
                                match tokio::task::spawn_blocking(move || store.list_names()).await
                                {
                                    Ok(Ok(names)) => Ok(names),
                                    Ok(Err(error)) => Err(WorkerError::new("playlist-list", error)),
                                    Err(error) => Err(WorkerError::new("playlist-list", error)),
                                }
                            }
                            None => Err(WorkerError::message(
                                "playlist-list",
                                "playlist store unavailable",
                            )),
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::PlaylistNamesCompleted {
                                    operation_id: operation.id(),
                                    request_id,
                                    request,
                                    result,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "playlist-list",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::Playlist { request_id },
                );
            }
            Effect::SavePlaylistNamed {
                request_id,
                name,
                contents,
                action,
            } => {
                let store = services.playlist_store();
                let event_name = name.clone();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_background_operation(
                    services,
                    pending_effects,
                    "playlist-save-named",
                    EffectErrorKind::Playlist,
                    move |operation| async move {
                        let result = match store {
                            Some(store) => {
                                match crate::playlist::PlaylistName::try_from(name.as_str()) {
                                    Ok(name) => match tokio::task::spawn_blocking(move || {
                                        store.save_rendered(&name, &contents)
                                    })
                                    .await
                                    {
                                        Ok(Ok(path)) => Ok(path),
                                        Ok(Err(error)) => {
                                            Err(WorkerError::new("playlist-save-named", error))
                                        }
                                        Err(error) => {
                                            Err(WorkerError::new("playlist-save-named", error))
                                        }
                                    },
                                    Err(error) => {
                                        Err(WorkerError::new("playlist-save-named", error))
                                    }
                                }
                            }
                            None => Err(WorkerError::message(
                                "playlist-save-named",
                                "playlist store unavailable",
                            )),
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::PlaylistSaved {
                                    operation_id: operation.id(),
                                    request_id,
                                    name: event_name,
                                    action,
                                    result,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "playlist-save-named",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::Playlist { request_id },
                );
            }
            Effect::RenamePlaylistNamed {
                request_id,
                old_name,
                new_name,
                action,
            } => {
                let store = services.playlist_store();
                let event_old_name = old_name.clone();
                let event_new_name = new_name.clone();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_background_operation(
                    services,
                    pending_effects,
                    "playlist-rename-named",
                    EffectErrorKind::Playlist,
                    move |operation| async move {
                        let result = match store {
                            Some(store) => {
                                let names = (
                                    crate::playlist::PlaylistName::try_from(old_name.as_str()),
                                    crate::playlist::PlaylistName::try_from(new_name.as_str()),
                                );
                                match names {
                                    (Ok(old_name), Ok(new_name)) => {
                                        match tokio::task::spawn_blocking(move || {
                                            let names = match store.list_names() {
                                                Ok(names) => names,
                                                Err(error) => {
                                                    return PlaylistRenameResult::Failed(
                                                        WorkerError::new(
                                                            "playlist-rename-named",
                                                            error,
                                                        ),
                                                    );
                                                }
                                            };
                                            let conflict = names.iter().any(|candidate| {
                                                candidate == new_name.as_str()
                                                    && old_name.as_str() != candidate
                                            });
                                            if conflict {
                                                PlaylistRenameResult::Conflict
                                            } else {
                                                store
                                                    .rename_playlist(&old_name, &new_name)
                                                    .map(|_| PlaylistRenameResult::Success)
                                                    .unwrap_or_else(|error| {
                                                        PlaylistRenameResult::Failed(
                                                            WorkerError::new(
                                                                "playlist-rename-named",
                                                                error,
                                                            ),
                                                        )
                                                    })
                                            }
                                        })
                                        .await
                                        {
                                            Ok(result) => result,
                                            Err(error) => PlaylistRenameResult::Failed(
                                                WorkerError::new("playlist-rename-named", error),
                                            ),
                                        }
                                    }
                                    (Err(error), _) | (_, Err(error)) => {
                                        PlaylistRenameResult::Failed(WorkerError::new(
                                            "playlist-rename-named",
                                            error,
                                        ))
                                    }
                                }
                            }
                            None => PlaylistRenameResult::Failed(WorkerError::message(
                                "playlist-rename-named",
                                "playlist store unavailable",
                            )),
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::PlaylistRenamed {
                                    operation_id: operation.id(),
                                    request_id,
                                    old_name: event_old_name,
                                    new_name: event_new_name,
                                    action,
                                    result,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "playlist-rename-named",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::Playlist { request_id },
                );
            }
            Effect::DeletePlaylistNamed {
                request_id,
                name,
                cursor,
                was_active,
            } => {
                let store = services.playlist_store();
                let event_name = name.clone();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_background_operation(
                    services,
                    pending_effects,
                    "playlist-delete-named",
                    EffectErrorKind::Playlist,
                    move |operation| async move {
                        let result = match store {
                            Some(store) => {
                                match crate::playlist::PlaylistName::try_from(name.as_str()) {
                                    Ok(name) => {
                                        let deletion = match tokio::task::spawn_blocking({
                                            let store = store.clone();
                                            move || store.delete(&name)
                                        })
                                        .await
                                        {
                                            Ok(Ok(())) => Ok(()),
                                            Ok(Err(error)) => Err(WorkerError::new(
                                                "playlist-delete-named",
                                                error,
                                            )),
                                            Err(error) => Err(WorkerError::new(
                                                "playlist-delete-named",
                                                error,
                                            )),
                                        };
                                        let names = match tokio::task::spawn_blocking(move || {
                                            store.list_names()
                                        })
                                        .await
                                        {
                                            Ok(Ok(names)) => Ok(names),
                                            Ok(Err(error)) => Err(WorkerError::new(
                                                "playlist-delete-named",
                                                error,
                                            )),
                                            Err(error) => Err(WorkerError::new(
                                                "playlist-delete-named",
                                                error,
                                            )),
                                        };
                                        PlaylistDeleteResult { deletion, names }
                                    }
                                    Err(error) => PlaylistDeleteResult {
                                        deletion: Err(WorkerError::new(
                                            "playlist-delete-named",
                                            error,
                                        )),
                                        names: Ok(Vec::new()),
                                    },
                                }
                            }
                            None => PlaylistDeleteResult {
                                deletion: Err(WorkerError::message(
                                    "playlist-delete-named",
                                    "playlist store unavailable",
                                )),
                                names: Ok(Vec::new()),
                            },
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::PlaylistDeleted {
                                    operation_id: operation.id(),
                                    request_id,
                                    name: event_name,
                                    cursor,
                                    was_active,
                                    result,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "playlist-delete-named",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::Playlist { request_id },
                );
            }
            Effect::LoadPlaylistNamed { request_id, name } => {
                let store = services.playlist_store();
                let event_name = name.clone();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "playlist-load-named",
                    OperationKind::Playlist,
                    move |operation| async move {
                        let result = match store {
                            Some(store) => {
                                match crate::playlist::PlaylistName::try_from(name.as_str()) {
                                    Ok(name) => {
                                        match tokio::task::spawn_blocking(move || store.load(&name))
                                            .await
                                        {
                                            Ok(Ok(playlist)) => Ok(playlist),
                                            Ok(Err(error)) => {
                                                Err(WorkerError::new("playlist-load-named", error))
                                            }
                                            Err(error) => {
                                                Err(WorkerError::new("playlist-load-named", error))
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        Err(WorkerError::new("playlist-load-named", error))
                                    }
                                }
                            }
                            None => Err(WorkerError::message(
                                "playlist-load-named",
                                "playlist store unavailable",
                            )),
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::PlaylistLoaded {
                                    operation_id: operation.id(),
                                    request_id,
                                    name: event_name,
                                    result,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "playlist-load-named",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::Playlist { request_id },
                );
            }
            Effect::SaveConfig {
                request_id,
                config,
                config_dir,
            } => {
                let writer = services.config_writer();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_background_operation(
                    services,
                    pending_effects,
                    "config-save",
                    EffectErrorKind::Task,
                    move |operation| async move {
                        let result = match tokio::task::spawn_blocking(move || {
                            writer.save(request_id, &config, &config_dir)
                        })
                        .await
                        {
                            Ok(result) => {
                                result.map_err(|error| WorkerError::new("config-save", error))
                            }
                            Err(error) => Err(WorkerError::new("config-save", error)),
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::ConfigSaved {
                                    operation_id: operation.id(),
                                    request_id,
                                    result,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "config-save",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::SaveRuntimeState {
                request_id,
                state,
                data_dir,
            } => {
                let writer = services.runtime_state_writer();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_background_operation(
                    services,
                    pending_effects,
                    "runtime-state-save",
                    EffectErrorKind::Task,
                    move |operation| async move {
                        let result = match tokio::task::spawn_blocking(move || {
                            writer.save(request_id, &state, &data_dir)
                        })
                        .await
                        {
                            Ok(result) => result
                                .map_err(|error| WorkerError::new("runtime-state-save", error)),
                            Err(error) => Err(worker_panic_error("runtime-state-save", error)),
                        };
                        if let Err(error) = &result {
                            tracing::warn!("{error}");
                        }
                        operation
                            .publish(
                                &sender,
                                AppEvent::RuntimeStateSaved {
                                    operation_id: operation.id(),
                                    request_id,
                                    result,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "runtime-state-save",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::LoadTheme {
                request_id,
                visit_id,
                themes_dir,
                name,
                include_names,
                purpose,
            } => {
                let sender = services.event_sender();
                let event_name = name.clone();
                let event_themes_dir = themes_dir.clone();
                let repository = services.theme_repository();
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "theme-load",
                    OperationKind::ThemeLoad,
                    move |operation| async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let theme_names = include_names
                                .then(|| crate::ui::theme::list_theme_names(&themes_dir));
                            let colors = repository.load(&themes_dir, &name);
                            (theme_names, colors)
                        })
                        .await;
                        let (theme_names, result) = match result {
                            Ok((theme_names, colors)) => (
                                theme_names,
                                colors.map_err(|error| WorkerError::new("theme-load", error)),
                            ),
                            Err(error) => (None, Err(WorkerError::new("theme-load", error))),
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::ThemeLoaded {
                                    operation_id: operation.id(),
                                    request_id,
                                    visit_id,
                                    themes_dir: event_themes_dir,
                                    name: event_name,
                                    theme_names,
                                    result,
                                    purpose,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "theme-load",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::ThemeLoad {
                        request_id,
                        visit_id,
                    },
                );
            }
            Effect::SaveTheme {
                request_id,
                visit_id,
                themes_dir,
                name,
                colors,
            } => {
                let sender = services.event_sender();
                let event_name = name.clone();
                let event_colors = colors.clone();
                let event_themes_dir = themes_dir.clone();
                let repository = services.theme_repository();
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "theme-save",
                    OperationKind::ThemeSave,
                    move |operation| async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let path = themes_dir.join(format!("{name}.toml"));
                            repository
                                .save(&themes_dir, &name, &colors)
                                .map_err(|error| {
                                    WorkerError::new(
                                        "theme-save",
                                        crate::error::HarmoniumError::io(path, error),
                                    )
                                })
                        })
                        .await;
                        let result = match result {
                            Ok(result) => result,
                            Err(error) => Err(WorkerError::new("theme-save", error)),
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::ThemeSaved {
                                    operation_id: operation.id(),
                                    request_id,
                                    visit_id,
                                    themes_dir: event_themes_dir,
                                    name: event_name,
                                    colors: event_colors,
                                    result,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "theme-save",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::ThemeSave {
                        request_id,
                        visit_id,
                    },
                );
            }
            Effect::SetLyricsRemote(enabled) => {
                // The flag is atomic and the service is shared, so this is
                // immediate and needs no background step, so we skip the
                // in-flight counter for this effect.
                services.set_lyrics_remote_enabled(enabled);
            }
            Effect::RenameFileOnDisk {
                request_id,
                from,
                new_name,
                browser_dir,
            } => {
                let Some(store) = services.playlist_store() else {
                    return failures;
                };
                let sender = services.event_sender();
                let event_from = from.clone();
                let fallback_to = from
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(&new_name);
                let dispatch_result = dispatch_with_operation(
                    services,
                    pending_effects,
                    "file-rename",
                    OperationKind::FileRename,
                    move |operation| async move {
                        let (event_to, result) = match tokio::task::spawn_blocking(move || {
                            FilesystemRenameService::default().rename_file_on_disk(
                                store.as_ref(),
                                &from,
                                &new_name,
                                &browser_dir,
                            )
                        })
                        .await
                        {
                            Ok(result) => result,
                            Err(error) => (
                                fallback_to,
                                RenameFileResult::Failed(worker_panic_error("file-rename", error)),
                            ),
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::RenameCompleted {
                                    operation_id: operation.id(),
                                    request_id,
                                    from: event_from,
                                    to: event_to,
                                    result,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "file-rename",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::FileRename { request_id },
                );
            }
            Effect::EditMetadataWrite { path, fields } => {
                let event_path = path.clone();
                // The blocking task consumes `fields` for the write, so the
                // completion event needs its own copy to propagate the new
                // Title into every saved playlist's EXTINF label.
                let fields_for_event = fields.clone();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_background_operation(
                    services,
                    pending_effects,
                    "metadata-write",
                    EffectErrorKind::Task,
                    move |operation| async move {
                        // Tag encoding is CPU bound, so it must not occupy an
                        // async worker thread while it parses
                        let result = match tokio::task::spawn_blocking(move || {
                            crate::metadata::writer::write_metadata(&path, &fields)
                        })
                        .await
                        {
                            Ok(result) => result.map_err(|error| {
                                let error = WorkerError::new("metadata-write", error);
                                tracing::warn!("{error}");
                                error
                            }),
                            Err(error) => Err(worker_panic_error("metadata-write", error)),
                        };
                        operation
                            .publish(
                                &sender,
                                AppEvent::MetadataWriteCompleted {
                                    operation_id: operation.id(),
                                    path: event_path,
                                    result,
                                    fields: fields_for_event,
                                },
                            )
                            .await
                    },
                );
                report_dispatch_error(
                    "metadata-write",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
            Effect::EditMetadataPrefill { path } => {
                let event_path = path.clone();
                let sender = services.event_sender();
                let dispatch_result = dispatch_with_background_operation(
                    services,
                    pending_effects,
                    "metadata-prefill",
                    EffectErrorKind::Task,
                    move |operation| async move {
                        // Tag decoding is CPU bound, so it must not occupy an
                        // async worker thread while it parses
                        let fields = tokio::task::spawn_blocking(move || {
                            crate::metadata::reader::editable_fields(&path)
                        })
                        .await
                        .context("metadata prefill worker crashed")?;
                        match fields {
                            Ok(fields) => operation.publish(
                                &sender,
                                AppEvent::MetadataPrefillReady {
                                    operation_id: operation.id(),
                                    path: event_path,
                                    fields,
                                },
                            ),
                            Err(error) => {
                                // The editor stays in its loading state; the
                                // notification explains why no values arrived
                                let error = WorkerError::new("metadata-prefill", error);
                                tracing::warn!("{error}");
                                operation.publish(
                                    &sender,
                                    AppEvent::Notification {
                                        kind: EffectErrorKind::Task,
                                        operation_id: Some(operation.id()),
                                        message: format!("Could not read tags: {error}"),
                                    },
                                )
                            }
                        }
                        .await
                    },
                );
                report_dispatch_error(
                    "metadata-prefill",
                    dispatch_result,
                    &mut failures,
                    DispatchCompensation::None,
                );
            }
        }
        failures
    }
}

/// Deliver a finished scan over the bus, reporting delivery failures.
///
/// Takes the operation context and cloned producer because background tasks
/// cannot borrow the bus owned by [`AppServices`]. Its awaitable send keeps
/// async contexts safe while retaining operation queue accounting.
async fn publish_scan_result(
    operation: &OperationContext,
    sender: &EventSender,
    requested_dir: PathBuf,
    tracks: Vec<PathBuf>,
) -> anyhow::Result<()> {
    operation
        .publish(
            sender,
            AppEvent::ScanCompleted {
                operation_id: operation.id(),
                requested_dir,
                tracks,
            },
        )
        .await
        .context("event bus receiver went away")
}

/// Deliver one extraction batch over the bridge bus.
async fn publish_metadata_result(
    operation: &OperationContext,
    sender: &EventSender,
    loaded: Vec<(PathBuf, TrackMetadata)>,
    failed: usize,
) -> anyhow::Result<()> {
    // Empty batches carry no information for the UI and are dropped here so
    // the status area stays quiet for no-op requests
    if loaded.is_empty() && failed == 0 {
        return Ok(());
    }

    operation
        .publish(
            sender,
            AppEvent::MetadataCompleted {
                operation_id: operation.id(),
                loaded,
                failed,
            },
        )
        .await
        .context("event bus receiver went away")
}
