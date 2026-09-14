//! Binary entrypoint wiring logging, terminal lifecycle and the event loop.

mod splash;

#[cfg(test)]
#[path = "test_support.rs"]
mod test_support;

use std::fs::{self, OpenOptions};
use std::io::{self, Stdout, stdout};
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui_image::picker::Picker;
use tracing_subscriber::EnvFilter;

use harmonium::APP_VERSION;
use harmonium::app::App;
use harmonium::artwork::{ArtworkBackend, ArtworkBackendKind, ArtworkTeardown};
use harmonium::config::{
    AlbumArtMode, AppConfig, Paths, PersistedState, erase_migrated_preferences,
    load_legacy_preferences, migrate, pending_legacy_preferences,
};
#[cfg(test)]
use harmonium::event::EventBus;
use harmonium::event::{AppEvent, EventSendError, EventSender};
use harmonium::playlist::{Playlist, PlaylistName, PlaylistStore};
#[cfg(test)]
use harmonium::runtime::OperationKind;
use harmonium::runtime::{AppServices, OperationId};
use harmonium::state::{Panel, Popup};
#[cfg(test)]
use harmonium::track::TrackLocation;
use harmonium::ui;
use harmonium::ui::theme::Theme;
use std::path::{Path, PathBuf};

/// Idle wait between polls balancing input latency against CPU usage.
const POLL_TIMEOUT: Duration = Duration::from_millis(250);
const LOG_FILE_NAME: &str = "harmonium.log";

/// Operations needed to enter and leave the terminal's interactive mode.
///
/// Keeping these operations behind a narrow port lets the lifecycle state be
/// tested without changing the production crossterm RAII guard.
trait TerminalControl {
    fn enable_raw_mode(&mut self) -> io::Result<()>;
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
    fn disable_raw_mode(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
}

struct CrosstermTerminalControl;

impl TerminalControl for CrosstermTerminalControl {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        execute!(stdout(), EnterAlternateScreen)
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        execute!(stdout(), LeaveAlternateScreen)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TerminalLifecycle {
    raw_mode: bool,
    alternate_screen: bool,
}

impl TerminalLifecycle {
    fn restore<C: TerminalControl>(&mut self, control: &mut C) {
        if self.raw_mode && control.disable_raw_mode().is_ok() {
            self.raw_mode = false;
        }
        if self.alternate_screen && control.leave_alternate_screen().is_ok() {
            self.alternate_screen = false;
        }
    }
}

struct TerminalLifecycleGuard<C: TerminalControl> {
    control: C,
    lifecycle: TerminalLifecycle,
}

impl<C: TerminalControl> TerminalLifecycleGuard<C> {
    fn new(mut control: C) -> io::Result<Self> {
        control.enable_raw_mode()?;
        let mut lifecycle = TerminalLifecycle {
            raw_mode: true,
            alternate_screen: false,
        };
        if let Err(error) = control.enter_alternate_screen() {
            lifecycle.restore(&mut control);
            return Err(error);
        }
        lifecycle.alternate_screen = true;
        Ok(Self { control, lifecycle })
    }

    fn restore(&mut self) {
        self.lifecycle.restore(&mut self.control);
    }
}

impl<C: TerminalControl> Drop for TerminalLifecycleGuard<C> {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Owns the terminal in raw mode and restores it deterministically on drop.
///
/// The guard is created in two phases: [`TerminalGuard::new`] only enables raw
/// mode and enters the alternate screen, leaving a window for the splash to
/// paint with crossterm. [`TerminalGuard::attach_ratatui`] then creates the
/// ratatui backend over that same alternate screen before the event loop runs.
/// Restoration is best effort and safe to run more than once, which makes the
/// guard resilient together with the panic hook.
struct TerminalGuard {
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
    lifecycle: TerminalLifecycleGuard<CrosstermTerminalControl>,
}

impl TerminalGuard {
    /// Switch the terminal to raw mode with an alternate screen buffer.
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            terminal: None,
            lifecycle: TerminalLifecycleGuard::new(CrosstermTerminalControl)?,
        })
    }

    /// Create the ratatui terminal over the already-active alternate screen.
    ///
    /// This is the second phase: the splash runs between [`TerminalGuard::new`]
    /// and this call, painting the same buffer with crossterm.
    fn attach_ratatui(&mut self) -> std::io::Result<()> {
        self.terminal = Some(Terminal::new(CrosstermBackend::new(stdout()))?);
        Ok(())
    }

    /// Mutable access for draw calls in the main loop.
    ///
    /// Only valid after [`TerminalGuard::attach_ratatui`]; violating that order
    /// is a programming error, not a user-data condition.
    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        self.terminal
            .as_mut()
            .expect("attach_ratatui before use (guaranteed by run())")
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best effort cleanup, safe to run more than once
        self.lifecycle.restore();
    }
}

/// Restore the terminal to a usable state, ignoring secondary failures.
fn restore_terminal() {
    let mut control = CrosstermTerminalControl;
    let mut lifecycle = TerminalLifecycle {
        raw_mode: true,
        alternate_screen: true,
    };
    lifecycle.restore(&mut control);
}

/// Wrap the default panic hook so a crash still yields a readable terminal.
fn install_panic_hook() -> ArtworkTeardown {
    let artwork_teardown = ArtworkTeardown::new();
    let cleanup = artwork_teardown.clone();
    install_panic_hook_with(restore_terminal, move || cleanup.cleanup());
    artwork_teardown
}

fn install_panic_hook_with<F, C>(restore: F, cleanup: C)
where
    F: Fn() + Send + Sync + 'static,
    C: Fn() + Send + Sync + 'static,
{
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        cleanup();
        restore();
        previous_hook(panic_info);
    }));
}

fn accepts_operation_notification(
    services: &AppServices,
    operation_id: Option<OperationId>,
) -> bool {
    operation_id.is_none_or(|operation_id| services.accepts_operation_completion(operation_id))
}

fn apply_config_saved(
    app: &mut App,
    services: &AppServices,
    operation_id: OperationId,
    result: harmonium::error::WorkerResult<bool>,
) -> bool {
    if !services.accepts_operation_completion(operation_id) {
        return false;
    }
    if let Err(error) = result {
        app.push_notification(format!("config-save: {error}"));
    }
    true
}

fn apply_runtime_theme(
    app: &mut App,
    theme: &mut Theme,
    colors: harmonium::ui::theme::ThemeColors,
) -> bool {
    match Theme::try_from_colors(colors) {
        Ok(parsed) => {
            *theme = parsed;
            true
        }
        Err(error) => {
            app.push_notification(format!("Could not apply theme colors: {error}"));
            false
        }
    }
}

fn load_startup_theme(app: &mut App, themes_dir: &std::path::Path, name: &str) -> Theme {
    match Theme::load(themes_dir, name) {
        Ok(theme) => theme,
        Err(error) => {
            app.push_notification(format!("Could not load theme: {error}"));
            Theme::default()
        }
    }
}

fn operation_id_for_event(event: &AppEvent) -> Option<OperationId> {
    match event {
        AppEvent::Notification { operation_id, .. } => *operation_id,
        AppEvent::BrowserDirectoryLoaded { operation_id, .. }
        | AppEvent::BrowserDirectoryValidated { operation_id, .. }
        | AppEvent::SearchCompleted { operation_id, .. }
        | AppEvent::OutputsEnumerated { operation_id, .. }
        | AppEvent::ArtworkLoaded { operation_id, .. }
        | AppEvent::LyricsLoaded { operation_id, .. }
        | AppEvent::StreamResolved { operation_id, .. }
        | AppEvent::ScanCompleted { operation_id, .. }
        | AppEvent::MetadataCompleted { operation_id, .. }
        | AppEvent::MetadataPrefillReady { operation_id, .. }
        | AppEvent::PlaylistNamesCompleted { operation_id, .. }
        | AppEvent::PlaylistSaved { operation_id, .. }
        | AppEvent::PlaylistRenamed { operation_id, .. }
        | AppEvent::PlaylistDeleted { operation_id, .. }
        | AppEvent::PlaylistLoaded { operation_id, .. }
        | AppEvent::ConfigSaved { operation_id, .. }
        | AppEvent::RuntimeStateSaved { operation_id, .. }
        | AppEvent::ThemeLoaded { operation_id, .. }
        | AppEvent::ThemeSaved { operation_id, .. }
        | AppEvent::RenameCompleted { operation_id, .. }
        | AppEvent::MetadataWriteCompleted { operation_id, .. } => Some(*operation_id),
        _ => None,
    }
}

#[cfg(test)]
trait TerminalInput {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

#[cfg(test)]
struct CrosstermInput;

#[cfg(test)]
impl TerminalInput for CrosstermInput {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        event::read()
    }
}

#[cfg(test)]
fn next_terminal_input<I: TerminalInput>(input: &mut I) -> Result<Option<AppEvent>> {
    if !input
        .poll(POLL_TIMEOUT)
        .context("polling terminal events failed")?
    {
        return Ok(None);
    }

    let native_event = input.read().context("reading terminal events failed")?;
    Ok(translate_event(native_event))
}

/// Initialize file logging, staying silent when the destination is unusable.
///
/// Logs must never reach the TUI so initialization problems are swallowed
/// instead of printed anywhere the user could see them.
fn init_file_logging(paths: Option<&Paths>, level: &str) {
    let Some(log_path) = paths.map(|paths| paths.cache_dir.join(LOG_FILE_NAME)) else {
        return;
    };

    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let Ok(log_file) = OpenOptions::new().create(true).append(true).open(&log_path) else {
        return;
    };

    // Libraries that log directly to stderr (notably ALSA/cpal device
    // enumeration) would otherwise print over the alternate screen and corrupt
    // the TUI, leaving stray characters and a black patch. Point stderr at the
    // same log file so everything the user should not see lands in the log.
    redirect_stderr_to(&log_file);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(Mutex::new(log_file))
        .try_init();

    tracing::info!("harmonium {APP_VERSION} starting");
}

/// Redirect the process stderr descriptor to `file`, so library diagnostics
/// never reach the terminal.
///
/// # Contract
///
/// - `dup2(oldfd, STDERR_FILENO)` atomically makes fd 2 point at the *same*
///   open file description as `oldfd`. Any process-wide stderr (all threads,
///   and any library that writes straight to descriptor 2) is therefore routed
///   into `file` for the rest of the process lifetime.
/// - This is a global, irreversible process change: there is no way back to
///   the original terminal stderr after this call short of reopening it. It is
///   deliberately done once, at startup, before the TUI attaches, so library
///   noise (ALSA/cpal device enumeration, panic backtraces) cannot smear across
///   the alternate screen.
/// - `file` is only used as a descriptor source (`AsRawFd`). Ownership stays
///   with the caller, who must keep it alive until it is moved into the tracing
///   writer right after; if that File were closed early the redirected fd 2
///   would keep referring to an invalidated description.
/// - `dup2` is async-signal-safe and does not raise a Rust error; it returns
///   `-1` and sets `errno` on failure (e.g. `EBADF`). This function ignores
///   that return on purpose: a failed redirection leaves the TUI running with
///   stderr untouched, which is strictly better than aborting startup, and the
///   call site logs nothing because there is no channel to report through yet.
fn redirect_stderr_to(file: &fs::File) {
    use std::os::fd::AsRawFd;
    // SAFETY: dup2 is async-signal-safe and only touches the process file
    // descriptors; the open `file` outlives the call because the caller keeps
    // it and moves it into the tracing writer afterwards.
    unsafe {
        libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
    }
}

/// Convert a native terminal event into an application event.
///
/// Unknown native events such as mouse or focus changes are intentionally
/// ignored until their features ship.
fn translate_event(native_event: Event) -> Option<AppEvent> {
    match native_event {
        Event::Key(key_event) => Some(AppEvent::Key(key_event)),
        Event::Resize(width, height) => Some(AppEvent::Resize(width, height)),
        _ => None,
    }
}

/// Owns the blocking terminal read so the UI loop can sleep on the same bus
/// that wakes it for worker completions. The stop flag bounds shutdown without
/// detaching a reader thread permanently.
struct TerminalReader {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

const TERMINAL_EVENT_TIMEOUT: Duration = Duration::from_millis(100);

fn publish_terminal_event(sender: &EventSender, event: AppEvent) -> Result<(), EventSendError> {
    match event {
        AppEvent::Key(_) => sender.send_critical_timeout(event, TERMINAL_EVENT_TIMEOUT),
        AppEvent::Resize(_, _) => sender.send(event),
        _ => unreachable!("terminal reader only emits key and resize"),
    }
}

impl TerminalReader {
    fn spawn(sender: EventSender) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("harmonium-terminal-reader".to_string())
            .spawn(move || {
                while !stop_thread.load(Ordering::Acquire) {
                    match event::poll(Duration::from_millis(50)) {
                        Ok(true) => match event::read() {
                            Ok(native_event) => {
                                let Some(app_event) = translate_event(native_event) else {
                                    continue;
                                };
                                if let Err(error) = publish_terminal_event(&sender, app_event) {
                                    tracing::warn!(
                                        ?error,
                                        "terminal event queue unavailable; stopping reader"
                                    );
                                    break;
                                }
                            }
                            Err(error) => {
                                let _ = sender.send(AppEvent::Notification {
                                    kind: harmonium::event::EffectErrorKind::Task,
                                    operation_id: None,
                                    message: format!("Terminal input failed: {error}"),
                                });
                                break;
                            }
                        },
                        Ok(false) => {}
                        Err(error) => {
                            let _ = sender.send(AppEvent::Notification {
                                kind: harmonium::event::EffectErrorKind::Task,
                                operation_id: None,
                                message: format!("Terminal input polling failed: {error}"),
                            });
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for TerminalReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Complete the legacy preference migration before asynchronous startup work.
///
/// The config replacement is the first commit point. Only after it succeeds do
/// we atomically remove the migrated keys from `state.toml`. A pre-replacement
/// failure leaves the legacy keys available for the next startup; a
/// post-replacement directory-sync error is classified from the target
/// postcondition. This remains recoverable, not an all-or-nothing transaction
/// across the two files.
fn migrate_legacy_preferences_at_startup(
    data_dir: &Path,
    config_dir: &Path,
    config: &mut AppConfig,
) -> bool {
    let Some(legacy) = load_legacy_preferences(data_dir) else {
        return false;
    };
    if legacy.is_empty() {
        return false;
    }

    let pending = pending_legacy_preferences(config_dir, &legacy);
    if pending.is_empty() {
        if let Err(error) = erase_migrated_preferences(data_dir) {
            tracing::warn!(
                "legacy preferences are already represented in config, but remain in state for cleanup: {error}"
            );
        }
        return false;
    }

    migrate(&pending, config);

    match config.save(config_dir) {
        Ok(()) => match erase_migrated_preferences(data_dir) {
            Ok(_) => {
                tracing::info!("migrated user preferences from state.toml to config.toml");
            }
            Err(error) => {
                tracing::warn!(
                    "migrated config was saved, but legacy preferences remain for retry: {error}"
                );
            }
        },
        Err(error) => {
            tracing::warn!(
                "could not save migrated config; legacy preferences remain for retry: {error}"
            );
        }
    }

    // The attempt was handled synchronously even on failure. Do not dispatch a
    // second startup save that could make config success precede state cleanup.
    true
}

/// Run the application until the user quits.
fn run() -> Result<()> {
    let artwork_teardown = install_panic_hook();

    // One resolution feeds both logging and configuration so every XDG
    // location comes from the same source of truth
    let paths = Paths::system().ok();

    // A missing or broken config degrades to defaults inside load, so
    // startup never fails on user configuration
    let mut config = paths
        .as_ref()
        .map(|paths| AppConfig::load(&paths.config_dir))
        .unwrap_or_default();

    init_file_logging(paths.as_ref(), &config.log.level);

    // Load persisted runtime state from the previous session, if any
    let persisted_state = paths
        .as_ref()
        .map(|paths| PersistedState::load(&paths.data_dir))
        .unwrap_or_default();

    // Migrate old preferences before async startup effects. The caller owns
    // the bounded config-save-then-state-cleanup transaction.
    let migrated_legacy_preferences = paths.as_ref().is_some_and(|paths| {
        migrate_legacy_preferences_at_startup(&paths.data_dir, &paths.config_dir, &mut config)
    });
    // Ensure the themes directory exists and ships bundled defaults
    if let Some(paths) = paths.as_ref() {
        harmonium::config::ensure_themes_dir(&paths.config_dir);
    }

    // The runtime is built before touching the terminal so a construction
    // failure aborts with a plain error instead of leaving raw mode behind.
    let mut services = AppServices::new().context("building the async runtime failed")?;
    tracing::info!("shared async runtime started");

    // The lyrics chain starts respecting the persisted preference, so a user
    // that turned Remote lyrics off never triggers a lookup before Settings
    // is opened. The settings apply path keeps it in sync at runtime.
    services.set_lyrics_remote_enabled(config.playback.remote_lyrics);

    // Build the playlist store once and share its write lock with the async
    // autosave path. A missing playlists directory degrades to an inert store.
    let playlist_store = match paths.as_ref() {
        Some(paths) => PlaylistStore::for_dir(paths.playlists_dir()),
        None => PlaylistStore::for_dir(PathBuf::new()),
    };
    services.set_playlist_store(Some(playlist_store.clone()));

    // Scoped block so the terminal guard restores the console on drop before
    // the runtime shuts down, keeping the teardown order deterministic
    let outcome = {
        let mut terminal = TerminalGuard::new().context("entering terminal mode failed")?;
        let mut app = App::from_config_and_store(config.keys.clone(), playlist_store);
        if let Some(p) = paths.as_ref() {
            app.set_config_paths(config.clone(), p.config_dir.clone(), p.data_dir.clone());
        }
        // Native PipeWire provider enumerates the sinks for startup replay and
        // the Sound tab; it degrades to "Default" only if unavailable.
        let output_provider = std::sync::Arc::new(harmonium::audio::PipeWireOutputProvider);
        let startup_output = harmonium::audio::select_startup_output(
            output_provider.as_ref(),
            &config.sound.output_sink_id,
        );
        if startup_output.used_session_default {
            tracing::warn!(
                saved_output_id = %config.sound.output_sink_id,
                "persisted audio output is unavailable; using the session default"
            );
        }
        app.set_output_provider(output_provider);
        // Replay the stable preference through the worker-owned routing path.
        // The saved id remains in config.toml when discovery falls back.
        if let Err(error) = services
            .audio()
            .send(harmonium::audio::AudioCommand::SetOutput(
                harmonium::audio::OutputTarget::from(&startup_output.output),
            ))
        {
            tracing::warn!("could not replay persisted audio output: {error}");
        }

        // Apply user preferences (now under [general] in config.toml)
        app.apply_startup_preferences(&config, &persisted_state);
        // The audio worker starts at DEFAULT_VOLUME_PERCENT and only learns a
        // changed value through a SetVolume command, so a persisted volume
        // must be pushed on launch or it silently reverts to the default until
        // the user presses +/-, which then snaps the output to the right level.
        if let Err(error) = services
            .audio()
            .send(harmonium::audio::AudioCommand::SetVolume(
                app.playback().volume_percent,
            ))
        {
            tracing::warn!("could not apply persisted volume: {error}");
        }
        // The audio worker defaults gain to 0 dB, so a persisted value must be
        // pushed on launch or it silently reverts. Same rationale as volume.
        if let Err(error) = services
            .audio()
            .send(harmonium::audio::AudioCommand::SetGain(
                config.playback.gain_db,
            ))
        {
            tracing::warn!("could not apply persisted gain: {error}");
        }
        if let Err(error) = services
            .audio()
            .send(harmonium::audio::AudioCommand::SetCrossfade(
                config.playback.crossfade_seconds,
            ))
        {
            tracing::warn!("could not apply persisted crossfade: {error}");
        }
        // Preserve the existing startup config save for legacy sort-track
        // migration and other config normalization. Legacy preference
        // migration already completed its ordered synchronous save above.
        if paths.is_some() && !migrated_legacy_preferences {
            let effect = app.save_config_effect();
            app.execute_effects(vec![effect], &services);
        }

        // Restore the previously active playlist, degrading to an empty queue
        // when the saved name is gone or its file no longer loads
        restore_last_playlist(&mut app, &persisted_state);

        // Guarantee the manager always has something to show on a fresh install
        ensure_default_playlist(&mut app);

        // A restored playlist stores resource locations: reload the tags of the
        // queue before the loop starts, or metadata-driven display and
        // sorting would silently fall back to file names until the user
        // re-adds tracks. The worker runs outside the event loop.
        app.execute_effects(app.load_metadata_for_queue(), &services);

        // Wire artwork source configuration and cache directory
        app.configure_artwork(
            config.ui.artwork_source,
            paths
                .as_ref()
                .map(|paths| paths.cache_dir.clone())
                .unwrap_or_default(),
        );

        // The picker query needs the alternate screen active and must run
        // before the loop starts reading terminal events
        let mut artwork_backend = build_artwork_backend(&config, artwork_teardown);
        app.initialize_artwork_backend(&artwork_backend);
        services.set_artwork_loader(artwork_backend.loader().cloned());

        // Restore the configured browser directory, falling back to the
        // default start directory when the saved path is missing or
        // invalid.
        if !config.general.browser_directory.is_empty() {
            let saved_dir = std::path::PathBuf::from(&config.general.browser_directory);
            if saved_dir.is_dir() {
                app.set_browser_start_dir(saved_dir.clone());
                tracing::info!(
                    start_dir = %saved_dir.display(),
                    "browser restoring persisted directory"
                );
                let effects = vec![app.request_browser_dir(saved_dir)];
                app.execute_effects(effects, &services);
            } else {
                let effects = app.open_start_dir();
                app.execute_effects(effects, &services);
            }
        } else {
            let effects = app.open_start_dir();
            app.execute_effects(effects, &services);
        }

        // Load theme from the themes directory
        let themes_dir = paths
            .as_ref()
            .map(|paths| paths.config_dir.join("themes"))
            .unwrap_or_default();
        let mut theme = if !themes_dir.as_os_str().is_empty() {
            load_startup_theme(&mut app, &themes_dir, &config.ui.theme)
        } else {
            Theme::default()
        };

        focus_startup_panel(&mut app);

        // Restore the playlist cursor independently from autoplay. When resume
        // is disabled, this returns no effects and leaves playback stopped.
        let effects = restore_last_track(
            &mut app,
            &persisted_state,
            config.general.resume_previous_track,
        );
        if !effects.is_empty() {
            // Fire effects before the loop starts so audio begins immediately.
            app.execute_effects(effects, &services);
        }

        // Run the startup splash on the shared alternate screen before Ratatui
        // attaches. It never blocks boot: failures degrade to a warning and the
        // app continues. Events it ignores (workers) stay in the bus because it
        // runs before the loop starts draining them.
        if splash::ENABLED {
            match splash::run(&theme, &services.events().sender()) {
                Ok(outcome) => tracing::info!(cancelled = outcome.cancelled, "splash finished"),
                Err(error) => tracing::warn!("splash failed, continuing: {error}"),
            }
        }

        // Ratatui renders onto the same alternate screen the splash used, so
        // the terminal is entered and left only once.
        terminal.attach_ratatui()?;

        // The backend owns the optional process. Spawn failure transitions to
        // the native half-block renderer without affecting boot.
        app.start_artwork_backend(&mut artwork_backend);
        let result = event_loop(
            &mut terminal,
            &mut app,
            &mut theme,
            &services,
            &mut artwork_backend,
        );

        // Persist state on graceful exit before terminal restore. Delegates to
        // the single persistence path on App so it writes the exact same set of
        // preferences the runtime saves after each settings edit.
        if app.should_quit() {
            let effects = app.persist_runtime_state();
            app.execute_effects(effects, &services);
        }

        result
    };

    services.shutdown();

    outcome
}

/// Build the artwork backend for the configured mode.
///
/// Artwork is optional by design: any detection failure degrades with a
/// warning instead of blocking startup. `image` mode accepts only real
/// graphics protocols, while `auto` keeps whatever the query found,
/// including the Unicode half-block fallback.
fn build_artwork_backend(config: &AppConfig, teardown: ArtworkTeardown) -> ArtworkBackend {
    if !config.ui.show_album_art {
        return ArtworkBackend::from_detected_picker_with_teardown(
            AlbumArtMode::Off,
            None,
            teardown,
        );
    }

    let mode = config.ui.album_art_mode;
    let picker = match mode {
        AlbumArtMode::Off | AlbumArtMode::Unicode => None,
        AlbumArtMode::Image | AlbumArtMode::Auto => match Picker::from_query_stdio() {
            Ok(picker) => Some(picker),
            Err(error) => {
                let fallback = if mode == AlbumArtMode::Image {
                    "artwork disabled"
                } else {
                    "trying ueberzugpp or half-blocks"
                };
                tracing::warn!("terminal capability query failed: {error}, {fallback}");
                None
            }
        },
    };
    let backend = ArtworkBackend::from_detected_picker_with_teardown(mode, picker, teardown);
    if mode == AlbumArtMode::Image && backend.kind() == ArtworkBackendKind::Disabled {
        tracing::warn!("no graphics protocol detected, artwork stays disabled");
    }
    backend
}

/// Restore the playlist the user left active in the previous session.
///
/// When the saved name is missing or the file no longer loads, the queue starts
/// empty and the failure is logged. Restore must never panic or block startup.
fn restore_last_playlist(app: &mut App, persisted: &PersistedState) {
    let Some(name) = persisted.last_playlist.clone() else {
        return;
    };
    let playlist_name = match PlaylistName::try_from(name.as_str()) {
        Ok(name) => name,
        Err(error) => {
            tracing::warn!("last playlist {name} unavailable: {error}, starting empty");
            return;
        }
    };
    match app.playlist_store().load(&playlist_name) {
        Ok(playlist) => {
            app.restore_playlist(playlist, Some(name.clone()));
            tracing::info!(name, "restored last playlist");
        }
        Err(error) => {
            tracing::warn!("last playlist {name} unavailable: {error}, starting empty");
        }
    }
}

/// Restore the saved track selection and optionally begin playback.
///
/// Matching uses the saved stable location rather than a queue index because a
/// restored playlist may have changed order. Missing resources are a safe
/// no-op.
fn restore_last_track(
    app: &mut App,
    persisted: &PersistedState,
    resume_previous_track: bool,
) -> Vec<harmonium::app::Effect> {
    let Some(saved_location) = persisted.track_location() else {
        return Vec::new();
    };
    let playlist_base = app.playlist_store().directory();
    let Some(index) = app.playlist().tracks().iter().position(|track| {
        track
            .track_location()
            .equivalent_with_base(&saved_location, playlist_base)
    }) else {
        return Vec::new();
    };

    let selected_location = app.playlist().tracks()[index].track_location();
    app.select_playlist_track(index);
    // Replace a relative compatibility identity with the queue's resolved
    // identity. Subsequent rename and runtime-state writes must follow the
    // same path representation as the loaded playlist.
    app.set_persistence_identity(
        resume_previous_track,
        Some(selected_location),
        persisted.last_track_position_ms,
    );
    if !resume_previous_track {
        return Vec::new();
    }

    let mut effects = app.begin_current_track();
    if persisted.last_track_position_ms > 0 {
        effects.push(harmonium::app::Effect::Audio(
            harmonium::audio::AudioCommand::SeekTo(Duration::from_millis(
                persisted.last_track_position_ms,
            )),
        ));
    }
    effects
}

/// Focus the panel that contains the startup queue, or the browser when it is empty.
fn focus_startup_panel(app: &mut App) {
    let panel = if app.playlist().is_empty() {
        Panel::Browser
    } else {
        Panel::Playlist
    };
    app.set_active_panel(panel);
}

/// Create a starter "Playlist" when the store is completely empty.
///
/// Users should never be dropped into an empty manager; a single default
/// playlist keeps the UI meaningful on a fresh launch. Existing saves are
/// left untouched so we never overwrite or duplicate what the user built.
fn ensure_default_playlist(app: &mut App) {
    // With no HOME the store is rooted at an empty path, so there is nowhere
    // sensible to persist a starter playlist. Writing there would fail loudly
    // and add nothing; skipping it leaves the user with an empty queue on a
    // session-scoped run, which is the least surprising outcome.
    if app.playlist_store().directory().as_os_str().is_empty() {
        tracing::debug!("no user data dir, skipping default playlist creation");
        return;
    }
    match app.playlist_store().list_names() {
        Ok(names) if !names.is_empty() => return,
        Ok(_) => {}
        Err(error) => {
            tracing::warn!("could not inspect saved playlists: {error}");
            return;
        }
    }
    let name = PlaylistName::try_from("Playlist").expect("built-in playlist name is valid");
    match app.playlist_store().save(&name, &Playlist::new()) {
        Ok(_) => {
            app.set_active_playlist_name(Some("Playlist".to_string()));
            tracing::info!("created default Playlist on fresh launch");
        }
        Err(error) => {
            tracing::warn!("could not create default playlist: {error}");
        }
    }
}

/// Drive drawing, input translation and event draining until quit or failure.
fn event_loop(
    terminal: &mut TerminalGuard,
    app: &mut App,
    theme: &mut Theme,
    services: &AppServices,
    artwork_backend: &mut ArtworkBackend,
) -> Result<()> {
    tracing::info!("event loop started");

    // Ratatui's diff only re-sends changed cells (ratatui#1606), so without a
    // full clear the terminal can keep stale cells when the full-window popup
    // opens or closes. Inside the popup the block always repaints the theme
    // background and the diff handles per-frame content changes, so we only
    // clear on that open/close transition (not on Sound navigation, which
    // would flicker).
    let mut last_settings_open = false;
    let _terminal_reader = TerminalReader::spawn(services.events().sender())
        .context("starting terminal reader failed")?;
    let mut dirty = true;
    let mut pending_event = None;

    loop {
        if !dirty {
            match services.events().recv_timeout(POLL_TIMEOUT) {
                Ok(event) => pending_event = Some(event),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => dirty = true,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        loop {
            let Some(app_event) = pending_event
                .take()
                .or_else(|| services.events().try_recv().ok())
            else {
                break;
            };
            dirty = true;
            let operation_id = operation_id_for_event(&app_event);
            match app_event {
                AppEvent::Key(key_event) => {
                    let effects = app.handle_key_event(key_event);
                    app.execute_effects(effects, services);
                }
                AppEvent::Resize(_, _) => {} // next draw picks up the new size
                AppEvent::Tick => {}         // reserved for future background workers
                AppEvent::Notification {
                    operation_id,
                    message,
                    ..
                } => {
                    if accepts_operation_notification(services, operation_id) {
                        app.push_notification(message);
                    }
                }
                AppEvent::BrowserDirectoryLoaded {
                    operation_id,
                    request_id,
                    dir,
                    restore_cursor_name,
                    entries,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects = app.apply_browser_directory_loaded(
                            request_id,
                            dir,
                            entries,
                            restore_cursor_name,
                        );
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::BrowserDirectoryValidated {
                    operation_id,
                    request_id,
                    path,
                    validation,
                    result,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects = app.apply_browser_directory_validated(
                            request_id, path, validation, result,
                        );
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::PlaybackProgress { snapshot } => {
                    // Progress arrives precomputed from the audio worker,
                    // so the loop only merges state and redraws on demand
                    app.apply_playback_progress(snapshot);
                }
                AppEvent::PlaybackStateChanged { snapshot } => {
                    app.apply_playback_progress(snapshot);
                }
                AppEvent::TrackEnded { track_index } => {
                    let effects = app.apply_track_ended(track_index);
                    app.execute_effects(effects, services);
                }
                AppEvent::CrossfadeCompleted {
                    track_index,
                    path,
                    elapsed,
                } => {
                    let effects = app.apply_crossfade_completed(track_index, path, elapsed);
                    app.execute_effects(effects, services);
                }
                AppEvent::ScanCompleted {
                    operation_id,
                    requested_dir,
                    tracks,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects = app.apply_scan_completed(requested_dir, tracks);
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::MetadataCompleted {
                    operation_id,
                    loaded,
                    failed,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects = app.apply_metadata_completed(loaded, failed);
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::SearchCompleted {
                    request_id,
                    operation_id,
                    scope,
                    results,
                    message,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        app.apply_search_completed(request_id, scope, results, message);
                    }
                }
                AppEvent::OutputsEnumerated {
                    operation_id,
                    request_id,
                    outputs,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        app.apply_outputs_enumerated(request_id, outputs);
                    }
                }
                AppEvent::ArtworkLoaded {
                    track_index,
                    operation_id,
                    artwork,
                } => {
                    // Artwork arrives with its resize worker attached, so
                    // applying it is a plain state merge that never blocks
                    if services.accepts_operation_completion(operation_id) {
                        app.apply_artwork_loaded(track_index, artwork.map(|boxed| *boxed));
                    }
                }
                AppEvent::LyricsLoaded {
                    track_index,
                    operation_id,
                    outcome,
                } => {
                    // The resolution already ran inside its blocking worker;
                    // merging the outcome is a plain state update.
                    if services.accepts_operation_completion(operation_id) {
                        app.apply_lyrics_loaded(track_index, outcome);
                    }
                }
                AppEvent::MetadataPrefillReady {
                    operation_id,
                    path,
                    fields,
                } => {
                    // The values arrived from the blocking worker, so filling
                    // the editor form is a plain state update.
                    if services.accepts_operation_completion(operation_id) {
                        app.apply_metadata_prefill_ready(path, fields);
                    }
                }
                AppEvent::PlaylistNamesCompleted {
                    operation_id,
                    request_id,
                    request,
                    result,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects =
                            app.apply_playlist_names_completed(request_id, request, result);
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::PlaylistSaved {
                    operation_id,
                    request_id,
                    name,
                    action,
                    result,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects = app.apply_playlist_saved(request_id, name, action, result);
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::PlaylistRenamed {
                    operation_id,
                    request_id,
                    old_name,
                    new_name,
                    action,
                    result,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects = app
                            .apply_playlist_renamed(request_id, old_name, new_name, action, result);
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::PlaylistDeleted {
                    operation_id,
                    request_id,
                    name,
                    cursor,
                    was_active,
                    result,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects = app
                            .apply_playlist_deleted(request_id, name, cursor, was_active, result);
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::PlaylistLoaded {
                    operation_id,
                    request_id,
                    name,
                    result,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects = app.apply_playlist_loaded(request_id, name, result);
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::ConfigSaved {
                    operation_id,
                    result,
                    ..
                } => {
                    apply_config_saved(app, services, operation_id, result);
                }
                AppEvent::RuntimeStateSaved { .. } => {}
                AppEvent::ThemeLoaded {
                    operation_id,
                    request_id,
                    visit_id,
                    themes_dir,
                    name,
                    theme_names,
                    result,
                    purpose,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let loaded_colors = result.clone().ok();
                        let accepted = app.apply_theme_loaded(
                            request_id,
                            visit_id,
                            themes_dir,
                            name,
                            theme_names,
                            result,
                            purpose,
                        );
                        if accepted && matches!(purpose, harmonium::app::ThemeLoadPurpose::Apply) {
                            if let Some(colors) = loaded_colors {
                                apply_runtime_theme(app, theme, colors);
                            }
                        }
                    }
                }
                AppEvent::ThemeSaved {
                    operation_id,
                    request_id,
                    visit_id,
                    themes_dir,
                    name,
                    colors,
                    result,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let (effects, saved) = app.apply_theme_saved(
                            request_id, visit_id, themes_dir, name, colors, result,
                        );
                        if let Some(colors) = saved {
                            apply_runtime_theme(app, theme, colors);
                        }
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::RenameCompleted {
                    operation_id,
                    request_id,
                    from,
                    to,
                    result,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects =
                            app.apply_rename_request_completed(request_id, from, to, result);
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::MetadataWriteCompleted {
                    operation_id,
                    path,
                    result,
                    fields,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects = app.apply_metadata_write_completed(path, result, fields);
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::StreamResolved {
                    request_id,
                    operation_id,
                    url,
                    track,
                    message,
                } => {
                    if services.accepts_operation_completion(operation_id) {
                        let effects = app.apply_stream_resolved(request_id, url, track, message);
                        app.execute_effects(effects, services);
                    }
                }
                AppEvent::SourceReady { url, generation } => {
                    // The audio engine just finished decoding the stream
                    // source; the unified loading flag must clear on the
                    // same frame so the Now Playing row stops showing the
                    // spinner and the progress bar takes over.
                    if let Some(generation) = generation {
                        app.apply_source_ready_with_generation(generation, url);
                    } else {
                        app.apply_source_ready(url);
                    }
                }
                AppEvent::SourceFailed { url, generation } => {
                    // A failed acquisition has no SourceReady boundary, so
                    // clear the stream spinner through the same URL gate.
                    if let Some(generation) = generation {
                        app.apply_source_failed_with_generation(generation, url);
                    } else {
                        app.apply_source_failed(url);
                    }
                }
            }
            services.release_operation_event(operation_id);
        }

        if dirty {
            // Apply completed artwork responses and render only after the bus
            // has been drained. Worker publication or terminal input is the
            // wake-up source; the timeout is retained solely for spinner and
            // playback-clock ticks.
            app.apply_artwork_resizes();
            if app.take_native_artwork_failure() {
                app.report_native_artwork_failure(artwork_backend);
            }

            let settings_open = matches!(app.active_popup_ref(), Some(Popup::Settings { .. }));
            if settings_open != last_settings_open {
                terminal
                    .terminal_mut()
                    .clear()
                    .context("clearing terminal for popup transition failed")?;
                last_settings_open = settings_open;
            }

            let area = terminal
                .terminal_mut()
                .size()
                .context("reading terminal size for frame metrics failed")?;
            let mut metrics = ui::frame_metrics(area.into());
            metrics.artwork_target = ui::artwork_resize_target(area.into(), app);
            app.tick_frame(metrics, std::time::Instant::now());
            terminal
                .terminal_mut()
                .draw(|frame| ui::render(frame, app, theme))
                .context("drawing a frame failed")?;

            let desired_overlay = ui::artwork_overlay(area.into(), app);
            app.reconcile_artwork(artwork_backend, desired_overlay);
            dirty = false;
        }

        if app.should_quit() {
            break;
        }
    }

    tracing::info!("event loop finished");
    Ok(())
}

/// Queue terminal input without losing it when critical worker events fill the
/// bounded bus. The next loop iteration retries after the receiver drains it.
#[cfg(test)]
fn enqueue_terminal_event(events: &EventBus, event: AppEvent) -> Result<Option<AppEvent>> {
    match event {
        AppEvent::Key(key) => {
            let retry = key;
            match events.send(AppEvent::Key(key)) {
                Ok(()) => Ok(None),
                Err(EventSendError::Full) => Ok(Some(AppEvent::Key(retry))),
                Err(EventSendError::Disconnected) => {
                    Err(anyhow::anyhow!("event bus receiver went away"))
                }
            }
        }
        AppEvent::Resize(width, height) => match events.send(AppEvent::Resize(width, height)) {
            Ok(()) => Ok(None),
            Err(EventSendError::Full) => Ok(Some(AppEvent::Resize(width, height))),
            Err(EventSendError::Disconnected) => {
                Err(anyhow::anyhow!("event bus receiver went away"))
            }
        },
        _ => unreachable!("terminal translation only produces key and resize events"),
    }
}

fn main() -> Result<()> {
    if std::env::args().any(|arg| arg == "--pipewire-test") {
        return pipewire_self_test();
    }
    run()
}

/// Open a native PipeWire output sink, push a one second tone, and exit. Set
/// `HARMONIUM_PIPEWIRE_TEST_TARGET` to a stable `node.name` to validate
/// concrete target routing; without it the session default is used.
fn pipewire_self_test() -> Result<()> {
    let target = std::env::var("HARMONIUM_PIPEWIRE_TEST_TARGET").ok();
    let mut sink = harmonium::audio::PipeWireSink::new(44100, 2, target.as_deref())
        .context("could not open the PipeWire output stream")?;
    tracing::info!(
        target = ?target,
        "pipewire self test: {} — push tone for 1s, check wpctl for 'harmonium-output'",
        sink.description(),
    );

    let rate = 44100u32;
    let channels = 2usize;
    let frames = rate as usize;
    let mut samples = Vec::with_capacity(frames * channels);
    for i in 0..frames {
        let sample = (std::f32::consts::PI * 2.0 * 440.0 * i as f32 / rate as f32).sin() * 0.25;
        for _ in 0..channels {
            samples.push(sample);
        }
    }

    for chunk in samples.chunks(4410) {
        if !matches!(
            sink.push_samples(chunk),
            harmonium::audio::PushSamplesResult::Accepted
        ) {
            anyhow::bail!("pipewire stream could not accept the tone chunk");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Let the stream drain before tearing down.
    std::thread::sleep(Duration::from_millis(200));
    sink.shutdown();
    tracing::info!("pipewire self test finished");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::test_support::unique_temp_dir;
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use harmonium::config::KeysConfig;
    use harmonium::playlist::Playlist;
    use harmonium::stream::StreamKind;
    use std::sync::{Arc, OnceLock};

    #[test]
    fn terminal_key_publication_is_bounded_when_queue_is_full() {
        let bus = harmonium::event::EventBus::with_capacity(1);
        bus.send_critical(AppEvent::TrackEnded { track_index: 1 })
            .expect("critical sentinel fits");
        let started = std::time::Instant::now();
        let result = publish_terminal_event(
            &bus.sender(),
            AppEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        );

        assert_eq!(result, Err(EventSendError::Full));
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn runtime_theme_application_rejects_invalid_colors_without_overwriting_theme() {
        let mut app = App::new();
        let mut theme = Theme::default();
        let original = theme;
        let mut colors = theme.to_colors();
        colors.background = "definitely-not-a-color".to_string();

        assert!(!apply_runtime_theme(&mut app, &mut theme, colors));

        assert_eq!(theme, original);
    }

    #[test]
    fn startup_theme_loading_surfaces_invalid_user_colors() {
        let root = unique_temp_dir("startup-invalid-theme");
        let themes_dir = root.join("themes");
        std::fs::create_dir_all(&themes_dir).expect("themes directory");
        std::fs::write(
            themes_dir.join("user.toml"),
            "[colors]\nbackground = \"not-a-color\"\n",
        )
        .expect("invalid theme fixture");

        let mut app = App::new();
        let theme = load_startup_theme(&mut app, &themes_dir, "user");

        assert_eq!(theme, Theme::default());
        assert!(
            app.notifications()
                .iter()
                .any(|message| message.contains("Could not load theme")
                    && message.contains("background"))
        );
    }

    fn volume(value: u16) -> harmonium::audio::VolumePercent {
        harmonium::audio::VolumePercent::new(value).expect("test volume must be valid")
    }

    struct FakeTerminalControl {
        log: Arc<Mutex<Vec<&'static str>>>,
        fail_enter: bool,
        fail_disable: bool,
        fail_leave: bool,
    }

    impl FakeTerminalControl {
        fn record(&self, operation: &'static str) {
            self.log.lock().unwrap().push(operation);
        }
    }

    impl TerminalControl for FakeTerminalControl {
        fn enable_raw_mode(&mut self) -> std::io::Result<()> {
            self.record("enable_raw_mode");
            Ok(())
        }

        fn enter_alternate_screen(&mut self) -> std::io::Result<()> {
            self.record("enter_alternate_screen");
            if self.fail_enter {
                Err(std::io::Error::other("alternate screen unavailable"))
            } else {
                Ok(())
            }
        }

        fn disable_raw_mode(&mut self) -> std::io::Result<()> {
            self.record("disable_raw_mode");
            if self.fail_disable {
                Err(std::io::Error::other("raw mode restoration failed"))
            } else {
                Ok(())
            }
        }

        fn leave_alternate_screen(&mut self) -> std::io::Result<()> {
            self.record("leave_alternate_screen");
            if self.fail_leave {
                Err(std::io::Error::other("alternate screen restoration failed"))
            } else {
                Ok(())
            }
        }
    }

    struct FakeInput {
        ready: bool,
        event: Option<Event>,
        poll_error: Option<std::io::Error>,
        read_error: Option<std::io::Error>,
        poll_calls: usize,
        read_calls: usize,
    }

    impl FakeInput {
        fn ready(event: Event) -> Self {
            Self {
                ready: true,
                event: Some(event),
                poll_error: None,
                read_error: None,
                poll_calls: 0,
                read_calls: 0,
            }
        }

        fn read_error() -> Self {
            Self {
                ready: true,
                event: None,
                poll_error: None,
                read_error: Some(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "input read failed",
                )),
                poll_calls: 0,
                read_calls: 0,
            }
        }

        fn poll_error() -> Self {
            Self {
                ready: false,
                event: None,
                poll_error: Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "input poll failed",
                )),
                read_error: None,
                poll_calls: 0,
                read_calls: 0,
            }
        }
    }

    impl TerminalInput for FakeInput {
        fn poll(&mut self, _timeout: Duration) -> std::io::Result<bool> {
            self.poll_calls += 1;
            self.poll_error.take().map_or(Ok(self.ready), Err)
        }

        fn read(&mut self) -> std::io::Result<Event> {
            self.read_calls += 1;
            if let Some(error) = self.read_error.take() {
                return Err(error);
            }
            Ok(self.event.take().expect("fake event was not configured"))
        }
    }

    #[test]
    fn terminal_lifecycle_restores_raw_mode_and_alternate_screen_on_drop() {
        let log = Arc::new(Mutex::new(Vec::new()));
        {
            let guard = TerminalLifecycleGuard::new(FakeTerminalControl {
                log: Arc::clone(&log),
                fail_enter: false,
                fail_disable: false,
                fail_leave: false,
            })
            .expect("fake terminal should enter interactive mode");

            assert_eq!(
                guard.lifecycle,
                TerminalLifecycle {
                    raw_mode: true,
                    alternate_screen: true,
                }
            );
        }

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "disable_raw_mode",
                "leave_alternate_screen",
            ]
        );
    }

    #[test]
    fn terminal_lifecycle_cleans_raw_mode_when_alternate_screen_fails() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let result = TerminalLifecycleGuard::new(FakeTerminalControl {
            log: Arc::clone(&log),
            fail_enter: true,
            fail_disable: false,
            fail_leave: false,
        });

        assert!(result.is_err());
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "enable_raw_mode",
                "enter_alternate_screen",
                "disable_raw_mode",
            ]
        );
    }

    #[test]
    fn terminal_lifecycle_retries_failed_restoration_without_clearing_ownership() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut guard = TerminalLifecycleGuard {
            control: FakeTerminalControl {
                log: Arc::clone(&log),
                fail_enter: false,
                fail_disable: true,
                fail_leave: false,
            },
            lifecycle: TerminalLifecycle {
                raw_mode: true,
                alternate_screen: true,
            },
        };

        guard.restore();
        assert_eq!(
            guard.lifecycle,
            TerminalLifecycle {
                raw_mode: true,
                alternate_screen: false,
            }
        );

        guard.control.fail_disable = false;
        guard.restore();
        assert_eq!(
            guard.lifecycle,
            TerminalLifecycle {
                raw_mode: false,
                alternate_screen: false,
            }
        );
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "disable_raw_mode",
                "leave_alternate_screen",
                "disable_raw_mode",
            ]
        );
    }

    #[test]
    fn panic_hook_restores_before_calling_the_previous_hook() {
        static PANIC_HOOK_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _lock = PANIC_HOOK_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let original_hook = panic::take_hook();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let previous_calls = Arc::clone(&calls);
        panic::set_hook(Box::new(move |_| {
            previous_calls.lock().unwrap().push("previous");
        }));
        let artwork_calls = Arc::clone(&calls);
        let restore_calls = Arc::clone(&calls);

        install_panic_hook_with(
            move || {
                restore_calls.lock().unwrap().push("restore");
            },
            move || {
                artwork_calls.lock().unwrap().push("artwork");
            },
        );
        let panic_result = std::panic::catch_unwind(|| panic!("panic hook test"));
        let _installed_hook = panic::take_hook();
        panic::set_hook(original_hook);

        assert!(panic_result.is_err());
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["artwork", "restore", "previous"]
        );
    }

    #[test]
    fn terminal_input_translates_events_through_the_test_port() {
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        let mut input = FakeInput::ready(Event::Key(key));

        let result = next_terminal_input(&mut input).expect("fake input should succeed");

        assert!(matches!(result, Some(AppEvent::Key(event)) if event == key));
        assert_eq!(input.poll_calls, 1);
        assert_eq!(input.read_calls, 1);
    }

    #[test]
    fn terminal_input_read_errors_are_returned_to_the_event_loop() {
        let mut input = FakeInput::read_error();

        let error = next_terminal_input(&mut input).expect_err("read failure must be visible");

        assert_eq!(error.to_string(), "reading terminal events failed");
        assert_eq!(input.poll_calls, 1);
        assert_eq!(input.read_calls, 1);
    }

    #[test]
    fn terminal_input_poll_errors_are_returned_to_the_event_loop() {
        let mut input = FakeInput::poll_error();

        let error = next_terminal_input(&mut input).expect_err("poll failure must be visible");

        assert_eq!(error.to_string(), "polling terminal events failed");
        assert_eq!(input.poll_calls, 1);
        assert_eq!(input.read_calls, 0);
    }

    #[test]
    fn stale_config_save_error_is_rejected_by_the_operation_gate() {
        let services = AppServices::new().expect("services construction");
        let operation = services
            .spawn_operation("config-save", OperationKind::Generic, |_| async { Ok(()) })
            .expect("config operation registration");
        let operation_id = operation.id();
        assert!(operation.cancel(), "test operation must become stale");

        let mut app = App::new();
        assert!(!apply_config_saved(
            &mut app,
            &services,
            operation_id,
            Err(harmonium::error::WorkerError::message(
                "config-save",
                "stale config failure",
            )),
        ));

        services.release_operation_event(Some(operation_id));
        services.shutdown();
    }

    #[test]
    fn stale_operation_notification_is_rejected_after_publication() {
        let services = AppServices::new().expect("services construction");
        let first = services
            .spawn_operation(
                "first-notification",
                OperationKind::Generic,
                |_operation| async { std::future::pending::<anyhow::Result<()>>().await },
            )
            .expect("first operation registers");
        services
            .events()
            .send_critical(AppEvent::Notification {
                kind: harmonium::event::EffectErrorKind::Task,
                operation_id: Some(first.id()),
                message: "stale failure".to_string(),
            })
            .expect("failure notification publishes");
        let second = services
            .spawn_operation(
                "second-notification",
                OperationKind::Generic,
                |_operation| async { std::future::pending::<anyhow::Result<()>>().await },
            )
            .expect("second operation registers");

        let published = services
            .events()
            .try_recv()
            .expect("published notification remains queued");
        let AppEvent::Notification { operation_id, .. } = published else {
            panic!("expected the published failure notification");
        };
        assert!(
            !accepts_operation_notification(&services, operation_id),
            "supersession after publication must reject the stale notification"
        );
        assert!(accepts_operation_notification(&services, Some(second.id())));
        assert!(accepts_operation_notification(&services, None));
        services.shutdown();
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires an explicit PTY subprocess run"]
    fn terminal_guard_restores_pty_after_quit() {
        use std::fs::File;
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::process::{Command, Stdio};

        let mut master_fd = -1;
        let mut slave_fd = -1;
        let openpty_result = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if openpty_result != 0 {
            eprintln!("skipping PTY test: openpty is unavailable");
            return;
        }

        let mut master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let mut baseline = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut baseline) },
            0,
            "could not read PTY baseline attributes"
        );

        let stdin = slave.try_clone().expect("clone PTY stdin");
        let stdout = slave.try_clone().expect("clone PTY stdout");
        let stderr = slave.try_clone().expect("clone PTY stderr");
        let mut child = Command::new(std::env::current_exe().expect("test executable path"))
            .args([
                "--ignored",
                "--exact",
                "tests::terminal_guard_pty_child",
                "--nocapture",
            ])
            .env("HARMONIUM_TERMINAL_PTY_CHILD", "1")
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn PTY lifecycle child");

        std::thread::sleep(Duration::from_millis(100));
        master.write_all(b"q").expect("send quit key to PTY child");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait for PTY child") {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("PTY lifecycle child did not exit within five seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "PTY lifecycle child failed: {status}");

        let mut restored = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut restored) },
            0,
            "could not read PTY restored attributes"
        );
        assert_eq!(baseline.c_iflag, restored.c_iflag);
        assert_eq!(baseline.c_oflag, restored.c_oflag);
        assert_eq!(baseline.c_cflag, restored.c_cflag);
        assert_eq!(baseline.c_lflag, restored.c_lflag);
        assert_eq!(baseline.c_cc, restored.c_cc);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "only run as the child of terminal_guard_restores_pty_after_quit"]
    fn terminal_guard_pty_child() {
        if std::env::var_os("HARMONIUM_TERMINAL_PTY_CHILD").is_none() {
            return;
        }

        let mut terminal = TerminalGuard::new().expect("enter PTY terminal mode");
        terminal
            .attach_ratatui()
            .expect("attach ratatui to PTY terminal");
        let mut input = CrosstermInput;
        loop {
            if let Some(AppEvent::Key(key)) =
                next_terminal_input(&mut input).expect("read PTY input")
                && key.code == KeyCode::Char('q')
            {
                break;
            }
        }
    }

    #[test]
    fn translate_event_key() {
        let key_event = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        let event = Event::Key(key_event);

        let result = translate_event(event);

        let inner = result.expect("expected Some(AppEvent::Key) for Event::Key");
        match inner {
            AppEvent::Key(k) => {
                assert_eq!(k.code, KeyCode::Char('q'));
            }
            other => panic!("expected AppEvent::Key, got {other:?}"),
        }
    }

    #[test]
    fn translate_event_resize() {
        let event = Event::Resize(120, 40);

        let result = translate_event(event);

        let inner = result.expect("expected Some(AppEvent::Resize) for Event::Resize");
        match inner {
            AppEvent::Resize(w, h) => {
                assert_eq!(w, 120);
                assert_eq!(h, 40);
            }
            other => panic!("expected AppEvent::Resize, got {other:?}"),
        }
    }

    #[test]
    fn translate_event_mouse_ignored() {
        let event = Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });

        assert!(translate_event(event).is_none(), "mouse events are ignored");
    }

    #[test]
    fn translate_event_focus_ignored() {
        assert!(
            translate_event(Event::FocusGained).is_none(),
            "FocusGained is ignored"
        );
        assert!(
            translate_event(Event::FocusLost).is_none(),
            "FocusLost is ignored"
        );
    }

    #[test]
    fn terminal_input_is_retained_when_the_event_bus_is_full() {
        let bus = EventBus::with_capacity(1);
        bus.send(AppEvent::Notification {
            kind: harmonium::event::EffectErrorKind::Task,
            operation_id: None,
            message: "worker event".into(),
        })
        .expect("critical event fits");
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);

        let pending = enqueue_terminal_event(&bus, AppEvent::Key(key));

        assert!(matches!(
            pending.expect("full input is not a fatal error"),
            Some(AppEvent::Key(retried)) if retried == key
        ));
        assert!(matches!(
            bus.try_recv(),
            Ok(AppEvent::Notification { message, .. }) if message == "worker event"
        ));
        assert!(
            enqueue_terminal_event(&bus, AppEvent::Key(key))
                .expect("space is available")
                .is_none()
        );
    }

    #[test]
    fn startup_migration_persists_once_and_keeps_runtime_state() {
        let root = unique_temp_dir("startup-migration");
        let data_dir = root.path().join("data");
        let config_dir = root.path().join("config");
        fs::create_dir_all(&data_dir).expect("data directory");

        fs::create_dir_all(&config_dir).expect("config directory");
        fs::write(
            config_dir.join("config.toml"),
            "[general]\nvolume_percent = 15\n",
        )
        .expect("initial config");
        fs::write(
            data_dir.join("state.toml"),
            "# Keep this runtime comment.\nrepeat_mode = \"track\"\nshuffle = true\nlast_playlist = \"Rock\"\nlast_track_path = \"/music/rock.mp3\"\nlast_track_position_ms = 42500\nvolume_percent = 60\nconfirm_quit = false\nresume_previous_track = true\nbrowser_directory = \"/music\"\nartwork_visible = false\nunknown_state = \"keep\"\n",
        )
        .expect("legacy state fixture");

        let mut config = AppConfig::load(&config_dir);
        assert!(migrate_legacy_preferences_at_startup(
            &data_dir,
            &config_dir,
            &mut config
        ));

        let migrated_config = AppConfig::load(&config_dir);
        assert_eq!(migrated_config.general.volume_percent, volume(15));
        assert!(!migrated_config.general.confirm_quit);
        assert!(migrated_config.general.resume_previous_track);
        assert_eq!(migrated_config.general.browser_directory, "/music");
        assert!(!migrated_config.general.artwork_visible);

        let state_contents = fs::read_to_string(data_dir.join("state.toml")).expect("state");
        assert!(state_contents.contains("# Keep this runtime comment."));
        assert!(state_contents.contains("unknown_state = \"keep\""));
        assert!(!state_contents.contains("volume_percent"));
        assert!(!state_contents.contains("confirm_quit"));
        assert!(!state_contents.contains("resume_previous_track"));
        assert!(!state_contents.contains("browser_directory"));
        assert!(!state_contents.contains("artwork_visible"));

        let state = PersistedState::load(&data_dir);
        assert_eq!(
            state.repeat_mode,
            harmonium::playback_mode::RepeatMode::Track
        );
        assert!(state.shuffle);
        assert_eq!(state.last_playlist.as_deref(), Some("Rock"));
        assert_eq!(state.last_track_path.as_deref(), Some("/music/rock.mp3"));
        assert_eq!(state.last_track_position_ms, 42_500);

        let mut second_startup_config = AppConfig::load(&config_dir);
        assert!(!migrate_legacy_preferences_at_startup(
            &data_dir,
            &config_dir,
            &mut second_startup_config
        ));
        assert_eq!(second_startup_config, migrated_config);
    }

    #[test]
    fn failed_migration_config_write_keeps_legacy_values_for_retry() {
        let root = unique_temp_dir("startup-migration-failure");
        let data_dir = root.path().join("data");
        let config_path = root.path().join("blocked-config");
        fs::create_dir_all(&data_dir).expect("data directory");
        fs::write(
            data_dir.join("state.toml"),
            "repeat_mode = \"all\"\nshuffle = true\nlast_playlist = \"Jazz\"\nlast_track_position_ms = 9000\nvolume_percent = 60\n",
        )
        .expect("legacy state fixture");
        fs::write(&config_path, "not a directory").expect("blocked config path");

        let mut config = AppConfig::default();
        assert!(migrate_legacy_preferences_at_startup(
            &data_dir,
            &config_path,
            &mut config
        ));
        assert!(
            fs::read_to_string(data_dir.join("state.toml"))
                .expect("state after failed config write")
                .contains("volume_percent = 60")
        );

        // A later runtime-state save must not erase migration input before the
        // next startup gets a chance to retry the failed config write.
        PersistedState::load(&data_dir).save(&data_dir);
        let state_after_runtime_save =
            fs::read_to_string(data_dir.join("state.toml")).expect("state after runtime save");
        assert!(state_after_runtime_save.contains("volume_percent = 60"));

        fs::remove_file(&config_path).expect("remove blocked config path");
        let mut retry_config = AppConfig::default();
        assert!(migrate_legacy_preferences_at_startup(
            &data_dir,
            &config_path,
            &mut retry_config
        ));
        assert_eq!(
            AppConfig::load(&config_path).general.volume_percent,
            volume(60)
        );
        assert!(
            !fs::read_to_string(data_dir.join("state.toml"))
                .expect("state after retry")
                .contains("volume_percent")
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_state_cleanup_does_not_override_a_later_config_edit() {
        let root = unique_temp_dir("startup-migration-cleanup-failure");
        let data_dir = root.path().join("data");
        let config_dir = root.path().join("config");
        let state_target = root.path().join("state-target.toml");
        fs::create_dir_all(&data_dir).expect("data directory");
        fs::write(&state_target, "volume_percent = 60\nconfirm_quit = false\n")
            .expect("legacy state fixture");
        std::os::unix::fs::symlink(&state_target, data_dir.join("state.toml"))
            .expect("state symlink");

        let mut config = AppConfig::default();
        assert!(migrate_legacy_preferences_at_startup(
            &data_dir,
            &config_dir,
            &mut config
        ));
        assert_eq!(
            AppConfig::load(&config_dir).general.volume_percent,
            volume(60)
        );

        let mut edited_config = AppConfig::load(&config_dir);
        edited_config.general.volume_percent = volume(80);
        edited_config.save(&config_dir).expect("user config edit");

        let mut retry_config = AppConfig::load(&config_dir);
        assert!(!migrate_legacy_preferences_at_startup(
            &data_dir,
            &config_dir,
            &mut retry_config
        ));
        assert_eq!(retry_config.general.volume_percent, volume(80));
        assert_eq!(
            AppConfig::load(&config_dir).general.volume_percent,
            volume(80)
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_config_save_keeps_only_missing_legacy_fields_pending() {
        let root = unique_temp_dir("startup-migration-partial-failure");
        let data_dir = root.path().join("data");
        let config_dir = root.path().join("config");
        let config_target = root.path().join("config-target.toml");
        fs::create_dir_all(&data_dir).expect("data directory");
        fs::create_dir_all(&config_dir).expect("config directory");
        fs::write(&config_target, "[general]\nvolume_percent = 75\n").expect("config target");
        std::os::unix::fs::symlink(&config_target, config_dir.join("config.toml"))
            .expect("config symlink");
        fs::write(
            data_dir.join("state.toml"),
            "volume_percent = 60\nconfirm_quit = false\n",
        )
        .expect("legacy state fixture");

        let mut config = AppConfig::load(&config_dir);
        assert_eq!(config.general.volume_percent, volume(75));
        assert!(migrate_legacy_preferences_at_startup(
            &data_dir,
            &config_dir,
            &mut config
        ));
        assert_eq!(config.general.volume_percent, volume(75));
        assert!(!config.general.confirm_quit);
        let state_after_failure =
            fs::read_to_string(data_dir.join("state.toml")).expect("state after failure");
        assert!(state_after_failure.contains("volume_percent = 60"));
        assert!(state_after_failure.contains("confirm_quit = false"));

        fs::remove_file(config_dir.join("config.toml")).expect("remove config symlink");
        fs::write(
            config_dir.join("config.toml"),
            "[general]\nvolume_percent = 75\n",
        )
        .expect("replace config");
        let mut retry_config = AppConfig::load(&config_dir);
        assert!(migrate_legacy_preferences_at_startup(
            &data_dir,
            &config_dir,
            &mut retry_config
        ));
        let migrated_config = AppConfig::load(&config_dir);
        assert_eq!(migrated_config.general.volume_percent, volume(75));
        assert!(!migrated_config.general.confirm_quit);
        let state_after_retry =
            fs::read_to_string(data_dir.join("state.toml")).expect("state after retry");
        assert!(!state_after_retry.contains("volume_percent"));
        assert!(!state_after_retry.contains("confirm_quit"));
    }

    #[test]
    fn startup_focus_uses_playlist_panel_for_non_empty_queue() {
        let mut app = App::new();
        app.extend_playlist([PathBuf::from("/music/track.mp3")]);

        focus_startup_panel(&mut app);

        assert_eq!(app.active_panel(), Panel::Playlist);
    }

    #[test]
    fn startup_focus_uses_browser_panel_for_empty_queue() {
        let mut app = App::new();
        app.set_active_panel(Panel::Playlist);

        focus_startup_panel(&mut app);

        assert_eq!(app.active_panel(), Panel::Browser);
    }

    #[test]
    fn restore_last_playlist_loads_an_existing_file() {
        let dir = unique_temp_dir("restore-ok");
        let store = PlaylistStore::for_dir(dir.path());
        let saved_path = PathBuf::from("/music/rock.mp3");
        let mut playlist = Playlist::new();
        playlist.extend([
            harmonium::track::Track::local("/music/other.mp3"),
            harmonium::track::Track::local(saved_path.clone()),
        ]);
        store
            .save(&PlaylistName::try_from("Rock").unwrap(), &playlist)
            .unwrap();
        let mut app = App::from_config_and_store(KeysConfig::default(), store);
        let persisted = PersistedState {
            last_playlist: Some("Rock".to_string()),
            last_track_path: Some(saved_path.to_string_lossy().into_owned()),
            ..PersistedState::default()
        };

        restore_last_playlist(&mut app, &persisted);
        let effects = restore_last_track(&mut app, &persisted, false);

        assert_eq!(app.active_playlist_name(), Some("Rock"));
        assert_eq!(app.playlist().cursor(), 1);
        assert!(
            effects.is_empty(),
            "selection restoration must not autoplay"
        );
    }

    #[test]
    fn restore_last_track_matches_legacy_relative_state_to_m3u_identity() {
        let dir = unique_temp_dir("restore-relative");
        let store = PlaylistStore::for_dir(dir.path());
        fs::write(dir.path().join("Rock.m3u8"), "#EXTM3U\nsong.mp3\n").expect("playlist fixture");
        let mut app = App::from_config_and_store(KeysConfig::default(), store);
        let persisted = PersistedState {
            last_playlist: Some("Rock".to_string()),
            last_track_path: Some("song.mp3".to_string()),
            ..PersistedState::default()
        };

        restore_last_playlist(&mut app, &persisted);
        let effects = restore_last_track(&mut app, &persisted, false);

        assert_eq!(app.playlist().cursor(), 0);
        assert!(effects.is_empty());
        assert_eq!(
            app.playlist().tracks()[0].path(),
            Some(dir.path().join("song.mp3").as_path())
        );
    }

    #[test]
    fn restore_last_playlist_skips_a_missing_name() {
        let dir = unique_temp_dir("restore-missing");
        let store = PlaylistStore::for_dir(dir.path());
        let mut app = App::from_config_and_store(KeysConfig::default(), store);
        let persisted = PersistedState {
            last_playlist: Some("Ghost".to_string()),
            ..PersistedState::default()
        };

        restore_last_playlist(&mut app, &persisted);
        let effects = restore_last_track(&mut app, &persisted, false);

        assert_eq!(app.active_playlist_name(), None);
        assert!(app.playlist().is_empty(), "queue stays empty on miss");
        assert!(effects.is_empty());
    }

    #[test]
    fn restore_last_track_selects_without_playback_when_resume_is_disabled() {
        let mut app = App::new();
        let saved_path = PathBuf::from("/music/saved.mp3");
        app.set_playlist_viewport_height(1);
        app.extend_playlist([PathBuf::from("/music/other.mp3"), saved_path.clone()]);
        let persisted = PersistedState {
            last_track_path: Some(saved_path.to_string_lossy().into_owned()),
            last_track_position_ms: 12_000,
            ..PersistedState::default()
        };

        focus_startup_panel(&mut app);
        let effects = restore_last_track(&mut app, &persisted, false);

        assert_eq!(app.active_panel(), Panel::Playlist);
        assert_eq!(app.playlist().cursor(), 1);
        assert_eq!(app.playlist_scroll_offset(), 1);
        assert!(
            effects.is_empty(),
            "resume disabled must not dispatch audio"
        );
        assert_eq!(app.playback().track_index, None);
    }

    #[test]
    fn restore_last_stream_selects_without_playback_when_resume_is_disabled() {
        let mut app = App::new();
        let saved_url = "https://radio.example.com/live";
        app.set_playlist_viewport_height(1);
        app.extend_playlist([PathBuf::from("/music/other.mp3")]);
        app.extend_playlist_tracks([harmonium::track::Track::from_stream(
            url::Url::parse(saved_url).expect("valid URL"),
            StreamKind::Http,
        )]);
        let persisted = PersistedState {
            last_track_path: Some(saved_url.to_string()),
            last_track_position_ms: 12_000,
            ..PersistedState::default()
        };

        focus_startup_panel(&mut app);
        let effects = restore_last_track(&mut app, &persisted, false);

        assert_eq!(app.active_panel(), Panel::Playlist);
        assert_eq!(app.playlist().cursor(), 1);
        assert_eq!(app.playlist_scroll_offset(), 1);
        assert!(
            effects.is_empty(),
            "resume disabled must not autoplay a stream"
        );
        assert_eq!(app.playback().track_index, None);
    }

    #[test]
    fn restore_last_stream_autoplays_and_seeks_when_resume_is_enabled() {
        let mut app = App::new();
        let saved_url = "https://radio.example.com/live";
        app.set_playlist_viewport_height(1);
        app.extend_playlist([PathBuf::from("/music/other.mp3")]);
        app.extend_playlist_tracks([harmonium::track::Track::from_stream(
            url::Url::parse(saved_url).expect("valid URL"),
            StreamKind::Http,
        )]);
        let persisted = PersistedState {
            last_track_path: Some(saved_url.to_string()),
            last_track_position_ms: 12_000,
            ..PersistedState::default()
        };

        focus_startup_panel(&mut app);
        let effects = restore_last_track(&mut app, &persisted, true);

        assert_eq!(app.active_panel(), Panel::Playlist);
        assert_eq!(app.playlist().cursor(), 1);
        assert_eq!(app.playlist_scroll_offset(), 1);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            harmonium::app::Effect::Audio(harmonium::audio::AudioCommand::Play { .. })
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            harmonium::app::Effect::Audio(harmonium::audio::AudioCommand::SeekTo(position))
                if *position == Duration::from_millis(12_000)
        )));
        assert_eq!(app.playback().track_index, Some(1));
    }

    #[test]
    fn restore_last_track_ignores_a_missing_saved_path() {
        let mut app = App::new();
        app.extend_playlist([PathBuf::from("/music/other.mp3")]);
        let persisted = PersistedState {
            last_track_path: Some("/music/missing.mp3".to_string()),
            ..PersistedState::default()
        };

        let effects = restore_last_track(&mut app, &persisted, true);

        assert_eq!(app.playlist().cursor(), 0);
        assert!(effects.is_empty());
        assert_eq!(app.playback().track_index, None);
    }

    #[test]
    fn restore_last_playlist_is_a_no_op_without_a_saved_name() {
        let dir = unique_temp_dir("restore-none");
        let store = PlaylistStore::for_dir(dir.path());
        let mut app = App::from_config_and_store(KeysConfig::default(), store);

        restore_last_playlist(&mut app, &PersistedState::default());

        assert_eq!(app.active_playlist_name(), None);
    }

    #[test]
    fn persisted_state_captures_the_active_playlist_name() {
        let mut app = App::new();
        app.set_active_playlist_name(Some("Jazz".to_string()));

        let state = app.persisted_state();

        assert_eq!(state.last_playlist.as_deref(), Some("Jazz"));
    }

    #[test]
    fn persisted_state_keeps_local_location_when_resume_is_disabled() {
        let data_dir = unique_temp_dir("state-local-no-resume");
        let config_dir = data_dir.join("config");
        let mut app = App::new();
        app.set_config_paths(
            AppConfig::default(),
            config_dir,
            data_dir.path().to_path_buf(),
        );
        app.set_persistence_identity(
            false,
            Some(TrackLocation::local("/music/saved.mp3")),
            12_000,
        );

        let effects = app.persist_runtime_state();
        let services = AppServices::new().expect("services construction");
        app.execute_effects(effects, &services);
        for _ in 0..2 {
            let event = services
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("persistence completion");
            let operation_id = match event {
                AppEvent::RuntimeStateSaved {
                    operation_id,
                    result: Ok(true),
                    ..
                }
                | AppEvent::ConfigSaved { operation_id, .. } => operation_id,
                other => panic!("expected persistence completion, got {other:?}"),
            };
            services.release_operation_event(Some(operation_id));
        }
        let state = PersistedState::load(&data_dir);

        let expected_path = TrackLocation::local("/music/saved.mp3").to_persisted();
        assert_eq!(
            state.last_track_path.as_deref(),
            Some(expected_path.as_str())
        );
        assert_eq!(state.last_track_position_ms, 12_000);
        services.shutdown();
    }
}
